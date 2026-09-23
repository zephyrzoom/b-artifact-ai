//! M2 端到端集成测试：ACL（§4）与锁（§5）。
//!
//! 与 `api_m1.rs` 的基建刻意独立：M2 需要预置用户组、strict 仓库、多档权限等场景，
//! 塞进 M1 的 `spawn_server` 会让两边都变复杂。
//!
//! 覆盖：
//! - ACL 继承 / 就近覆盖 / **继承屏障**（§4.2）
//! - 读路径（info / tree / log / changes）按权限过滤；提交逐路径校验 write（§4.3）
//! - 配置目录权限需要本目录 admin（§4.3）
//! - 锁：file/dir 递归互斥、本人幂等、强制解锁留痕与 reason 必填、子树连带强制解锁、心跳续期（§5）
//! - strict 仓库 `needs_lock` 未持锁拒绝提交（§5.2）

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
    /// 无人的组，用于演示屏障（li/zhao 都不命中）
    design: i64,
    _dir: TempDir,
}

fn pw_of(name: &str) -> String {
    format!("{name}-PW-123456")
}

/// 预置：
/// - 用户 admin（系统管理员）/ li（art 组）/ zhao（无组）
/// - 仓库 `assets`（基线 everyone:read）
/// - 仓库 `game`（根 everyone:write —— 锁测试用，让 li/zhao 都能加锁）
/// - 仓库 `locked`（锁测试用；v0.4.17 起所有仓库都是"先锁后提交"，不再有策略差异）
///
/// 注：`needs_lock` 列保留但已废弃（§5.2），fixture 不再设置它。
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

        // 仓库
        repo::create_repo(&conn, "assets", admin).unwrap();
        let game = repo::create_repo(&conn, "game", admin).unwrap();
        let locked = repo::create_repo(&conn, "locked", admin).unwrap();
        // game：根整体放开 write，锁测试不必再纠结目录权限
        acl::upsert_rule(&conn, game, "", Subject::Everyone, Level::Write, true).unwrap();
        // locked：strict + psd 必须持锁
        conn.execute(
            "UPDATE repos SET needs_lock = '*.psd' WHERE id = ?1",
            rusqlite::params![locked],
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

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut r = client.request(method, url).header("Authorization", auth(token));
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    (status, v)
}

/// 发原始字节体的请求（blob 上传走 `PUT /blobs/{hash}`，body 必须是文件原文，
/// 不能走 `call()` 的 JSON 分支，否则服务端算出的 sha256 与 URL 声明的不一致 → 412）。
async fn call_raw(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    data: &[u8],
) -> (u16, Value) {
    let resp = client
        .request(method, url)
        .header("Authorization", auth(token))
        .header("Content-Type", "application/octet-stream")
        .body(data.to_vec())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    (status, v)
}

/// `changes` 端点返回 NDJSON（不是 JSON 数组），需要按文本读。
async fn get_text(client: &reqwest::Client, fx: &Fx, token: &str, path: &str) -> (u16, String) {
    let resp = client
        .get(format!("{}{}", fx.base, path))
        .header("Authorization", auth(token))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

async fn get(client: &reqwest::Client, fx: &Fx, token: &str, path: &str) -> (u16, Value) {
    call(client, reqwest::Method::GET, format!("{}{}", fx.base, path), token, None).await
}

async fn post(
    client: &reqwest::Client,
    fx: &Fx,
    token: &str,
    path: &str,
    body: Value,
) -> (u16, Value) {
    call(client, reqwest::Method::POST, format!("{}{}", fx.base, path), token, Some(body)).await
}

async fn del(client: &reqwest::Client, fx: &Fx, token: &str, path: &str) -> (u16, Value) {
    call(client, reqwest::Method::DELETE, format!("{}{}", fx.base, path), token, None).await
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

fn uuid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

/// prepare 一次提交（M2 的权限/锁校验都发生在 prepare 阶段）。
/// 用固定的假 blob hash——prepare 只把它记进 `need_blobs`，不要求已就位。
async fn prepare(
    client: &reqwest::Client,
    fx: &Fx,
    token: &str,
    repo: &str,
    tag: u64,
    base_rev: i64,
    path: &str,
) -> (u16, Value) {
    let body = json!({
        "commit_id": uuid(tag),
        "base_rev": base_rev,
        "message": "m2",
        "changes": [{ "path": path, "op": "add", "kind": "file",
                      "blob_hash": "0".repeat(64), "size": 1 }]
    });
    post(client, fx, token, &format!("/api/v1/repos/{repo}/commit/prepare"), body).await
}

/// 真实写入一个文件：上传 blob → prepare → commit，返回新 rev。
async fn add_file(
    client: &reqwest::Client,
    fx: &Fx,
    token: &str,
    repo: &str,
    path: &str,
    data: &[u8],
    tag: u64,
    base_rev: i64,
) -> i64 {
    let hash = sha256_hex(data);
    let (st, v) = call_raw(
        client,
        reqwest::Method::PUT,
        format!("{}/api/v1/repos/{repo}/blobs/{hash}", fx.base),
        token,
        data,
    )
    .await;
    assert_eq!(st, 200, "上传 blob {path} 应成功：{v}");
    // §5.2：先锁后提交
    let (st, v) = lock(client, fx, token, repo, path).await;
    assert_eq!(st, 200, "加锁 {path} 应成功：{v}");
    let (st, v) = post(
        client,
        fx,
        token,
        &format!("/api/v1/repos/{repo}/commit/prepare"),
        json!({
            "commit_id": uuid(tag),
            "base_rev": base_rev,
            "message": "m2",
            "changes": [{ "path": path, "op": "add", "kind": "file",
                          "blob_hash": hash, "size": data.len() }]
        }),
    )
    .await;
    assert_eq!(st, 200, "prepare {path} 应成功：{v}");
    let tok = v["commit_token"].as_str().unwrap().to_string();
    let (st, v) = post(
        client,
        fx,
        token,
        &format!("/api/v1/repos/{repo}/commit"),
        json!({ "commit_id": uuid(tag), "commit_token": tok }),
    )
    .await;
    assert_eq!(st, 200, "commit {path} 应成功：{v}");
    v["rev"].as_i64().unwrap()
}

/// 配置一条 ACL 规则。
async fn acl_rule(
    client: &reqwest::Client,
    fx: &Fx,
    token: &str,
    repo: &str,
    prefix: &str,
    subject: &str,
    subject_id: i64,
    level: &str,
    inherit: bool,
) -> (u16, Value) {
    post(
        client,
        fx,
        token,
        &format!("/api/v1/admin/repos/{repo}/acl"),
        json!({
            "path_prefix": prefix,
            "subject_type": subject,
            "subject_id": subject_id,
            "level": level,
            "inherit": inherit,
        }),
    )
    .await
}

async fn lock(
    client: &reqwest::Client,
    fx: &Fx,
    token: &str,
    repo: &str,
    path: &str,
) -> (u16, Value) {
    // v0.4.17：锁只剩文件级，请求不再带 kind
    post(
        client,
        fx,
        token,
        &format!("/api/v1/repos/{repo}/locks"),
        json!({ "path": path, "comment": "m2" }),
    )
    .await
}

// ============ ACL：继承 / 就近覆盖 / 屏障 ============

#[tokio::test(flavor = "multi_thread")]
async fn acl_inheritance_cover_and_barrier() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;
    let li = login(&client, &fx, "li").await;
    let zhao = login(&client, &fx, "zhao").await;

    // art 目录：art 组可写
    let (st, _) =
        acl_rule(&client, &fx, &admin, "assets", "art", "group", fx.art, "write", true).await;
    assert_eq!(st, 200);
    // 屏障：art/secret 只给 design 组，且 inherit=false —— 未命中者不向父目录回溯（§4.2）
    let (st, _) = acl_rule(
        &client, &fx, &admin, "assets", "art/secret", "group", fx.design, "read", false,
    )
    .await;
    assert_eq!(st, 200);
    // docs 目录：zhao 是管理员，可在其下自行配置权限
    let (st, _) =
        acl_rule(&client, &fx, &admin, "assets", "docs", "user", fx.zhao, "admin", true).await;
    assert_eq!(st, 200);

    // --- li（art 组）---
    let (st, v) = get(&client, &fx, &li, "/api/v1/repos/assets/info").await;
    assert_eq!(st, 200);
    assert_eq!(v["my_permissions"]["read"], json!(true));
    assert_eq!(v["my_permissions"]["write"], json!(false), "li 在仓库根只有 read");

    // art：可列、可提交
    let (st, _) = get(&client, &fx, &li, "/api/v1/repos/assets/tree?prefix=art").await;
    assert_eq!(st, 200);
    let (st, _) = lock(&client, &fx, &li, "assets", "art/mob.psd").await;
    assert_eq!(st, 200, "li 在 art 下有 write，可以加锁");
    let (st, _) = prepare(&client, &fx, &li, "assets", 1, 0, "art/mob.psd").await;
    assert_eq!(st, 200, "li 在 art 下有 write");

    // art/secret：屏障阻断，连 read 都没有
    let (st, v) = get(&client, &fx, &li, "/api/v1/repos/assets/tree?prefix=art/secret").await;
    assert_eq!(st, 403);
    assert_eq!(code(&v), "PERMISSION_DENIED");
    let (st, v) = prepare(&client, &fx, &li, "assets", 2, 0, "art/secret/boss.psd").await;
    assert_eq!(st, 403);
    assert_eq!(code(&v), "PERMISSION_DENIED");

    // --- zhao（无组）---
    // art：回落根的 everyone:read，可读不可写
    let (st, _) = get(&client, &fx, &zhao, "/api/v1/repos/assets/tree?prefix=art").await;
    assert_eq!(st, 200, "zhao 经继承拿到根的 read");
    let (st, v) = prepare(&client, &fx, &zhao, "assets", 3, 0, "art/mob.psd").await;
    assert_eq!(st, 403);
    assert_eq!(code(&v), "PERMISSION_DENIED");

    // art/secret：同样被屏障挡住
    let (st, _) = get(&client, &fx, &zhao, "/api/v1/repos/assets/tree?prefix=art/secret").await;
    assert_eq!(st, 403);

    // docs：zhao 是 admin，可配置其子目录
    let (st, _) = acl_rule(
        &client, &fx, &zhao, "assets", "docs/internal", "everyone", 0, "read", true,
    )
    .await;
    assert_eq!(st, 200, "zhao 在 docs 上是 admin，可配置其子目录");
    // 但管不到 art
    let (st, v) =
        acl_rule(&client, &fx, &zhao, "assets", "art", "everyone", 0, "read", true).await;
    assert_eq!(st, 403);
    assert_eq!(code(&v), "PERMISSION_DENIED");

    // li 不能配置任何目录权限（根只有 read）
    let (st, _) = acl_rule(&client, &fx, &li, "assets", "art", "everyone", 0, "read", true).await;
    assert_eq!(st, 403);

    // 规则列表：管理员看全部；li 在任何目录都没有 admin → 空
    let (st, v) = get(&client, &fx, &admin, "/api/v1/admin/repos/assets/acl").await;
    assert_eq!(st, 200);
    assert!(v["total"].as_i64().unwrap() >= 4, "基线 + 3 条新规则，实际 {}", v["total"]);
    let (st, v) = get(&client, &fx, &li, "/api/v1/admin/repos/assets/acl").await;
    assert_eq!(st, 200);
    assert_eq!(v["total"], json!(0), "li 在任何目录都没有 admin");
}

// ============ 读路径按权限过滤 ============

#[tokio::test(flavor = "multi_thread")]
async fn read_paths_are_filtered_by_acl() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;
    let li = login(&client, &fx, "li").await;

    // secret 只对 design 组开放，且是屏障（li 不在 design → 完全不可见）
    let (st, _) =
        acl_rule(&client, &fx, &admin, "assets", "secret", "group", fx.design, "read", false).await;
    assert_eq!(st, 200);

    let r1 = add_file(&client, &fx, &admin, "assets", "art/a.psd", b"art", 11, 0).await;
    let r2 = add_file(&client, &fx, &admin, "assets", "secret/b.txt", b"sec", 12, r1).await;
    assert_eq!((r1, r2), (1, 2));

    // tree 根：li 看不到 secret（按权限过滤，§7.2）
    let (st, v) = get(&client, &fx, &li, "/api/v1/repos/assets/tree?depth=1").await;
    assert_eq!(st, 200);
    let paths: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["path"].as_str())
        .collect();
    assert!(paths.contains(&"art"), "art 应对 li 可见：{paths:?}");
    assert!(!paths.contains(&"secret"), "secret 不应对 li 可见：{paths:?}");

    // 直接请求无权限前缀 → 403
    let (st, _) = get(&client, &fx, &li, "/api/v1/repos/assets/tree?prefix=secret").await;
    assert_eq!(st, 403);

    // log / changes 同样要求 read
    let (st, _) = get(&client, &fx, &li, "/api/v1/repos/assets/log?prefix=art").await;
    assert_eq!(st, 200);
    let (st, _) = get(&client, &fx, &li, "/api/v1/repos/assets/log?prefix=secret").await;
    assert_eq!(st, 403);
    let (st, _) = get(&client, &fx, &li, "/api/v1/repos/assets/changes?from=1&to=2&prefix=secret")
        .await;
    assert_eq!(st, 403);
    // changes 里也不该出现越权路径（NDJSON，逐行 JSON）
    let (st, text) = get_text(&client, &fx, &li, "/api/v1/repos/assets/changes?from=1&to=2").await;
    assert_eq!(st, 200);
    assert!(text.contains("art/a.psd"), "应有可读路径的变更：{text}");
    assert!(!text.contains("secret/b.txt"), "越权路径的变更不得外泄");

    // 管理员穿透一切
    let (st, _) = get(&client, &fx, &admin, "/api/v1/repos/assets/tree?prefix=secret").await;
    assert_eq!(st, 200);
    let (st, v) = get(&client, &fx, &admin, "/api/v1/repos").await;
    assert_eq!(st, 200);
    let names: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert_eq!(names, vec!["assets", "game", "locked"], "系统管理员看到全部仓库");
}

// ============ 锁：互斥（同一路径）/ 幂等 / 路径作用域 / 强制解锁 / 续期 ============

#[tokio::test(flavor = "multi_thread")]
async fn lock_conflicts_are_path_scoped_and_break_is_single() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;
    let li = login(&client, &fx, "li").await;
    let zhao = login(&client, &fx, "zhao").await;

    // li 锁住一个文件
    let (st, v) = lock(&client, &fx, &li, "game", "art/x.psd").await;
    assert_eq!(st, 200);
    assert_eq!(v["token"].as_str().unwrap().len(), 64);
    assert_eq!(v["kind"], json!("file"), "v0.4.17：锁只剩文件级");

    // 本人重复加锁 → 幂等
    let (st, v2) = lock(&client, &fx, &li, "game", "art/x.psd").await;
    assert_eq!(st, 200, "本人重复加锁应幂等");
    assert_eq!(v["id"], v2["id"]);

    // 他人锁同一文件 → 409 LOCKED，relation=self
    let (st, v) = lock(&client, &fx, &zhao, "game", "art/x.psd").await;
    assert_eq!(st, 409);
    assert_eq!(code(&v), "LOCKED");
    // 冲突清单在 `details.locks`（`locks::conflicts_json`），path 是**仓库内相对路径**
    assert_eq!(v["error"]["details"]["locks"][0]["relation"], json!("self"));
    assert_eq!(v["error"]["details"]["locks"][0]["owner"], json!("li"));
    assert_eq!(v["error"]["details"]["locks"][0]["path"], json!("art/x.psd"));

    // v0.4.17：锁只作用于**精确路径**。同一目录下的另一个文件、别的子树都不受影响
    // （dir 锁整体删除之后，"路径区间相交"那套判定不复存在）
    let (st, _) = lock(&client, &fx, &zhao, "game", "art/y.psd").await;
    assert_eq!(st, 200, "同目录的另一个文件不受影响（没有目录锁了）");
    let (st, _) = lock(&client, &fx, &li, "game", "art/li2.psd").await;
    assert_eq!(st, 200);

    // 他人提交被 li 持锁的路径 → 409 NEEDS_LOCK，并点名持锁人
    let (st, v) = prepare(&client, &fx, &zhao, "game", 21, 0, "art/x.psd").await;
    assert_eq!(st, 409);
    assert_eq!(code(&v), "NEEDS_LOCK");
    assert_eq!(v["error"]["details"]["paths"][0], json!("art/x.psd"));
    assert_eq!(v["error"]["details"]["locked_by"][0]["owner"], json!("li"));
    // 持锁人自己可以提交
    let (st, _) = prepare(&client, &fx, &li, "game", 22, 0, "art/x.psd").await;
    assert_eq!(st, 200);

    // 落一个真实提交，让 `docs` 成为目录；随后对它加锁应被拒（目录不是变更路径，§5.2）
    let rev = add_file(&client, &fx, &admin, "game", "docs/readme.md", b"readme", 23, 0).await;
    assert_eq!(rev, 1);
    let (st, v) = lock(&client, &fx, &zhao, "game", "docs").await;
    assert_eq!(st, 400, "目录路径不能加锁：{v}");
    assert_eq!(code(&v), "INVALID_ARGUMENT");
    // 但 docs 下的**新文件**是普通路径，可以加锁 —— 不会有人"占着目录"挡别人
    let (st, _) = lock(&client, &fx, &zhao, "game", "docs/new.md").await;
    assert_eq!(st, 200);

    // 本人释放
    let (st, v) = del(&client, &fx, &zhao, "/api/v1/repos/game/locks/art/y.psd").await;
    assert_eq!(st, 200);
    assert_eq!(v["released"], json!(true));

    // 强制解锁：reason 必填（§5.5 审计必填）
    let (st, v) = del(&client, &fx, &admin, "/api/v1/repos/game/locks/art/x.psd?break=true").await;
    assert_eq!(st, 400, "强制解锁必须填 reason");
    assert_eq!(code(&v), "INVALID_ARGUMENT");

    // 非 admin 强制解锁 → 403（li 在 game 上只有 everyone:write，不是任何目录的 admin）
    let (st, v) = del(
        &client,
        &fx,
        &li,
        "/api/v1/repos/game/locks/art/x.psd?break=true&reason=takeover",
    )
    .await;
    assert_eq!(st, 403);
    assert_eq!(code(&v), "PERMISSION_DENIED");

    // admin 强制解锁 → 留痕
    let (st, v) = del(
        &client,
        &fx,
        &admin,
        "/api/v1/repos/game/locks/art/x.psd?break=true&reason=left company",
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(v["broken"], json!(1));
    let (st, v) = get(&client, &fx, &admin, "/api/v1/repos/game/locks?include_broken=true").await;
    assert_eq!(st, 200);
    let broken = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"] == "art/x.psd")
        .expect("强制解锁后仍应留痕（include_broken=true）");
    assert_eq!(broken["break_reason"], json!("left company"));
    assert!(broken["broken_at"].is_string());
    assert_eq!(broken["broken_by_name"], json!("admin"));
    assert_eq!(broken["broken_by"], json!(fx.admin), "强制解锁人应记在 broken_by");
    assert_eq!(broken["owner_id"], json!(fx.li), "被破的锁仍记着原持锁人");

    // 一次强制解锁只影响一个路径（原"破 dir 锁连带释放整棵子树"已删除）
    let (st, _) = lock(&client, &fx, &zhao, "game", "docs/a.md").await;
    assert_eq!(st, 200);
    let (st, v) = del(
        &client,
        &fx,
        &admin,
        "/api/v1/repos/game/locks/docs/a.md?break=true&reason=cleanup",
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(v["broken"], json!(1), "只破这一个路径");
    let (st, v) = get(&client, &fx, &admin, "/api/v1/repos/game/locks").await;
    assert_eq!(st, 200);
    let live: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["path"].as_str())
        .collect();
    assert!(
        live.contains(&"art/li2.psd"),
        "相邻路径上的锁不应被牵连：{live:?}"
    );
    assert!(!live.contains(&"docs/a.md"), "被破的锁不再有效");

    // 强制解锁后其他人可以重新加锁
    let (st, _) = lock(&client, &fx, &li, "game", "docs/a.md").await;
    assert_eq!(st, 200);

    // 心跳续期：返回新的绝对到期时间
    let (st, v) =
        post(&client, &fx, &li, "/api/v1/repos/game/locks/refresh", json!({ "ttl_secs": 3600 }))
            .await;
    assert_eq!(st, 200);
    assert!(v["refreshed"].as_i64().unwrap() >= 1);
    assert!(v["expires_at"].as_str().unwrap().contains('T'));
}

// ============ 先锁后提交（v0.4.17：所有仓库、所有文件路径） ============

#[tokio::test(flavor = "multi_thread")]
async fn commit_requires_own_lock_on_every_path() {
    let fx = spawn().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &fx, "admin").await;

    // 未持锁提交 → 409 NEEDS_LOCK
    let (st, v) = prepare(&client, &fx, &admin, "locked", 31, 0, "hero.psd").await;
    assert_eq!(st, 409);
    assert_eq!(code(&v), "NEEDS_LOCK");
    assert_eq!(v["error"]["details"]["paths"][0], json!("hero.psd"));
    assert_eq!(v["error"]["details"]["locked_by"], json!([]), "没人持锁时不该乱点名");

    // **没有 glob 白名单**：文本文件同样要求持锁（旧的 needs_lock='*.psd' 语义已删除）
    let (st, v) = prepare(&client, &fx, &admin, "locked", 32, 0, "notes.txt").await;
    assert_eq!(st, 409, "v0.4.17：任何文件路径都要持锁");
    assert_eq!(code(&v), "NEEDS_LOCK");

    // 加锁后再提交 → 通过
    let (st, _) = lock(&client, &fx, &admin, "locked", "hero.psd").await;
    assert_eq!(st, 200);
    let (st, _) = prepare(&client, &fx, &admin, "locked", 33, 0, "hero.psd").await;
    assert_eq!(st, 200);

    // 部分持锁、部分没锁 → 整单拒绝，且**只点名未持锁的那一个**
    let (st, v) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/repos/locked/commit/prepare",
        json!({
            "commit_id": uuid(34), "base_rev": 0, "message": "混合",
            "changes": [
                { "path": "hero.psd", "op": "add", "kind": "file",
                  "blob_hash": "0".repeat(64), "size": 1 },
                { "path": "boss.psd", "op": "add", "kind": "file",
                  "blob_hash": "0".repeat(64), "size": 1 }
            ]
        }),
    )
    .await;
    assert_eq!(st, 409);
    assert_eq!(code(&v), "NEEDS_LOCK");
    assert_eq!(v["error"]["details"]["paths"], json!(["boss.psd"]));

    // 纯目录变更不是变更路径 → 不参与锁校验
    let (st, _) = post(
        &client,
        &fx,
        &admin,
        "/api/v1/repos/locked/commit/prepare",
        json!({
            "commit_id": uuid(35), "base_rev": 0, "message": "目录",
            "changes": [{ "path": "dir1", "op": "add", "kind": "dir" }]
        }),
    )
    .await;
    assert_eq!(st, 200, "目录条目不需要锁");
}
