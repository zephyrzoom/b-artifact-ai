//! HTTP 路由装配（§7.2 接口按里程碑逐步补齐）。

pub mod admin;
pub mod auth;
pub mod commit;
pub mod locks;
pub mod repos;
pub mod static_files;
pub mod transfer;

use crate::acl::Principal;
use crate::auth::session::SessionUser;
use crate::error::AppError;
use crate::state::SharedState;
use crate::storage::repo::RepoRow;
use axum::extract::{DefaultBodyLimit, State};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::json;

/// 大请求体上限：整块/分块 PUT（chunk_threshold 默认 64MB，留 1MB 余量）。
const BIG_BODY_LIMIT: usize = 65 * 1024 * 1024;

pub fn router(state: SharedState) -> Router {
    let transfer_body = Router::new()
        // 传输（§7.2）
        .route(
            "/api/v1/repos/{repo}/blobs/missing",
            post(transfer::missing),
        )
        .route(
            "/api/v1/repos/{repo}/blobs/uploads",
            post(transfer::create_upload),
        )
        .route(
            "/api/v1/repos/{repo}/blobs/uploads/{id}",
            get(transfer::upload_status).delete(transfer::delete_upload),
        )
        .route(
            "/api/v1/repos/{repo}/blobs/uploads/{id}/complete",
            post(transfer::complete_upload),
        )
        .route(
            "/api/v1/repos/{repo}/blobs/uploads/{id}/{n}",
            axum::routing::put(transfer::put_chunk),
        )
        .route(
            "/api/v1/repos/{repo}/blobs/{hash}",
            axum::routing::put(transfer::put_blob).get(transfer::download_blob).head(transfer::head_blob),
        )
        .layer(DefaultBodyLimit::max(BIG_BODY_LIMIT));

    Router::new()
        .route("/health", get(health))
        .route("/api/v1/health", get(health))
        // 认证（§9.1 / §9.2）
        .route("/api/v1/auth/providers", get(auth::providers))
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/me", get(auth::me))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/auth/password", post(auth::change_password))
        // 提交（§5.3 / §7.3）
        .route("/api/v1/repos/{repo}/commit/prepare", post(commit::prepare))
        .route("/api/v1/repos/{repo}/commit", post(commit::commit))
        // 仓库与浏览（§7.2）
        .route("/api/v1/repos", get(repos::list_repos).post(repos::create_repo))
        .route("/api/v1/repos/{repo}/info", get(repos::repo_info))
        .route("/api/v1/repos/{repo}/tree", get(repos::tree))
        .route("/api/v1/repos/{repo}/log", get(repos::log))
        .route("/api/v1/repos/{repo}/changes", get(repos::changes))
        // 锁（§7.2 / §5）
        .route("/api/v1/repos/{repo}/locks", get(locks::list_locks).post(locks::acquire))
        .route("/api/v1/repos/{repo}/locks/refresh", post(locks::refresh))
        .route(
            "/api/v1/repos/{repo}/locks/{*path}",
            axum::routing::delete(locks::release),
        )
        // ---- 管理端（§7.2 管理 / §8 管理页面）----
        // 概览与审计
        .route("/api/v1/admin/stats", get(admin::stats))
        .route("/api/v1/admin/audit", get(admin::audit))
        // 用户
        .route(
            "/api/v1/admin/users",
            get(admin::list_users).post(admin::create_user),
        )
        .route(
            "/api/v1/admin/users/{id}",
            put(admin::update_user).delete(admin::delete_user),
        )
        .route("/api/v1/admin/users/{id}/password", post(admin::reset_password))
        // 组
        .route(
            "/api/v1/admin/groups",
            get(admin::list_groups).post(admin::create_group),
        )
        .route(
            "/api/v1/admin/groups/{id}",
            put(admin::update_group).delete(admin::delete_group),
        )
        .route(
            "/api/v1/admin/groups/{id}/members",
            get(admin::list_members).put(admin::set_members),
        )
        // 仓库设置 / 删除 / 清除历史
        .route("/api/v1/admin/repos/{repo}/settings", put(admin::update_repo_settings))
        .route("/api/v1/admin/repos/{repo}", delete(admin::delete_repo))
        .route("/api/v1/admin/repos/{repo}/purge", post(admin::purge))
        // 目录权限规则 + 可视化辅助接口
        .route(
            "/api/v1/admin/repos/{repo}/acl",
            get(admin::list_acl).post(admin::put_acl).put(admin::put_acl).delete(admin::delete_acl),
        )
        .route("/api/v1/admin/repos/{repo}/acl/preview", get(admin::acl_preview))
        .route("/api/v1/admin/repos/{repo}/acl/who", get(admin::acl_who))
        // 目录树（管理端懒加载）
        .route("/api/v1/admin/repos/{repo}/dirs", get(admin::list_dirs))
        // 系统设置与维护
        .route("/api/v1/admin/settings", get(admin::settings))
        .route("/api/v1/admin/maintenance", get(admin::maintenance_status))
        .route(
            "/api/v1/admin/maintenance/rebuild-refcount",
            post(admin::rebuild_refcount),
        )
        .route("/api/v1/admin/maintenance/gc", post(admin::run_gc))
        // 管理端前端（§8.1）：SPA，未匹配路径回落到 index.html
        .route("/admin", get(static_files::admin_root))
        .route("/admin/", get(static_files::admin_root))
        .route("/admin/{*path}", get(static_files::admin_path))
        // 浏览器直接访问根路径时跳到管理端
        .route("/", get(root_redirect))
        .merge(transfer_body)
        .with_state(state)
}

/// 根路径 → 管理端。
async fn root_redirect() -> axum::response::Redirect {
    axum::response::Redirect::temporary("/admin/")
}

async fn health(State(state): State<SharedState>) -> Json<serde_json::Value> {
    // 顺带验证 DB 连接可用
    let db_ok = state.db.get().map(|c| c.is_autocommit()).unwrap_or(false);
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "db": if db_ok { "ok" } else { "unknown" },
    }))
}

// ============ M1 权限辅助（完整 ACL 引擎 M2 落地） ============

/// 按名字查仓库；不存在 → 404。
pub async fn resolve_repo(state: &SharedState, name: &str) -> Result<RepoRow, AppError> {
    let conn = state.lock_db()?;
    crate::storage::repo::repo_by_name(&conn, name)?
        .ok_or_else(|| AppError::NotFound(format!("仓库 `{name}` 不存在")))
}

/// 会话用户 → ACL 主体（§4.2 `pick` 需要用户 id、所属组、是否系统管理员）。
pub fn principal_of(user: &SessionUser) -> Principal {
    Principal::new(user.id, user.groups.clone(), user.is_admin)
}
