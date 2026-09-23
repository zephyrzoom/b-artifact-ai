use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::Compression;
use crate::error::AppError;

/// 内容寻址存储（CAS）。
/// 磁盘布局：`<root>/<h[0..2]>/<h[2..4]>/<hash>`，zstd 压缩版追加 `.z` 后缀。
/// 哈希永远是"原始内容"的 sha256，压缩对客户端透明。
pub struct BlobStore {
    root: PathBuf,
    tmp: PathBuf,
    compression: Compression,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BlobInfo {
    pub hash: String,
    pub size: u64,
    pub stored_size: u64,
    pub codec: &'static str,
}

/// zstd 仅在收益 ≥ 5% 且原始体积 ≥ 1KB 时启用
const COMPRESS_MIN_SIZE: usize = 1024;

impl BlobStore {
    pub fn new(root: &Path, tmp: &Path, compression: Compression) -> anyhow::Result<Self> {
        fs::create_dir_all(root)?;
        fs::create_dir_all(tmp)?;
        Ok(Self {
            root: root.to_path_buf(),
            tmp: tmp.to_path_buf(),
            compression,
        })
    }

    pub fn validate_hash(hash: &str) -> Result<(), AppError> {
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(AppError::validation("非法 blob hash（应为 64 位十六进制）"));
        }
        Ok(())
    }

    fn raw_path(&self, hash: &str) -> PathBuf {
        self.root.join(&hash[0..2]).join(&hash[2..4]).join(hash)
    }

    fn zstd_path(&self, hash: &str) -> PathBuf {
        self.raw_path(hash).with_extension("z")
    }

    pub fn exists(&self, hash: &str) -> bool {
        Self::validate_hash(hash).is_ok()
            && (self.raw_path(hash).is_file() || self.zstd_path(hash).is_file())
    }

    pub fn codec_of(&self, hash: &str) -> Option<&'static str> {
        if !Self::validate_hash(hash).is_ok() {
            return None;
        }
        if self.raw_path(hash).is_file() {
            Some("raw")
        } else if self.zstd_path(hash).is_file() {
            Some("zstd")
        } else {
            None
        }
    }

    /// 写入一份内容：校验哈希 → 压缩决策 → 临时文件 fsync → 原子 rename 入位。
    /// 幂等：同哈希已存在时直接返回（内容寻址保证一致）。
    pub fn put(&self, data: &[u8]) -> Result<BlobInfo, AppError> {
        if data.is_empty() {
            return Err(AppError::validation("blob 内容为空"));
        }
        let hash = hex::encode(Sha256::digest(data));

        let (stored, codec) = match self.compression {
            Compression::Auto if data.len() >= COMPRESS_MIN_SIZE => {
                match zstd::bulk::compress(data, 3) {
                    Ok(c) if c.len() * 20 <= data.len() * 19 => (c, "zstd"),
                    _ => (data.to_vec(), "raw"),
                }
            }
            _ => (data.to_vec(), "raw"),
        };

        let dest = self.raw_path(&hash);
        let final_path = if codec == "zstd" { self.zstd_path(&hash) } else { dest.clone() };

        if !final_path.is_file() {
            if let Some(parent) = final_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let tmp_path = self
                .tmp
                .join(format!("blob-{}-{}", &hash[..8], std::process::id()));
            {
                let mut f = fs::File::create(&tmp_path)?;
                f.write_all(&stored)?;
                f.sync_all()?;
            }
            fs::rename(&tmp_path, &final_path)?;
        }

        Ok(BlobInfo {
            hash,
            size: data.len() as u64,
            stored_size: stored.len() as u64,
            codec,
        })
    }

    /// 读取并解压（如适用），返回原始内容。
    pub fn get(&self, hash: &str) -> Result<Vec<u8>, AppError> {
        Self::validate_hash(hash)?;
        let raw_path = self.raw_path(hash);
        let zstd_path = self.zstd_path(hash);

        if raw_path.is_file() {
            let data = fs::read(&raw_path)?;
            Self::verify(data.as_slice(), hash)?;
            Ok(data)
        } else if zstd_path.is_file() {
            let compressed = fs::read(&zstd_path)?;
            let data = zstd::stream::decode_all(Cursor::new(&compressed))
                .map_err(|e| AppError::internal(format!("zstd 解码失败: {e}")))?;
            Self::verify(data.as_slice(), hash)?;
            Ok(data)
        } else {
            Err(AppError::NotFound)
        }
    }

    /// 读取落盘的原始字节（不解压，供 HTTP 层做流式/Range 传输时使用）
    pub fn get_stored_bytes(&self, hash: &str) -> Result<(Vec<u8>, &'static str), AppError> {
        Self::validate_hash(hash)?;
        if let Ok(data) = fs::read(self.raw_path(hash)) {
            Ok((data, "raw"))
        } else if let Ok(data) = fs::read(self.zstd_path(hash)) {
            Ok((data, "zstd"))
        } else {
            Err(AppError::NotFound)
        }
    }

    fn verify(data: &[u8], hash: &str) -> Result<(), AppError> {
        let actual = hex::encode(Sha256::digest(data));
        if actual != hash {
            return Err(AppError::internal(format!(
                "blob 内容损坏: 期望 {hash}，实际 {actual}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(compression: Compression) -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(
            &dir.path().join("blobs"),
            &dir.path().join("tmp"),
            compression,
        )
        .unwrap();
        (dir, blobs)
    }

    #[test]
    fn put_get_roundtrip_raw() {
        let (_d, s) = store(Compression::Off);
        let data = b"hello b-artifact \xE2\x82\xAC binary \x00\x01\x02".to_vec();
        let info = s.put(&data).unwrap();
        assert_eq!(info.codec, "raw");
        assert_eq!(s.get(&info.hash).unwrap(), data);
        assert!(s.exists(&info.hash));
        assert_eq!(s.codec_of(&info.hash), Some("raw"));
    }

    #[test]
    fn compressible_data_uses_zstd_when_auto() {
        let (_d, s) = store(Compression::Auto);
        let data = "the quick brown fox jumps over the lazy dog. ".repeat(500);
        let info = s.put(data.as_bytes()).unwrap();
        assert_eq!(info.codec, "zstd");
        assert!(info.stored_size < info.size);
        assert_eq!(s.get(&info.hash).unwrap(), data.as_bytes());
    }

    #[test]
    fn incompressible_data_stays_raw_when_auto() {
        let (_d, s) = store(Compression::Auto);
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let data: Vec<u8> = (0..4096).map(|_| next() as u8).collect();
        let info = s.put(&data).unwrap();
        assert_eq!(info.codec, "raw");
        assert_eq!(s.get(&info.hash).unwrap(), data);
    }

    #[test]
    fn put_is_idempotent() {
        let (_d, s) = store(Compression::Off);
        let data = b"same content twice";
        let a = s.put(data).unwrap();
        let b = s.put(data).unwrap();
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn rejects_bad_hash_and_missing() {
        let (_d, s) = store(Compression::Off);
        assert!(s.get("not-a-hash").is_err());
        let valid = "a".repeat(64);
        assert!(matches!(s.get(&valid), Err(AppError::NotFound)));
    }
}
