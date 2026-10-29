/**
 * pristine 与全局缓存（§6.1）。
 *
 * 三层布局，磁盘成本算清楚再动手：
 *
 *   全局缓存  ~/.b-artifact/cache/<hash>      ← 唯一物理副本（跨工作副本、跨修订共享）
 *   pristine  <wc>/.b-artifact/pristine/<hash> ← 硬链接到缓存（0 额外空间）
 *   工作文件  <wc>/characters/hero.psd          ← 独立副本
 *
 * 工作文件**必须**是独立副本而不是硬链接：用户用编辑器原地保存（truncate + write）
 * 会顺着硬链接改掉基线，三态对比与 revert 就全废了。这一点与 SVN 的取舍不同——
 * SVN 把工作文件也硬链接到 pristine，代价是依赖外部编辑器不原地写。
 */
use std::path::{Path, PathBuf};

use crate::engine::db::WC_DIR;
use crate::engine::errors::{ClientError, ClientResult};
use crate::engine::home::artifact_home;

/// 全局缓存根目录。
pub fn default_cache_dir() -> PathBuf {
    artifact_home().join("cache")
}

/// 两级分片的缓存路径：`<dir>/3f/a1/3fa1c8...`。
pub fn blob_path(hash: &str, dir: &Path) -> PathBuf {
    let a = hash.get(0..2).unwrap_or(hash);
    let b = hash.get(2..4).unwrap_or("");
    dir.join(a).join(b).join(hash)
}

pub fn cache_path_for(hash: &str, cache_dir: &Path) -> PathBuf {
    blob_path(hash, cache_dir)
}

pub fn pristine_root(wc_root: &Path) -> PathBuf {
    wc_root.join(WC_DIR).join("pristine")
}

pub fn pristine_path_for(wc_root: &Path, hash: &str) -> PathBuf {
    blob_path(hash, &pristine_root(wc_root))
}

fn ensure_dir(p: &Path) -> ClientResult<()> {
    std::fs::create_dir_all(p)?;
    Ok(())
}

fn rand_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..6].to_string()
}

/// 原子地把内容纳入缓存。
///
/// `src_abs` 通常是下载/上传用的临时文件，直接 rename 最省一次全量拷贝；
/// 跨设备（EXDEV）时降级为复制。已有同 hash 内容则丢弃临时文件——内容寻址下二者等价。
pub fn store_to_cache(src_abs: &Path, hash: &str, cache_dir: &Path) -> ClientResult<PathBuf> {
    let dest = cache_path_for(hash, cache_dir);
    if dest.exists() {
        let _ = std::fs::remove_file(src_abs);
        return Ok(dest);
    }
    ensure_dir(dest.parent().unwrap_or(cache_dir))?;
    let tmp = dest.with_extension(format!("tmp-{}-{}", std::process::id(), rand_suffix()));
    if std::fs::rename(src_abs, &tmp).is_err() {
        // 跨设备 / 其它 rename 失败：降级复制（不认识的错误也走这条路，宁可多拷一次）
        std::fs::copy(src_abs, &tmp)?;
        let _ = std::fs::remove_file(src_abs);
    }
    match std::fs::rename(&tmp, &dest) {
        Ok(()) => Ok(dest),
        Err(_) => {
            // 并发写入同一 hash 是常态：对方先落位即视为成功。
            if dest.exists() {
                let _ = std::fs::remove_file(&tmp);
                Ok(dest)
            } else {
                Err(ClientError::wc("IO", format!("缓存落位失败：{}", dest.display())))
            }
        }
    }
}

/// 建立 pristine 硬链接；文件系统不支持时降级为复制。
pub fn ensure_pristine(wc_root: &Path, hash: &str, cache_dir: &Path) -> ClientResult<PathBuf> {
    let src = cache_path_for(hash, cache_dir);
    let dest = pristine_path_for(wc_root, hash);
    if dest.exists() {
        return Ok(dest);
    }
    ensure_dir(dest.parent().unwrap_or(&pristine_root(wc_root)))?;
    match std::fs::hard_link(&src, &dest) {
        Ok(()) => Ok(dest),
        Err(e) => {
            // EEXIST：并发下别人先建好了；EXDEV/EPERM/ENOSYS/EMLINK：不支持硬链接 → 复制。
            let code = e.raw_os_error().unwrap_or(0);
            if dest.exists() {
                return Ok(dest);
            }
            #[cfg(unix)]
            let copy_fallback = code == libc::EXDEV
                || code == libc::EPERM
                || code == libc::ENOSYS
                || code == libc::EMLINK;
            #[cfg(not(unix))]
            let copy_fallback = true;
            if copy_fallback {
                std::fs::copy(&src, &dest)?;
                Ok(dest)
            } else {
                Err(ClientError::wc(
                    "IO",
                    format!("建立 pristine 硬链接失败：{}", e),
                ))
            }
        }
    }
}

/// 把基线内容安装到工作副本（检出 / update / revert 都走这里）。
///
/// 刻意用复制而非硬链接：工作文件要能被用户随意改写，绝不能与基线共享 inode。
pub fn install_working_file(wc_root: &Path, hash: &str, dest_abs: &Path) -> ClientResult<()> {
    let src = pristine_path_for(wc_root, hash);
    if let Some(parent) = dest_abs.parent() {
        ensure_dir(parent)?;
    }
    let tmp = dest_abs.with_file_name(format!(
        "{}.b-artifact-tmp-{}",
        dest_abs
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::copy(&src, &tmp)?;
    std::fs::rename(&tmp, dest_abs)?;
    Ok(())
}

/// 工作文件 → pristine 缓存：提交成功后把新基线入库，供后续本地三态对比。
pub fn import_working_file(abs_path: &Path, cache_dir: &Path, hash: &str) -> ClientResult<()> {
    let dest = cache_path_for(hash, cache_dir);
    if dest.exists() {
        return Ok(());
    }
    ensure_dir(dest.parent().unwrap_or(cache_dir))?;
    let tmp = dest.with_extension(format!("tmp-{}-{}", std::process::id(), rand_suffix()));
    std::fs::copy(abs_path, &tmp)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

/// 缓存是否存在指定内容（checkout 前求差用）。
pub fn cache_has(hash: &str, cache_dir: &Path) -> bool {
    cache_path_for(hash, cache_dir).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "3fa1c8d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c";

    #[test]
    fn blob_path_is_two_level_sharded() {
        let p = blob_path(H, Path::new("/c"));
        assert_eq!(
            p,
            Path::new("/c/3f/a1").join(H),
            "两级分片：<dir>/3f/a1/<hash>"
        );
    }

    #[test]
    fn store_moves_file_into_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let src = dir.path().join("dl.tmp");
        std::fs::write(&src, b"hello").unwrap();
        let dest = store_to_cache(&src, H, &cache).unwrap();
        assert!(dest.exists());
        assert!(!src.exists(), "临时文件应被搬走");
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn store_discards_duplicate_source() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let src = dir.path().join("a.tmp");
        std::fs::write(&src, b"first").unwrap();
        store_to_cache(&src, H, &cache).unwrap();

        let src2 = dir.path().join("b.tmp");
        std::fs::write(&src2, b"second").unwrap();
        let dest = store_to_cache(&src2, H, &cache).unwrap();
        assert!(!src2.exists(), "重复内容应丢弃临时文件");
        assert_eq!(std::fs::read(&dest).unwrap(), b"first", "先落位的副本保留");
    }

    #[test]
    fn pristine_is_hardlinked_to_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let src = dir.path().join("dl.tmp");
        std::fs::write(&src, b"data").unwrap();
        store_to_cache(&src, H, &cache).unwrap();

        let wc = dir.path().join("wc");
        std::fs::create_dir_all(&wc).unwrap();
        let p = ensure_pristine(&wc, H, &cache).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = std::fs::metadata(cache_path_for(H, &cache)).unwrap();
            let b = std::fs::metadata(&p).unwrap();
            assert_eq!(a.ino(), b.ino(), "pristine 必须是硬链接（0 额外空间）");
        }
        // 幂等
        assert_eq!(ensure_pristine(&wc, H, &cache).unwrap(), p);
    }

    #[test]
    fn working_file_is_an_independent_copy() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let src = dir.path().join("dl.tmp");
        std::fs::write(&src, b"baseline").unwrap();
        store_to_cache(&src, H, &cache).unwrap();
        let wc = dir.path().join("wc");
        std::fs::create_dir_all(&wc).unwrap();
        ensure_pristine(&wc, H, &cache).unwrap();

        let dest = wc.join("characters/hero.psd");
        install_working_file(&wc, H, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"baseline");

        // 原地改写工作文件不能影响基线（这正是"工作文件必须独立副本"的原因）
        std::fs::write(&dest, b"edited").unwrap();
        let pristine = pristine_path_for(&wc, H);
        assert_eq!(std::fs::read(&pristine).unwrap(), b"baseline");
    }

    #[test]
    fn install_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let src = dir.path().join("dl.tmp");
        std::fs::write(&src, b"x").unwrap();
        store_to_cache(&src, H, &cache).unwrap();
        let wc = dir.path().join("wc");
        std::fs::create_dir_all(&wc).unwrap();
        ensure_pristine(&wc, H, &cache).unwrap();
        let dest = wc.join("a/b/c/d.bin");
        install_working_file(&wc, H, &dest).unwrap();
        assert!(dest.exists());
    }

    #[test]
    fn import_skips_when_cache_already_has_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let src = dir.path().join("dl.tmp");
        std::fs::write(&src, b"cached").unwrap();
        store_to_cache(&src, H, &cache).unwrap();

        let local = dir.path().join("local.bin");
        std::fs::write(&local, b"different").unwrap();
        import_working_file(&local, &cache, H).unwrap();
        assert_eq!(
            std::fs::read(cache_path_for(H, &cache)).unwrap(),
            b"cached",
            "已有同 hash 内容时不应覆盖"
        );
    }

    #[test]
    fn cache_has_detects_presence() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        assert!(!cache_has(H, &cache));
        let src = dir.path().join("dl.tmp");
        std::fs::write(&src, b"x").unwrap();
        store_to_cache(&src, H, &cache).unwrap();
        assert!(cache_has(H, &cache));
    }
}
