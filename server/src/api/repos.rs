//! 仓库管理与查询 HTTP 端点（§7.2 仓库与浏览）。
//!
//! - GET  /repos                          可见仓库列表
//! - POST /repos                          建仓（系统管理员）
//! - GET  /repos/{repo}/info              head_rev / 策略 / 我的权限
//! - GET  /repos/{repo}/tree?rev=&prefix=&depth=   列目录
//! - GET  /repos/{repo}/log?prefix=&from=&limit=   修订历史（分页信封）
//! - GET  /repos/{repo}/changes?from=&to=&prefix=  增量变更（JSONL）
//!
//! 权限：读取（列目录 / 历史 / 增量 / 下载）要求 `read`（§4.3），
//! 由 [`crate::storage::acl`] 按目录逐路径求解（M2）。

use crate::acl::Level;
use crate::auth::session::SessionUser;
use crate::error::AppError;
use crate::state::SharedState;
use crate::storage::repo::{self, Kind};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};

use super::auth::require_user;
use super::{principal_of, resolve_repo};

fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.starts_with('.')
        && !name.starts_with('-')
}

/// 角色：取仓库根（`''`）上的有效级别；owner 作为并列展示信息（§4.3）。
fn my_role(user: &SessionUser, repo: &repo::RepoRow, root_level: Level) -> &'static str {
    if user.is_admin {
        "admin"
    } else if root_level.at_least(Level::Admin) {
        "admin"
    } else if repo.owner_id == user.id {
        "owner"
    } else {
        "member"
    }
}

// ---------- GET /repos ----------

pub async fn list_repos(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let items = tokio::task::block_in_place(|| -> Result<Vec<Value>, AppError> {
        let conn = state.lock_db()?;
        // v0.4.17：不再下发 lock_policy / needs_lock（策略只剩"先锁后提交"一种，见 §5.2）
        let mut stmt = conn.prepare(
            "SELECT r.id, r.name, r.description, r.owner_id, u.username, r.head_rev, r.created_at
               FROM repos r JOIN users u ON u.id = r.owner_id
              ORDER BY r.name",
        )?;
        let mut rows = stmt.query([])?;
        // 先收全，再逐个查 ACL（避免游标与 prepare_cached 交错借用）
        let mut all: Vec<(i64, String, String, i64, String, i64, String)> = vec![];
        while let Some(row) = rows.next()? {
            all.push((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ));
        }
        drop(rows);
        drop(stmt);

        let principal = principal_of(&user);
        let mut out = vec![];
        for (id, name, description, owner_id, owner, head_rev, created_at) in all {
            // §7.2：只返回「当前用户可见仓库」——根（`''`）上无 read 即不可见；
            // 系统管理员穿透一切（§4.2）。
            let level = crate::storage::acl::load(&conn, id)?.level("", &principal);
            if !user.is_admin && !level.at_least(Level::Read) {
                continue;
            }
            let role = if user.is_admin || level.at_least(Level::Admin) {
                "admin"
            } else if owner_id == user.id {
                "owner"
            } else {
                "member"
            };
            out.push(json!({
                "id": id,
                "name": name,
                "description": description,
                "owner": owner,
                "head_rev": head_rev,
                "created_at": created_at,
                "my_role": role,
                "my_permissions": {
                    "read": level.at_least(Level::Read),
                    "write": level.at_least(Level::Write),
                    "admin": level.at_least(Level::Admin),
                },
            }));
        }
        Ok(out)
    })?;
    Ok(Json(json!({ "items": items, "total": items.len() })))
}

// ---------- POST /repos ----------

/// 建仓请求。v0.4.17：**只有名称与描述** —— `lock_policy` 字段已废弃
/// （serde 默认忽略未知字段，老客户端/老脚本仍传该字段不会报错，但取值一律被忽略）。
#[derive(Deserialize)]
pub struct CreateRepoReq {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

pub async fn create_repo(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<CreateRepoReq>,
) -> Result<impl IntoResponse, AppError> {
    let user = require_user(&state, &headers).await?;
    if !user.is_admin {
        return Err(AppError::PermissionDenied("建仓需要系统管理员权限".into()));
    }
    if !valid_repo_name(&req.name) {
        return Err(AppError::InvalidArgument(format!(
            "仓库名 `{}` 不合法（[A-Za-z0-9._-]，1~64 位，不可以 . 或 - 开头）",
            req.name
        )));
    }
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let repo_id = tokio::task::block_in_place(|| -> Result<i64, AppError> {
        let mut conn = state.lock_db()?;
        let exists: Option<i64> = conn
            .query_row("SELECT id FROM repos WHERE name = ?1", params![req.name], |r| r.get(0))
            .ok();
        if exists.is_some() {
            return Err(AppError::Conflict {
                code: "NAME_COLLISION",
                message: format!("仓库 `{}` 已存在", req.name),
                details: json!({ "name": req.name }),
            });
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
        let desc = req.description.clone().unwrap_or_default();
        let id = repo::create_repo(&tx, &req.name, user.id)?;
        if !desc.is_empty() {
            tx.execute("UPDATE repos SET description = ?2 WHERE id = ?1", params![id, desc])?;
        }
        crate::audit::log(
            &tx,
            Some(user.id),
            Some(id),
            "repo.create",
            &format!("repo:{}", req.name),
            "lock_policy=strict(唯一策略)",
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(id)
    })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": repo_id,
            "name": req.name,
            "head_rev": 0,
        })),
    ))
}

// ---------- GET /repos/{repo}/info ----------

pub async fn repo_info(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let (stats, root_level) = tokio::task::block_in_place(|| -> Result<(Value, Level), AppError> {
        let conn = state.lock_db()?;
        let (file_count, total_size): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM head_entries
              WHERE repo_id = ?1 AND kind = 'file'",
            params![repo.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let rev_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM revisions WHERE repo_id = ?1",
            params![repo.id],
            |r| r.get(0),
        )?;
        // §4.3：查看仓库信息 = read，取仓库根（''）的有效级别
        let acl = crate::storage::acl::load(&conn, repo.id)?;
        let root_level = acl.level("", &principal_of(&user));
        Ok((
            json!({
                "file_count": file_count,
                "total_size": total_size,
                "rev_count": rev_count,
            }),
            root_level,
        ))
    })?;
    if !root_level.at_least(Level::Read) {
        return Err(AppError::PermissionDenied(format!(
            "仓库 `{}` 需要 read 权限",
            repo.name
        )));
    }
    Ok(Json(json!({
        "name": repo.name,
        "description": repo.description,
        "owner_id": repo.owner_id,
        "head_rev": repo.head_rev,
        "my_role": my_role(&user, &repo, root_level),
        "my_permissions": {
            "read": root_level.at_least(Level::Read),
            "write": root_level.at_least(Level::Write),
            "admin": root_level.at_least(Level::Admin),
        },
        "stats": stats,
    })))
}

// ---------- GET /repos/{repo}/tree ----------

#[derive(Deserialize)]
pub struct TreeQuery {
    #[serde(default)]
    pub rev: Option<i64>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub depth: Option<u32>,
}

pub async fn tree(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    Query(q): Query<TreeQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let prefix = q.prefix.clone().unwrap_or_default();
    if !prefix.is_empty() {
        repo::validate_path(&prefix)?;
    }
    let rev = q.rev.unwrap_or(0);
    let depth = q.depth.unwrap_or(1).clamp(1, 16);

    let entries = tokio::task::block_in_place(|| -> Result<Vec<Value>, AppError> {
        let conn = state.lock_db()?;
        // §4.3 列目录 = read；§7.2 要求「按权限过滤」：请求前缀本身必须可读，
        // 展开子目录时无 read 的子树直接不出现在结果里。
        let acl = crate::storage::acl::load(&conn, repo.id)?;
        let principal = principal_of(&user);
        acl.require(&prefix, &principal, Level::Read)?;

        let use_head = rev == 0 || rev == repo.head_rev;
        let mut out: Vec<Value> = vec![];
        let mut queue: Vec<(String, u32)> = vec![(prefix.clone(), 1)];
        while let Some((dir, d)) = queue.pop() {
            let list = if use_head {
                repo::list_dir_head(&conn, repo.id, &dir)?
            } else {
                repo::list_dir_at(&conn, repo.id, &dir, rev)?
            };
            for e in list {
                let is_dir = e.kind == Kind::Dir;
                if !acl.level(&e.path, &principal).at_least(Level::Read) {
                    continue;
                }
                out.push(json!({
                    "path": e.path,
                    "kind": e.kind.as_str(),
                    "blob_hash": e.blob_hash,
                    "size": e.size,
                    "mode": e.mode,
                    "mtime": e.mtime,
                    "changed_rev": e.changed_rev,
                }));
                if is_dir && d < depth {
                    queue.push((e.path.clone(), d + 1));
                }
            }
        }
        out.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        Ok(out)
    })?;

    Ok(Json(json!({
        "repo": repo.name,
        "rev": if rev == 0 { repo.head_rev } else { rev },
        "prefix": prefix,
        "depth": depth,
        "items": entries,
        "total": entries.len(),
    })))
}

// ---------- GET /repos/{repo}/log ----------

#[derive(Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    pub prefix: Option<String>,
    /// 游标：只返回 rev <= from（按修订号倒序分页）
    #[serde(default)]
    pub from: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

pub async fn log(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    Query(q): Query<LogQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    let prefix = q.prefix.clone().unwrap_or_default();
    if !prefix.is_empty() {
        repo::validate_path(&prefix)?;
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0).max(0);

    let items = tokio::task::block_in_place(|| -> Result<(Vec<Value>, i64), AppError> {
        let conn = state.lock_db()?;
        // §4.3 查历史 = read；带 prefix 时按该前缀求权限
        let acl = crate::storage::acl::load(&conn, repo.id)?;
        acl.require(&prefix, &principal_of(&user), Level::Read)?;

        // 有前缀 → 只返回真正动过该子树的修订（半开区间下推索引，M1.6 B1）
        let bounds = if prefix.is_empty() {
            None
        } else {
            repo::prefix_bounds(&format!("{prefix}/"))
        };
        let pfx_sql = if bounds.is_some() {
            " AND EXISTS (SELECT 1 FROM changes c
                           WHERE c.repo_id = r.repo_id AND c.rev = r.rev
                             AND c.path >= ? AND c.path < ?)"
        } else {
            ""
        };

        let mut binds: Vec<rusqlite::types::Value> = vec![repo.id.into()];
        if let Some(from) = q.from.filter(|v| *v > 0) {
            binds.push(from.into());
        }
        if let Some((lo, hi)) = &bounds {
            binds.push(lo.clone().into());
            binds.push(hi.clone().into());
        }

        let mut total_sql = String::from("SELECT COUNT(*) FROM revisions r WHERE r.repo_id = ?");
        if q.from.filter(|v| *v > 0).is_some() {
            total_sql.push_str(" AND r.rev <= ?");
        }
        total_sql.push_str(pfx_sql);
        let total: i64 = conn.query_row(
            &total_sql,
            rusqlite::params_from_iter(binds.iter()),
            |r| r.get(0),
        )?;

        let select = "SELECT r.rev, r.author_id, u.username, r.message, r.created_at,
                             r.file_count, r.byte_delta, r.manifest_hash
                        FROM revisions r JOIN users u ON u.id = r.author_id
                       WHERE r.repo_id = ?";
        let mut sql = String::from(select);
        if q.from.filter(|v| *v > 0).is_some() {
            sql.push_str(" AND r.rev <= ?");
        }
        sql.push_str(pfx_sql);
        sql.push_str(" ORDER BY r.rev DESC LIMIT ? OFFSET ?");

        let mut binds = binds;
        binds.push(limit.into());
        binds.push(offset.into());

        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(binds.iter()))?;
        let mut out = vec![];
        while let Some(row) = rows.next()? {
            out.push(json!({
                "rev": row.get::<_, i64>(0)?,
                "author_id": row.get::<_, i64>(1)?,
                "author": row.get::<_, String>(2)?,
                "message": row.get::<_, String>(3)?,
                "created_at": row.get::<_, String>(4)?,
                "file_count": row.get::<_, i64>(5)?,
                "byte_delta": row.get::<_, i64>(6)?,
                "manifest_hash": row.get::<_, String>(7)?,
            }));
        }
        Ok((out, total))
    })?;

    Ok(Json(json!({ "items": items.0, "total": items.1 })))
}

// ---------- GET /repos/{repo}/changes（JSONL） ----------

#[derive(Deserialize)]
pub struct ChangesQuery {
    pub from: i64,
    pub to: Option<i64>,
    #[serde(default)]
    pub prefix: Option<String>,
}

pub async fn changes(
    State(state): State<SharedState>,
    Path(repo_name): Path<String>,
    Query(q): Query<ChangesQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let user = require_user(&state, &headers).await?;
    let repo = resolve_repo(&state, &repo_name).await?;
    if q.from < 1 {
        return Err(AppError::InvalidArgument("from 必须 ≥ 1".into()));
    }
    let to = q.to.unwrap_or(repo.head_rev);
    if to < q.from {
        return Err(AppError::InvalidArgument("to 必须 ≥ from".into()));
    }
    if to - q.from > 10_000 {
        return Err(AppError::InvalidArgument("一次最多拉取 10000 个修订的增量".into()));
    }
    let prefix = q.prefix.clone().unwrap_or_default();
    if !prefix.is_empty() {
        repo::validate_path(&prefix)?;
    }

    let body = tokio::task::block_in_place(|| -> Result<String, AppError> {
        let conn = state.lock_db()?;
        // §4.3 查增量 = read；无前缀时按仓库根求权限，并按路径逐条过滤（与 tree 一致）
        let acl = crate::storage::acl::load(&conn, repo.id)?;
        let principal = principal_of(&user);
        acl.require(&prefix, &principal, Level::Read)?;

        // 有前缀 → 精确前缀 + 子树（目录级增量）；无前缀 → 全部。
        // 子树用半开区间 `path >= lo AND path < hi` 而不是 `LIKE 'pfx/%'`：后者在 SQLite
        // 默认 `case_sensitive_like=OFF` 下不下推索引（M1.5 B1 的教训），前者可以走
        // `idx_changes_path(repo_id, path, rev DESC)` 的两次范围探测。
        let mut stmt = if prefix.is_empty() {
            conn.prepare(
                "SELECT rev, path, op, kind, blob_hash, size, mode, mtime
                   FROM changes WHERE repo_id = ?1 AND rev >= ?2 AND rev <= ?3
                   ORDER BY rev, path",
            )?
        } else {
            conn.prepare(
                "SELECT rev, path, op, kind, blob_hash, size, mode, mtime
                   FROM changes
                  WHERE repo_id = ?1 AND rev >= ?2 AND rev <= ?3
                    AND (path = ?4 OR (path >= ?5 AND path < ?6))
                  ORDER BY rev, path",
            )?
        };
        let (lo, hi) = match repo::prefix_bounds(&format!("{prefix}/")) {
            Some(b) => b,
            None if prefix.is_empty() => (String::new(), String::new()),
            None => (prefix.clone(), prefix.clone()),
        };
        let mut rows = if prefix.is_empty() {
            stmt.query(params![repo.id, q.from, to])?
        } else {
            stmt.query(params![repo.id, q.from, to, prefix, lo, hi])?
        };
        let mut out = String::new();
        while let Some(row) = rows.next()? {
            let op: String = row.get(2)?;
            let kind: String = row.get(3)?;
            let path: String = row.get(1)?;
            if !acl.level(&path, &principal).at_least(Level::Read) {
                continue; // 无权路径的变更不外泄
            }
            let line = json!({
                "rev": row.get::<_, i64>(0)?,
                "path": path,
                "op": op,
                "kind": kind,
                "blob_hash": row.get::<_, Option<String>>(4)?,
                "size": row.get::<_, i64>(5)?,
                "mode": row.get::<_, i64>(6)?,
                "mtime": row.get::<_, i64>(7)?,
            });
            out.push_str(
                &serde_json::to_string(&line)
                    .map_err(|e| AppError::Internal(format!("JSONL 序列化失败: {e}")))?,
            );
            out.push('\n');
        }
        Ok(out)
    })?;

    let resp = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .body(axum::body::Body::from(body))
        .map_err(|e| AppError::Internal(format!("{e}")))?;
    Ok(resp)
}
