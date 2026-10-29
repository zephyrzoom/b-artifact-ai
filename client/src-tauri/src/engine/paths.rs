/**
 * 路径工具（§6.6 文件名与平台兼容性）。
 *
 * 核心原则：服务端永远存规范化的 posix 相对路径（UTF-8 NFC、`/` 分隔），
 * 兼容性问题全部在客户端检出/提交边界消化。
 */
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use unicode_normalization::UnicodeNormalization;

/// 单段名上限（字节）。
pub const MAX_SEGMENT_BYTES: usize = 255;
/// 全路径上限（字节）。
pub const MAX_PATH_BYTES: usize = 1024;

/// Windows 保留设备名（不区分大小写，含扩展名也算：CON.txt 同样非法）。
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 反斜杠转斜杠。
pub fn to_posix(p: &str) -> String {
    p.replace('\\', "/")
}

/// NFC 规范化。
pub fn normalize_nfc(p: &str) -> String {
    p.nfc().collect()
}

/// 判断字符串是否已是 NFC 形态。
pub fn is_nfc(p: &str) -> bool {
    normalize_nfc(p) == p
}

/// UTF-8 字节长度。
pub fn utf8_len(s: &str) -> usize {
    s.len()
}

/// 拆段（已假定是 posix 相对路径）。
pub fn segments(p: &str) -> Vec<&str> {
    p.split('/').filter(|s| !s.is_empty()).collect()
}

/// posix 路径拼接，自动去掉多余分隔符与 `.` 段。
pub fn join_path(parts: &[&str]) -> String {
    let mut out: Vec<String> = Vec::new();
    for raw in parts {
        if raw.is_empty() {
            continue;
        }
        let posix = to_posix(raw);
        for seg in posix.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            out.push(seg.to_string());
        }
    }
    out.join("/")
}

/// 父路径；根节点（无 `/`）返回 `None`。
pub fn parent_of(p: &str) -> Option<&str> {
    p.rfind('/').map(|i| &p[..i])
}

/// 末段名。
pub fn base_name(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

/// `prefix` 是否覆盖 `p`（自身视为被覆盖）。
///
/// 用于部分检出范围判定（§5.1）。
pub fn covers(prefix: &str, p: &str) -> bool {
    if prefix.is_empty() || prefix == p {
        return true;
    }
    p.strip_prefix(prefix)
        .map(|rest| rest.starts_with('/'))
        .unwrap_or(false)
}

fn has_control_chars(s: &str) -> bool {
    s.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7F)
}

fn has_illegal_chars(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
}

/// 路径合法性校验（§6.6 ③④）。
///
/// 返回 `None` 表示合法；否则返回人类可读的违规原因（文案与 TS 版一致）。
pub fn validate_path(p: &str) -> Option<String> {
    if p.is_empty() {
        return Some("路径为空".to_string());
    }
    if p.starts_with('/') {
        return Some(format!("路径必须是相对路径：{p}"));
    }
    if to_posix(p) != p {
        return Some(format!("路径含反斜杠：{p}"));
    }
    if !is_nfc(p) {
        return Some(format!("路径非 NFC 规范化：{p}"));
    }
    if utf8_len(p) > MAX_PATH_BYTES {
        return Some(format!(
            "路径超长（{} > {} 字节）：{}",
            utf8_len(p),
            MAX_PATH_BYTES,
            p
        ));
    }

    for seg in segments(p) {
        if seg == "." || seg == ".." {
            return Some(format!("路径含 . 或 .. 段：{p}"));
        }
        if utf8_len(seg) > MAX_SEGMENT_BYTES {
            return Some(format!("路径段超长：{seg}"));
        }
        if has_control_chars(seg) {
            return Some(format!("路径段含控制字符：{seg}"));
        }
        if has_illegal_chars(seg) {
            return Some(format!("路径段含非法字符 < > : \" \\ | ? *：{seg}"));
        }
        // 段以空格或点结尾在 Windows 上会被静默截断。
        if seg.ends_with(' ') || seg.ends_with('.') {
            return Some(format!("路径段以空格或点结尾：{seg}"));
        }
        let stem = seg.split('.').next().unwrap_or("").to_uppercase();
        if WINDOWS_RESERVED.contains(&stem.as_str()) {
            return Some(format!("路径段是 Windows 保留名：{seg}"));
        }
    }
    None
}

/// 大小写折叠冲突检测（§6.6 ②）。
///
/// 大小写不敏感文件系统上，`a.psd` 与 `A.psd` 无法共存。传入同一目录下的
/// 全部分段名，返回互为 case-only 重复的路径集合。
pub fn find_case_collisions(paths: &[&str]) -> Vec<Vec<String>> {
    let mut buckets: Vec<(String, Vec<String>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for p in paths {
        let key = p.to_lowercase();
        match index.get(&key).copied() {
            Some(i) => buckets[i].1.push((*p).to_string()),
            None => {
                buckets.push((key.clone(), vec![(*p).to_string()]));
                index.insert(key, buckets.len() - 1);
            }
        }
    }
    buckets
        .into_iter()
        .filter(|(_, g)| g.len() > 1)
        .map(|(_, g)| g)
        .collect()
}

static CASE_PROBE_CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

/// 大小敏感文件系统探测：在给定目录里造一个文件，看全大写名是否也存在。
/// 结果按目录缓存，避免重复 I/O。
///
/// 探测失败时保守按「不敏感」处理：宁可多报冲突，也不要在 Windows 上静默覆盖。
pub fn is_case_sensitive_fs(dir: &Path) -> bool {
    let cache = CASE_PROBE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = dir.to_string_lossy().to_string();
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }

    let probe = dir.join(".b-artifact-case-probe");
    let result = match std::fs::write(&probe, b"") {
        Ok(()) => {
            let upper = dir.join(".B-ARTIFACT-CASE-PROBE");
            let coexists = upper.exists();
            let _ = std::fs::remove_file(&probe);
            !coexists
        }
        Err(_) => false,
    };
    cache.lock().unwrap().insert(key, result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_posix_converts_backslashes() {
        assert_eq!(to_posix("a\\b\\c.psd"), "a/b/c.psd");
    }

    #[test]
    fn nfd_input_normalizes_to_nfc() {
        let nfd = "e\u{301}";
        let nfc = "\u{e9}";
        assert!(!is_nfc(nfd));
        assert!(is_nfc(nfc));
        assert_eq!(normalize_nfc(nfd), nfc);
    }

    #[test]
    fn join_drops_empty_and_dot_segments() {
        assert_eq!(join_path(&["a", "", "b", ".", "c"]), "a/b/c");
        assert_eq!(join_path(&["a\\b", "c"]), "a/b/c");
    }

    #[test]
    fn segments_parent_base() {
        assert_eq!(segments("a/b/c.psd"), vec!["a", "b", "c.psd"]);
        assert_eq!(parent_of("a/b/c.psd"), Some("a/b"));
        assert_eq!(parent_of("c.psd"), None);
        assert_eq!(base_name("a/b/c.psd"), "c.psd");
    }

    #[test]
    fn covers_prefix_subtree_but_not_sibling() {
        assert!(covers("", "anything"));
        assert!(covers("char", "char"));
        assert!(covers("char", "char/hero.psd"));
        assert!(!covers("char", "chars/hero.psd"));
        assert!(!covers("char/hero.psd", "char"));
    }

    #[test]
    fn accepts_plain_path() {
        assert_eq!(validate_path("characters/hero/texture.psd"), None);
    }

    #[test]
    fn rejects_absolute_dotdot_and_empty() {
        assert!(validate_path("/abs/path").unwrap().contains("相对路径"));
        assert!(validate_path("a/../b").unwrap().contains(".."));
        assert!(validate_path("").unwrap().contains("为空"));
    }

    #[test]
    fn rejects_non_nfc() {
        assert!(validate_path("cafe\u{301}/logo.png").unwrap().contains("NFC"));
    }

    #[test]
    fn rejects_illegal_and_control_chars() {
        assert!(validate_path("a/b?.psd").unwrap().contains("非法字符"));
        assert!(validate_path("a/b|c.psd").unwrap().contains("非法字符"));
        assert!(validate_path("a/b\u{1}c.psd").unwrap().contains("控制字符"));
    }

    #[test]
    fn rejects_windows_reserved_names() {
        assert!(validate_path("a/con.psd").unwrap().contains("保留名"));
        assert!(validate_path("a/NUL").unwrap().contains("保留名"));
        assert!(validate_path("a/lpt9.txt").unwrap().contains("保留名"));
    }

    #[test]
    fn rejects_trailing_space_or_dot() {
        assert!(validate_path("a/b.psd ").unwrap().contains("空格或点"));
        assert!(validate_path("a/b.").unwrap().contains("空格或点"));
        assert_eq!(validate_path("a/ b.psd"), None); // 段首空格不违规
    }

    #[test]
    fn rejects_overlong_path_and_segment() {
        assert!(validate_path(&"x".repeat(256)).unwrap().contains("段超长"));
        let long = vec!["y".repeat(20); 60].join("/");
        assert!(validate_path(&long).unwrap().contains("路径超长"));
    }

    #[test]
    fn finds_case_only_duplicates() {
        let groups = find_case_collisions(&["a/Hero.psd", "a/hero.psd", "b/x.png", "a/HERO.psd"]);
        assert_eq!(groups.len(), 1);
        let mut g = groups[0].clone();
        g.sort();
        assert_eq!(g, vec!["a/HERO.psd", "a/Hero.psd", "a/hero.psd"]);
    }

    #[test]
    fn no_collision_returns_empty() {
        assert!(find_case_collisions(&["a.psd", "b.psd"]).is_empty());
    }
}
