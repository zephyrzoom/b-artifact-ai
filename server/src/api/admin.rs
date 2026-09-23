//! 管理端点（`/api/v1/admin/*`）——M3 管理端的数据源（§7.2 管理 / §8 管理页面）。
//!
//! 权限分三档，与 §4.3 一致：
//!
//! * **系统管理员**：用户、组、审计、统计、维护、系统设置、删仓
//! * **目录 admin**：本仓库的权限规则、仓库设置、purge
//! * **系统管理员 + 目录 admin**：purge（§3.6：两者都要，且需 `confirm_name` 二次确认）
//!
//! - 用户与组     GET/POST   /admin/users                 增删改、禁用、重置密码
//!               PUT/DELETE /admin/users/{id}
//!               POST       /admin/users/{id}/password
//!               GET/POST   /admin/groups
//!               PUT/DELETE /admin/groups/{id}
//!               GET/PUT    /admin/groups/{id}/members
//! - 审计         GET        /admin/audit?...&format=csv
//! - 概览         GET        /admin/stats
//! - 仓库         PUT        /admin/repos/{repo}/settings
//!               DELETE     /admin/repos/{repo}
//!               POST       /admin/repos/{repo}/purge
//! - 权限规则     GET/POST/PUT/DELETE /admin/repos/{repo}/acl
//!               GET        /admin/repos/{repo}/acl/preview   有效权限预览器（§8.3）
//!               GET        /admin/repos/{repo}/acl/who       目录级"谁有权限"反查
//! - 系统设置     GET        /admin/settings
//! - 维护         GET        /admin/maintenance
//!               POST       /admin/maintenance/rebuild-refcount
//!               POST       /admin/maintenance/gc

use crate::acl::{Level, Subject};
use crate::auth::password::hash_password;
use crate::auth::session::SessionUser;
use crate::error::AppError;
use crate::state::SharedState;
use crate::storage::{acl, gc, purge, repo};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use rusqlite::params;
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

/// 系统管理员门禁（§7.2：用户/组/审计/统计/维护均为系统级操作）。
async fn require_sysadmin(
    state: &SharedState,
    headers: &HeaderMap,
) -> Result<SessionUser, AppError> {
    let user = require_user(state, headers).await?;
    if !user.is_admin {
        return Err(AppError::PermissionDenied("需要系统管理员权限".into()));
    }
    Ok(user)
}

/// 仓库根 admin 门禁（目录 admin 或系统管理员）。
fn require_repo_admin(
    conn: &rusqlite::Connection,
    repo_id: i64,
    user: &SessionUser,
) -> Result<acl::AclSet, AppError> {
    let set = acl::load(conn, repo_id)?;
    if user.is_admin {
        return Ok(set);
    }
    set.require("", &principal_of(user), Level::Admin)?;
    Ok(set)
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

// ============================================================
// 概览统计（§8.2 Dashboard）
// ============================================================

pub async fn stats(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let _user = require_sysadmin(&state, &headers).await?;
    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        let now = now_rfc3339();

        let count = |sql: &str| -> Result<i64, AppError> {
            Ok(conn.query_row(sql, [], |r| r.get(0))?)
        };
        let repos = count("SELECT COUNT(*) FROM repos")?;
        let users = count("SELECT COUNT(*) FROM users")?;
        let users_active = count("SELECT COUNT(*) FROM users WHERE disabled = 0")?;
        let sessions: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE expires_at > ?1",
            params![now],
            |r| r.get(0),
        )?;
        let files = count("SELECT COUNT(*) FROM head_entries WHERE kind = 'file'")?;
        let revisions = count("SELECT COUNT(*) FROM revisions")?;
        let locks: i64 = conn.query_row(
            "SELECT COUNT(*) FROM locks
              WHERE broken_at IS NULL AND (expires_at IS NULL OR expires_at > ?1)",
            params![now],
            |r| r.get(0),
        )?;

        // 存储：逻辑字节（HEAD 全部文件）/ 唯一内容字节 / 落盘字节
        let logical_bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM head_entries WHERE kind = 'file'",
            [],
            |r| r.get(0),
        )?;
        let (blob_count, unique_bytes, stored_bytes): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0), COALESCE(SUM(stored_size), 0) FROM blobs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let dedup_ratio = if logical_bytes > 0 {
            1.0 - (unique_bytes as f64 / logical_bytes as f64)
        } else {
            0.0
        };
        let compress_ratio = if unique_bytes > 0 {
            1.0 - (stored_bytes as f64 / unique_bytes as f64)
        } else {
            0.0
        };

        // 提交趋势：最近 30 天按天
        let since = (Utc::now() - chrono::Duration::days(29)).to_rfc3339();
        let mut trend = vec![];
        {
            let mut stmt = conn.prepare(
                "SELECT substr(created_at, 1, 10) AS d, COUNT(*), COALESCE(SUM(file_count), 0)
                   FROM revisions WHERE created_at >= ?1
                  GROUP BY d ORDER BY d",
            )?;
            for row in stmt.query_map(params![since], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })? {
                let (d, n, fc) = row?;
                trend.push(json!({ "date": d, "commits": n, "files": fc }));
            }
        }

        // 最近活动（审计）
        let mut recent = vec![];
        {
            let mut stmt = conn.prepare(
                "SELECT a.ts, a.action, a.target, a.detail, u.username
                   FROM audit_log a LEFT JOIN users u ON u.id = a.user_id
                  ORDER BY a.id DESC LIMIT 12",
            )?;
            for row in stmt.query_map([], |r| {
                Ok(json!({
                    "ts": r.get::<_, String>(0)?,
                    "action": r.get::<_, String>(1)?,
                    "target": r.get::<_, String>(2)?,
                    "detail": r.get::<_, String>(3)?,
                    "username": r.get::<_, Option<String>>(4)?,
                }))
            })? {
                recent.push(row?);
            }
        }

        // 仓库规模 Top（v0.4.17：不再下发已废弃的 lock_policy）
        let mut top_repos = vec![];
        {
            let mut stmt = conn.prepare(
                "SELECT r.name, r.head_rev,
                        COUNT(h.path) AS files, COALESCE(SUM(h.size), 0) AS bytes
                   FROM repos r
                   LEFT JOIN head_entries h ON h.repo_id = r.id AND h.kind = 'file'
                  GROUP BY r.id ORDER BY bytes DESC LIMIT 10",
            )?;
            for row in stmt.query_map([], |r| {
                Ok(json!({
                    "name": r.get::<_, String>(0)?,
                    "head_rev": r.get::<_, i64>(1)?,
                    "files": r.get::<_, i64>(2)?,
                    "bytes": r.get::<_, i64>(3)?,
                }))
            })? {
                top_repos.push(row?);
            }
        }

        let gc_status = gc::status(&conn)?;

        Ok(json!({
            "repos": repos,
            "users": users,
            "users_active": users_active,
            "sessions": sessions,
            "files": files,
            "revisions": revisions,
            "locks": locks,
            "storage": {
                "logical_bytes": logical_bytes,
                "unique_bytes": unique_bytes,
                "stored_bytes": stored_bytes,
                "blob_count": blob_count,
                "dedup_ratio": dedup_ratio,
                "compress_ratio": compress_ratio,
            },
            "gc": {
                "queued": gc_status.queued,
                "due_now": gc_status.due_now,
                "queued_bytes": gc_status.queued_bytes,
                "orphan_blobs": gc_status.orphan_blobs,
            },
            "commit_trend": trend,
            "recent_activity": recent,
            "top_repos": top_repos,
        }))
    })?;
    Ok(Json(out))
}

// ============================================================
// 审计日志（§7.2 GET /admin/audit）
// ============================================================

#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    /// 用户名或用户 id
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    /// `csv` → 导出 CSV（忽略分页上限）
    #[serde(default)]
    pub format: Option<String>,
}

pub async fn audit(
    State(state): State<SharedState>,
    Query(q): Query<AuditQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let _user = require_sysadmin(&state, &headers).await?;
    let is_csv = q.format.as_deref() == Some("csv");
    let limit = q.limit.unwrap_or(100).clamp(1, if is_csv { 100_000 } else { 1000 });
    let offset = q.offset.unwrap_or(0).max(0);

    let rows = tokio::task::block_in_place(|| -> Result<Vec<Value>, AppError> {
        let conn = state.lock_db()?;
        let mut sql = String::from(
            "SELECT a.id, a.ts, a.user_id, u.username, a.repo_id, r.name, a.action, a.target, a.detail, a.ip
               FROM audit_log a
               LEFT JOIN users u ON u.id = a.user_id
               LEFT JOIN repos r ON r.id = a.repo_id
              WHERE 1 = 1",
        );
        let mut binds: Vec<rusqlite::types::Value> = vec![];
        if let Some(f) = &q.from {
            sql.push_str(" AND a.ts >= ?");
            binds.push(f.clone().into());
        }
        if let Some(t) = &q.to {
            sql.push_str(" AND a.ts <= ?");
            binds.push(t.clone().into());
        }
        match q.user.as_deref() {
            Some(u) if !u.is_empty() => match u.parse::<i64>() {
                Ok(id) => {
                    sql.push_str(" AND a.user_id = ?");
                    binds.push(id.into());
                }
                Err(_) => {
                    sql.push_str(" AND u.username = ?");
                    binds.push(u.to_string().into());
                }
            },
            _ => {}
        }
        if let Some(a) = &q.action {
            if !a.is_empty() {
                sql.push_str(" AND a.action = ?");
                binds.push(a.clone().into());
            }
        }
        if let Some(rp) = &q.repo {
            if !rp.is_empty() {
                sql.push_str(" AND r.name = ?");
                binds.push(rp.clone().into());
            }
        }
        sql.push_str(" ORDER BY a.id DESC LIMIT ? OFFSET ?");
        binds.push(limit.into());
        binds.push(offset.into());

        let mut stmt = conn.prepare(&sql)?;
        let mut out = vec![];
        for row in stmt.query_map(rusqlite::params_from_iter(binds.iter()), |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "ts": r.get::<_, String>(1)?,
                "user_id": r.get::<_, Option<i64>>(2)?,
                "username": r.get::<_, Option<String>>(3)?,
                "repo_id": r.get::<_, Option<i64>>(4)?,
                "repo": r.get::<_, Option<String>>(5)?,
                "action": r.get::<_, String>(6)?,
                "target": r.get::<_, String>(7)?,
                "detail": r.get::<_, String>(8)?,
                "ip": r.get::<_, String>(9)?,
            }))
        })? {
            out.push(row?);
        }
        Ok(out)
    })?;

    if is_csv {
        let mut body = String::from('\u{feff}'.to_string() + "id,时间,用户,仓库,动作,对象,详情,IP\n");
        for r in &rows {
            body.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                r["id"],
                r["ts"].as_str().unwrap_or(""),
                r["username"].as_str().unwrap_or(""),
                r["repo"].as_str().unwrap_or(""),
                r["action"].as_str().unwrap_or(""),
                csv_cell(r["target"].as_str().unwrap_or("")),
                csv_cell(r["detail"].as_str().unwrap_or("")),
                r["ip"].as_str().unwrap_or(""),
            ));
        }
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
            .header(
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"b-artifact-audit.csv\"",
            )
            .body(axum::body::Body::from(body))
            .map_err(|e| AppError::Internal(format!("{e}")))?);
    }

    Ok(Json(json!({ "items": rows, "total": rows.len() })).into_response())
}

fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ============================================================
// 用户（§7.2 / §9.1）
// ============================================================

pub async fn list_users(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let _user = require_sysadmin(&state, &headers).await?;
    let items = tokio::task::block_in_place(|| -> Result<Vec<Value>, AppError> {
        let conn = state.lock_db()?;
        let mut stmt = conn.prepare(
            "SELECT u.id, u.username, u.display_name, u.source, u.ldap_dn,
                    u.is_admin, u.disabled, u.created_at, u.last_login_at
               FROM users u ORDER BY u.username",
        )?;
        let mut out = vec![];
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)? != 0,
                r.get::<_, i64>(6)? != 0,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })? {
            let (id, username, display_name, source, ldap_dn, is_admin, disabled, created_at, last_login_at) = row?;
            let groups = user_groups(&conn, id)?;
            out.push(json!({
                "id": id,
                "username": username,
                "display_name": display_name,
                "source": source,
                "ldap_dn": ldap_dn,
                "is_admin": is_admin,
                "disabled": disabled,
                "created_at": created_at,
                "last_login_at": last_login_at,
                "groups": groups,
            }));
        }
        Ok(out)
    })?;
    Ok(Json(json!({ "items": items, "total": items.len() })))
}

fn user_groups(conn: &rusqlite::Connection, user_id: i64) -> Result<Vec<Value>, AppError> {
    let mut stmt = conn.prepare(
        "SELECT g.id, g.name FROM group_members m JOIN groups g ON g.id = m.group_id
          WHERE m.user_id = ?1 ORDER BY g.name",
    )?;
    let mut out = vec![];
    for row in stmt.query_map(params![user_id], |r| {
        Ok(json!({ "id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)? }))
    })? {
        out.push(row?);
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct CreateUserReq {
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub is_admin: bool,
}

pub async fn create_user(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<CreateUserReq>,
) -> Result<impl IntoResponse, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    let ip = ip_of(&headers);
    let name = req.username.trim().to_string();
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::InvalidArgument("用户名长度需在 1~64 之间".into()));
    }
    let pw = req.password.clone().unwrap_or_default();
    // §9.2 口令强度（v0.4.17）：字符数下限 + 四类字符，唯一入口在 auth::password
    crate::auth::password::validate_policy(&pw)?;
    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let mut conn = state.lock_db()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        let exists: Option<i64> = tx
            .query_row("SELECT id FROM users WHERE username = ?1", params![name], |r| r.get(0))
            .ok();
        if exists.is_some() {
            return Err(AppError::Conflict {
                code: "NAME_COLLISION",
                message: format!("用户 `{name}` 已存在"),
                details: json!({ "username": name }),
            });
        }
        let phc = hash_password(&pw)?;
        tx.execute(
            "INSERT INTO users (username, display_name, password_hash, source, is_admin, disabled, created_at)
             VALUES (?1, ?2, ?3, 'local', ?4, 0, ?5)",
            params![
                name,
                req.display_name.clone().unwrap_or_default(),
                phc,
                req.is_admin as i64,
                now_rfc3339(),
            ],
        )?;
        let id = tx.last_insert_rowid();
        crate::audit::log(
            &tx,
            Some(user.id),
            None,
            "user.create",
            &format!("user:{id}"),
            &format!("username={name} is_admin={}", req.is_admin),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(json!({ "id": id, "username": name }))
    })?;
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Deserialize)]
pub struct UpdateUserReq {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub is_admin: Option<bool>,
    #[serde(default)]
    pub disabled: Option<bool>,
}

pub async fn update_user(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(req): Json<UpdateUserReq>,
) -> Result<Json<Value>, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    let ip = ip_of(&headers);
    if id == user.id && req.disabled == Some(true) {
        return Err(AppError::InvalidArgument("不能禁用当前登录的账号".into()));
    }
    if id == user.id && req.is_admin == Some(false) {
        return Err(AppError::InvalidArgument("不能撤销自己的系统管理员权限".into()));
    }
    tokio::task::block_in_place(|| -> Result<(), AppError> {
        let mut conn = state.lock_db()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        let exists: Option<i64> = tx
            .query_row("SELECT id FROM users WHERE id = ?1", params![id], |r| r.get(0))
            .ok();
        if exists.is_none() {
            return Err(AppError::NotFound(format!("用户 #{id} 不存在")));
        }
        if let Some(d) = &req.display_name {
            tx.execute("UPDATE users SET display_name = ?2 WHERE id = ?1", params![id, d])?;
        }
        if let Some(a) = req.is_admin {
            tx.execute("UPDATE users SET is_admin = ?2 WHERE id = ?1", params![id, a as i64])?;
        }
        if let Some(d) = req.disabled {
            tx.execute("UPDATE users SET disabled = ?2 WHERE id = ?1", params![id, d as i64])?;
            if d {
                // 禁用即吊销全部会话（§9.2）
                tx.execute("DELETE FROM sessions WHERE user_id = ?1", params![id])?;
            }
        }
        crate::audit::log(
            &tx,
            Some(user.id),
            None,
            "user.update",
            &format!("user:{id}"),
            &format!(
                "display_name={:?} is_admin={:?} disabled={:?}",
                req.display_name, req.is_admin, req.disabled
            ),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(())
    })?;
    Ok(Json(json!({ "id": id, "updated": true })))
}

pub async fn delete_user(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    if id == user.id {
        return Err(AppError::InvalidArgument("不能删除当前登录的账号".into()));
    }
    let ip = ip_of(&headers);
    tokio::task::block_in_place(|| -> Result<(), AppError> {
        let mut conn = state.lock_db()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        // 仓库 owner 有外键约束：先把仓库转给操作者，避免删不动也避免留下悬挂
        tx.execute("UPDATE repos SET owner_id = ?2 WHERE owner_id = ?1", params![id, user.id])?;
        let n = tx.execute("DELETE FROM users WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(AppError::NotFound(format!("用户 #{id} 不存在")));
        }
        crate::audit::log(
            &tx,
            Some(user.id),
            None,
            "user.delete",
            &format!("user:{id}"),
            "仓库归属已转移给操作者",
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(())
    })?;
    Ok(Json(json!({ "deleted": true, "id": id })))
}

#[derive(Deserialize)]
pub struct ResetPasswordReq {
    pub new_password: String,
}

/// 管理员重置他人密码（仅本地账号；LDAP 账号密码在目录服务侧）。
pub async fn reset_password(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(req): Json<ResetPasswordReq>,
) -> Result<StatusCode, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    // §9.2 口令强度（v0.4.17）
    crate::auth::password::validate_policy(&req.new_password)?;
    let ip = ip_of(&headers);
    tokio::task::block_in_place(|| -> Result<(), AppError> {
        let mut conn = state.lock_db()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        let source: Option<String> = tx
            .query_row("SELECT source FROM users WHERE id = ?1", params![id], |r| r.get(0))
            .ok();
        let source = source.ok_or_else(|| AppError::NotFound(format!("用户 #{id} 不存在")))?;
        if source != "local" {
            return Err(AppError::PermissionDenied(
                "LDAP 账号请通过目录服务修改密码，本系统不托管其口令".into(),
            ));
        }
        let phc = hash_password(&req.new_password)?;
        tx.execute("UPDATE users SET password_hash = ?2 WHERE id = ?1", params![id, phc])?;
        let revoked = tx.execute("DELETE FROM sessions WHERE user_id = ?1", params![id])?;
        crate::audit::log(
            &tx,
            Some(user.id),
            None,
            "user.password_reset",
            &format!("user:{id}"),
            &format!("由管理员重置，吊销会话 {revoked} 个"),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(())
    })?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// 用户组
// ============================================================

pub async fn list_groups(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let _user = require_sysadmin(&state, &headers).await?;
    let items = tokio::task::block_in_place(|| -> Result<Vec<Value>, AppError> {
        let conn = state.lock_db()?;
        let mut stmt = conn.prepare(
            "SELECT g.id, g.name, g.comment, g.created_at, COUNT(m.user_id) AS members
               FROM groups g LEFT JOIN group_members m ON m.group_id = g.id
              GROUP BY g.id ORDER BY g.name",
        )?;
        let mut out = vec![];
        for row in stmt.query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "comment": r.get::<_, String>(2)?,
                "created_at": r.get::<_, String>(3)?,
                "members": r.get::<_, i64>(4)?,
            }))
        })? {
            out.push(row?);
        }
        Ok(out)
    })?;
    Ok(Json(json!({ "items": items, "total": items.len() })))
}

#[derive(Deserialize)]
pub struct GroupReq {
    pub name: String,
    #[serde(default)]
    pub comment: Option<String>,
}

pub async fn create_group(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<GroupReq>,
) -> Result<impl IntoResponse, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    let ip = ip_of(&headers);
    let name = req.name.trim().to_string();
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::InvalidArgument("组名长度需在 1~64 之间".into()));
    }
    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        let exists: Option<i64> = conn
            .query_row("SELECT id FROM groups WHERE name = ?1", params![name], |r| r.get(0))
            .ok();
        if exists.is_some() {
            return Err(AppError::Conflict {
                code: "NAME_COLLISION",
                message: format!("组 `{name}` 已存在"),
                details: json!({ "name": name }),
            });
        }
        conn.execute(
            "INSERT INTO groups (name, comment, created_at) VALUES (?1, ?2, ?3)",
            params![name, req.comment.clone().unwrap_or_default(), now_rfc3339()],
        )?;
        let id = conn.last_insert_rowid();
        crate::audit::log(
            &conn,
            Some(user.id),
            None,
            "group.create",
            &format!("group:{id}"),
            &format!("name={name}"),
            &ip,
        )?;
        Ok(json!({ "id": id, "name": name }))
    })?;
    Ok((StatusCode::CREATED, Json(out)))
}

pub async fn update_group(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(req): Json<GroupReq>,
) -> Result<Json<Value>, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    let ip = ip_of(&headers);
    tokio::task::block_in_place(|| -> Result<(), AppError> {
        let conn = state.lock_db()?;
        let n = conn.execute(
            "UPDATE groups SET name = ?2, comment = ?3 WHERE id = ?1",
            params![id, req.name.trim(), req.comment.clone().unwrap_or_default()],
        )?;
        if n == 0 {
            return Err(AppError::NotFound(format!("组 #{id} 不存在")));
        }
        crate::audit::log(
            &conn,
            Some(user.id),
            None,
            "group.update",
            &format!("group:{id}"),
            &format!("name={}", req.name),
            &ip,
        )?;
        Ok(())
    })?;
    Ok(Json(json!({ "id": id, "updated": true })))
}

pub async fn delete_group(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    let ip = ip_of(&headers);
    tokio::task::block_in_place(|| -> Result<(), AppError> {
        let conn = state.lock_db()?;
        // 组被 ACL 规则引用时拒绝删除（否则规则会指向不存在的主体，权限静默失效）
        let used: i64 = conn.query_row(
            "SELECT COUNT(*) FROM acl_rules WHERE subject_type = 'group' AND subject_id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        if used > 0 {
            return Err(AppError::Conflict {
                code: "GROUP_IN_USE",
                message: format!("该组仍被 {used} 条权限规则引用，请先移除这些规则"),
                details: json!({ "acl_rules": used }),
            });
        }
        let n = conn.execute("DELETE FROM groups WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(AppError::NotFound(format!("组 #{id} 不存在")));
        }
        crate::audit::log(
            &conn,
            Some(user.id),
            None,
            "group.delete",
            &format!("group:{id}"),
            "",
            &ip,
        )?;
        Ok(())
    })?;
    Ok(Json(json!({ "deleted": true, "id": id })))
}

pub async fn list_members(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let _user = require_sysadmin(&state, &headers).await?;
    let items = tokio::task::block_in_place(|| -> Result<Vec<Value>, AppError> {
        let conn = state.lock_db()?;
        let mut stmt = conn.prepare(
            "SELECT u.id, u.username, u.display_name, u.source, u.disabled
               FROM group_members m JOIN users u ON u.id = m.user_id
              WHERE m.group_id = ?1 ORDER BY u.username",
        )?;
        let mut out = vec![];
        for row in stmt.query_map(params![id], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "username": r.get::<_, String>(1)?,
                "display_name": r.get::<_, String>(2)?,
                "source": r.get::<_, String>(3)?,
                "disabled": r.get::<_, i64>(4)? != 0,
            }))
        })? {
            out.push(row?);
        }
        Ok(out)
    })?;
    Ok(Json(json!({ "items": items, "total": items.len() })))
}

#[derive(Deserialize)]
pub struct MembersReq {
    pub user_ids: Vec<i64>,
}

pub async fn set_members(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(req): Json<MembersReq>,
) -> Result<Json<Value>, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    let ip = ip_of(&headers);
    tokio::task::block_in_place(|| -> Result<(), AppError> {
        let mut conn = state.lock_db()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        let exists: Option<i64> = tx
            .query_row("SELECT id FROM groups WHERE id = ?1", params![id], |r| r.get(0))
            .ok();
        if exists.is_none() {
            return Err(AppError::NotFound(format!("组 #{id} 不存在")));
        }
        tx.execute("DELETE FROM group_members WHERE group_id = ?1", params![id])?;
        for uid in &req.user_ids {
            tx.execute(
                "INSERT OR IGNORE INTO group_members (group_id, user_id) VALUES (?1, ?2)",
                params![id, uid],
            )?;
        }
        crate::audit::log(
            &tx,
            Some(user.id),
            None,
            "group.members",
            &format!("group:{id}"),
            &format!("成员数 {}", req.user_ids.len()),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(())
    })?;
    Ok(Json(json!({ "id": id, "members": req.user_ids.len() })))
}

// ============================================================
// 仓库设置 / 删仓 / purge
// ============================================================

/// 仓库设置。v0.4.17：只剩 `description` —— `lock_policy` / `needs_lock` 已废弃
/// （唯一策略是"先锁后提交"，§5.1/§5.2；serde 忽略未知字段，老客户端传了也不会报错）。
#[derive(Deserialize)]
pub struct RepoSettingsReq {
    #[serde(default)]
    pub description: Option<String>,
}

pub async fn update_repo_settings(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<RepoSettingsReq>,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let ip = ip_of(&headers);
    tokio::task::block_in_place(|| -> Result<(), AppError> {
        let mut conn = state.lock_db()?;
        require_repo_admin(&conn, repo.id, &user)?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        if let Some(d) = &req.description {
            tx.execute("UPDATE repos SET description = ?2 WHERE id = ?1", params![repo.id, d])?;
        }
        crate::audit::log(
            &tx,
            Some(user.id),
            Some(repo.id),
            "repo.settings",
            &format!("repo:{}", repo.name),
            &format!("description={:?}", req.description),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(())
    })?;
    Ok(Json(json!({ "name": repo.name, "updated": true })))
}

pub async fn delete_repo(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let ip = ip_of(&headers);
    let summary = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let mut conn = state.lock_db()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        // 删仓先 purge 全部路径历史，blob 才能正确进入 GC 队列（否则永久泄漏）
        let affected: i64 = tx.query_row(
            "SELECT COUNT(*) FROM changes WHERE repo_id = ?1",
            params![repo.id],
            |r| r.get(0),
        )?;
        let hashes: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT blob_hash FROM changes
                  WHERE repo_id = ?1 AND blob_hash IS NOT NULL",
            )?;
            let mut out = vec![];
            for row in stmt.query_map(params![repo.id], |r| r.get::<_, String>(0))? {
                out.push(row?);
            }
            out
        };
        tx.execute("DELETE FROM head_entries WHERE repo_id = ?1", params![repo.id])?;
        tx.execute("DELETE FROM changes WHERE repo_id = ?1", params![repo.id])?;
        tx.execute("DELETE FROM revisions WHERE repo_id = ?1", params![repo.id])?;
        tx.execute("DELETE FROM locks WHERE repo_id = ?1", params![repo.id])?;
        tx.execute("DELETE FROM acl_rules WHERE repo_id = ?1", params![repo.id])?;
        tx.execute("DELETE FROM commits WHERE repo_id = ?1", params![repo.id])?;
        tx.execute("DELETE FROM pending_commits WHERE repo_id = ?1", params![repo.id])?;
        tx.execute("DELETE FROM repos WHERE id = ?1", params![repo.id])?;
        // 引用重建：该仓独有的 blob 归零 → 进 GC 队列
        gc::rebuild_refcount_for(&tx, &hashes)?;
        gc::enqueue_zero_ref(&tx, "repo-delete", gc::GC_GRACE_SECS)?;
        crate::audit::log(
            &tx,
            Some(user.id),
            None,
            "repo.delete",
            &format!("repo:{}", repo.name),
            &format!("变更记录 {affected} 条，涉及 blob {} 个", hashes.len()),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(json!({ "changes_removed": affected, "blobs_affected": hashes.len() }))
    })?;
    Ok(Json(json!({ "deleted": true, "name": repo.name, "summary": summary })))
}

#[derive(Deserialize)]
pub struct PurgeReq {
    /// 要清除历史的目录（非空）
    pub prefix: String,
    /// 原因（必填，进审计）
    pub reason: String,
    /// 防误触：必须等于仓库名
    pub confirm_name: String,
}

/// 清除历史（L2，不可逆）——§3.6。
pub async fn purge(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<PurgeReq>,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let ip = ip_of(&headers);
    if req.reason.trim().len() < 4 {
        return Err(AppError::InvalidArgument("清除历史必须填写原因（≥4 字，进审计）".into()));
    }
    if req.confirm_name != repo.name {
        return Err(AppError::InvalidArgument(format!(
            "confirm_name 必须等于仓库名 `{}`",
            repo.name
        )));
    }
    if !user.is_admin {
        return Err(AppError::PermissionDenied(
            "清除历史需要系统管理员权限（§3.6：目录 admin + 系统管理员两者都要）".into(),
        ));
    }

    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let mut conn = state.lock_db()?;
        // 目录 admin（系统管理员也要满足——§3.6 前置校验第 1 条两者并列）
        let set = acl::load(&conn, repo.id)?;
        set.require(&req.prefix, &principal_of(&user), Level::Admin)?;

        // 与提交共用 head_rev 乐观锁通道做写互斥（§3.6 实现要点）
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        let head_rev: i64 = tx.query_row(
            "SELECT head_rev FROM repos WHERE id = ?1",
            params![repo.id],
            |r| r.get(0),
        )?;
        let s = purge::purge_prefix_tx(&tx, repo.id, &req.prefix, user.id)?;
        crate::audit::log(
            &tx,
            Some(user.id),
            Some(repo.id),
            "repo.purge",
            &format!("repo:{} prefix:{}", repo.name, req.prefix),
            &format!(
                "reason={} paths={} revisions={} rev_range={:?} blobs={} bytes={} locks_released={} head_rev={}",
                req.reason.trim(),
                s.paths_removed,
                s.revisions_affected,
                s.rev_range,
                s.blobs_reclaimed,
                s.bytes_reclaimed,
                s.locks_released,
                head_rev
            ),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(json!({
            "prefix": s.prefix,
            "paths_removed": s.paths_removed,
            "revisions_affected": s.revisions_affected,
            "rev_range": s.rev_range.map(|(a, b)| json!({ "from": a, "to": b })),
            "blobs_reclaimed": s.blobs_reclaimed,
            "bytes_reclaimed": s.bytes_reclaimed,
            "locks_released": s.locks_released,
            "gc_scheduled_at": s.gc_scheduled_at,
        }))
    })?;
    Ok(Json(out))
}

// ============================================================
// ACL：规则列表 / 预览器 / 反查（§8.3）
// ============================================================

pub async fn list_acl(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;

    let items = tokio::task::block_in_place(|| -> Result<Vec<Value>, AppError> {
        let conn = state.lock_db()?;
        let set = acl::load(&conn, repo.id)?;
        let p = principal_of(&user);
        // 系统管理员 / 仓库根 admin 看全部；否则只看自己有 admin 的前缀（§4.3）
        let sees_all = user.is_admin || set.level("", &p).at_least(Level::Admin);
        let mut out = vec![];
        for r in set.rules() {
            if !sees_all && !set.level(&r.path_prefix, &p).at_least(Level::Admin) {
                continue;
            }
            let mut v = acl::rule_json(r);
            // 规则冲突/冗余检测（§8.3）：该规则在自己所在层是否会被更具体的规则压过
            v["shadowed"] = json!(!rule_is_effective(&set, r));
            v["subject_label"] = json!(subject_name(&conn, r.subject));
            out.push(v);
        }
        Ok(out)
    })?;

    Ok(Json(json!({ "items": items, "total": items.len() })))
}

/// 一条规则是否"永不生效"：用它自己的主体作为探针，看它是不是本层的 `pick` 赢家。
///
/// 这是 §8.3「规则冲突/冗余检测」的精确判据——同层里被更具体主体（或同具体度但
/// 级别更低的 `none`）压过的规则，永远不会被 `resolve_layer` 返回。
fn rule_is_effective(set: &acl::AclSet, rule: &crate::acl::AclRule) -> bool {
    let probe = match rule.subject {
        Subject::Everyone => crate::acl::Principal::new(0, vec![], false),
        Subject::User(u) => crate::acl::Principal::new(u, vec![], false),
        Subject::Group(g) => crate::acl::Principal::new(-1, vec![g], false),
    };
    match crate::acl::pick(set.layer(&rule.path_prefix), &probe) {
        Some(winner) => winner.id == rule.id,
        None => false,
    }
}

fn subject_name(conn: &rusqlite::Connection, s: Subject) -> String {
    match s {
        Subject::Everyone => "所有人".to_string(),
        Subject::User(u) => conn
            .query_row("SELECT username FROM users WHERE id = ?1", params![u], |r| r.get::<_, String>(0))
            .map(|n| format!("用户 {n}"))
            .unwrap_or_else(|_| format!("用户 #{u}（已删除）")),
        Subject::Group(g) => conn
            .query_row("SELECT name FROM groups WHERE id = ?1", params![g], |r| r.get::<_, String>(0))
            .map(|n| format!("组 {n}"))
            .unwrap_or_else(|_| format!("组 #{g}（已删除）")),
    }
}

#[derive(Deserialize)]
pub struct AclReq {
    /// PUT 时必填（按 id 修改）
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub subject_type: Option<String>,
    #[serde(default)]
    pub subject_id: Option<i64>,
    #[serde(default)]
    pub level: Option<String>,
    /// `false` = 继承屏障（§4.2）
    #[serde(default = "default_true")]
    pub inherit: bool,
}

fn default_true() -> bool {
    true
}

fn parse_subject(kind: &str, id: i64) -> Result<Subject, AppError> {
    match kind {
        "user" => {
            if id <= 0 {
                return Err(AppError::InvalidArgument("user 主体必须给正的 subject_id".into()));
            }
            Ok(Subject::User(id))
        }
        "group" => {
            if id <= 0 {
                return Err(AppError::InvalidArgument("group 主体必须给正的 subject_id".into()));
            }
            Ok(Subject::Group(id))
        }
        "everyone" => Ok(Subject::Everyone),
        _ => Err(AppError::InvalidArgument(
            "subject_type 仅支持 user | group | everyone".into(),
        )),
    }
}

fn parse_level(s: &str) -> Result<Level, AppError> {
    Level::parse(s)
        .ok_or_else(|| AppError::InvalidArgument("level 仅支持 none | read | write | admin".into()))
}

pub async fn put_acl(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<AclReq>,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let ip = ip_of(&headers);

    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        let set = acl::load(&conn, repo.id)?;
        let p = principal_of(&user);

        // PUT（带 id）= 修改既有规则；POST = 新建
        if let Some(id) = req.id {
            let existing = set
                .rules()
                .iter()
                .find(|r| r.id == id)
                .ok_or_else(|| AppError::NotFound(format!("规则 #{id} 不存在")))?;
            set.require(&existing.path_prefix, &p, Level::Admin)?;
            let level = match &req.level {
                Some(l) => parse_level(l)?,
                None => existing.level,
            };
            conn.execute(
                "UPDATE acl_rules SET level = ?1, inherit = ?2 WHERE id = ?3 AND repo_id = ?4",
                params![level.as_str(), req.inherit as i64, id, repo.id],
            )?;
            crate::audit::log(
                &conn,
                Some(user.id),
                Some(repo.id),
                "acl.update",
                &format!("repo:{} rule:{id}", repo.name),
                &format!("level={} inherit={}", level.as_str(), req.inherit),
                &ip,
            )?;
            return Ok(json!({ "id": id, "level": level.as_str(), "inherit": req.inherit }));
        }

        let prefix = req
            .path_prefix
            .clone()
            .ok_or_else(|| AppError::InvalidArgument("缺少 path_prefix".into()))?;
        let subject = parse_subject(
            req.subject_type.as_deref().unwrap_or("everyone"),
            req.subject_id.unwrap_or(0),
        )?;
        let level = parse_level(req.level.as_deref().unwrap_or("read"))?;
        // 配置目录权限 = 本目录 admin（§4.3）
        set.require(&prefix, &p, Level::Admin)?;

        let id = acl::upsert_rule(&conn, repo.id, &prefix, subject, level, req.inherit)?;
        crate::audit::log(
            &conn,
            Some(user.id),
            Some(repo.id),
            "acl.set",
            &format!("repo:{} prefix:{prefix}", repo.name),
            &format!("subject={subject:?} level={} inherit={}", level.as_str(), req.inherit),
            &ip,
        )?;
        Ok(json!({
            "id": id,
            "path_prefix": prefix,
            "subject_type": req.subject_type.unwrap_or_else(|| "everyone".into()),
            "subject_id": req.subject_id.unwrap_or(0),
            "level": level.as_str(),
            "inherit": req.inherit,
        }))
    })?;

    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct DeleteAclQuery {
    pub id: i64,
}

pub async fn delete_acl(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    Query(q): Query<DeleteAclQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let ip = ip_of(&headers);

    tokio::task::block_in_place(|| -> Result<(), AppError> {
        let conn = state.lock_db()?;
        let set = acl::load(&conn, repo.id)?;
        let existing = set
            .rules()
            .iter()
            .find(|r| r.id == q.id)
            .ok_or_else(|| AppError::NotFound(format!("规则 #{} 不存在", q.id)))?;
        set.require(&existing.path_prefix, &principal_of(&user), Level::Admin)?;
        if !acl::delete_rule(&conn, repo.id, q.id)? {
            return Err(AppError::NotFound(format!("规则 #{} 不存在", q.id)));
        }
        crate::audit::log(
            &conn,
            Some(user.id),
            Some(repo.id),
            "acl.delete",
            &format!("repo:{} rule:{}", repo.name, q.id),
            &format!("prefix={}", existing.path_prefix),
            &ip,
        )?;
        Ok(())
    })?;

    Ok(Json(json!({ "deleted": true, "id": q.id })))
}

#[derive(Deserialize)]
pub struct PreviewQuery {
    pub user_id: i64,
    #[serde(default)]
    pub path: Option<String>,
}

/// 有效权限预览器（§8.3）：把 §4.2 的回溯算法逐层展开，回答"为什么是这个权限"。
pub async fn acl_preview(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    Query(q): Query<PreviewQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let path = q.path.clone().unwrap_or_default();

    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        let set = acl::load(&conn, repo.id)?;
        // 查看权限解析 = 本仓库可见（read）；管理员穿透
        if !user.is_admin {
            set.require("", &principal_of(&user), Level::Read)?;
        }
        // 目标用户的主体：id + 所属组 + 是否系统管理员
        let (is_admin, username): (bool, String) = conn
            .query_row(
                "SELECT is_admin, username FROM users WHERE id = ?1",
                params![q.user_id],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get::<_, String>(1)?)),
            )
            .map_err(|_| AppError::NotFound(format!("用户 #{} 不存在", q.user_id)))?;
        let groups = crate::auth::session::load_groups(&conn, q.user_id)?;
        let group_names: Vec<String> = groups
            .iter()
            .map(|g| {
                conn.query_row("SELECT name FROM groups WHERE id = ?1", params![g], |r| r.get::<_, String>(0))
                    .unwrap_or_else(|_| format!("#{g}"))
            })
            .collect();
        let target = crate::acl::Principal::new(q.user_id, groups.clone(), is_admin);
        let mut trace = set.trace(&path, &target);
        trace["user"] = json!({
            "id": q.user_id,
            "username": username,
            "is_admin": is_admin,
            "groups": group_names,
        });
        Ok(trace)
    })?;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct WhoQuery {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub level: Option<String>,
}

/// 目录级"谁有权限"反查（§8.3）：列出最终达到某级别的全部用户（含通过组继承来的）。
pub async fn acl_who(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    Query(q): Query<WhoQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let path = q.path.clone().unwrap_or_default();
    let need = parse_level(q.level.as_deref().unwrap_or("read"))?;

    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        let set = acl::load(&conn, repo.id)?;
        if !user.is_admin {
            set.require("", &principal_of(&user), Level::Read)?;
        }

        // 一次性取全部用户 + 全部组成员关系，避免 N+1
        let users: Vec<(i64, String, String, bool, bool)> = {
            let mut stmt = conn.prepare(
                "SELECT id, username, display_name, is_admin, disabled FROM users ORDER BY username",
            )?;
            let mut out = vec![];
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)? != 0,
                    r.get::<_, i64>(4)? != 0,
                ))
            })? {
                out.push(row?);
            }
            out
        };
        let mut members: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT user_id, group_id FROM group_members")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
                let (uid, gid) = row?;
                members.entry(uid).or_default().push(gid);
            }
        }

        let mut items = vec![];
        for (id, username, display_name, is_admin, disabled) in users {
            let groups = members.get(&id).cloned().unwrap_or_default();
            let p = crate::acl::Principal::new(id, groups.clone(), is_admin);
            let level = set.level(&path, &p);
            if !level.at_least(need) {
                continue;
            }
            // 归因：直接命中还是通过组
            let mut via = "直接规则".to_string();
            if !is_admin && !groups.is_empty() {
                let solo = crate::acl::Principal::new(id, vec![], false);
                if set.level(&path, &solo) < level {
                    via = format!(
                        "通过组 {}",
                        groups
                            .iter()
                            .map(|g| conn
                                .query_row("SELECT name FROM groups WHERE id = ?1", params![g], |r| r
                                    .get::<_, String>(0))
                                .unwrap_or_else(|_| format!("#{g}")))
                            .collect::<Vec<_>>()
                            .join("、")
                    );
                }
            }
            if is_admin {
                via = "系统管理员穿透".to_string();
            }
            items.push(json!({
                "id": id,
                "username": username,
                "display_name": display_name,
                "disabled": disabled,
                "level": level.as_str(),
                "via": via,
            }));
        }
        Ok(json!({ "path": path, "level": need.as_str(), "items": items, "total": items.len() }))
    })?;
    Ok(Json(out))
}

// ============================================================
// 系统设置与维护
// ============================================================

pub async fn settings(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    if !user.is_admin {
        return Err(AppError::PermissionDenied("需要系统管理员权限".into()));
    }
    let c = state.config.clone();
    Ok(Json(json!({
        "server": {
            "listen": c.server.listen,
            "data_dir": c.server.data_dir,
            "base_url": c.server.base_url,
        },
        "storage": {
            "compression": c.storage.compression,
            "max_file_size_mb": c.storage.max_file_size_mb,
            "chunk_threshold_mb": c.storage.chunk_threshold_mb,
        },
        "auth": {
            "local_enabled": c.auth.local_enabled,
            "first_user_admin": c.auth.first_user_admin,
            "session_ttl_days": c.auth.session_ttl_days,
            "login_max_fails": c.auth.login_max_fails,
            "login_lockout_secs": c.auth.login_lockout_secs,
            "ldap": {
                "enabled": c.auth.ldap.enabled,
                "url": c.auth.ldap.url,
                "bind_dn": c.auth.ldap.bind_dn,
                // 口令绝不回显（§9.2）
                "bind_password_set": !c.auth.ldap.bind_password.is_empty(),
                "user_base": c.auth.ldap.user_base,
                "user_filter": c.auth.ldap.user_filter,
                "allow_insecure": c.auth.ldap.allow_insecure,
            },
        },
        "db": { "pool_size": c.db.pool_size },
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

#[derive(Deserialize)]
pub struct GcReq {
    #[serde(default)]
    pub limit: Option<usize>,
}

pub async fn maintenance_status(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let _user = require_sysadmin(&state, &headers).await?;
    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        let s = gc::status(&conn)?;
        Ok(json!({
            "queued": s.queued,
            "due_now": s.due_now,
            "queued_bytes": s.queued_bytes,
            "orphan_blobs": s.orphan_blobs,
            "grace_secs": gc::GC_GRACE_SECS,
        }))
    })?;
    Ok(Json(out))
}

/// `POST /admin/maintenance/rebuild-refcount`——refcount 自愈入口（§3.7 关键语义 2）。
pub async fn rebuild_refcount(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    let ip = ip_of(&headers);
    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        let st = gc::rebuild_refcount(&conn)?;
        let queued = gc::enqueue_zero_ref(&conn, "refcount-zero", gc::GC_GRACE_SECS)?;
        crate::audit::log(
            &conn,
            Some(user.id),
            None,
            "maintenance.rebuild_refcount",
            "blobs",
            &format!("blobs={} zero_ref={} queued={}", st.blobs, st.zero_ref, queued),
            &ip,
        )?;
        Ok(json!({
            "blobs": st.blobs,
            "zero_ref": st.zero_ref,
            "reclaimable_bytes": st.reclaimable_bytes,
            "queued": queued,
        }))
    })?;
    Ok(Json(out))
}

/// `POST /admin/maintenance/gc`——处理已到期的 GC 队列项。
pub async fn run_gc(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<GcReq>,
) -> Result<Json<Value>, AppError> {
    let user = require_sysadmin(&state, &headers).await?;
    let ip = ip_of(&headers);
    let limit = req.limit.unwrap_or(1000).clamp(1, 100_000);
    let out = tokio::task::block_in_place(|| -> Result<Value, AppError> {
        let conn = state.lock_db()?;
        let st = gc::run_due(&state.blob, &conn, limit)?;
        crate::audit::log(
            &conn,
            Some(user.id),
            None,
            "maintenance.gc",
            "gc_queue",
            &format!(
                "deleted={} skipped_pending={} bytes={}",
                st.deleted, st.skipped_pending, st.bytes
            ),
            &ip,
        )?;
        Ok(json!({
            "deleted": st.deleted,
            "skipped_pending": st.skipped_pending,
            "bytes": st.bytes,
        }))
    })?;
    Ok(Json(out))
}

/// 目录树浏览（管理端"目录树"懒加载用）。
///
/// 与 `/repos/{repo}/tree` 的差别：只返回**目录**，且带上"是否配置了独立规则"标记（§8.3）。
#[derive(Deserialize)]
pub struct DirsQuery {
    #[serde(default)]
    pub prefix: Option<String>,
}

pub async fn list_dirs(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    Query(q): Query<DirsQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let prefix = q.prefix.clone().unwrap_or_default();
    if !prefix.is_empty() {
        repo::validate_path(&prefix)?;
    }
    let items = tokio::task::block_in_place(|| -> Result<Vec<Value>, AppError> {
        let conn = state.lock_db()?;
        let set = acl::load(&conn, repo.id)?;
        let p = principal_of(&user);
        set.require(&prefix, &p, Level::Read)?;
        let configured: std::collections::HashSet<String> = set
            .rules()
            .iter()
            .map(|r| r.path_prefix.clone())
            .collect();

        let mut out = vec![];
        for e in repo::list_dir_head(&conn, repo.id, &prefix)? {
            let is_dir = matches!(e.kind, repo::Kind::Dir);
            if !set.level(&e.path, &p).at_least(Level::Read) {
                continue;
            }
            if is_dir {
                out.push(json!({
                    "path": e.path,
                    "name": e.path.rsplit('/').next().unwrap_or(&e.path),
                    "kind": "dir",
                    "has_rules": configured.contains(&e.path),
                    "has_children": repo::list_dir_head(&conn, repo.id, &e.path)?
                        .iter()
                        .any(|c| matches!(c.kind, repo::Kind::Dir)),
                }));
            }
        }
        Ok(out)
    })?;
    Ok(Json(json!({ "items": items, "total": items.len() })))
}
