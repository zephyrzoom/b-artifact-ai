//! 锁 HTTP 端点（§7.2 锁 / §5 锁定与并发编辑）。
//!
//! v0.4.17：只剩**文件锁**（`kind` 请求字段已删除，响应里保留 `"kind": "file"` 兼容老客户端）。
//!
//! - GET    /repos/{repo}/locks?path=&owner=&include_broken=   列表 / 查询某路径上的锁
//! - POST   /repos/{repo}/locks                                加锁
//! - POST   /repos/{repo}/locks/refresh                        心跳续期
//! - DELETE /repos/{repo}/locks/{*path}                        释放；`?break=true` 强制解锁（需 admin）

use crate::acl::Level;
use crate::error::AppError;
use crate::locks::{self, LOCK_KIND};
use crate::state::SharedState;
use crate::storage::acl;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::auth::require_user;
use super::{principal_of, resolve_repo};

fn ip_of(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn lock_json(l: &locks::LockRow) -> Value {
    json!({
        "id": l.id,
        "path": l.path,
        "kind": LOCK_KIND,
        "owner_id": l.owner_id,
        "owner": l.owner_name,
        "comment": l.comment,
        "created_at": l.created_at,
        "expires_at": l.expires_at,
        "broken_at": l.broken_at,
        "broken_by": l.broken_by,
        "broken_by_name": l.broken_by_name,
        "break_reason": l.break_reason,
    })
}

// ---------- GET /repos/{repo}/locks ----------

#[derive(Deserialize)]
pub struct ListQuery {
    /// 给定时只返回该**精确路径**上的锁（v0.4.17：不再有祖先目录锁）
    #[serde(default)]
    pub path: Option<String>,
    /// `me` = 本人（§5.5 跨副本管理）
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub include_broken: Option<bool>,
}

pub async fn list_locks(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;

    let items = tokio::task::block_in_place(|| -> Result<Vec<Value>, AppError> {
        let conn = state.lock_db()?;
        // 查看他人锁列表 = read（§4.3）
        let set = acl::load(&conn, repo.id)?;
        set.require("", &principal_of(&user), Level::Read)?;

        if let Some(path) = &q.path {
            if !path.is_empty() {
                crate::storage::repo::validate_path(path)?;
            }
            let covered = locks::covering_lock(&conn, repo.id, path)?;
            return Ok(covered.as_ref().map(lock_json).into_iter().collect());
        }

        let owner = match q.owner.as_deref() {
            Some("me") => Some(user.id),
            Some(raw) => Some(raw.parse::<i64>().map_err(|_| {
                AppError::InvalidArgument("owner 只支持 `me` 或用户 id".into())
            })?),
            None => None,
        };
        let rows = locks::list_locks(&conn, repo.id, owner, q.include_broken.unwrap_or(false))?;
        Ok(rows.iter().map(lock_json).collect())
    })?;

    Ok(Json(json!({ "items": items, "total": items.len() })))
}

// ---------- POST /repos/{repo}/locks ----------

/// 加锁请求。v0.4.17：`kind` 字段已删除 —— serde 默认忽略未知字段，
/// 老客户端仍传 `"kind": "dir"` 不会报错，但语义就是"对这个路径加文件锁"。
#[derive(Deserialize)]
pub struct AcquireReq {
    pub path: String,
    #[serde(default)]
    pub comment: Option<String>,
    /// 秒；缺省用仓库默认 TTL（7 天）。显式给 0 = 永不过期。
    #[serde(default)]
    pub expires_in: Option<i64>,
}

pub async fn acquire(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<AcquireReq>,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let ip = ip_of(&headers);

    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        // 加锁 = write（§4.3）
        let set = acl::load(&conn, repo.id)?;
        set.require(&req.path, &principal_of(&user), Level::Write)?;

        // §5.2：锁只保护文件路径。对**已存在的目录**加锁一律拒绝 ——
        // 目录不是变更路径、也不会参与提交校验，这种锁只会变成一份谁也用不上的脏数据。
        // 注意判据必须是"当前 HEAD 上它是目录"：**尚未提交的新文件不在树里**，
        // 那正是"先锁后建"的主路径，必须放行。
        if crate::storage::repo::dir_exists_at(&conn, repo.id, &req.path, repo.head_rev)? {
            return Err(AppError::InvalidArgument(format!(
                "`{}` 是目录：v0.4.17 起只支持文件锁，请对具体文件加锁",
                req.path
            )));
        }

        // 0 显式表示永不过期；未给则用默认 TTL
        let ttl = req.expires_in.map(|s| if s <= 0 { 0 } else { s });
        let ttl = match ttl {
            Some(0) => None,
            Some(s) => Some(s),
            None => Some(locks::DEFAULT_TTL_SECS),
        };

        match locks::acquire(
            &conn,
            repo.id,
            &req.path,
            user.id,
            req.comment.as_deref().unwrap_or(""),
            ttl,
        )? {
            Err(conflicts) => Err(AppError::locked(
                format!("路径 `{}` 已被他人持锁", req.path),
                locks::conflicts_json(&conflicts),
            )),
            Ok(l) => {
                crate::audit::log(
                    &conn,
                    Some(user.id),
                    Some(repo.id),
                    "lock.acquire",
                    &format!("repo:{} path:{}", repo.name, l.path),
                    &format!("kind={LOCK_KIND} ttl={:?}", ttl),
                    &ip,
                )?;
                Ok(json!({
                    "id": l.id,
                    "path": l.path,
                    "kind": LOCK_KIND,
                    "token": l.token_hash,
                    "expires_at": l.expires_at,
                }))
            }
        }
    })?;

    Ok(Json(out))
}

// ---------- POST /repos/{repo}/locks/refresh ----------

#[derive(Deserialize)]
pub struct RefreshReq {
    /// 秒；0 或省略时按仓库默认 TTL 顺延。
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

pub async fn refresh(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
    body: Option<Json<RefreshReq>>,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let ttl = body
        .and_then(|Json(b)| b.ttl_secs)
        .map(|s| if s <= 0 { locks::DEFAULT_TTL_SECS } else { s })
        .unwrap_or(locks::DEFAULT_TTL_SECS);

    let (n, expires_at) = tokio::task::block_in_place(|| -> Result<(usize, Option<String>), AppError> {
        let conn = state.lock_db()?;
        locks::refresh(&conn, repo.id, user.id, Some(ttl))
    })?;

    Ok(Json(json!({ "refreshed": n, "expires_at": expires_at })))
}

// ---------- DELETE /repos/{repo}/locks/{*path} ----------

#[derive(Deserialize)]
pub struct DeleteQuery {
    /// 文档（§5.5 / §7.2）约定的查询参数是 `?break=true`，字段名需显式改名。
    #[serde(default, rename = "break")]
    pub break_lock: Option<bool>,
    /// 强制解锁原因（必填，§5.5）
    #[serde(default)]
    pub reason: Option<String>,
    /// 释放时校验所有权凭据
    #[serde(default)]
    pub token: Option<String>,
}

pub async fn release(
    State(state): State<SharedState>,
    Path((repo_name, raw_path)): Path<(String, String)>,
    Query(q): Query<DeleteQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let path = raw_path.trim_start_matches('/');
    crate::storage::repo::validate_path(path)?;
    let ip = ip_of(&headers);

    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        let set = acl::load(&conn, repo.id)?;
        let principal = principal_of(&user);

        if q.break_lock.unwrap_or(false) {
            // 强制解锁 = 本目录 admin（§4.3）
            set.require(path, &principal, Level::Admin)?;
            let reason = q.reason.clone().unwrap_or_default();
            let n = locks::break_lock(&conn, repo.id, path, user.id, &reason)?;
            crate::audit::log(
                &conn,
                Some(user.id),
                Some(repo.id),
                "lock.break",
                &format!("repo:{} path:{path}", repo.name),
                &format!("affected={n} reason={reason}"),
                &ip,
            )?;
            return Ok(json!({ "broken": n, "path": path, "reason": reason }));
        }

        // 释放自己的锁：允许持锁人操作，即便其当前权限已变（避免锁死无出口）
        let is_owner = locks::get_lock(&conn, repo.id, path)?
            .map(|l| l.owner_id == user.id && l.broken_at.is_none())
            .unwrap_or(false);
        if !is_owner {
            set.require(path, &principal, Level::Write)?;
        }
        locks::release(&conn, repo.id, path, user.id, q.token.as_deref())?;
        crate::audit::log(
            &conn,
            Some(user.id),
            Some(repo.id),
            "lock.release",
            &format!("repo:{} path:{path}", repo.name),
            "",
            &ip,
        )?;
        Ok(json!({ "released": true, "path": path }))
    })?;

    Ok(Json(out))
}
