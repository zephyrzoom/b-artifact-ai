//! BlobStore：内容寻址存储（§3.1 / §3.4）。
//!
//! - sha256 两级分片目录 `blobs/xx/yy/<hash>`，天然去重
//! - zstd auto：压缩后体积 ≤ 原体积 95% 才存压缩版（codec 记录在 blobs 表）
//! - 原子写：tmp/<uuid> → fsync → rename 到最终位置，不存在"半个 blob"
//! - `blobs.refcount` 只是可重建缓存，引用事实来源是 changes 表（§3.7）

use crate::error::AppError;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const ZSTD_LEVEL: i32 = 3;
/// 仅当压缩收益 > 5% 时才存压缩版（compressed * 20 <= original * 19）。
const GAIN_NUM: u64 = 19;
const GAIN_DEN: u64 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Raw,
    Zstd,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::Raw => "raw",
            Codec::Zstd => "zstd",
        }
    }
    fn parse(s: &str) -> Codec {
        if s == "zstd" {
            Codec::Zstd
        } else {
            Codec::Raw
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredBlob {
    pub hash: String,
    /// 原始大小
    pub size: u64,
    /// 落盘大小（压缩后）
    pub stored_size: u64,
    pub codec: Codec,
    /// true = 内容此前已存在（去重命中，未重复写盘）
    pub already_present: bool,
}

#[derive(Debug, Clone)]
pub struct BlobMeta {
    pub size: u64,
    pub stored_size: u64,
    pub codec: Codec,
    pub refcount: i64,
}

pub struct BlobStore {
    blobs_dir: PathBuf,
    tmp_dir: PathBuf,
}

impl BlobStore {
    /// 初始化 data/blobs 与 data/tmp 目录。
    pub fn new(data_dir: &Path) -> std::io::Result<BlobStore> {
        let blobs_dir = data_dir.join("blobs");
        let tmp_dir = data_dir.join("tmp");
        fs::create_dir_all(&blobs_dir)?;
        fs::create_dir_all(&tmp_dir)?;
        Ok(BlobStore { blobs_dir, tmp_dir })
    }

    /// sha256 前 2 + 次 2 位分片：blobs/xx/yy/<hash>（§3.1）。
    pub fn blob_path(&self, hash: &str) -> PathBuf {
        let (a, b) = (&hash[..2], &hash[2..4]);
        self.blobs_dir.join(a).join(b).join(hash)
    }

    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    /// 计算 sha256（hex）。
    pub fn hash_of(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    /// auto 策略下选择编码（§3.4）。
    pub fn choose_codec(data: &[u8]) -> Result<(Codec, Vec<u8>), AppError> {
        if data.is_empty() {
            return Ok((Codec::Raw, Vec::new()));
        }
        let compressed = zstd::bulk::compress(data, ZSTD_LEVEL)
            .map_err(|e| AppError::Internal(format!("zstd compress failed: {e}")))?;
        // 压缩收益 ≤ 5% → 存原始
        if (compressed.len() as u64) * GAIN_DEN <= (data.len() as u64) * GAIN_NUM {
            Ok((Codec::Zstd, compressed))
        } else {
            Ok((Codec::Raw, data.to_vec()))
        }
    }

    /// 写入 blob：tmp → fsync → rename 原子入位；重复内容直接去重返回。
    pub fn put(&self, conn: &Connection, data: &[u8]) -> Result<StoredBlob, AppError> {
        let hash = Self::hash_of(data);
        let size = data.len() as u64;

        // 去重：磁盘文件与元数据行都在 → 直接返回
        let dst = self.blob_path(&hash);
        if let Some(meta) = self.meta(conn, &hash)? {
            if dst.exists() {
                return Ok(StoredBlob {
                    hash,
                    size,
                    stored_size: meta.stored_size,
                    codec: meta.codec,
                    already_present: true,
                });
            }
        }

        let (codec, payload) = Self::choose_codec(data)?;
        let stored_size = payload.len() as u64;

        // 原子写：先落 tmp，fsync 后 rename 入位（§3.1）
        let tmp_path = self.tmp_dir.join(format!(
            "{}.blob",
            uuid::Uuid::new_v4().simple()
        ));
        {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(&payload)?;
            f.sync_all()?;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        // 并发写同一 blob：rename 原子，后到者覆盖同内容文件，无副作用
        fs::rename(&tmp_path, &dst)?;

        conn.execute(
            "INSERT OR IGNORE INTO blobs (hash, size, stored_size, codec, refcount, created_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![hash, size as i64, stored_size as i64, codec.as_str(), now()],
        )?;

        Ok(StoredBlob {
            hash,
            size,
            stored_size,
            codec,
            already_present: false,
        })
    }

    /// 从文件流式写入 blob（分块上传 complete 用，§7.2）：
    /// - pass 1 流式计算 sha256 与 size（不整体进内存）
    /// - expected_hash 不匹配 → Err(HashMismatch)（不落库不落盘）
    /// - raw 编码直接 rename 源文件入位（调用方保证 src 不再使用）；zstd 走流式压缩临时文件
    pub fn put_from_file(
        &self,
        conn: &Connection,
        src: &Path,
        expected_hash: Option<&str>,
    ) -> Result<StoredBlob, AppError> {
        use std::io::{Read, Seek};

        // pass 1：哈希 + 大小
        let mut f = fs::File::open(src)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut size: u64 = 0;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as u64;
        }
        let hash = hex::encode(hasher.finalize());
        if let Some(exp) = expected_hash {
            if !exp.eq_ignore_ascii_case(&hash) {
                return Err(AppError::HashMismatch(format!(
                    "分块拼装内容 sha256 不符：期望 {exp}，实际 {hash}"
                )));
            }
        }

        // 去重
        let dst = self.blob_path(&hash);
        if let Some(meta) = self.meta(conn, &hash)? {
            if dst.exists() {
                return Ok(StoredBlob {
                    hash,
                    size,
                    stored_size: meta.stored_size,
                    codec: meta.codec,
                    already_present: true,
                });
            }
        }

        // pass 2：编码落盘
        f.rewind()?;
        let (codec, stored_size) = if size == 0 {
            // 空文件：raw，直接 rename（源为空文件）
            (Codec::Raw, 0u64)
        } else {
            // 先流式压缩到临时文件，再按收益决定
            let tmp_z = self.tmp_dir.join(format!("{}.zst", uuid::Uuid::new_v4().simple()));
            let zlen = {
                let mut out = fs::File::create(&tmp_z)?;
                {
                    let mut enc =
                        zstd::stream::Encoder::new(&mut out, ZSTD_LEVEL)?;
                    std::io::copy(&mut f, &mut enc)?;
                    enc.finish()?;
                }
                out.sync_all()?;
                out.metadata()?.len()
            };
            if zlen.saturating_mul(GAIN_DEN) <= size.saturating_mul(GAIN_NUM) {
                // 压缩有收益：用压缩临时文件，重读源文件无需
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&tmp_z, &dst)?;
                (Codec::Zstd, zlen)
            } else {
                // 回退 raw：删临时压缩文件，直接 rename 源文件
                let _ = fs::remove_file(&tmp_z);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(src, &dst)?;
                (Codec::Raw, size)
            }
        };
        if size == 0 {
            // 空文件：src 可能已被上层约定为不可用，直接建空文件
            let tmp_e = self.tmp_dir.join(format!("{}.empty", uuid::Uuid::new_v4().simple()));
            {
                let e = fs::File::create(&tmp_e)?;
                e.sync_all()?;
            }
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&tmp_e, &dst)?;
            let _ = fs::remove_file(src);
        }

        conn.execute(
            "INSERT OR IGNORE INTO blobs (hash, size, stored_size, codec, refcount, created_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![hash, size as i64, stored_size as i64, codec.as_str(), now()],
        )?;

        Ok(StoredBlob { hash, size, stored_size, codec, already_present: false })
    }

    /// 读取 blob：按 codec 透明解压。
    pub fn get(&self, conn: &Connection, hash: &str) -> Result<Option<Vec<u8>>, AppError> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AppError::InvalidArgument(format!("非法 blob hash: {hash}")));
        }
        let Some(meta) = self.meta(conn, hash)? else {
            return Ok(None);
        };
        let path = self.blob_path(hash);
        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let data = match meta.codec {
            Codec::Raw => raw,
            Codec::Zstd => zstd::bulk::decompress(&raw, meta.size as usize)
                .map_err(|e| AppError::Internal(format!("zstd decompress failed: {e}")))?,
        };
        Ok(Some(data))
    }

    /// blobs 表元数据。
    pub fn meta(&self, conn: &Connection, hash: &str) -> Result<Option<BlobMeta>, AppError> {
        let row = conn
            .query_row(
                "SELECT size, stored_size, codec, refcount FROM blobs WHERE hash = ?1",
                params![hash],
                |r| {
                    Ok(BlobMeta {
                        size: r.get::<_, i64>(0)? as u64,
                        stored_size: r.get::<_, i64>(1)? as u64,
                        codec: Codec::parse(&r.get::<_, String>(2)?),
                        refcount: r.get(3)?,
                    })
                },
            );
        match row {
            Ok(m) => Ok(Some(m)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db;
    use std::sync::Arc;

    /// 可压缩样本（重复模式）
    fn compressible(n: usize) -> Vec<u8> {
        let pattern = b"b-artifact compressible sample block. ";
        pattern.iter().copied().cycle().take(n).collect()
    }

    /// 不可压缩样本（固定种子 xorshift64，§12.2）
    fn incompressible(n: usize) -> Vec<u8> {
        let mut s: u64 = 0x9E3779B97F4A7C15;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 32) as u8
            })
            .collect()
    }

    #[test]
    fn put_get_roundtrip_zstd_auto() {
        let (dir, conn) = db::tests::test_db();
        let store = BlobStore::new(dir.path()).unwrap();

        let data = compressible(200_000);
        let stored = store.put(&conn, &data).unwrap();
        assert_eq!(stored.codec, Codec::Zstd, "可压缩内容应选 zstd");
        assert!(stored.stored_size < stored.size);

        let back = store.get(&conn, &stored.hash).unwrap().unwrap();
        assert_eq!(back, data, "取回应与原始字节一致");
        assert_eq!(BlobStore::hash_of(&back), stored.hash);
    }

    #[test]
    fn auto_threshold_skips_incompressible() {
        let (dir, conn) = db::tests::test_db();
        let store = BlobStore::new(dir.path()).unwrap();

        let data = incompressible(1_000_000);
        let stored = store.put(&conn, &data).unwrap();
        assert_eq!(stored.codec, Codec::Raw, "随机数据压缩收益 ≤5% 应回退 raw");
        assert_eq!(stored.stored_size, stored.size);

        let back = store.get(&conn, &stored.hash).unwrap().unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn dedup_same_content_single_file() {
        let (dir, conn) = db::tests::test_db();
        let store = BlobStore::new(dir.path()).unwrap();

        let data = compressible(50_000);
        let first = store.put(&conn, &data).unwrap();
        assert!(!first.already_present);

        let second = store.put(&conn, &data).unwrap();
        assert!(second.already_present, "同内容重传应命中去重");
        assert_eq!(first.hash, second.hash);

        // 磁盘上只有一个 blob 文件，blobs 表只有一行
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let mut files = vec![];
        collect_files(dir.path().join("blobs"), &mut files);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn atomic_write_no_leftover_tmp() {
        let (dir, conn) = db::tests::test_db();
        let store = BlobStore::new(dir.path()).unwrap();

        let data = compressible(10_000);
        let stored = store.put(&conn, &data).unwrap();

        // tmp 目录不留残余；目标文件完整存在
        let leftovers: Vec<_> = std::fs::read_dir(store.tmp_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(leftovers.is_empty(), "tmp 不应残留临时文件: {leftovers:?}");
        let p = store.blob_path(&stored.hash);
        assert!(p.exists());
        assert!(std::fs::metadata(&p).unwrap().len() > 0);
    }

    #[test]
    fn concurrent_put_same_blob_lands_once() {
        let (dir, _conn) = db::tests::test_db();
        let dir = Arc::new(dir);
        let db_path = dir.path().join("b-artifact.db");
        let store = Arc::new(BlobStore::new(dir.path()).unwrap());

        let data = Arc::new(compressible(120_000));
        let mut handles = vec![];
        for _ in 0..8 {
            let store = Arc::clone(&store);
            let data = Arc::clone(&data);
            let db_path = db_path.clone();
            handles.push(std::thread::spawn(move || {
                // 每线程独立连接（WAL 支持并发读 + busy_timeout 串行写）
                let conn = db::open(&db_path).unwrap();
                let r = store.put(&conn, &data).unwrap();
                assert_eq!(r.hash, BlobStore::hash_of(&data));
            }));
        }
        for h in handles {
            h.join().expect("thread panicked");
        }

        // 恰好一个磁盘文件、一行元数据，内容可正常读回
        let mut files = vec![];
        collect_files(dir.path().join("blobs"), &mut files);
        assert_eq!(files.len(), 1, "并发写同一 blob 只落一个文件");

        let check_conn = db::open(&db_path).unwrap();
        let count: i64 = check_conn
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let back = store.get(&check_conn, &BlobStore::hash_of(&data)).unwrap();
        assert_eq!(back.as_deref(), Some(data.as_slice()));
    }

    fn collect_files(dir: std::path::PathBuf, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    collect_files(p, out);
                } else {
                    out.push(p);
                }
            }
        }
    }
}
