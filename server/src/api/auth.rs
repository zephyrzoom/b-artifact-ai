//! 认证 HTTP 端点（§9.1 / §9.2）。
//!
//! - GET  /api/v1/auth/providers  登录方式探测（无需认证）
//! - POST /api/v1/auth/login      统一登录（本地 argon2 / LDAP 两段式 bind + JIT + 首登管理员）
//! - GET  /api/v1/auth/me         当前会话用户
//! - POST /api/v1/auth/logout     吊销当前会话
//! - POST /api/v1/auth/password   修改本人密码（仅 local 用户）

use crate::auth::{self, provision, session, AuthOutcome};
use crate::error::AppError;
use crate::state::SharedState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};

fn user_json(u: &session::SessionUser) -> Value {
    json!({
        "id": u.id,
        "username": u.username,
        "display_name": u.display_name,
        "source": u.source,
        "is_admin": u.is_admin,
    })
}

/// 提取 Authorization: Bearer <token>。
pub fn extract_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
}

/// 鉴权中间件等价物：解析 Bearer token → 当前用户；失败 → 401。
pub async fn require_user(
    state: &SharedState,
    headers: &HeaderMap,
) -> Result<session::SessionUser, AppError> {
    let token = extract_token(headers)
        .ok_or_else(|| AppError::Unauthenticated("缺少 Authorization: Bearer <token>".into()))?;
    let conn = state.lock_db()?;
    session::resolve(&conn, token)?
        .ok_or_else(|| AppError::Unauthenticated("会话无效或已过期，请重新登录".into()))
}

fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn user_agent(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(256)
        .collect()
}

// ---------- GET /auth/providers ----------

pub async fn providers(State(state): State<SharedState>) -> Json<Value> {
    // §9.1：登录页据此渲染（是否显示"域账号登录"、是否提示"首位登录者将成为管理员"）
    Json(json!({
        "local": state.config.auth.local_enabled,
        "ldap": state.config.auth.ldap.enabled,
        "first_user_admin": state.config.auth.first_user_admin,
    }))
}

// ---------- POST /auth/login ----------

#[derive(Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

pub async fn login(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> Result<impl IntoResponse, AppError> {
    let username = req.username.trim().to_string();
    if username.is_empty() || username.len() > 64 || req.password.is_empty() || req.password.len() > 256 {
        return Err(AppError::InvalidArgument("用户名或密码格式不合法".into()));
    }
    // 登录失败退避（§9.2）
    state.limiter.check(&username)?;

    // 认证（LDAP 网络调用可能阻塞 → block_in_place，避免卡住整个 runtime；
    // M1 持锁期间走 LDAP 是已知简化，M1.5 压测时再评估拆锁）
    let outcome = tokio::task::block_in_place(|| {
        let conn = state.lock_db()?;
        auth::authenticate(&conn, &state.config.auth, &username, &req.password)
    })?;

    let AuthOutcome::Authenticated { user_id, source, ldap_user } = outcome else {
        state.limiter.record_fail(&username);
        return Err(AppError::Unauthenticated("用户名或密码错误".into()));
    };
    state.limiter.record_success(&username);

    // LDAP 成功：事务内 JIT 建号/刷新 + 首登管理员（users 表为空时原子授予）
    let user_id = if let Some(lu) = ldap_user.as_ref() {
        let ip = client_ip(&headers);
        tokio::task::block_in_place(|| -> Result<i64, AppError> {
            let mut conn = state.lock_db()?;
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
            let id = provision::provision(&tx, lu)?;
            if state.config.auth.first_user_admin {
                provision::grant_first_admin_if_needed(&tx, id)?;
            }
            crate::audit::log(
                &tx,
                Some(id),
                None,
                "user.login",
                &format!("user:{id}"),
                "source=ldap",
                &ip,
            )?;
            tx.commit()
                .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
            Ok(id)
        })?
    } else if state.config.auth.first_user_admin {
        // 本地用户同样参与首登管理员判定（§9.1："本地或 LDAP 均可"）。
        // 早期实现只在这件事上照顾了 LDAP 分支——本地首登者拿不到管理员，
        // 与方案声明的"无需预建管理员"直接冲突（冒烟脚本一直预建管理员，所以没暴露）。
        tokio::task::block_in_place(|| -> Result<i64, AppError> {
            let mut conn = state.lock_db()?;
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;
            provision::grant_first_admin_if_needed(&tx, user_id)?;
            tx.commit()
                .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
            Ok(user_id)
        })?
    } else {
        user_id
    };

    // 签发会话（明文 token 只在响应中出现一次，DB 存 sha256）
    let ua = user_agent(&headers);
    let ip = client_ip(&headers);
    let (token, expires_at, user) = tokio::task::block_in_place(|| {
        let conn = state.lock_db()?;
        let (token, expires_at) =
            session::issue(&conn, user_id, state.config.auth.session_ttl_days, &ua)?;
        if source == "local" {
            let _ = crate::audit::log(
                &conn,
                Some(user_id),
                None,
                "user.login",
                &format!("user:{user_id}"),
                "source=local",
                &ip,
            );
        }
        let user = session::resolve(&conn, &token)?
            .ok_or_else(|| AppError::Internal("刚签发的会话无法解析（不应发生）".into()))?;
        Ok::<_, AppError>((token, expires_at, user))
    })?;

    Ok(Json(json!({
        "token": token,
        "expires_at": expires_at,
        "user": user_json(&user),
    })))
}

// ---------- GET /auth/me ----------

pub async fn me(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    Ok(Json(user_json(&user)))
}

// ---------- POST /auth/logout ----------

pub async fn logout(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    let token = extract_token(&headers).expect("require_user 已校验");
    let ip = client_ip(&headers);
    let conn = state.lock_db()?;
    session::revoke(&conn, token)?;
    crate::audit::log(
        &conn,
        Some(user.id),
        None,
        "user.logout",
        &format!("user:{}", user.id),
        "",
        &ip,
    )?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- POST /auth/password ----------

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    pub old_password: String,
    pub new_password: String,
}

pub async fn change_password(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordReq>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    if user.source != "local" {
        return Err(AppError::PermissionDenied(
            "LDAP 账号请通过目录服务修改密码，本系统不托管其口令".into(),
        ));
    }
    // §9.2 口令强度（v0.4.17）：字符数下限/上限 + 四类字符，统一走 auth::password
    crate::auth::password::validate_policy(&req.new_password)?;

    let token = extract_token(&headers).expect("require_user 已校验");
    let ip = client_ip(&headers);
    tokio::task::block_in_place(|| {
        let mut conn = state.lock_db()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| AppError::Internal(format!("开启事务失败: {e}")))?;

        let old_hash: Option<String> = tx
            .query_row(
                "SELECT password_hash FROM users WHERE id = ?1 AND source = 'local'",
                params![user.id],
                |r| r.get(0),
            )
            .map_err(|_| AppError::Internal("查询用户失败".into()))?;
        let ok = old_hash
            .as_deref()
            .and_then(|h| crate::auth::password::verify_password(&req.old_password, h))
            .unwrap_or(false);
        if !ok {
            return Err(AppError::Unauthenticated("旧密码错误".into()));
        }

        let new_hash = crate::auth::password::hash_password(&req.new_password)?;
        tx.execute(
            "UPDATE users SET password_hash = ?2 WHERE id = ?1",
            params![user.id, new_hash],
        )?;
        let revoked = session::revoke_others(&tx, user.id, token)?;
        crate::audit::log(
            &tx,
            Some(user.id),
            None,
            "user.password_changed",
            &format!("user:{}", user.id),
            &format!("吊销其他会话 {revoked} 个"),
            &ip,
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(format!("提交事务失败: {e}")))?;
        Ok(())
    })?;

    Ok(StatusCode::NO_CONTENT)
}
