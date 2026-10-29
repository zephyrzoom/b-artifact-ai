/**
 * 全局 blob 缓存的统计与清理（§6.5 设置页）。
 *
 * **为什么删缓存是安全的**：缓存是内容寻址的唯一物理副本，但每个工作副本的
 * `<wc>/.b-artifact/pristine/<hash>` 是与它**硬链接**的另一个目录项——删掉缓存里的名字，
 * 链接数减一，pristine 里的内容依然可读，工作副本不会损坏。清理的代价只是下次
 * 需要重新下载（这正是"清理缓存"该有的语义）。
 *
 * 因此这里**只删文件、保留目录骨架**：目录本身很便宜，重建反而要处理并发创建的竞态。
 */
use std::path::Path;

use serde::Serialize;
use walkdir::WalkDir;

/// 下载中转目录（`storeToCache` 之前落的临时文件）。
pub const CACHE_TMP_DIR: &str = "tmp";

#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub dir: String,
    /// 已缓存的 blob 个数与总字节。
    pub blobs: u64,
    pub bytes: u64,
    /// 残留的临时文件（上次下载中断留下的）。
    pub tmp_files: u64,
    pub tmp_bytes: u64,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CacheClearResult {
    pub removed: u64,
    pub bytes: u64,
    /// 是否连临时目录一起清了（默认会）。
    pub cleared_tmp: bool,
}

/// 遍历目录，对**普通文件**回调（符号链接一律不碰）。
fn walk(dir: &Path, mut visit: impl FnMut(&Path, u64)) {
    for entry in WalkDir::new(dir).follow_links(false).into_iter().flatten() {
        // file_type().is_file() 对符号链接返回 false —— 正是我们要的
        if !entry.file_type().is_file() {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        visit(entry.path(), size);
    }
}

fn under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// 统计缓存占用（同时报告残留临时文件，便于界面提示"可清理 N 个"）。
pub fn cache_stats(cache_dir: &Path) -> CacheStats {
    let mut stats = CacheStats {
        dir: cache_dir.to_string_lossy().to_string(),
        ..Default::default()
    };
    let tmp_root = cache_dir.join(CACHE_TMP_DIR);

    walk(cache_dir, |abs, size| {
        if under(abs, &tmp_root) && abs != cache_dir {
            stats.tmp_files += 1;
            stats.tmp_bytes += size;
            return;
        }
        stats.blobs += 1;
        stats.bytes += size;
    });

    stats
}

/// 清空缓存。
///
/// `keep_tmp = true` 时**只清临时文件**，保留已缓存的 blob（"清理残留"用）——
/// 注意这个参数名沿用 TS 版，语义是"只处理 tmp"，不是"保留 tmp"。
pub fn clear_cache(cache_dir: &Path, keep_tmp: bool) -> CacheClearResult {
    let mut result = CacheClearResult::default();
    let tmp_root = cache_dir.join(CACHE_TMP_DIR);

    if keep_tmp {
        walk(&tmp_root, |abs, size| {
            if std::fs::remove_file(abs).is_ok() {
                result.removed += 1;
                result.bytes += size;
            }
        });
        return result;
    }

    walk(cache_dir, |abs, size| {
        if std::fs::remove_file(abs).is_ok() {
            result.removed += 1;
            result.bytes += size;
            if abs.starts_with(&tmp_root) {
                result.cleared_tmp = true;
            }
        }
    });
    result.cleared_tmp = result.cleared_tmp || tmp_root.exists();
    // 临时目录整棵删掉（里面的空目录骨架留着没意义，而且常有残留的 .part 目录）
    let _ = std::fs::remove_dir_all(&tmp_root);

    result
}

#[cfg(test)]
fn write_file(dir: &Path, rel: &str, bytes: usize) -> std::path::PathBuf {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, vec![0u8; bytes]).unwrap();
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_split_blobs_and_tmp() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "ab/cdef", 10);
        write_file(dir.path(), "tmp/part-1", 3);
        let s = cache_stats(dir.path());
        assert_eq!(s.blobs, 1);
        assert_eq!(s.bytes, 10);
        assert_eq!(s.tmp_files, 1);
        assert_eq!(s.tmp_bytes, 3);
        assert_eq!(s.dir, dir.path().to_string_lossy());
    }

    #[test]
    fn missing_dir_is_empty_cache() {
        let s = cache_stats(Path::new("/nonexistent-b-artifact-cache"));
        assert_eq!(s.blobs, 0);
        assert_eq!(s.bytes, 0);
    }

    #[test]
    fn clear_all_removes_files_and_tmp_tree() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "ab/cdef", 10);
        write_file(dir.path(), "tmp/part-1", 3);
        let r = clear_cache(dir.path(), false);
        assert_eq!(r.removed, 2);
        assert_eq!(r.bytes, 13);
        assert!(r.cleared_tmp);
        assert!(!dir.path().join("tmp").exists(), "临时目录应整棵删掉");
        // 目录骨架保留
        assert!(dir.path().join("ab").exists());
    }

    #[test]
    fn keep_tmp_only_clears_tmp_files() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "ab/cdef", 10);
        write_file(dir.path(), "tmp/part-1", 3);
        let r = clear_cache(dir.path(), true);
        assert_eq!(r.removed, 1);
        assert_eq!(r.bytes, 3);
        assert!(!r.cleared_tmp);
        assert!(dir.path().join("ab/cdef").exists(), "blob 必须保留");
        assert!(!dir.path().join("tmp/part-1").exists());
    }

    #[test]
    fn json_shape_is_camel_case() {
        let v = serde_json::to_value(CacheStats {
            dir: "/c".into(),
            blobs: 1,
            bytes: 2,
            tmp_files: 3,
            tmp_bytes: 4,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "dir": "/c", "blobs": 1, "bytes": 2, "tmpFiles": 3, "tmpBytes": 4 })
        );
        let v = serde_json::to_value(CacheClearResult::default()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "removed": 0, "bytes": 0, "clearedTmp": false })
        );
    }
}
