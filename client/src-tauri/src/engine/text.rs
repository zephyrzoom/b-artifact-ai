/**
 * 文本判定与受限读取（冲突三方对比用，§6.5 冲突视图）。
 *
 * 美术资源库里绝大多数文件是二进制（psd / png / fbx…），所以"能不能文本合并"
 * 必须先判出来，而不是等把几百 MB 读进内存再发现。三条硬约束：
 *   1. **不读大文件**：超过上限直接判"太大，请用二选一"，不做 diff；
 *   2. **二进制探测**：前若干字节出现 NUL 就当二进制（git 的经验规则）；
 *   3. **严格 UTF-8**：解不出来就是二进制。
 */
use std::io;
use std::path::Path;

use serde::Serialize;

/// 文本对比的大小上限：超过就不做文本 diff（保护 UI 与内存）。
pub const TEXT_DIFF_LIMIT: u64 = 1024 * 1024;
/// 二进制探测的采样字节数（对齐 git 的 8000）。
pub const BINARY_SNIFF_BYTES: usize = 8000;

/// 判别式结果 —— "不存在 / 太大 / 二进制"都是**正常业务状态**，不是异常。
///
/// 序列化形状与 TS 版逐一对应（`kind` 判别 + `size`），渲染层按 `kind` 分支。
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TextReadResult {
    Text { text: String, size: u64 },
    Binary { size: u64 },
    TooLarge { size: u64 },
    Missing { size: u64 },
}

impl TextReadResult {
    pub fn is_text(&self) -> bool {
        matches!(self, TextReadResult::Text { .. })
    }
}

/// 判断一段字节是否"看起来像二进制"：要么有 NUL，要么不是合法 UTF-8。
///
/// 只看前 `BINARY_SNIFF_BYTES` 字节。
pub fn is_probably_binary(buf: &[u8]) -> bool {
    let n = buf.len().min(BINARY_SNIFF_BYTES);
    let head = &buf[..n];
    if head.contains(&0) {
        return true;
    }
    // 截断在多字节序列中间时同样判失败 —— 与 TextDecoder({fatal:true}) 的行为一致
    std::str::from_utf8(head).is_err()
}

/// 读取一个文件用于文本对比（同步；调用方负责放在 blocking 上下文里）。
pub fn read_text_capped(
    abs_path: &Path,
    limit: Option<u64>,
    sniff: Option<&dyn Fn(&[u8]) -> bool>,
) -> TextReadResult {
    let limit = limit.unwrap_or(TEXT_DIFF_LIMIT);

    let meta = match std::fs::metadata(abs_path) {
        Ok(m) => m,
        Err(_) => return TextReadResult::Missing { size: 0 },
    };
    if !meta.is_file() {
        return TextReadResult::Missing { size: 0 };
    }
    let size = meta.len();
    if size > limit {
        return TextReadResult::TooLarge { size };
    }

    let bytes = match std::fs::read(abs_path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return TextReadResult::Missing { size: 0 },
        Err(_) => return TextReadResult::Missing { size: 0 },
    };

    let is_binary = match sniff {
        Some(f) => f(&bytes),
        None => is_probably_binary(&bytes),
    };
    if is_binary {
        return TextReadResult::Binary { size };
    }

    match String::from_utf8(bytes) {
        Ok(text) => TextReadResult::Text {
            text,
            size,
        },
        // 探测只看前 8000 字节，整体仍可能不是合法 UTF-8
        Err(_) => TextReadResult::Binary { size },
    }
}

/// 三方内容能否做**文本**合并：三方都必须是文本。
pub fn can_merge_text(sides: &[TextReadResult]) -> bool {
    sides.iter().all(|s| s.is_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(dir: &Path, name: &str, content: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn ascii_text_is_not_binary() {
        assert!(!is_probably_binary("hello world\n".as_bytes()));
    }

    #[test]
    fn cjk_and_emoji_are_not_binary() {
        assert!(!is_probably_binary("美术资源 · 贴图 😀".as_bytes()));
    }

    #[test]
    fn nul_means_binary() {
        assert!(is_probably_binary(&[0x89, 0x50, 0x4e, 0x47, 0x00, 0x01]));
    }

    #[test]
    fn nul_outside_sniff_window_is_ignored() {
        let mut big = vec![0x41u8; BINARY_SNIFF_BYTES + 10];
        big[BINARY_SNIFF_BYTES + 5] = 0;
        assert!(!is_probably_binary(&big));
    }

    #[test]
    fn invalid_utf8_is_binary() {
        assert!(is_probably_binary(&[0x41, 0xff, 0xfe, 0x41]));
    }

    #[test]
    fn empty_is_not_binary() {
        assert!(!is_probably_binary(&[]));
    }

    #[test]
    fn reads_text_with_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = put(dir.path(), "a.txt", b"hello");
        assert_eq!(
            read_text_capped(&p, None, None),
            TextReadResult::Text {
                text: "hello".into(),
                size: 5
            }
        );
    }

    #[test]
    fn decodes_utf8_cjk() {
        let dir = tempfile::tempdir().unwrap();
        let p = put(dir.path(), "cn.txt", "美术资源".as_bytes());
        match read_text_capped(&p, None, None) {
            TextReadResult::Text { text, .. } => assert_eq!(text, "美术资源"),
            other => panic!("应为 text：{other:?}"),
        }
    }

    #[test]
    fn binary_reports_size_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = put(dir.path(), "a.psd", &[0x00, 0x01, 0x02, 0x03]);
        assert_eq!(
            read_text_capped(&p, None, None),
            TextReadResult::Binary { size: 4 }
        );
    }

    #[test]
    fn over_limit_is_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let p = put(dir.path(), "big.txt", &vec![b'x'; 100]);
        assert_eq!(
            read_text_capped(&p, Some(10), None),
            TextReadResult::TooLarge { size: 100 }
        );
    }

    #[test]
    fn missing_file_and_dir_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_text_capped(&dir.path().join("nope.txt"), None, None),
            TextReadResult::Missing { size: 0 }
        );
        let sub = dir.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            read_text_capped(&sub, None, None),
            TextReadResult::Missing { size: 0 }
        );
    }

    #[test]
    fn empty_file_is_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = put(dir.path(), "empty.txt", b"");
        assert_eq!(
            read_text_capped(&p, None, None),
            TextReadResult::Text {
                text: String::new(),
                size: 0
            }
        );
    }

    #[test]
    fn sniff_is_injectable() {
        let dir = tempfile::tempdir().unwrap();
        let p = put(dir.path(), "a.txt", b"hello");
        let force_binary = |_: &[u8]| true;
        assert_eq!(
            read_text_capped(&p, None, Some(&force_binary)),
            TextReadResult::Binary { size: 5 }
        );
    }

    #[test]
    fn whole_file_must_be_valid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let p = put(dir.path(), "mixed.bin", &[b'o', b'k', 0xff, 0xff]);
        let never = |_: &[u8]| false;
        match read_text_capped(&p, None, Some(&never)) {
            TextReadResult::Binary { .. } => {}
            other => panic!("应为 binary：{other:?}"),
        }
    }

    #[test]
    fn json_shape_matches_renderer_expectation() {
        let v = serde_json::to_value(TextReadResult::TooLarge { size: 1 }).unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "too-large", "size": 1 }));
        let v = serde_json::to_value(TextReadResult::Missing { size: 0 }).unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "missing", "size": 0 }));
        let v = serde_json::to_value(TextReadResult::Text {
            text: "x".into(),
            size: 1,
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "text", "text": "x", "size": 1 }));
    }

    #[test]
    fn can_merge_only_when_all_text() {
        let t = TextReadResult::Text {
            text: "x".into(),
            size: 1,
        };
        let b = TextReadResult::Binary { size: 9 };
        let m = TextReadResult::Missing { size: 0 };
        let big = TextReadResult::TooLarge { size: 1_000_000_000 };
        assert!(!can_merge_text(&[t.clone(), b, t.clone()]));
        assert!(!can_merge_text(&[t.clone(), m, t.clone()]));
        assert!(!can_merge_text(&[t.clone(), big, t.clone()]));
        assert!(can_merge_text(&[t.clone(), t.clone(), t]));
    }
}
