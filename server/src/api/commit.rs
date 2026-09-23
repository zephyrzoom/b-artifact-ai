//! 提交 HTTP 端点（§5.3 / §7.3）。
//!
//! - POST /repos/{repo}/commit/prepare  校验 + 签发 commit_token（同 commit_id 幂等）
//! - POST /repos/{repo}/commit          落库（commit_id 幂等，网络重试安全）

use crate::acl::Level;
use crate::error::AppError;
use crate::state::SharedState;
use crate::storage::repo::{self, Change, Kind, Op, RepoRow};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use chrono::{Duration, Utc};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::auth::require_user;
use super::{principal_of, resolve_repo};

/// commit_token 有效期（秒）。
pub const TOKEN_TTL_SECS: i64 = 300;

// ---------- 请求/响应模型 ----------

#[derive(Deserialize, Debug)]
pub struct ChangeJson {
    pub path: String,
    pub op: String,
    pub kind: String,
    pub blob_hash: Option<String>,
    #[serde(default)]
    pub size: i64,
    #[serde(default = "default_mode")]
    pub mode: i64,
    #[serde(default)]
    pub mtime: i64,
}

fn default_mode() -> i64 {
    0o644
}

fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// ChangeJson → Change（语义校验：op/kind 合法、file+add/modify 必带 blob_hash、无重复路径）。
fn to_changes(raw: &[ChangeJson]) -> Result<Vec<Change>, AppError> {
    let mut changes = Vec::with_capacity(raw.len());
    let mut seen = std::collections::HashSet::new();
    for c in raw {
        let op = Op::parse(&c.op).ok_or_else(|| {
            AppError::InvalidArgument(format!("路径 `{}`: 未知 op `{}`", c.path, c.op))
        })?;
        let kind = Kind::parse(&c.kind).ok_or_else(|| {
            AppError::InvalidArgument(format!("路径 `{}`: 未知 kind `{}`", c.path, c.kind))
        })?;
        if !seen.insert(c.path.clone()) {
            return Err(AppError::InvalidArgument(format!(
                "变更清单中路径 `{}` 重复",
                c.path
            )));
        }
        let blob_hash = match (kind, op) {
            (Kind::File, Op::Add | Op::Modify) => {
                let h = c.blob_hash.as_deref().ok_or_else(|| {
                    AppError::InvalidArgument(format!(
                        "路径 `{}`: file {} 变更必须携带 blob_hash",
                        c.path, op.as_str()
                    ))
                })?;
                if !valid_hash(h) {
                    return Err(AppError::InvalidArgument(format!(
                        "路径 `{}`: 非法 blob hash {h}",
                        c.path
                    )));
                }
                Some(h.to_string())
            }
            (Kind::Dir, Op::Modify) => {
                return Err(AppError::InvalidArgument(format!(
                    "路径 `{}`: 目录不支持 modify（无内容可改）",
                    c.path
                )));
            }
            _ => None,
        };
        changes.push(Change {
            path: c.path.clone(),
            op,
            kind,
            blob_hash,
            size: c.size.max(0),
            mode: c.mode,
            mtime: c.mtime.max(0),
        });
    }
    Ok(changes)
}

/// changes_hash：变更按 path 排序的 canonical JSON 的 sha256（与 commit_token 绑定，§5.3）。
fn changes_hash(changes: &[Change]) -> String {
    let mut sorted: Vec<&Change> = changes.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let arr: Vec<Value> = sorted
        .iter()
        .map(|c| {
            json!({
                "path": c.path,
                "op": c.op.as_str(),
                "kind": c.kind.as_str(),
                "blob_hash": c.blob_hash,
                "size": c.size,
                "mode": c.mode,
                "mtime": c.mtime,
            })
        })
        .collect();
    let canonical = serde_json::to_string(&Value::Array(arr)).expect("canonical json");
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn normalize_commit_id(raw: &str) -> Result<String, AppError> {
    let id = uuid::Uuid::parse_str(raw)
        .map_err(|_| AppError::InvalidArgument(format!("commit_id `{raw}` 不是合法 UUID")))?;
    Ok(id.to_string())
}

/// 逐路径校验（§5.3 步骤 3/4，§4.3 逐路径校验）：
///
/// 1. **ACL**：每个变更路径独立求 `effective_level`，任一不满足 `write` 即整单拒绝，
///    并返回具体哪条路径越权。
/// 2. **锁（v0.4.17 唯一语义）**：**每个文件变更路径都必须由本人持有效锁**，否则
///    整单 412 `NEEDS_LOCK` + 未持锁路径清单（客户端据此自动补锁，§6.5）。
///    不再有 advisory 放行，也不再有 `needs_lock` glob 白名单；目录条目不是变更路径
///    （§5.2），不参与锁校验。
///
/// 被**他人**持锁的路径同样计入"未持锁"，但额外把持有者带在 `details.locked_by` ——
/// 客户端自动补锁时这类路径注定失败，提前点名比逐个试错友好。
///
/// 幂等重放不经过这里（在调用前已短路返回）。
fn check_paths(
    conn: &rusqlite::Connection,
    repo: &RepoRow,
    user: &crate::auth::session::SessionUser,
    changes: &[Change],
) -> Result<(), AppError> {
    let set = crate::storage::acl::load(conn, repo.id)?;
    let principal = principal_of(user);
    for c in changes {
        set.require(&c.path, &principal, Level::Write)?;
    }

    let mut needs_lock_paths: Vec<String> = vec![];
    let mut locked_by_others: Vec<serde_json::Value> = vec![];
    for c in changes {
        if c.kind != Kind::File {
            continue;
        }
        match crate::locks::covering_lock(conn, repo.id, &c.path)? {
            // 本人持锁 —— 唯一放行条件
            Some(l) if l.owner_id == user.id => {}
            Some(l) => {
                locked_by_others.push(serde_json::json!({
                    "path": c.path,
                    "owner": l.owner_name,
                    "owner_id": l.owner_id,
                    "lock_path": l.path,
                    "kind": crate::locks::LOCK_KIND,
                }));
                needs_lock_paths.push(c.path.clone());
            }
            None => needs_lock_paths.push(c.path.clone()),
        }
    }

    if !needs_lock_paths.is_empty() {
        return Err(AppError::needs_lock(
            needs_lock_paths,
            serde_json::Value::Array(locked_by_others),
        ));
    }
    Ok(())
}

/// 变更清单里引用的全部 blob（file 且非 delete，去重）。
fn referenced_blobs(changes: &[Change]) -> Vec<String> {
    let mut hashes: Vec<String> = changes
        .iter()
        .filter(|c| c.kind == Kind::File && c.op != Op::Delete)
        .filter_map(|c| c.blob_hash.clone())
        .collect();
    hashes.sort();
    hashes.dedup();
    hashes
}

// ---------- POST /repos/{repo}/commit/prepare ----------

#[derive(Deserialize)]
pub struct PrepareReq {
    pub commit_id: String,
    pub base_rev: i64,
    #[serde(default)]
    pub message: Option<String>,
    pub changes: Vec<ChangeJson>,
}

/// prepare 的两种结果：命中幂等短路时重放旧 rev，否则签发新凭据。
enum PrepareOutcome {
    Replayed { rev: i64 },
    Prepared { token: String, need_blobs: Vec<String> },
}

pub async fn prepare(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<PrepareReq>,
) -> Result<impl IntoResponse, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo_row = resolve_repo(&state, &repo_name).await?;
    let commit_id = normalize_commit_id(&req.commit_id)?;
    let changes = to_changes(&req.changes)?;
    if changes.is_empty() {
        return Err(AppError::InvalidArgument("提交必须包含至少一条变更".into()));
    }
    let ip = client_ip(&headers);

    let outcome = tokio::task::block_in_place(|| -> Result<PrepareOutcome, AppError> {
        let mut conn = state.lock_db()?;

        // 幂等短路：同 commit_id 已成功 → 返旧 rev
        let done: Option<(i64, i64)> = conn
            .query_row(
                "SELECT repo_id, rev FROM commits WHERE commit_id = ?1",
                params![commit_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some((rrepo, rrev)) = done {
            if rrepo != repo_row.id {
                return Err(AppError::InvalidArgument(format!(
                    "commit_id {commit_id} 已属于其他仓库"
                )));
            }
            return Ok(PrepareOutcome::Replayed { rev: rrev });
        }

        // base_rev 严格相等（§5.3 步骤 1）
        let head_rev: i64 = conn.query_row(
            "SELECT head_rev FROM repos WHERE id = ?1",
            params![repo_row.id],
            |r| r.get(0),
        )?;
        if req.base_rev != head_rev {
            // 冲突路径：base_rev 之后变更过、且与本次清单相交的路径
            let mut conflicted = vec![];
            {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT path FROM changes WHERE repo_id = ?1 AND rev > ?2 AND rev <= ?3",
                )?;
                let mut rows = stmt.query(params![repo_row.id, req.base_rev, head_rev])?;
                while let Some(row) = rows.next()? {
                    let p: String = row.get(0)?;
                    if changes.iter().any(|c| c.path == p || p.starts_with(&format!("{}/", c.path)) || c.path.starts_with(&format!("{p}/"))) {
                        conflicted.push(p);
                    }
                }
            }
            return Err(AppError::out_of_date(head_rev, conflicted));
        }

        // 路径合法性（§5.3 步骤 5）
        for c in &changes {
            repo::validate_path(&c.path)?;
        }

        // 逐路径 ACL + 锁校验（§5.3 步骤 3/4）
        check_paths(&conn, &repo_row, &user, &changes)?;

        // 缺失 blob（§5.3 步骤 6）
        let mut need = vec![];
        for h in referenced_blobs(&changes) {
            let present: bool = conn
                .query_row("SELECT 1 FROM blobs WHERE hash = ?1", params![h], |_| Ok(true))
                .unwrap_or(false);
            if !present {
                need.push(h);
            }
        }

        let ch = changes_hash(&changes);
        let token_raw = {
            let mut b = [0u8; 32];
            getrandom::getrandom(&mut b)
                .map_err(|e| AppError::Internal(format!("CSPRNG: {e}")))?;
            hex::encode(b)
        };
        let now = Utc::now();
        let payload = json!({
            "message": req.message.clone().unwrap_or_default(),
            "changes": changes,
        });

        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        // 同 commit_id 重新 prepare：替换旧凭据（同 commit_id 幂等，§7.1）
        tx.execute("DELETE FROM pending_commits WHERE commit_id = ?1", params![commit_id])?;
        tx.execute(
            "INSERT INTO pending_commits
                (commit_token, commit_id, repo_id, user_id, base_rev, changes_hash,
                 need_blobs, payload_json, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                sha256_hex(&token_raw),
                commit_id,
                repo_row.id,
                user.id,
                req.base_rev,
                ch,
                serde_json::to_string(&need).unwrap(),
                serde_json::to_string(&payload).unwrap(),
                now.to_rfc3339(),
                (now + Duration::seconds(TOKEN_TTL_SECS)).to_rfc3339(),
            ],
        )?;
        crate::audit::log(
            &tx,
            Some(user.id),
            Some(repo_row.id),
            "commit.prepare",
            &format!("repo:{} commit:{commit_id}", repo_row.name),
            &format!("base_rev={} changes={} need_blobs={}", req.base_rev, changes.len(), need.len()),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;

        Ok(PrepareOutcome::Prepared { token: token_raw, need_blobs: need })
    })?;

    Ok(Json(match outcome {
        // 同 commit_id 已成功 → 重放旧 rev，不再发新凭据（§5.3 幂等）
        PrepareOutcome::Replayed { rev } => json!({
            "commit_id": commit_id,
            "replayed": true,
            "rev": rev,
        }),
        PrepareOutcome::Prepared { token, need_blobs } => json!({
            "commit_id": commit_id,
            "commit_token": token,
            "need_blobs": need_blobs,
            "expires_in": TOKEN_TTL_SECS,
        }),
    }))
}

// ---------- POST /repos/{repo}/commit ----------

#[derive(Deserialize)]
pub struct CommitReq {
    pub commit_id: String,
    pub commit_token: String,
    #[serde(default)]
    pub message: Option<String>,
}

pub async fn commit(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<CommitReq>,
) -> Result<impl IntoResponse, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo_row = resolve_repo(&state, &repo_name).await?;
    let commit_id = normalize_commit_id(&req.commit_id)?;
    let ip = client_ip(&headers);

    let rev = tokio::task::block_in_place(|| -> Result<(i64, bool), AppError> {
        let mut conn = state.lock_db()?;

        // 幂等检查：已成功过 → 返同一 rev（§5.3 阶段二步骤 2）
        let done: Option<(i64, i64)> = conn
            .query_row(
                "SELECT repo_id, rev FROM commits WHERE commit_id = ?1",
                params![commit_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some((rrepo, rrev)) = done {
            if rrepo != repo_row.id {
                return Err(AppError::InvalidArgument(format!(
                    "commit_id {commit_id} 已属于其他仓库"
                )));
            }
            return Ok((rrev, true));
        }

        // 凭据校验：五元组绑定（§5.3 阶段二步骤 1）
        let pending: Option<(i64, i64, i64, i64, String, String, String)> = {
            conn.query_row(
                "SELECT user_id, repo_id, base_rev, 0, changes_hash, need_blobs, payload_json
                   FROM pending_commits WHERE commit_token = ?1",
                params![sha256_hex(&req.commit_token)],
                |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)?,
                        r.get(4)?, r.get(5)?, r.get(6)?,
                    ))
                },
            )
            .ok()
        };
        let Some((p_user, p_repo, base_rev, _p_pad, ch, need_blobs, payload_json)) = pending else {
            return Err(AppError::commit_token_expired());
        };
        // 过期检查
        let expires_at: String = conn.query_row(
            "SELECT expires_at FROM pending_commits WHERE commit_token = ?1",
            params![sha256_hex(&req.commit_token)],
            |r| r.get(0),
        )?;
        if expires_at <= Utc::now().to_rfc3339() {
            conn.execute(
                "DELETE FROM pending_commits WHERE commit_token = ?1",
                params![sha256_hex(&req.commit_token)],
            )?;
            return Err(AppError::commit_token_expired());
        }
        if p_user != user.id || p_repo != repo_row.id {
            return Err(AppError::commit_token_expired());
        }

        // base_rev 仍须与当前 head 一致（§5.3 阶段二步骤 1）
        let head_rev: i64 = conn.query_row(
            "SELECT head_rev FROM repos WHERE id = ?1",
            params![repo_row.id],
            |r| r.get(0),
        )?;
        if base_rev != head_rev {
            conn.execute(
                "DELETE FROM pending_commits WHERE commit_token = ?1",
                params![sha256_hex(&req.commit_token)],
            )?;
            return Err(AppError::out_of_date(head_rev, vec![]));
        }

        // 反序列化服务端留存的变更清单（§7.3：commit 不重传清单）
        let payload: Value = serde_json::from_str(&payload_json)
            .map_err(|e| AppError::Internal(format!("pending payload 解析失败: {e}")))?;
        let raw_changes: Vec<ChangeJson> = serde_json::from_value(payload["changes"].clone())
            .map_err(|e| AppError::Internal(format!("pending changes 解析失败: {e}")))?;
        let changes = to_changes(&raw_changes)?;
        // 完整性：重算 changes_hash 须与 token 绑定值一致
        if changes_hash(&changes) != ch {
            return Err(AppError::Internal("changes_hash 与 commit_token 绑定不一致（数据异常）".into()));
        }
        // 防御性复查：prepare 到 commit 之间权限/锁可能变化（fail-closed）
        check_paths(&conn, &repo_row, &user, &changes)?;
        let message = req
            .message
            .clone()
            .filter(|m| !m.trim().is_empty())
            .or_else(|| payload["message"].as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        // blob 就位校验（§5.3 阶段二步骤 3）
        let need: Vec<String> = serde_json::from_str(&need_blobs)
            .map_err(|e| AppError::Internal(format!("need_blobs 解析失败: {e}")))?;
        let missing: Vec<String> = need
            .iter()
            .filter(|h| {
                !conn
                    .query_row("SELECT 1 FROM blobs WHERE hash = ?1", params![h], |_| Ok(true))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(AppError::InvalidArgument(format!(
                "以下 blob 尚未上传完成: {}",
                missing.join(", ")
            )));
        }

        // 单事务落库（§5.3 阶段二步骤 4）
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        let result = repo::apply_commit_tx(&tx, repo_row.id, user.id, &message, &changes, base_rev)?;

        tx.execute(
            "INSERT INTO commits (commit_id, repo_id, rev, user_id, base_rev, changes_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![commit_id, repo_row.id, result.rev, user.id, base_rev, ch, Utc::now().to_rfc3339()],
        )?;
        for h in referenced_blobs(&changes) {
            tx.execute("UPDATE blobs SET refcount = refcount + 1 WHERE hash = ?1", params![h])?;
            // GC 复活：提交重新引用 → 出队（§3.7 关键语义 4）
            tx.execute("DELETE FROM gc_queue WHERE blob_hash = ?1", params![h])?;
        }
        tx.execute("DELETE FROM pending_commits WHERE commit_id = ?1", params![commit_id])?;
        crate::audit::log(
            &tx,
            Some(user.id),
            Some(repo_row.id),
            "commit",
            &format!("repo:{} rev:{}", repo_row.name, result.rev),
            &format!(
                "commit_id={commit_id} base_rev={base_rev} changes={} byte_delta={}",
                changes.len(),
                result.byte_delta
            ),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;

        Ok((result.rev, false))
    })?;

    let (rev, replayed) = rev;

    // manifest_hash 后台异步补算（§3.3，不阻塞提交响应）
    let st = state.clone();
    let repo_id = repo_row.id;
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(conn) = st.lock_db() {
                let _ = repo::refresh_head_manifest_hash(&conn, repo_id);
            }
        })
        .await;
    });

    Ok(Json(json!({
        "commit_id": commit_id,
        "rev": rev,
        "replayed": replayed,
    })))
}
