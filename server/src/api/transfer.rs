//! 传输协议 HTTP 端点（§7.2 传输部分）。
//!
//! - POST   /repos/{repo}/blobs/missing           先问后传
//! - PUT    /repos/{repo}/blobs/{hash}            整块上传（< chunk_threshold，幂等，sha256 校验）
//! - POST   /repos/{repo}/blobs/uploads           大文件分块上传：创建
//! - PUT    /repos/{repo}/blobs/uploads/{id}/{n}  第 n 块（幂等，可乱序重发）
//! - GET    /repos/{repo}/blobs/uploads/{id}      断点续传查询
//! - POST   /repos/{repo}/blobs/uploads/{id}/complete  拼装结算（流式复算 sha256）
//! - DELETE /repos/{repo}/blobs/uploads/{id}      中止清理
//! - GET    /repos/{repo}/blobs/{hash}            下载（ETag / Range）
//! - HEAD   /repos/{repo}/blobs/{hash}            探测存在与大小
//!
//! 权限（M2，§4.3）：
//! - **上传**：blob 是内容寻址的，路径级写权限在 `prepare`/`commit` 逐路径校验；
//!   这里额外要求「该 hash 属于本人在本仓库的一个在途 prepare 的 `need_blobs`」，
//!   或该 blob 已存在于仓库（去重/重放，不引入新数据）——避免任何人往仓库塞数据。
//! - **下载**：`read`，按 HEAD 中引用该 blob 的全部路径求解，任一路径不可读即拒绝（fail-closed）。

use crate::acl::Level;
use crate::auth::session::SessionUser;
use crate::error::AppError;
use crate::state::SharedState;
use crate::storage::blob::{BlobStore, Codec};
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Json;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;

use super::auth::require_user;
use super::{principal_of, resolve_repo};

const MB: u64 = 1024 * 1024;

fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 上传授权。满足任一即放行：
/// 1. blob 已存在（内容去重 / 幂等重放，不引入新数据）；
/// 2. hash 属于本人在该仓库的在途 prepare 的 `need_blobs`（该 prepare 已逐路径校验 write）；
/// 3. 本人对仓库根有 `write`（管理员 / 仓库级维护者，保持 M1 语义）。
fn authorize_upload(
    conn: &Connection,
    repo_id: i64,
    user: &SessionUser,
    hash: &str,
) -> Result<(), AppError> {
    let already: bool = conn
        .query_row("SELECT 1 FROM blobs WHERE hash = ?1", params![hash], |_| Ok(true))
        .unwrap_or(false);
    if already {
        return Ok(());
    }

    let now = chrono::Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT need_blobs FROM pending_commits
          WHERE repo_id = ?1 AND user_id = ?2 AND expires_at > ?3",
    )?;
    let mut rows = stmt.query(params![repo_id, user.id, now])?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        if let Ok(need) = serde_json::from_str::<Vec<String>>(&raw) {
            if need.iter().any(|h| h == hash) {
                return Ok(());
            }
        }
    }
    drop(rows);
    drop(stmt);

    let set = crate::storage::acl::load(conn, repo_id)?;
    if set.level("", &principal_of(user)).at_least(Level::Write) {
        return Ok(());
    }
    Err(AppError::PermissionDenied(format!(
        "blob {hash} 无上传授权：需先 POST /commit/prepare 取得 need_blobs，或对仓库根有 write 权限"
    )))
}

/// 下载授权（§4.3 下载 = read）。
///
/// blob 是内容寻址的：只要**存在**一条本人可读的路径其内容为该 blob，即可下载——
/// 这些字节对该用户而言本来就可得（可能来自 HEAD，也可能来自历史修订）。
/// 没有任何路径引用该 blob → 404（不属于本仓库的内容）。
fn authorize_download(
    conn: &Connection,
    repo_id: i64,
    hash: &str,
    user: &SessionUser,
) -> Result<(), AppError> {
    if user.is_admin {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT path FROM (
             SELECT path FROM head_entries WHERE repo_id = ?1 AND blob_hash = ?2
             UNION
             SELECT path FROM changes    WHERE repo_id = ?1 AND blob_hash = ?2
         )",
    )?;
    let paths: Vec<String> = stmt
        .query_map(params![repo_id, hash], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if paths.is_empty() {
        return Err(AppError::NotFound(format!(
            "blob {hash} 未被本仓库任何路径引用"
        )));
    }
    let set = crate::storage::acl::load(conn, repo_id)?;
    let principal = principal_of(user);
    let mut denied: Option<String> = None;
    for p in &paths {
        match set.require(p, &principal, Level::Read) {
            Ok(()) => return Ok(()),
            Err(e) => {
                denied.get_or_insert(e.to_string());
            }
        }
    }
    Err(AppError::PermissionDenied(denied.unwrap_or_else(|| "无 read 权限".into())))
}

// ---------- POST /repos/{repo}/blobs/missing ----------

#[derive(Deserialize)]
pub struct MissingReq {
    pub hashes: Vec<String>,
}

pub async fn missing(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<MissingReq>,
) -> Result<Json<Value>, AppError> {
    require_user(&state, &headers).await?;
    resolve_repo(&state, &repo_name).await?;

    if req.hashes.len() > 1000 {
        return Err(AppError::InvalidArgument("hashes 一次最多 1000 个".into()));
    }
    let missing = tokio::task::block_in_place(|| -> Result<Vec<String>, AppError> {
        let conn = state.lock_db()?;
        let mut out = vec![];
        for h in &req.hashes {
            if !valid_hash(h) {
                return Err(AppError::InvalidArgument(format!("非法 blob hash: {h}")));
            }
            let present: bool = conn
                .query_row("SELECT 1 FROM blobs WHERE hash = ?1", params![h], |_| Ok(true))
                .unwrap_or(false);
            if !present {
                out.push(h.clone());
            }
        }
        Ok(out)
    })?;
    Ok(Json(json!({ "missing": missing })))
}

// ---------- PUT /repos/{repo}/blobs/{hash} ----------

pub async fn put_blob(
    State(state): State<SharedState>,
    Path((repo_name, hash)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    if !valid_hash(&hash) {
        return Err(AppError::InvalidArgument(format!("非法 blob hash: {hash}")));
    }
    let max = state.config.storage.max_file_size_mb * MB;
    let threshold = state.config.storage.chunk_threshold_mb * MB;
    if body.len() as u64 > max {
        return Err(AppError::PayloadTooLarge(format!(
            "超过单文件上限 {} MB",
            state.config.storage.max_file_size_mb
        )));
    }
    if body.len() as u64 > threshold {
        return Err(AppError::PayloadTooLarge(format!(
            "整块上传上限 {} MB，请改走分块上传（POST /repos/{}/blobs/uploads）",
            state.config.storage.chunk_threshold_mb, repo.name
        )));
    }
    let computed = BlobStore::hash_of(&body);
    if !computed.eq_ignore_ascii_case(&hash) {
        return Err(AppError::HashMismatch(format!(
            "上传内容 sha256 不符：路径声明 {hash}，实际 {computed}"
        )));
    }

    let stored = tokio::task::block_in_place(|| {
        let conn = state.lock_db()?;
        authorize_upload(&conn, repo.id, &user, &hash)?;
        state.blob.put(&conn, &body)
    })?;
    Ok(Json(json!({
        "hash": stored.hash,
        "size": stored.size,
        "stored_size": stored.stored_size,
        "codec": stored.codec.as_str(),
        "already_present": stored.already_present,
    })))
}

// ---------- 分块上传 ----------

#[derive(Serialize, Deserialize, Clone)]
struct UploadMeta {
    hash: String,
    size: u64,
    chunk_size: u64,
    /// 归属（M2）：后续 chunk / status / complete / delete 只认创建者本人
    #[serde(default)]
    repo_id: i64,
    #[serde(default)]
    user_id: i64,
}

/// 上传会话归属校验（系统管理员可穿透）。
fn require_upload_owner(
    meta: &UploadMeta,
    user: &SessionUser,
    repo_id: i64,
) -> Result<(), AppError> {
    if (meta.user_id == user.id && meta.repo_id == repo_id) || user.is_admin {
        Ok(())
    } else {
        Err(AppError::PermissionDenied("上传会话不属于当前用户（或不属于该仓库）".into()))
    }
}

fn upload_dir(state: &SharedState, upload_id: &str) -> Option<PathBuf> {
    // upload_id 只允许 uuid simple 形态（32 hex），防路径穿越
    if upload_id.len() != 32 || !upload_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(state.uploads_dir.join(upload_id))
}

fn read_meta(dir: &std::path::Path) -> Result<UploadMeta, AppError> {
    let raw = std::fs::read_to_string(dir.join("meta.json"))
        .map_err(|_| AppError::NotFound("上传会话不存在（可能已完成或被清理）".into()))?;
    serde_json::from_str(&raw)
        .map_err(|e| AppError::Internal(format!("meta.json 解析失败: {e}")))
}

fn write_meta(dir: &std::path::Path, meta: &UploadMeta) -> Result<(), AppError> {
    let tmp = dir.join("meta.json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(serde_json::to_string(meta).unwrap().as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join("meta.json"))?;
    Ok(())
}

fn total_chunks(meta: &UploadMeta) -> u64 {
    meta.size.div_ceil(meta.chunk_size)
}

/// 扫描目录中已落的分块号（磁盘是事实来源，meta.received 不存）。
fn scan_chunks(dir: &std::path::Path) -> Result<Vec<u64>, AppError> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).map_err(|_| AppError::NotFound("上传会话不存在".into()))? {
        let name = e.map_err(|e| AppError::Internal(format!("读目录失败: {e}")))?
            .file_name()
            .into_string()
            .unwrap_or_default();
        if let Some(stem) = name.strip_suffix(".chunk") {
            if let Ok(n) = stem.parse::<u64>() {
                out.push(n);
            }
        }
    }
    out.sort_unstable();
    Ok(out)
}

#[derive(Deserialize)]
pub struct CreateUploadReq {
    pub hash: String,
    pub size: u64,
    pub chunk_size: u64,
}

pub async fn create_upload(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<CreateUploadReq>,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    if !valid_hash(&req.hash) {
        return Err(AppError::InvalidArgument(format!("非法 blob hash: {}", req.hash)));
    }
    let max = state.config.storage.max_file_size_mb * MB;
    let threshold = state.config.storage.chunk_threshold_mb * MB;
    if req.size == 0 {
        return Err(AppError::InvalidArgument("size 必须大于 0（空文件请走整块上传）".into()));
    }
    if req.size > max {
        return Err(AppError::PayloadTooLarge(format!(
            "超过单文件上限 {} MB",
            state.config.storage.max_file_size_mb
        )));
    }
    if req.chunk_size == 0 || req.chunk_size > threshold {
        return Err(AppError::InvalidArgument(format!(
            "chunk_size 必须在 1 ~ {} MB 之间",
            state.config.storage.chunk_threshold_mb
        )));
    }

    let upload_id = uuid::Uuid::new_v4().simple().to_string();
    let result = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        authorize_upload(&conn, repo.id, &user, &req.hash)?;
        let dir = state.uploads_dir.join(&upload_id);
        std::fs::create_dir_all(&dir)?;
        write_meta(
            &dir,
            &UploadMeta {
                hash: req.hash.clone(),
                size: req.size,
                chunk_size: req.chunk_size,
                repo_id: repo.id,
                user_id: user.id,
            },
        )?;
        Ok(json!({
            "upload_id": upload_id,
            "hash": req.hash,
            "size": req.size,
            "chunk_size": req.chunk_size,
            "received_chunks": [],
        }))
    })?;
    Ok(Json(result))
}

pub async fn put_chunk(
    State(state): State<SharedState>,
    Path((repo_name, upload_id, n)): Path<(String, String, u64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let Some(dir) = upload_dir(&state, &upload_id) else {
        return Err(AppError::InvalidArgument("非法 upload_id".into()));
    };

    // 可选：逐块 sha256 头校验（X-Chunk-SHA256）
    if let Some(cv) = headers.get("x-chunk-sha256").and_then(|v| v.to_str().ok()) {
        let computed = BlobStore::hash_of(&body);
        if !computed.eq_ignore_ascii_case(cv) {
            return Err(AppError::HashMismatch(format!(
                "分块 sha256 不符：头声明 {cv}，实际 {computed}"
            )));
        }
    }

    let _upload_lock = state.lock_upload(&upload_id);
    let _guard = _upload_lock.lock().unwrap();
    tokio::task::block_in_place(|| -> Result<(), AppError> {
        if !dir.is_dir() {
            return Err(AppError::NotFound("上传会话不存在（可能已完成或被清理）".into()));
        }
        let meta = read_meta(&dir)?;
        require_upload_owner(&meta, &user, repo.id)?;
        let total = total_chunks(&meta);
        if n >= total {
            return Err(AppError::InvalidArgument(format!(
                "分块号 {n} 越界（共 {total} 块）"
            )));
        }
        let expected = if n + 1 == total {
            meta.size - meta.chunk_size * (total - 1)
        } else {
            meta.chunk_size
        };
        if body.len() as u64 != expected {
            return Err(AppError::InvalidArgument(format!(
                "分块 {n} 长度应为 {expected}，实际 {}",
                body.len()
            )));
        }
        // 原子落盘：tmp → fsync → rename（幂等重发覆盖同内容）
        let tmp = dir.join(format!("chunk.{n}.tmp"));
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join(format!("{n}.chunk")))?;
        Ok(())
    })?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn upload_status(
    State(state): State<SharedState>,
    Path((repo_name, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let Some(dir) = upload_dir(&state, &upload_id) else {
        return Err(AppError::InvalidArgument("非法 upload_id".into()));
    };
    let _upload_lock = state.lock_upload(&upload_id);
    let _guard = _upload_lock.lock().unwrap();
    let (meta, received) = tokio::task::block_in_place(|| -> Result<(UploadMeta, Vec<u64>), AppError> {
        let meta = read_meta(&dir)?;
        require_upload_owner(&meta, &user, repo.id)?;
        let received = scan_chunks(&dir)?;
        Ok((meta, received))
    })?;
    Ok(Json(json!({
        "upload_id": upload_id,
        "hash": meta.hash,
        "size": meta.size,
        "chunk_size": meta.chunk_size,
        "received_chunks": received,
    })))
}

pub async fn complete_upload(
    State(state): State<SharedState>,
    Path((repo_name, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let Some(dir) = upload_dir(&state, &upload_id) else {
        return Err(AppError::InvalidArgument("非法 upload_id".into()));
    };
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let _upload_lock = state.lock_upload(&upload_id);
    let _guard = _upload_lock.lock().unwrap();
    let stored = tokio::task::block_in_place(|| -> Result<crate::storage::blob::StoredBlob, AppError> {
        let meta = read_meta(&dir)?;
        require_upload_owner(&meta, &user, repo.id)?;
        let total = total_chunks(&meta);
        let received = scan_chunks(&dir)?;
        if received.len() as u64 != total {
            let missing: Vec<u64> = (0..total).filter(|n| !received.contains(n)).collect();
            return Err(AppError::Conflict {
                code: "OUT_OF_DATE",
                message: format!("分块不完整（缺 {}/{} 块），请先补传", missing.len(), total),
                details: json!({ "missing_chunks": missing }),
            });
        }
        // 逐块长度校验（磁盘是事实来源）
        for n in 0..total {
            let expected = if n + 1 == total {
                meta.size - meta.chunk_size * (total - 1)
            } else {
                meta.chunk_size
            };
            let len = std::fs::metadata(dir.join(format!("{n}.chunk")))?.len();
            if len != expected {
                return Err(AppError::InvalidArgument(format!(
                    "分块 {n} 长度 {len} ≠ 期望 {expected}，请重发该块"
                )));
            }
        }

        // 拼装到 blob tmp 区（流式追加，不整体进内存）
        let assembled = state.blob.tmp_dir().join(format!(
            "{}.assemble",
            uuid::Uuid::new_v4().simple()
        ));
        {
            let mut out = std::fs::File::create(&assembled)?;
            for n in 0..total {
                let mut f = std::fs::File::open(dir.join(format!("{n}.chunk")))?;
                std::io::copy(&mut f, &mut out)?;
            }
            out.sync_all()?;
        }

        // 流式复算 sha256 并入位；不匹配则 412
        let conn = state.lock_db()?;
        let stored = state.blob.put_from_file(&conn, &assembled, Some(&meta.hash))?;

        // 清理暂存目录
        let _ = std::fs::remove_dir_all(&dir);
        crate::audit::log(
            &conn,
            Some(user.id),
            Some(repo.id),
            "blob.upload.complete",
            &format!("blob:{}", stored.hash),
            &format!("分块上传结算 {} 字节", stored.size),
            &ip,
        )?;
        Ok(stored)
    })?;
    state.forget_upload(&upload_id);

    Ok(Json(json!({
        "hash": stored.hash,
        "size": stored.size,
        "stored_size": stored.stored_size,
        "codec": stored.codec.as_str(),
        "already_present": stored.already_present,
    })))
}

pub async fn delete_upload(
    State(state): State<SharedState>,
    Path((repo_name, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let Some(dir) = upload_dir(&state, &upload_id) else {
        return Err(AppError::InvalidArgument("非法 upload_id".into()));
    };
    let _upload_lock = state.lock_upload(&upload_id);
    let _guard = _upload_lock.lock().unwrap();
    tokio::task::block_in_place(|| {
        require_upload_owner(&read_meta(&dir)?, &user, repo.id)?;
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok::<_, AppError>(())
    })?;
    state.forget_upload(&upload_id);
    Ok(StatusCode::NO_CONTENT)
}

// ---------- GET / HEAD /repos/{repo}/blobs/{hash} ----------

fn parse_range(h: Option<&HeaderValue>, size: u64) -> Option<(u64, u64)> {
    let h = h?.to_str().ok()?;
    let spec = h.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: u64 = a.parse().ok()?;
    if start >= size {
        return None;
    }
    let end = if b.is_empty() {
        size - 1
    } else {
        b.parse::<u64>().ok()?.min(size - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

/// raw 文件从 offset 起的流式读取。
async fn file_stream(path: PathBuf, offset: u64) -> Result<Body, AppError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut f = tokio::fs::File::open(&path)
        .await
        .map_err(|e| AppError::Internal(format!("打开 blob 失败: {e}")))?;
    if offset > 0 {
        f.seek(std::io::SeekFrom::Start(offset)).await?;
    }
    let stream = futures_util::stream::unfold(f, |mut f| async move {
        let mut buf = vec![0u8; 256 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => Some((Ok(Bytes::copy_from_slice(&buf[..n])), f)),
            Err(e) => Some((Err(e), f)),
        }
    });
    Ok(Body::from_stream(stream))
}

/// zstd blob：后台线程流式解压 → channel → body（Range 不支持，返回全量）。
fn zstd_stream(path: PathBuf, expected_size: u64) -> Result<Body, AppError> {
    use std::io::Read as _;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    std::thread::spawn(move || {
        let fail = |e: std::io::Error| {
            let _ = tx.blocking_send(Err(e));
        };
        let f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => return fail(e),
        };
        let mut dec = match zstd::stream::read::Decoder::new(f) {
            Ok(d) => d,
            Err(e) => return fail(std::io::Error::other(e)),
        };
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match dec.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(Ok(Bytes::copy_from_slice(&buf[..n]))).is_err() {
                        return; // 接收端（客户端）已断开
                    }
                }
                Err(e) => return fail(e),
            }
        }
        let _ = expected_size;
    });
    Ok(Body::from_stream(tokio_stream_wrapper(rx)))
}

/// tokio mpsc Receiver 本身实现 Stream（sync feature），包一层适配。
fn tokio_stream_wrapper(
    mut rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

fn blob_headers(
    builder: axum::http::response::Builder,
    hash: &str,
    size: u64,
) -> axum::http::response::Builder {
    builder
        .header(header::ETAG, format!("\"{hash}\""))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size)
}

pub async fn download_blob(
    State(state): State<SharedState>,
    Path((repo_name, hash)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    serve_blob(state, repo_name, hash, headers, false).await
}

pub async fn head_blob(
    State(state): State<SharedState>,
    Path((repo_name, hash)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    serve_blob(state, repo_name, hash, headers, true).await
}

async fn serve_blob(
    state: SharedState,
    repo_name: String,
    hash: String,
    headers: HeaderMap,
    head_only: bool,
) -> Result<Response, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    if !valid_hash(&hash) {
        return Err(AppError::InvalidArgument(format!("非法 blob hash: {hash}")));
    }

    let (meta, path) = {
        let conn = state.lock_db()?;
        // §4.3：下载 = read，按 HEAD 中引用该 blob 的路径求解
        authorize_download(&conn, repo.id, &hash, &user)?;
        let meta = state
            .blob
            .meta(&conn, &hash)?
            .ok_or_else(|| AppError::NotFound(format!("blob {hash} 不存在")))?;
        (meta, state.blob.blob_path(&hash))
    };
    if !path.exists() {
        return Err(AppError::NotFound(format!("blob {hash} 文件缺失（元数据存在但磁盘无文件）")));
    }

    // Range：仅 raw 支持；zstd 忽略 Range 返回全量（M1 简化）
    let range = if meta.codec == Codec::Raw {
        headers.get(header::RANGE).and_then(|v| parse_range(Some(v), meta.size))
    } else {
        None
    };

    if head_only {
        let mut b = blob_headers(
            axum::http::Response::builder().status(StatusCode::OK),
            &hash,
            meta.size,
        );
        if let Some((s, e)) = range {
            b = b
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{}", meta.size))
                .header(header::CONTENT_LENGTH, e - s + 1);
        }
        return Ok(b.body(Body::empty()).map_err(|e| AppError::Internal(format!("{e}")))?);
    }

    match (meta.codec, range) {
        (Codec::Raw, Some((s, e))) => {
            let stream = file_stream(path, s).await?;
            let resp = axum::http::Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{}", meta.size))
                .header(header::ETAG, format!("\"{hash}\""))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, e - s + 1)
                .body(stream)
                .map_err(|e| AppError::Internal(format!("{e}")))?;
            Ok(resp)
        }
        (Codec::Raw, None) => {
            let stream = file_stream(path, 0).await?;
            let resp = blob_headers(
                axum::http::Response::builder().status(StatusCode::OK),
                &hash,
                meta.size,
            )
            .body(stream)
            .map_err(|e| AppError::Internal(format!("{e}")))?;
            Ok(resp)
        }
        (Codec::Zstd, _) => {
            let stream = zstd_stream(path, meta.size)?;
            let resp = blob_headers(
                axum::http::Response::builder().status(StatusCode::OK),
                &hash,
                meta.size,
            )
            .body(stream)
            .map_err(|e| AppError::Internal(format!("{e}")))?;
            Ok(resp)
        }
    }
}
