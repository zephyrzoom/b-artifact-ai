/**
 * 内容哈希（§6.3 提交流程步骤 2）。
 *
 * 状态判定以内容哈希为准（mtime 只作快速跳过的启发式），因此哈希必须流式——
 * 美术资源单个文件动辄几百 MB，不能整个读进内存。
 */
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

/// 空内容的 sha256（与 git empty blob 同款语义，便于对齐心智模型）。
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// 分块大小：与 blob 上传的分块默认对齐。
pub const DEFAULT_CHUNK_SIZE: u64 = 8 * 1024 * 1024;
/// 整块上传上限（§7.2）：超过则走分块协议。
pub const WHOLE_BLOB_LIMIT: u64 = 64 * 1024 * 1024;

const READ_BUF: usize = 64 * 1024;

/// 内存数据哈希。
pub fn hash_buffer(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// 文件哈希（流式，大文件不爆内存）。
pub fn hash_file(abs_path: &Path) -> io::Result<String> {
    let mut f = File::open(abs_path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; READ_BUF];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// 是否需要走分块上传。
pub fn needs_chunked_upload(size: u64) -> bool {
    size > WHOLE_BLOB_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_blob_hash_matches_git() {
        assert_eq!(hash_buffer(b""), EMPTY_SHA256);
    }

    #[test]
    fn known_vector() {
        // sha256("abc")
        assert_eq!(
            hash_buffer(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn file_hash_streams_multi_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.bin");
        // 跨过 READ_BUF 边界，验证多次 read 拼接的正确性
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        assert_eq!(hash_file(&p).unwrap(), hash_buffer(&data));
    }

    #[test]
    fn chunked_threshold_is_strictly_greater() {
        assert!(!needs_chunked_upload(WHOLE_BLOB_LIMIT));
        assert!(needs_chunked_upload(WHOLE_BLOB_LIMIT + 1));
    }
}
