/**
 * 工作副本扫描与状态计算（§6.2 文件状态机）。
 *
 * 三条不变量：
 *   1. 路径一律 posix + NFC（§6.6 ①），否则 macOS 上"看起来同名"的两个文件会状态错乱；
 *   2. 符号链接一律跳过（§6.6 ④）——v1 不入库，避免目录逃逸与跨平台语义混乱；
 *   3. mtime + size 只作**快速跳过**的启发式，最终判定看内容哈希（§6.1）。
 */
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::db::{EntryKind, EntryStatus, PendingOp, Wc, WC_DIR};
use crate::engine::errors::ClientResult;
use crate::engine::hash::hash_file;
use crate::engine::ignore::{IgnoreRules, IGNORE_FILE};
use crate::engine::paths::normalize_nfc;

/// 状态机全集（含两个纯计算态 `unversioned` / `ignored`，它们不落库）。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum StatusCode {
    Normal,
    Modified,
    Added,
    Deleted,
    Missing,
    Conflicted,
    NeedsUpdate,
    Unversioned,
    Ignored,
}

impl StatusCode {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusCode::Normal => "normal",
            StatusCode::Modified => "modified",
            StatusCode::Added => "added",
            StatusCode::Deleted => "deleted",
            StatusCode::Missing => "missing",
            StatusCode::Conflicted => "conflicted",
            StatusCode::NeedsUpdate => "needs-update",
            StatusCode::Unversioned => "unversioned",
            StatusCode::Ignored => "ignored",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskEntry {
    pub path: String,
    pub abs_path: PathBuf,
    pub kind: EntryKind,
    pub size: i64,
    pub mtime_ms: i64,
    /// 仅保留可执行位。
    pub mode: i64,
}

#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    pub entries: Vec<DiskEntry>,
    pub ignored: Vec<String>,
    /// 存在 `<path>.mine` 伴生文件的主体路径集合（§6.2 `conflicted`）。
    pub conflicts: HashSet<String>,
    pub warnings: Vec<String>,
}

pub const MINE_SUFFIX: &str = ".mine";

/// 递归扫描工作副本（跳过 `.b-artifact` 与符号链接）。
///
/// `ignore` 为 `None` 时按目录逐层加载 `.b-artifactignore`（与 TS 版一致）；
/// 传入规则集则不加载文件（调用方自己管理）。
pub fn scan_disk(root: &Path, ignore: Option<IgnoreRules>) -> ScanResult {
    // 传 None = "扫描时顺带加载各层 .b-artifactignore"；传 Some = 用调用方给的规则集。
    let load_files = ignore.is_none();
    let mut rules = ignore.unwrap_or_default();

    let mut out = ScanResult::default();
    // 栈式遍历：避免递归深度随目录层级爆掉。
    let mut stack: Vec<(String, PathBuf)> = vec![(String::new(), root.to_path_buf())];

    while let Some((frame_rel, frame_abs)) = stack.pop() {
        // 每层目录都可能带自己的 .b-artifactignore（base = 该目录）。
        if load_files {
            rules.add_file(&frame_rel, &frame_abs.join(IGNORE_FILE));
        }

        let dirents = match std::fs::read_dir(&frame_abs) {
            Ok(d) => d,
            Err(e) => {
                let shown = if frame_rel.is_empty() { "." } else { &frame_rel };
                out.warnings.push(format!("无法读取目录 {shown}：{e}"));
                continue;
            }
        };

        for dirent in dirents.flatten() {
            let raw_name = dirent.file_name().to_string_lossy().to_string();
            let name = normalize_nfc(&raw_name);
            let rel = if frame_rel.is_empty() {
                name.clone()
            } else {
                format!("{frame_rel}/{name}")
            };
            let abs = frame_abs.join(&raw_name);

            if frame_rel.is_empty() && name == WC_DIR {
                continue; // 元数据目录不参与版本管理
            }

            let ft = match dirent.file_type() {
                Ok(t) => t,
                Err(e) => {
                    out.warnings.push(format!("无法读取类型 {rel}：{e}"));
                    continue;
                }
            };
            if ft.is_symlink() {
                out.warnings
                    .push(format!("跳过符号链接：{rel}（v1 不支持 symlink）"));
                continue;
            }

            if name.ends_with(MINE_SUFFIX) {
                // 冲突伴生文件：登记主体路径，自身不作为版本化条目。
                let base = rel.trim_end_matches(MINE_SUFFIX).to_string();
                out.conflicts.insert(base);
                continue;
            }

            let is_dir = ft.is_dir();
            if rules.matches(&rel, is_dir) {
                out.ignored.push(rel);
                continue;
            }

            if is_dir {
                out.entries.push(DiskEntry {
                    path: rel.clone(),
                    abs_path: abs.clone(),
                    kind: EntryKind::Dir,
                    size: 0,
                    mtime_ms: 0,
                    mode: 0,
                });
                stack.push((rel, abs));
                continue;
            }
            if !ft.is_file() {
                out.warnings.push(format!("跳过非常规文件：{rel}"));
                continue;
            }

            let meta = match std::fs::metadata(&abs) {
                Ok(m) => m,
                Err(e) => {
                    out.warnings.push(format!("无法读取 {rel}：{e}"));
                    continue;
                }
            };
            if meta.is_dir() {
                out.entries.push(DiskEntry {
                    path: rel.clone(),
                    abs_path: abs.clone(),
                    kind: EntryKind::Dir,
                    size: 0,
                    mtime_ms: 0,
                    mode: 0,
                });
                stack.push((rel, abs));
                continue;
            }
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.entries.push(DiskEntry {
                path: rel,
                abs_path: abs,
                kind: EntryKind::File,
                size: meta.len() as i64,
                mtime_ms,
                mode: file_mode(&meta),
            });
        }
    }

    out.entries.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::PermissionsExt;
    (meta.permissions().mode() & 0o111) as i64
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> i64 {
    0
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct StatusItem {
    pub path: String,
    pub kind: EntryKind,
    pub status: StatusCode,
    pub base_rev: i64,
    pub base_hash: Option<String>,
    pub size: i64,
    /// 服务端该路径已变更为更新的修订（由 update 扫描填）。
    pub needs_update: bool,
}

#[derive(Default, Clone)]
pub struct StatusOptions {
    /// 强制重算哈希，不走 mtime 启发式。
    pub force_hash: bool,
    /// 服务端已变更路径（update 后填 needs_update）。
    pub server_changed: Option<HashSet<String>>,
}

/// 状态计算：`scan(root) × wc.entries` 的集合运算 + pristine 哈希比对。
///
/// 目录不做哈希比对，只判存在性；文件走哈希（命中 mtime+size 启发式则跳过）。
pub fn compute_status(wc: &Wc, opts: &StatusOptions) -> ClientResult<Vec<StatusItem>> {
    let scan = scan_disk(&wc.root, None);
    let disk: std::collections::HashMap<String, DiskEntry> = scan
        .entries
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let base_entries = wc.all_entries()?;
    // 一次性取待提交集（TS 版是逐路径查库，语义相同但这里是 N 次 → 1 次）
    let pending: std::collections::HashMap<String, PendingOp> = wc
        .all_pending()?
        .into_iter()
        .map(|p| (p.path, p.op))
        .collect();

    let mut out: Vec<StatusItem> = Vec::new();

    // ① 基线视角：normal / modified / deleted / missing / conflicted
    for b in &base_entries {
        let d = disk.get(&b.path);
        let make = |status: StatusCode| StatusItem {
            path: b.path.clone(),
            kind: b.kind,
            status,
            base_rev: b.base_rev,
            base_hash: b.base_hash.clone(),
            size: d.map(|x| x.size).unwrap_or(b.size),
            needs_update: opts
                .server_changed
                .as_ref()
                .map(|s| s.contains(&b.path))
                .unwrap_or(false),
        };

        let Some(d) = d else {
            let deleted = pending.get(&b.path) == Some(&PendingOp::Delete);
            out.push(make(if deleted {
                StatusCode::Deleted
            } else {
                StatusCode::Missing
            }));
            continue;
        };
        if scan.conflicts.contains(&b.path) {
            out.push(make(StatusCode::Conflicted));
            continue;
        }
        if d.kind == EntryKind::Dir || b.kind == EntryKind::Dir {
            out.push(make(StatusCode::Normal));
            continue;
        }
        // mtime + size 完全一致 → 视为未改（启发式，§6.1）。
        if !opts.force_hash && d.mtime_ms == b.mtime_ms && d.size == b.size {
            out.push(make(StatusCode::Normal));
            continue;
        }
        let h = hash_file(&d.abs_path)?;
        out.push(make(if Some(h) == b.base_hash {
            StatusCode::Normal
        } else {
            StatusCode::Modified
        }));
    }

    // ② 磁盘视角：added / unversioned
    let base_paths: HashSet<&String> = base_entries.iter().map(|e| &e.path).collect();
    for d in disk.values() {
        if base_paths.contains(&d.path) {
            continue;
        }
        let status = if pending.get(&d.path) == Some(&PendingOp::Add) {
            StatusCode::Added
        } else {
            StatusCode::Unversioned
        };
        out.push(StatusItem {
            path: d.path.clone(),
            kind: d.kind,
            status,
            base_rev: 0,
            base_hash: None,
            size: d.size,
            needs_update: false,
        });
    }

    for p in &scan.ignored {
        let d = disk.get(p);
        out.push(StatusItem {
            path: p.clone(),
            kind: d.map(|x| x.kind).unwrap_or(EntryKind::File),
            status: StatusCode::Ignored,
            base_rev: 0,
            base_hash: None,
            size: d.map(|x| x.size).unwrap_or(0),
            needs_update: false,
        });
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// 状态是否为"有本地改动"（用于阻止 update / 提示提交）。
pub fn is_local_change(s: StatusCode) -> bool {
    matches!(
        s,
        StatusCode::Modified
            | StatusCode::Added
            | StatusCode::Deleted
            | StatusCode::Missing
            | StatusCode::Conflicted
    )
}

/// `EntryStatus` → `StatusCode`（落库态与计算态共用一张表）。
pub fn status_from_entry(e: EntryStatus) -> StatusCode {
    match e {
        EntryStatus::Normal => StatusCode::Normal,
        EntryStatus::Modified => StatusCode::Modified,
        EntryStatus::Added => StatusCode::Added,
        EntryStatus::Deleted => StatusCode::Deleted,
        EntryStatus::Missing => StatusCode::Missing,
        EntryStatus::Conflicted => StatusCode::Conflicted,
        EntryStatus::NeedsUpdate => StatusCode::NeedsUpdate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::db::WcMeta;

    fn init_wc(dir: &Path) -> Wc {
        Wc::init(
            dir,
            &WcMeta {
                server: "http://x".into(),
                repo: "r".into(),
                revision: 1,
                sparse_paths: vec![],
                user: "u".into(),
                cache_dir: dir.join("cache").to_string_lossy().to_string(),
            },
        )
        .unwrap()
    }

    fn put(root: &Path, rel: &str, content: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn entry(path: &str, kind: EntryKind, hash: Option<&str>, size: i64, mtime: i64) -> crate::engine::db::EntryRow {
        crate::engine::db::EntryRow {
            path: path.into(),
            kind,
            base_rev: 1,
            base_hash: hash.map(|s| s.to_string()),
            size,
            mtime_ms: mtime,
            mode: 0,
            status: EntryStatus::Normal,
            case_conflict: false,
        }
    }

    #[test]
    fn scan_finds_nested_entries_and_skips_metadata_dir() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "characters/hero.psd", b"x");
        put(dir.path(), ".b-artifact/wc.db", b"db");
        let s = scan_disk(dir.path(), None);
        let paths: Vec<&str> = s.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["characters", "characters/hero.psd"]);
    }

    #[test]
    fn scan_registers_mine_companion_as_conflict() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "a.psd", b"server");
        put(dir.path(), "a.psd.mine", b"mine");
        let s = scan_disk(dir.path(), None);
        assert!(s.conflicts.contains("a.psd"));
        assert!(
            !s.entries.iter().any(|e| e.path.ends_with(".mine")),
            ".mine 自身不应作为条目"
        );
    }

    #[test]
    fn scan_applies_root_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), ".b-artifactignore", b"*.tmp\nbuild/\n");
        put(dir.path(), "a.tmp", b"x");
        put(dir.path(), "build/out.bin", b"x");
        put(dir.path(), "keep.psd", b"x");
        let s = scan_disk(dir.path(), None);
        let paths: Vec<&str> = s.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec![".b-artifactignore", "keep.psd"]);
        assert!(s.ignored.iter().any(|p| p == "a.tmp"));
        assert!(s.ignored.iter().any(|p| p == "build"));
    }

    #[test]
    fn scan_applies_nested_ignore_file_with_base() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "art/.b-artifactignore", b"*.psd\n");
        put(dir.path(), "art/x.psd", b"x");
        put(dir.path(), "other/x.psd", b"x");
        let s = scan_disk(dir.path(), None);
        let paths: Vec<&str> = s.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"art/.b-artifactignore"));
        assert!(!paths.contains(&"art/x.psd"), "art 下的 psd 应被忽略");
        assert!(paths.contains(&"other/x.psd"), "规则只作用于 art/");
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinks_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), b"x").unwrap();
        std::os::unix::fs::symlink("real.txt", dir.path().join("link.txt")).unwrap();
        let s = scan_disk(dir.path(), None);
        let paths: Vec<&str> = s.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["real.txt"]);
        assert!(s.warnings.iter().any(|w| w.contains("符号链接")));
    }

    #[test]
    fn status_normal_when_mtime_and_size_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = init_wc(dir.path());
        put(dir.path(), "a.psd", b"1234");
        let st = std::fs::metadata(dir.path().join("a.psd")).unwrap();
        let mtime = st
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        wc.upsert_entry(&entry("a.psd", EntryKind::File, Some("h"), 4, mtime))
            .unwrap();
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, StatusCode::Normal);
    }

    #[test]
    fn status_modified_when_hash_differs() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = init_wc(dir.path());
        put(dir.path(), "a.psd", b"1234");
        // mtime/size 故意给错 → 强制走哈希
        wc.upsert_entry(&entry("a.psd", EntryKind::File, Some("deadbeef"), 4, 1))
            .unwrap();
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        assert_eq!(items[0].status, StatusCode::Modified);
    }

    #[test]
    fn force_hash_recomputes_even_when_heuristic_would_skip() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = init_wc(dir.path());
        put(dir.path(), "a.psd", b"1234");
        let real = crate::engine::hash::hash_file(&dir.path().join("a.psd")).unwrap();
        let st = std::fs::metadata(dir.path().join("a.psd")).unwrap();
        let mtime = st
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        // 基线哈希是错的，但 mtime+size 一致：启发式会说 normal
        wc.upsert_entry(&entry("a.psd", EntryKind::File, Some("bad"), 4, mtime))
            .unwrap();
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        assert_eq!(items[0].status, StatusCode::Normal);
        let items = compute_status(
            &wc,
            &StatusOptions {
                force_hash: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(items[0].status, StatusCode::Modified);
        assert_eq!(items[0].base_hash.as_deref(), Some("bad"));
        assert_ne!(real, "bad");
    }

    #[test]
    fn missing_vs_deleted_depends_on_pending_delete() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = init_wc(dir.path());
        wc.upsert_entry(&entry("gone.psd", EntryKind::File, Some("h"), 1, 1))
            .unwrap();
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        assert_eq!(items[0].status, StatusCode::Missing);

        wc.stage("gone.psd", PendingOp::Delete).unwrap();
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        assert_eq!(items[0].status, StatusCode::Deleted);
    }

    #[test]
    fn conflicted_wins_over_normal() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = init_wc(dir.path());
        put(dir.path(), "a.psd", b"1234");
        put(dir.path(), "a.psd.mine", b"mine");
        wc.upsert_entry(&entry("a.psd", EntryKind::File, Some("h"), 4, 1))
            .unwrap();
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        let a = items.iter().find(|i| i.path == "a.psd").unwrap();
        assert_eq!(a.status, StatusCode::Conflicted);
    }

    #[test]
    fn untracked_is_unversioned_until_staged_as_add() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = init_wc(dir.path());
        put(dir.path(), "new.psd", b"x");
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        assert_eq!(items[0].status, StatusCode::Unversioned);

        wc.stage("new.psd", PendingOp::Add).unwrap();
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        assert_eq!(items[0].status, StatusCode::Added);
    }

    #[test]
    fn ignored_entries_are_reported_but_not_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let wc = init_wc(dir.path());
        put(dir.path(), ".b-artifactignore", b"*.tmp\n");
        put(dir.path(), "a.tmp", b"x");
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        let t = items.iter().find(|i| i.path == "a.tmp").unwrap();
        assert_eq!(t.status, StatusCode::Ignored);
        assert!(!is_local_change(t.status));
    }

    #[test]
    fn server_changed_marks_needs_update() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = init_wc(dir.path());
        put(dir.path(), "a.psd", b"1234");
        let st = std::fs::metadata(dir.path().join("a.psd")).unwrap();
        let mtime = st
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        wc.upsert_entry(&entry("a.psd", EntryKind::File, Some("h"), 4, mtime))
            .unwrap();
        let mut changed = HashSet::new();
        changed.insert("a.psd".to_string());
        let items = compute_status(
            &wc,
            &StatusOptions {
                server_changed: Some(changed),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(items[0].needs_update);
    }

    #[test]
    fn status_is_sorted_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let wc = init_wc(dir.path());
        put(dir.path(), "b.psd", b"x");
        put(dir.path(), "a.psd", b"x");
        let items = compute_status(&wc, &StatusOptions::default()).unwrap();
        let paths: Vec<&str> = items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["a.psd", "b.psd"]);
    }

    #[test]
    fn local_change_covers_the_five_dirty_states() {
        for s in [
            StatusCode::Modified,
            StatusCode::Added,
            StatusCode::Deleted,
            StatusCode::Missing,
            StatusCode::Conflicted,
        ] {
            assert!(is_local_change(s), "{s:?} 应算本地改动");
        }
        for s in [
            StatusCode::Normal,
            StatusCode::Unversioned,
            StatusCode::Ignored,
            StatusCode::NeedsUpdate,
        ] {
            assert!(!is_local_change(s), "{s:?} 不应算本地改动");
        }
    }

    #[test]
    fn status_code_json_matches_renderer() {
        assert_eq!(serde_json::to_value(StatusCode::NeedsUpdate).unwrap(), serde_json::json!("needs-update"));
        assert_eq!(serde_json::to_value(StatusCode::Unversioned).unwrap(), serde_json::json!("unversioned"));
        let item = StatusItem {
            path: "a".into(),
            kind: EntryKind::File,
            status: StatusCode::Normal,
            base_rev: 1,
            base_hash: None,
            size: 2,
            needs_update: false,
        };
        let v = serde_json::to_value(item).unwrap();
        assert_eq!(v["needs_update"], serde_json::json!(false), "字段名保持 snake_case");
    }
}
