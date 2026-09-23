use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::auth::{self, session};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/me", get(me))
        .route("/api/v1/auth/logout", post(logout))
        .with_state(state)
}

async fn health(State(st): State<SharedState>) -> AppResult<Json<serde_json::Value>> {
    let db_ok = st.db.with(|c| {
        c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).map_err(AppError::from)?;
        Ok(())
    });
    Ok(Json(json!({
        "status": if db_ok.is_ok() { "ok" } else { "degraded" },
        "service": "b-artifact-server",
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

async fn login(
    State(st): State<SharedState>,
    Json(req): Json<LoginReq>,
) -> AppResult<Json<serde_json::Value>> {
    if req.username.trim().is_empty() || req.password.is_empty() {
        return Err(AppError::validation("用户名和密码不能为空"));
    }
    let ttl = st.cfg.session_ttl_days;
    let (user, issued) = st.db.with(|c| {
        let user = auth::authenticate(c, req.username.trim(), &req.password)?;
        let user = user.ok_or(AppError::InvalidCredentials)?;
        let issued = session::create(c, user.id, ttl, "")?;
        Ok((user, issued))
    })?;

    Ok(Json(json!({
        "token": issued.token,
        "expires_at": issued.expires_at,
        "user": user,
    })))
}

fn bearer(headers: &HeaderMap) -> AppResult<String> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;
    value
        .strip_prefix("Bearer ")
        .map(|t| t.trim().to_string())
        .ok_or(AppError::Unauthenticated)
}

async fn me(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    let token = bearer(&headers)?;
    let user = st
        .db
        .with(|c| session::resolve(c, &token))?
        .ok_or(AppError::Unauthenticated)?;
    Ok(Json(json!({ "user": user })))
}

async fn logout(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    let token = bearer(&headers)?;
    let revoked = st.db.with(|c| session::revoke(c, &token))?;
    Ok(Json(json!({ "ok": revoked })))
}
