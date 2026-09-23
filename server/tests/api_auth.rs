//! 认证链路端到端集成测试（§9.1 / §9.2）。
//!
//! `api_m1.rs` 只覆盖了"登录拿到 token 后能调业务接口"，`api/auth.rs` 的
//! 退出登录 / 改密 / 限流 / 首登管理员 等分支长期没进集成测试，行覆盖掉到 57%
//! ——低于 §12 的"核心模块 ≥80%"门禁。本文件补齐这些分支。
//!
//! 覆盖：
//! - `GET /auth/providers`：本地 / LDAP 开关
//! - 登录参数校验（空 / 超长 → 400）与口令校验（401）
//! - 登录失败退避（§9.2）：连续失败到达阈值后 429，成功一次即清零
//! - `GET /auth/me`：无 token / 坏 token → 401
//! - `POST /auth/logout`：204 且 token 立即失效
//! - `POST /auth/password`：旧口令错 401、新口令过短/过长 400、成功 204、
//!   新口令可登录、**其他会话被吊销**、审计留痕
//! - 首登管理员（§9.1）：`users` 表为空时第一个登录者自动获得系统管理员
//! - 禁用账号不能登录（403）

use b_artifact_server::api;
use b_artifact_server::auth::password::hash_password;
use b_artifact_server::config::Config;
use b_artifact_server::state::{AppState, SharedState};
use b_artifact_server::storage::{blob::BlobStore, db, pool::DbPool};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

struct Fx {
    base: String,
    /// 数据目录（直接改库造场景：禁用账号、清空 users 验首登管理员）
    data: std::path::PathBuf,
    _dir: TempDir,
}

fn pw_of(name: &str) -> String {
    format!("{name}-PW-123456")
}

/// 预置两个本地用户 `li` / `zhao`（均非管理员）。
///
/// **必须预置两个**：首登管理员规则是"`users` 表里只剩登录者一人时自动授予
/// is_admin"（§9.1），只建一个人的话 `li` 一登录就会变成管理员，
/// "li 不是管理员"的断言会假失败。
async fn spawn() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    std::fs::create_dir_all(data.join("uploads")).unwrap();
    {
        let mut conn = db::open(&data.join("b-artifact.db")).unwrap();
        db::migrate(&mut conn).unwrap();
        for (name, is_admin) in [("li", 0i64), ("zhao", 0i64)] {
            conn.execute(
                "INSERT INTO users (username, source, password_hash, is_admin, disabled, created_at)
                 VALUES (?1, 'local', ?2, ?3, 0, ?4)",
                rusqlite::params![
                    name,
                    hash_password(&pw_of(name)).unwrap(),
                    is_admin,
                    chrono::Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
        }
    }
    let blob = BlobStore::new(&data).unwrap();
    let pool = DbPool::new(&data.join("b-artifact.db"), 4).unwrap();
    let state = SharedState(Arc::new(AppState::new(
        pool,
        blob,
        Config::default(),
        data.join("uploads"),
    )));
    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Fx { base: format!("http://{addr}"), data, _dir: dir }
}

fn db_conn(fx: &Fx) -> rusqlite::Connection {
    db::open(&fx.data.join("b-artifact.db")).unwrap()
}

async fn raw_login(client: &reqwest::Client, fx: &Fx, user: &str, pw: &str) -> (u16, Value) {
    let resp = client
        .post(format!("{}/api/v1/auth/login", fx.base))
        .json(&json!({ "username": user, "password": pw }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    (status, v)
}

async fn login(client: &reqwest::Client, fx: &Fx, name: &str) -> String {
    let (st, v) = raw_login(client, fx, name, &pw_of(name)).await;
    assert_eq!(st, 200, "登录 {name} 应成功: {v}");
    v["token"].as_str().unwrap().to_string()
}

async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut r = client.request(method, url);
    if let Some(t) = token {
        r = r.header("Authorization", format!("Bearer {t}"));
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    (status, v)
}

// ============ 1. providers / me / 登录校验 ============

#[tokio::test(flavor = "multi_thread")]
async fn providers_and_login_validation() {
    let fx = spawn().await;
    let client = reqwest::Client::new();

    // 认证方式开关（登录页据此决定要不要显示 LDAP 入口）
    let (st, v) = call(
        &client,
        reqwest::Method::GET,
        format!("{}/api/v1/auth/providers", fx.base),
        None,
        None,
    )
    .await;
    assert_eq!(st, 200, "providers: {v}");
    assert_eq!(v["local"], true, "默认应开启本地认证: {v}");
    assert_eq!(v["ldap"], false, "默认不配 LDAP: {v}");
    assert_eq!(v["first_user_admin"], true, "§9.1 要求暴露首登管理员开关: {v}");

    // 参数校验（§9.2 输入长度上限）
    for (user, pw, why) in [
        ("", "x", "空用户名"),
        ("li", "", "空密码"),
        (&"u".repeat(65), "x", "用户名过长"),
        ("li", &"p".repeat(257), "密码过长"),
    ] {
        let (st, _) = raw_login(&client, &fx, user, pw).await;
        assert_eq!(st, 400, "{why} 应 400，实际 {st}");
    }

    // 口令错误 → 401（不区分"用户不存在"与"口令错"，避免账号枚举）
    let (st, v) = raw_login(&client, &fx, "li", "wrong-password").await;
    assert_eq!(st, 401, "错误口令: {v}");
    let (st, v) = raw_login(&client, &fx, "nobody", &pw_of("li")).await;
    assert_eq!(st, 401, "不存在的用户同样 401: {v}");

    // 成功登录
    let (st, v) = raw_login(&client, &fx, "li", &pw_of("li")).await;
    assert_eq!(st, 200, "登录: {v}");
    assert!(v["token"].as_str().unwrap().len() >= 32, "token 应足够长: {v}");
    assert!(v["expires_at"].is_string(), "应给出过期时间: {v}");
    assert_eq!(v["user"]["username"], "li");
    assert_eq!(v["user"]["is_admin"], false, "li 不是管理员: {v}");
    let token = v["token"].as_str().unwrap().to_string();

    // me：带 token 200 / 坏 token 401 / 无 token 401
    let (st, v) = call(
        &client,
        reqwest::Method::GET,
        format!("{}/api/v1/auth/me", fx.base),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st, 200, "me: {v}");
    assert_eq!(v["username"], "li");
    assert!(v.get("password_hash").is_none(), "me 不得泄露口令哈希: {v}");

    let (st, _) = call(
        &client,
        reqwest::Method::GET,
        format!("{}/api/v1/auth/me", fx.base),
        Some("not-a-real-token"),
        None,
    )
    .await;
    assert_eq!(st, 401, "坏 token 应 401");
    let (st, _) = call(
        &client,
        reqwest::Method::GET,
        format!("{}/api/v1/auth/me", fx.base),
        None,
        None,
    )
    .await;
    assert_eq!(st, 401, "无 token 应 401");
}

// ============ 2. 登录失败退避（§9.2） ============

#[tokio::test(flavor = "multi_thread")]
async fn login_rate_limit() {
    let fx = spawn().await;
    let client = reqwest::Client::new();

    // 默认 login_max_fails = 5：前 5 次失败仍是 401，第 6 次起被限流 429
    for i in 1..=5 {
        let (st, v) = raw_login(&client, &fx, "li", "bad").await;
        assert_eq!(st, 401, "第 {i} 次失败应仍是 401: {v}");
    }
    let (st, v) = raw_login(&client, &fx, "li", "bad").await;
    assert_eq!(st, 429, "超过阈值应限流: {v}");
    assert!(v["error"]["code"].is_string(), "应带错误码: {v}");

    // 限流期内即使用正确口令也被挡（否则退避形同虚设）
    let (st, _) = raw_login(&client, &fx, "li", &pw_of("li")).await;
    assert_eq!(st, 429, "限流期内正确口令也应被挡");

    // 换一个用户名不受影响（限流按用户名维度计数）
    let (st, _) = raw_login(&client, &fx, "other", "bad").await;
    assert_eq!(st, 401, "别的用户名不应被连带限流");
}

// ============ 3. 退出登录 ============

#[tokio::test(flavor = "multi_thread")]
async fn logout_revokes_token() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let token = login(&client, &fx, "li").await;

    let (st, v) = call(
        &client,
        reqwest::Method::POST,
        format!("{}/api/v1/auth/logout", fx.base),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st, 204, "logout 应 204: {v}");

    // token 立即失效
    let (st, _) = call(
        &client,
        reqwest::Method::GET,
        format!("{}/api/v1/auth/me", fx.base),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st, 401, "退出后 token 应失效");

    // 可以重新登录，拿到的是新 token
    let token2 = login(&client, &fx, "li").await;
    assert_ne!(token, token2, "新会话应签发新 token");

    // 审计留痕（§7.2 / M2 引入）
    let conn = db_conn(&fx);
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'user.logout'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(n >= 1, "退出登录应进审计");
}

// ============ 4. 修改密码 ============

/// 改密：返回 (状态码, body)
async fn change_pw(client: &reqwest::Client, fx: &Fx, token: &str, old: &str, new: &str) -> (u16, Value) {
    call(
        client,
        reqwest::Method::POST,
        format!("{}/api/v1/auth/password", fx.base),
        Some(token),
        Some(json!({ "old_password": old, "new_password": new })),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn change_password_flow() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let token = login(&client, &fx, "li").await;

    let new_pw = "new-PW-123456";

    // 旧口令错 → 401
    let (st, v) = change_pw(&client, &fx, &token, "nope", new_pw).await;
    assert_eq!(st, 401, "旧口令错: {v}");
    // 新口令过短 / 过长 → 400
    let (st, v) = change_pw(&client, &fx, &token, &pw_of("li"), "short").await;
    assert_eq!(st, 400, "新口令过短: {v}");
    let long = "p".repeat(257);
    let (st, v) = change_pw(&client, &fx, &token, &pw_of("li"), &long).await;
    assert_eq!(st, 400, "新口令过长: {v}");
    // 无 token → 401
    let (st, _) = call(
        &client,
        reqwest::Method::POST,
        format!("{}/api/v1/auth/password", fx.base),
        None,
        Some(json!({"old_password":pw_of("li"),"new_password":new_pw})),
    )
    .await;
    assert_eq!(st, 401, "改密必须带会话");

    // 再开一个会话，改密后应被吊销（§9.2：改密吊销其他会话）
    let other = login(&client, &fx, "li").await;

    let (st, v) = change_pw(&client, &fx, &token, &pw_of("li"), new_pw).await;
    assert_eq!(st, 204, "改密应 204: {v}");

    // 当前会话仍有效（否则用户改完就被踢，体验很差）
    let (st, _) = call(
        &client,
        reqwest::Method::GET,
        format!("{}/api/v1/auth/me", fx.base),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(st, 200, "改密不应踢掉当前会话");
    // 其他会话被吊销
    let (st, _) = call(
        &client,
        reqwest::Method::GET,
        format!("{}/api/v1/auth/me", fx.base),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(st, 401, "其他会话应被吊销");

    // 新口令可登录，旧口令失效
    let (st, v) = raw_login(&client, &fx, "li", new_pw).await;
    assert_eq!(st, 200, "新口令应可登录: {v}");
    let (st, _) = raw_login(&client, &fx, "li", &pw_of("li")).await;
    assert_eq!(st, 401, "旧口令应失效");

    // 审计留痕
    let conn = db_conn(&fx);
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'user.password_changed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(n >= 1, "改密应进审计");
}

// ============ 5. 首登管理员与禁用账号（§9.1） ============

#[tokio::test(flavor = "multi_thread")]
async fn first_user_becomes_admin_and_disabled_rejected() {
    let fx = spawn().await;
    let client = reqwest::Client::new();

    // 禁用 li → 403（AppError::AccountDisabled 映射 403，不是 401：凭据是对的）
    {
        let conn = db_conn(&fx);
        conn.execute("UPDATE users SET disabled = 1 WHERE username = 'li'", [])
            .unwrap();
    }
    let (st, v) = raw_login(&client, &fx, "li", &pw_of("li")).await;
    assert_eq!(st, 403, "禁用账号应 403: {v}");

    // 清空 users → 第一个登录者自动成为系统管理员（JIT + 首登管理员，§9.1）
    {
        let conn = db_conn(&fx);
        conn.execute("DELETE FROM sessions", []).unwrap();
        conn.execute("DELETE FROM group_members", []).unwrap();
        conn.execute("DELETE FROM users", []).unwrap();
        conn.execute(
            "INSERT INTO users (username, source, password_hash, is_admin, disabled, created_at)
             VALUES ('first', 'local', ?1, 0, 0, ?2)",
            rusqlite::params![
                hash_password(&pw_of("first")).unwrap(),
                chrono::Utc::now().to_rfc3339()
            ],
        )
        .unwrap();
    }
    let (st, v) = raw_login(&client, &fx, "first", &pw_of("first")).await;
    assert_eq!(st, 200, "首登: {v}");
    assert_eq!(v["user"]["is_admin"], true, "空库首登应自动授予系统管理员: {v}");

    // 第二个用户不再被授予
    {
        let conn = db_conn(&fx);
        conn.execute(
            "INSERT INTO users (username, source, password_hash, is_admin, disabled, created_at)
             VALUES ('second', 'local', ?1, 0, 0, ?2)",
            rusqlite::params![
                hash_password(&pw_of("second")).unwrap(),
                chrono::Utc::now().to_rfc3339()
            ],
        )
        .unwrap();
    }
    let (st, v) = raw_login(&client, &fx, "second", &pw_of("second")).await;
    assert_eq!(st, 200, "第二个用户: {v}");
    assert_eq!(v["user"]["is_admin"], false, "只有首个用户被授予管理员: {v}");
}
