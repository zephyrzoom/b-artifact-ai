//! M3 端到端集成测试：管理 API（§7.2 / §8.3）与静态托管（§8.1）。
//!
//! 与 `api_m1.rs` / `api_m2.rs` 的基建刻意独立：M3 需要系统管理员 + 普通用户 + 组
//! + 已提交数据的多重场景，且要驱动 purge / GC 这类"不可逆"能力，混进前面的文件
//! 会让三边都难维护。
//!
//! 覆盖：
//! - 系统管理员门禁：非管理员访问 `/api/v1/admin/*` 一律 403
//! - 统计与审计（含 CSV 导出、按 action 过滤）
//! - 用户 CRUD（重名 409 / 弱密码 400 / 禁止禁用自己 / 重置密码）
//! - 组 CRUD + 成员管理 + 被 ACL 引用时拒绝删除（409 GROUP_IN_USE）
//! - 仓库设置（非法 lock_policy 400）
//! - ACL 规则 CRUD + 有效权限预览器（hit / barrier / miss / skip）+ "谁有权限"反查
//! - purge 全链路（confirm_name / 原因长度 / 他人锁 PURGE_BLOCKED / 成功 / GC 入队）
//! - 维护：rebuild-refcount 与 GC
//! - 静态托管：SPA 回退、带 hash 资源长缓存、路径穿越防护、未构建时的提示页

use b_artifact_server::acl::{Level, Subject};
use b_artifact_server::api;
use b_artifact_server::auth::password::hash_password;
use b_artifact_server::config::Config;
use b_artifact_server::state::{AppState, SharedState};
use b_artifact_server::storage::{acl, blob::BlobStore, db, pool::DbPool, repo};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

// ============ 基建 ============

struct Fx {
    base: String,
    /// 系统管理员
    admin: i64,
    /// 普通用户，属于 art 组
    li: i64,
    /// 普通用户，不属于任何组
    zhao: i64,
    art: i64,
    /// 空组（li/zhao 都不命中），用于演示屏障
    design: i64,
    _dir: TempDir,
}

fn pw_of(name: &str) -> String {
    format!("{name}-PW-123456")
}

/// 预置：admin（系统管理员）/ li（art 组）/ zhao（无组）、空组 design、
/// 仓库 `assets`（advisory，默认根 everyone:read），并给 li 在 `secret` 上开 write
/// ——purge 的"他人持锁 → PURGE_BLOCKED"场景需要 li 能真的加上锁。
async fn spawn() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    std::fs::create_dir_all(data.join("uploads")).unwrap();

    let (admin, li, zhao, art, design) = {
        let mut conn = db::open(&data.join("b-artifact.db")).unwrap();
        db::migrate(&mut conn).unwrap();

        let mk = |conn: &rusqlite::Connection, name: &str, is_admin: bool| -> i64 {
            conn.execute(
                "INSERT INTO users (username, source, password_hash, is_admin, disabled, created_at)
                 VALUES (?1, 'local', ?2, ?3, 0, ?4)",
                rusqlite::params![
                    name,
                    hash_password(&pw_of(name)).unwrap(),
                    is_admin as i64,
                    chrono::Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let admin = mk(&conn, "admin", true);
        let li = mk(&conn, "li", false);
        let zhao = mk(&conn, "zhao", false);

        let grp = |conn: &rusqlite::Connection, name: &str| -> i64 {
            conn.execute(
                "INSERT INTO groups (name, comment, created_at) VALUES (?1, '', ?2)",
                rusqlite::params![name, chrono::Utc::now().to_rfc3339()],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let art = grp(&conn, "art");
        let design = grp(&conn, "design");
        conn.execute(
            "INSERT INTO group_members (group_id, user_id) VALUES (?1, ?2)",
            rusqlite::params![art, li],
        )
        .unwrap();

        let assets = repo::create_repo(&conn, "assets", admin).unwrap();
        // li 在 secret 上可写（加锁需要 write）
        acl::upsert_rule(&conn, assets, "secret", Subject::User(li), Level::Write, true).unwrap();
        // 演示屏障用的规则（li/zhao 都命中不了 design 组）
        acl::upsert_rule(
            &conn,
            assets,
            "secret",
            Subject::Group(design),
            Level::Read,
            true,
        )
        .unwrap();
        (admin, li, zhao, art, design)
    };

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
    Fx { base: format!("http://{addr}"), admin, li, zhao, art, design, _dir: dir }
}

async fn login(client: &reqwest::Client, fx: &Fx, name: &str) -> String {
    let resp = client
        .post(format!("{}/api/v1/auth/login", fx.base))
        .json(&json!({ "username": name, "password": pw_of(name) }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "登录 {name} 应成功");
    let body: Value = resp.json().await.unwrap();
    body["token"].as_str().unwrap().to_string()
}

async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut r = client.request(method, url).header("Authorization", format!("Bearer {token}"));
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    (status, v)
}

/// 原始字节体（blob 上传）；走 JSON 分支会让算出的 sha256 与 URL 声明的不一致 → 412。
async fn call_raw(
    client: &reqwest::Client,
    url: String,
    token: &str,
    data: &[u8],
) -> (u16, Value) {
    let resp = client
        .put(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/octet-stream")
        .body(data.to_vec())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    (status, v)
}

async fn get_text(client: &reqwest::Client, url: String, token: &str) -> (u16, String) {
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

async fn get(client: &reqwest::Client, fx: &Fx, token: &str, path: &str) -> (u16, Value) {
    call(client, reqwest::Method::GET, format!("{}{}", fx.base, path), token, None).await
}

async fn post(client: &reqwest::Client, fx: &Fx, token: &str, path: &str, body: Value) -> (u16, Value) {
    call(client, reqwest::Method::POST, format!("{}{}", fx.base, path), token, Some(body)).await
}

async fn put(client: &reqwest::Client, fx: &Fx, token: &str, path: &str, body: Value) -> (u16, Value) {
    call(client, reqwest::Method::PUT, format!("{}{}", fx.base, path), token, Some(body)).await
}

async fn del(client: &reqwest::Client, fx: &Fx, token: &str, path: &str) -> (u16, Value) {
    call(client, reqwest::Method::DELETE, format!("{}{}", fx.base, path), token, None).await
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

/// 走完整两阶段提交把 `bytes` 提交到 `path`，返回新修订号。
async fn commit_file(
    client: &reqwest::Client,
    fx: &Fx,
    token: &str,
    repo: &str,
    path: &str,
    bytes: &[u8],
) -> i64 {
    let hash = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    };
    let (st, _) = call_raw(
        client,
        format!("{}/api/v1/repos/{repo}/blobs/{hash}", fx.base),
        token,
        bytes,
    )
    .await;
    assert_eq!(st, 200, "上传 blob 应成功");

    // §5.2：先锁后提交（v0.4.17 起每个文件路径都必须由本人持锁）
    let (st, lk) = post(
        client,
        fx,
        token,
        &format!("/api/v1/repos/{repo}/locks"),
        json!({ "path": path }),
    )
    .await;
    assert_eq!(st, 200, "加锁 {path} 应成功: {lk}");

    let cid = uuid::Uuid::new_v4().to_string();
    let (st, prep) = post(
        client,
        fx,
        token,
        &format!("/api/v1/repos/{repo}/commit/prepare"),
        json!({
            "commit_id": cid,
            "base_rev": 0,
            "message": "m3 fixture",
            "changes": [{
                "path": path, "op": "add", "kind": "file",
                "blob_hash": hash, "size": bytes.len()
            }]
        }),
    )
    .await;
    assert_eq!(st, 200, "prepare 应成功: {prep}");
    let commit_token = prep["commit_token"].as_str().unwrap().to_string();
    let (st, done) = post(
        client,
        fx,
        token,
        &format!("/api/v1/repos/{repo}/commit"),
        json!({ "commit_id": cid, "commit_token": commit_token }),
    )
    .await;
    assert_eq!(st, 200, "commit 应成功: {done}");
    done["rev"].as_i64().unwrap()
}

// ============ 1. 系统管理员门禁 ============

#[tokio::test(flavor = "multi_thread")]
async fn admin_endpoints_require_sysadmin() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;
    let li = login(&client, &fx, "li").await;

    // 非管理员一律 403
    for p in [
        "/api/v1/admin/stats",
        "/api/v1/admin/audit",
        "/api/v1/admin/users",
        "/api/v1/admin/groups",
        "/api/v1/admin/settings",
        "/api/v1/admin/maintenance",
    ] {
        let (st, v) = get(&client, &fx, &li, p).await;
        assert_eq!(st, 403, "{p} 非管理员应 403，实际 {st} {v}");
        assert_eq!(code(&v), "PERMISSION_DENIED");
    }

    // 无 token → 401
    let resp = client.get(format!("{}/api/v1/admin/stats", fx.base)).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    // 管理员正常
    let (st, v) = get(&client, &fx, &admin, "/api/v1/admin/stats").await;
    assert_eq!(st, 200);
    assert!(v["repos"].is_number(), "stats 应含 repos: {v}");
    assert!(v["storage"]["dedup_ratio"].is_number(), "stats 应含存储收益: {v}");
    assert!(v["commit_trend"].is_array(), "stats 应含提交趋势: {v}");

    // 系统设置只读展示
    let (st, v) = get(&client, &fx, &admin, "/api/v1/admin/settings").await;
    assert_eq!(st, 200, "settings: {v}");
}

// ============ 2. 统计与审计 ============

#[tokio::test(flavor = "multi_thread")]
async fn stats_and_audit_export() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;

    let (st, v) = get(&client, &fx, &admin, "/api/v1/admin/audit?limit=5").await;
    assert_eq!(st, 200, "audit: {v}");
    assert!(v["items"].is_array());
    assert!(
        v["items"].as_array().unwrap().iter().any(|i| i["action"].as_str().unwrap().contains("login")),
        "审计应含登录动作: {v}"
    );

    // 按 action 过滤
    let (st, v) = get(&client, &fx, &admin, "/api/v1/admin/audit?action=auth.login&limit=50").await;
    assert_eq!(st, 200);
    assert!(
        v["items"].as_array().unwrap().iter().all(|i| i["action"] == "auth.login"),
        "过滤应只返回 auth.login: {v}"
    );

    // CSV 导出（带 BOM，供 Excel 直接打开不乱码）
    let (st, text) = get_text(
        &client,
        format!("{}/api/v1/admin/audit?limit=1000&format=csv", fx.base),
        &admin,
    )
    .await;
    assert_eq!(st, 200);
    assert!(text.starts_with('\u{feff}'), "CSV 应带 UTF-8 BOM");
    // 表头是中文（给运维直接看的），用"动作"列确认导出格式而非英文列名
    assert!(text.contains("动作"), "CSV 应含中文表头: {text}");
    assert!(text.contains("user.login"), "CSV 应含登录记录: {text}");

    // 用户维度筛选（用户名或 id 都可）
    let (st, v) = get(&client, &fx, &admin, &format!("/api/v1/admin/audit?user=admin&limit=10")).await;
    assert_eq!(st, 200, "按用户过滤: {v}");
}

// ============ 3. 用户与组 ============

#[tokio::test(flavor = "multi_thread")]
async fn user_and_group_management() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;

    // 建用户
    let (st, u) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/users",
        json!({ "username": "wang", "password": "wang-PW-123456", "display_name": "小王" }),
    )
    .await;
    assert_eq!(st, 201, "建用户: {u}");
    let uid = u["id"].as_i64().unwrap();
    assert!(uid > 0);

    // 重名 → 409
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/users",
        json!({ "username": "wang", "password": "wang-PW-123456" }),
    )
    .await;
    assert_eq!(st, 409, "重名应 409: {v}");

    // 弱密码 → 400
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/users",
        json!({ "username": "x", "password": "123" }),
    )
    .await;
    assert_eq!(st, 400, "弱密码应 400: {v}");

    // 改名 / 提管理员 / 禁用
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/users/{uid}"),
        json!({ "display_name": "老王", "is_admin": true }),
    )
    .await;
    assert_eq!(st, 200, "改用户: {v}");
    assert_eq!(v["updated"], true);

    // 重置密码 → 204
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/users/{uid}/password"),
        json!({ "new_password": "wang-PW-654321" }),
    )
    .await;
    assert_eq!(st, 204, "重置密码: {v}");
    // 新密码可登录
    let resp = client
        .post(format!("{}/api/v1/auth/login", fx.base))
        .json(&json!({ "username": "wang", "password": "wang-PW-654321" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "新密码应能登录");

    // 禁止把自己禁用/降权（防止把自己锁在系统外）
    let me = fx.admin;
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/users/{me}"),
        json!({ "disabled": true }),
    )
    .await;
    assert_eq!(st, 400, "禁用自己应 400: {v}");
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/users/{me}"),
        json!({ "is_admin": false }),
    )
    .await;
    assert_eq!(st, 400, "给自己降权应 400: {v}");

    // 不存在的用户 → 404
    let (st, _) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/users/999999",
        json!({ "display_name": "nope" }),
    )
    .await;
    assert_eq!(st, 404);

    // 组：建 / 重名 / 重名冲突
    let (st, g) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/groups",
        json!({ "name": "audio", "comment": "音频组" }),
    )
    .await;
    assert_eq!(st, 201, "建组: {g}");
    let gid = g["id"].as_i64().unwrap();
    let (st, v) = post(&client, &fx, &admin, "/api/v1/admin/groups", json!({ "name": "audio" })).await;
    assert_eq!(st, 409, "组重名应 409: {v}");
    let (st, v) = post(&client, &fx, &admin, "/api/v1/admin/groups", json!({ "name": "  " })).await;
    assert_eq!(st, 400, "空组名应 400: {v}");

    // 成员设置 / 读取
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/groups/{gid}/members"),
        json!({ "user_ids": [fx.li, fx.zhao] }),
    )
    .await;
    assert_eq!(st, 200, "设成员: {v}");
    assert_eq!(v["members"], 2);
    let (st, v) = get(&client, &fx, &admin, &format!("/api/v1/admin/groups/{gid}/members")).await;
    assert_eq!(st, 200);
    assert_eq!(v["items"].as_array().unwrap().len(), 2);

    // 被 ACL 引用 → 拒绝删除（否则规则会指向不存在的主体，权限静默失效）
    let (st, _) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({
            "path_prefix": "snd", "subject_type": "group", "subject_id": gid,
            "level": "read", "inherit": true
        }),
    )
    .await;
    assert_eq!(st, 200);
    let (st, v) = del(&client, &fx, &admin, &format!("/api/v1/admin/groups/{gid}")).await;
    assert_eq!(st, 409, "被引用的组应拒绝删除: {v}");
    assert_eq!(code(&v), "GROUP_IN_USE");

    // 删掉规则后即可删组
    let rule_id = {
        let (st, v) = get(&client, &fx, &admin, "/api/v1/admin/repos/assets/acl").await;
        assert_eq!(st, 200);
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["path_prefix"] == "snd")
            .unwrap()["id"]
            .as_i64()
            .unwrap()
    };
    let (st, _) = del(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/repos/assets/acl?id={rule_id}"),
    )
    .await;
    assert_eq!(st, 200);
    let (st, v) = del(&client, &fx, &admin, &format!("/api/v1/admin/groups/{gid}")).await;
    assert_eq!(st, 200, "解引用后可删组: {v}");

    // 删用户
    let (st, v) = del(&client, &fx, &admin, &format!("/api/v1/admin/users/{uid}")).await;
    assert_eq!(st, 200, "删用户: {v}");
}

// ============ 4. 仓库设置与删除 ============

#[tokio::test(flavor = "multi_thread")]
async fn repo_settings_and_delete() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;

    let (st, v) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/settings",
        json!({ "description": "美术资产" }),
    )
    .await;
    assert_eq!(st, 200, "改仓库设置: {v}");
    assert_eq!(v["updated"], true);

    let (st, v) = get(&client, &fx, &admin, "/api/v1/repos/assets/info").await;
    assert_eq!(st, 200);
    assert_eq!(v["description"], "美术资产", "设置应生效: {v}");
    // v0.4.17：lock_policy / needs_lock 已从响应里消失（不能只是"值不对"，而是不该存在）
    assert!(v.get("lock_policy").is_none(), "info 不应再下发 lock_policy: {v}");
    assert!(v.get("needs_lock").is_none(), "info 不应再下发 needs_lock: {v}");

    // 已废弃字段被**忽略**而不是报错（serde 默认忽略未知字段）
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/settings",
        json!({ "lock_policy": "loose", "needs_lock": "*.psd" }),
    )
    .await;
    assert_eq!(st, 200, "废弃字段应被忽略而非 400: {v}");

    // 建仓同样忽略 lock_policy（老客户端/老脚本不必同批升级）
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/repos",
        json!({ "name": "legacy-client", "lock_policy": "advisory" }),
    )
    .await;
    assert_eq!(st, 201, "建仓应忽略 lock_policy: {v}");
    assert!(v.get("lock_policy").is_none(), "建仓响应不该回 lock_policy: {v}");

    // 不存在的仓库 → 404
    let (st, _) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/nope/settings",
        json!({ "description": "x" }),
    )
    .await;
    assert_eq!(st, 404);

    // 删仓：先把 blob 引用清零再删，避免永久泄漏
    let rev = commit_file(&client, &fx, &admin, "assets", "del/p.bin", b"to be deleted").await;
    assert_eq!(rev, 1);
    let (st, v) = del(&client, &fx, &admin, "/api/v1/admin/repos/assets").await;
    assert_eq!(st, 200, "删仓: {v}");
    assert_eq!(v["deleted"], true);
    assert!(v["summary"]["changes_removed"].as_i64().unwrap() >= 1);

    // 再删一次 → 404
    let (st, _) = del(&client, &fx, &admin, "/api/v1/admin/repos/assets").await;
    assert_eq!(st, 404);
}

// ============ 5. ACL 矩阵 / 预览器 / 反查 ============

#[tokio::test(flavor = "multi_thread")]
async fn acl_matrix_preview_and_who() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;

    // 给 art 组在 secret 上加 read；再加一条屏障规则（everyone=none, inherit=false）
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({
            "path_prefix": "secret", "subject_type": "group", "subject_id": fx.art,
            "level": "read", "inherit": true
        }),
    )
    .await;
    assert_eq!(st, 200, "加组规则: {v}");
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({
            "path_prefix": "secret", "subject_type": "everyone",
            "level": "none", "inherit": false
        }),
    )
    .await;
    assert_eq!(st, 200, "加屏障规则: {v}");

    // 非法 level / subject_type → 400
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({ "path_prefix": "x", "subject_type": "everyone", "level": "godmode" }),
    )
    .await;
    assert_eq!(st, 400, "非法 level: {v}");
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({ "path_prefix": "x", "subject_type": "alien", "level": "read" }),
    )
    .await;
    assert_eq!(st, 400, "非法 subject_type: {v}");

    // 列表：带 subject_label 与 shadowed（冗余规则提示）
    let (st, list) = get(&client, &fx, &admin, "/api/v1/admin/repos/assets/acl").await;
    assert_eq!(st, 200);
    let items = list["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["subject_label"].is_string()), "应带 subject_label: {list}");
    assert!(items.iter().any(|i| i["shadowed"].is_boolean()), "应带 shadowed: {list}");

    // 预览器：li 在 secret 上既有"组 art read"也有"用户 li write"（fixture 为加锁而设），
    // 主体具体度更高的"用户"规则胜出（§4.2 pick 排序）→ write
    let (st, v) = get(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/repos/assets/acl/preview?user_id={}&path=secret/a.bin", fx.li),
    )
    .await;
    assert_eq!(st, 200, "预览器: {v}");
    assert_eq!(v["level"], "write", "用户规则应压过组规则: {v}");
    let steps = v["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "预览器应给出回溯步骤: {v}");
    assert!(steps.iter().any(|s| s["outcome"] == "hit"), "应有一层是命中: {v}");

    // 继承屏障（§4.2）：vault 层只有"design 组 read（屏障）"与"li 个人 read"，
    // zhao 两条都不命中 → 被屏障截断为 none，且**不再向父目录回溯**
    let (st, _) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({
            "path_prefix": "vault", "subject_type": "group", "subject_id": fx.design,
            "level": "read", "inherit": false
        }),
    )
    .await;
    assert_eq!(st, 200, "加屏障规则(vault): {v}");
    let (st, _) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({
            "path_prefix": "vault", "subject_type": "user", "subject_id": fx.li,
            "level": "read", "inherit": true
        }),
    )
    .await;
    assert_eq!(st, 200, "加个人规则(vault): {v}");

    let (st, v) = get(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/repos/assets/acl/preview?user_id={}&path=vault/a.bin", fx.zhao),
    )
    .await;
    assert_eq!(st, 200, "预览器(zhao@vault): {v}");
    assert_eq!(v["level"], "none", "屏障应截断为 none: {v}");
    // 注意：steps 从**最具体**的层开始（vault/a.bin 无规则 → skip），
    // 屏障出现在 vault 这一层，不能假设 steps[0] 就是 barrier。
    assert!(
        v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["outcome"] == "barrier"),
        "回溯路径上应出现屏障层: {v}"
    );

    let (st, v) = get(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/repos/assets/acl/preview?user_id={}&path=vault/a.bin", fx.li),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(v["level"], "read", "同层 li 命中个人规则: {v}");
    assert!(
        v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["outcome"] == "hit"),
        "li 应命中某层规则: {v}"
    );

    // 每一层的 outcome 只能是四种之一（前端 el-steps 依赖这个枚举）
    for uid in [fx.li, fx.zhao] {
        let (st, v) = get(
            &client,
            &fx,
            &admin,
            &format!("/api/v1/admin/repos/assets/acl/preview?user_id={uid}&path=secret/a.bin"),
        )
        .await;
        assert_eq!(st, 200, "预览器(uid={uid}): {v}");
        for s in v["steps"].as_array().unwrap() {
            let o = s["outcome"].as_str().unwrap();
            assert!(matches!(o, "hit" | "barrier" | "miss" | "skip"), "outcome 合法: {s}");
        }
    }

    // 系统管理员穿透一切（§4.2）
    let (st, v) = get(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/repos/assets/acl/preview?user_id={}&path=secret/a.bin", fx.admin),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(v["level"], "admin", "管理员应穿透: {v}");

    // 不存在的用户 → 404
    let (st, _) = get(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl/preview?user_id=999999&path=secret",
    )
    .await;
    assert_eq!(st, 404);

    // "谁有权限"反查：li 在 secret 上有直接规则（write ≥ read）所以会列出
    let (st, v) = get(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl/who?path=secret&level=read",
    )
    .await;
    assert_eq!(st, 200, "反查: {v}");
    let who = v["items"].as_array().unwrap();
    assert!(who.iter().any(|i| i["id"] == fx.li), "li 应在列表里: {v}");
    assert!(
        who.iter().all(|i| i["via"].as_str().unwrap_or("").len() > 0),
        "每条都要说明归属: {v}"
    );
    assert!(
        who.iter()
            .any(|i| i["via"] == "系统管理员穿透" && i["id"] == fx.admin),
        "管理员应标注穿透: {v}"
    );

    // 组归因（§4.2）：换一个 li 只有组规则、没有直接规则的路径
    let (st, _) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({ "path_prefix": "shared", "subject_type": "group", "subject_id": fx.art,
                "level": "write", "inherit": true }),
    )
    .await;
    assert_eq!(st, 200, "加组规则(shared): {v}");
    let (st, v) = get(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl/who?path=shared&level=write",
    )
    .await;
    assert_eq!(st, 200, "反查(shared): {v}");
    let who2 = v["items"].as_array().unwrap();
    let li_row = who2.iter().find(|i| i["id"] == fx.li).unwrap_or_else(|| panic!("li 应通过组拿到 write: {v}"));
    assert!(li_row["via"].as_str().unwrap().contains("组"), "应归因到组: {v}");
    assert!(
        !who2.iter().any(|i| i["id"] == fx.zhao),
        "zhao 不在 art 组，不应列出: {v}"
    );

    // 目录树（ACL 编辑器的目录下拉用）
    let (st, v) = get(&client, &fx, &admin, "/api/v1/admin/repos/assets/dirs").await;
    assert_eq!(st, 200, "dirs: {v}");
    assert!(v["items"].is_array());

    // 改规则级别（PUT 带 id）
    let rule_id = items
        .iter()
        .find(|r| r["path_prefix"] == "secret" && r["subject_type"] == "group")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let (st, v) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({ "id": rule_id, "level": "write" }),
    )
    .await;
    assert_eq!(st, 200, "改规则: {v}");
    let (st, list2) = get(&client, &fx, &admin, "/api/v1/admin/repos/assets/acl").await;
    assert_eq!(st, 200);
    assert_eq!(
        list2["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == rule_id)
            .unwrap()["level"],
        "write",
        "改完应立即生效: {list2}"
    );

    // 改不存在的规则 → 404
    let (st, _) = put(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/acl",
        json!({ "id": 999999, "level": "read" }),
    )
    .await;
    assert_eq!(st, 404);

    // 删规则
    let (st, v) = del(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/repos/assets/acl?id={rule_id}"),
    )
    .await;
    assert_eq!(st, 200, "删规则: {v}");
    let (st, _) = del(
        &client,
        &fx,
        &admin,
        &format!("/api/v1/admin/repos/assets/acl?id={rule_id}"),
    )
    .await;
    assert_eq!(st, 404, "重复删应 404");
}

// ============ 6. purge 全链路（§3.6） ============

#[tokio::test(flavor = "multi_thread")]
async fn purge_full_chain() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;
    let li = login(&client, &fx, "li").await;

    let rev = commit_file(&client, &fx, &admin, "assets", "secret/p.bin", b"purge me").await;
    assert_eq!(rev, 1);
    let (st, v) = get(&client, &fx, &admin, "/api/v1/repos/assets/tree?prefix=secret").await;
    assert_eq!(st, 200);
    assert_eq!(v["items"][0]["path"], "secret/p.bin");

    // 非系统管理员 → 403（§3.6：系统管理员 + 目录 admin 两者都要）
    let (st, v) = post(
        &client,
        &fx,
        &li,
        "/api/v1/admin/repos/assets/purge",
        json!({ "prefix": "secret", "reason": "我不是管理员", "confirm_name": "assets" }),
    )
    .await;
    assert_eq!(st, 403, "purge 需系统管理员: {v}");

    // confirm_name 必须等于仓库名（防误触）
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/purge",
        json!({ "prefix": "secret", "reason": "清除敏感数据", "confirm_name": "wrong-name" }),
    )
    .await;
    assert_eq!(st, 400, "confirm_name 不符应 400: {v}");

    // 原因必须 ≥4 字（进审计，必须能追溯是谁为什么删的）
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/purge",
        json!({ "prefix": "secret", "reason": "短", "confirm_name": "assets" }),
    )
    .await;
    assert_eq!(st, 400, "原因过短应 400: {v}");

    // 空前缀 → 400（要清空整个仓库请直接删仓）
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/purge",
        json!({ "prefix": "", "reason": "清空仓库", "confirm_name": "assets" }),
    )
    .await;
    assert_eq!(st, 400, "空前缀应 400: {v}");

    // 他人持锁 → 409 PURGE_BLOCKED（§3.6 决策：先强制解锁再清除）
    //
    // 注意：**提交前必须先持锁**（§5.2），所以上面那次提交让 admin 成了 `secret/p.bin`
    // 的持锁人。这里先由 admin 释放自己的锁（正常的"提交完就放锁"流程），
    // 再由 li 接管，才谈得上"被他人持锁"。
    let (st, v) = del(&client, &fx, &admin, "/api/v1/repos/assets/locks/secret/p.bin").await;
    assert_eq!(st, 200, "admin 释放自己的锁: {v}");
    let (st, v) = post(
        &client,
        &fx,
        &li,
        "/api/v1/repos/assets/locks",
        json!({ "path": "secret/p.bin" }),
    )
    .await;
    assert_eq!(st, 200, "li 加锁: {v}");
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/purge",
        json!({ "prefix": "secret", "reason": "清除敏感数据", "confirm_name": "assets" }),
    )
    .await;
    assert_eq!(st, 409, "他人持锁应拒绝: {v}");
    assert_eq!(code(&v), "PURGE_BLOCKED");

    // 强制解锁后即可清除
    let (st, v) = del(
        &client,
        &fx,
        &admin,
        "/api/v1/repos/assets/locks/secret/p.bin?break=true&reason=purge%E5%89%8D%E7%A0%B4%E9%94%81",
    )
    .await;
    assert_eq!(st, 200, "强制解锁: {v}");

    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/purge",
        json!({ "prefix": "secret", "reason": "冒烟清除敏感数据", "confirm_name": "assets" }),
    )
    .await;
    assert_eq!(st, 200, "purge: {v}");
    assert!(v["paths_removed"].as_i64().unwrap() >= 1, "应删掉路径: {v}");
    assert_eq!(v["revisions_affected"], 1, "应影响 r1: {v}");
    assert_eq!(v["locks_released"], 0, "锁已破，不应再释放: {v}");
    assert!(v["gc_scheduled_at"].is_string(), "应给出 GC 计划时间: {v}");

    // 路径如同从未存在过；修订本身仍保留（只抹路径，不抹历史）
    let (st, v) = get(&client, &fx, &admin, "/api/v1/repos/assets/tree?prefix=secret").await;
    assert_eq!(st, 200);
    assert_eq!(v["items"].as_array().unwrap().len(), 0, "purge 后子树应为空: {v}");
    let (st, v) = get(&client, &fx, &admin, "/api/v1/repos/assets/log").await;
    assert_eq!(st, 200);
    assert!(v["items"].as_array().unwrap().len() >= 1, "修订仍应保留: {v}");

    // 进审计
    let (st, v) = get(&client, &fx, &admin, "/api/v1/admin/audit?action=repo.purge&limit=5").await;
    assert_eq!(st, 200);
    assert!(v["items"].as_array().unwrap().len() >= 1, "purge 应进审计: {v}");
    // 注意字段名是单数 `detail`（与 audit_log 表列名一致）
    let detail = v["items"][0]["detail"].as_str().unwrap_or("").to_string();
    assert!(detail.contains("reason="), "审计应带原因，实际: {detail}");
    assert!(detail.contains("paths="), "审计应带影响面，实际: {detail}");
    assert!(detail.contains("blobs="), "审计应带回收统计，实际: {detail}");
}

// ============ 7. 维护与 GC（§3.7） ============

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_refcount_and_gc() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;

    let (st, v) = get(&client, &fx, &admin, "/api/v1/admin/maintenance").await;
    assert_eq!(st, 200, "maintenance: {v}");
    assert!(v["queued"].is_number());
    assert_eq!(v["grace_secs"], 86400, "撤销期应 24h: {v}");

    let _rev = commit_file(&client, &fx, &admin, "assets", "gc/a.bin", b"gc candidate").await;

    let (st, v) = post(&client, &fx, &admin, "/api/v1/admin/maintenance/rebuild-refcount", json!({})).await;
    assert_eq!(st, 200, "rebuild-refcount: {v}");
    assert!(v["blobs"].as_i64().unwrap() >= 1, "应有 blob 被统计: {v}");

    // purge 触发的队列项 grace=0，可立即回收；宽限期内（24h）的项不删
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/repos/assets/purge",
        json!({ "prefix": "gc", "reason": "清除可回收数据", "confirm_name": "assets" }),
    )
    .await;
    assert_eq!(st, 200, "purge(gc): {v}");

    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/admin/maintenance/gc",
        json!({ "limit": 100 }),
    )
    .await;
    assert_eq!(st, 200, "gc: {v}");
    assert!(v["deleted"].as_i64().unwrap() >= 1, "purge 项应立即可回收: {v}");
    assert!(v["skipped_pending"].is_number(), "应复查 pending_commits: {v}");
}

// ============ 8. 静态托管（§8.1） ============

#[tokio::test(flavor = "multi_thread")]
async fn admin_static_hosting() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    std::fs::create_dir_all(data.join("uploads")).unwrap();
    let web = data.join("web");
    std::fs::create_dir_all(web.join("assets")).unwrap();
    std::fs::write(web.join("index.html"), r#"<div id="app"></div><script src="/admin/assets/index-abc.js"></script>"#)
        .unwrap();
    std::fs::write(web.join("assets/index-abc.js"), b"console.log(1)").unwrap();

    {
        let mut conn = db::open(&data.join("b-artifact.db")).unwrap();
        db::migrate(&mut conn).unwrap();
    }
    let blob = BlobStore::new(&data).unwrap();
    let pool = DbPool::new(&data.join("b-artifact.db"), 4).unwrap();
    let mut cfg = Config::default();
    cfg.server.admin_dir = web.to_string_lossy().to_string();
    let state = SharedState(Arc::new(AppState::new(pool, blob, cfg, data.join("uploads"))));
    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // 根与深链都回退到 index.html（Vue Router history 模式）
    let resp = client.get(format!("{base}/admin/")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let cc = resp.headers().get("cache-control").unwrap().to_str().unwrap().to_string();
    let html = resp.text().await.unwrap();
    assert!(html.contains(r#"<div id="app">"#));
    assert_eq!(cc, "no-cache", "index.html 必须不缓存，否则发版后前端不更新");

    let resp = client.get(format!("{base}/admin/repos/demo/acl")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "深链应回退");
    assert!(resp.text().await.unwrap().contains(r#"<div id="app">"#));

    let resp = client.get(format!("{base}/admin")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "无尾斜杠也应 200");

    // 带 hash 的资源长缓存
    let resp = client.get(format!("{base}/admin/assets/index-abc.js")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"].to_str().unwrap().contains("javascript"));
    assert!(resp.headers()["cache-control"].to_str().unwrap().contains("immutable"));

    // 不存在的资源 → 404（不能回退成 HTML，否则前端 404 变 200）
    let resp = client.get(format!("{base}/admin/assets/nope.js")).send().await.unwrap();
    assert_eq!(resp.status(), 404);

    // 路径穿越 → 400
    let resp = client
        .get(format!("{base}/admin/../../etc/passwd"))
        .header("Content-Length", "0")
        .send()
        .await
        .unwrap();
    assert!(resp.status() == 400 || resp.status() == 404, "穿越应被拒，实际 {}", resp.status());

    // `/` 重定向到管理端（reqwest 默认跟随重定向，必须用不跟随的客户端才能看到 307）
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = no_redirect.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(resp.status(), 307);
    assert_eq!(resp.headers()["location"], "/admin/");
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_static_missing_dist_hint() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    std::fs::create_dir_all(data.join("uploads")).unwrap();
    {
        let mut conn = db::open(&data.join("b-artifact.db")).unwrap();
        db::migrate(&mut conn).unwrap();
    }
    let blob = BlobStore::new(&data).unwrap();
    let pool = DbPool::new(&data.join("b-artifact.db"), 4).unwrap();
    let mut cfg = Config::default();
    cfg.server.admin_dir = data.join("not-built").to_string_lossy().to_string();
    let state = SharedState(Arc::new(AppState::new(pool, blob, cfg, data.join("uploads"))));
    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = reqwest::Client::new();
    // 未构建时给出可操作的提示，而不是干巴巴的 404
    let resp = client.get(format!("http://{addr}/admin/")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let html = resp.text().await.unwrap();
    assert!(html.contains("npm run build"), "应提示构建命令: {html}");
}
