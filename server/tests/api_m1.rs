//! M1 端到端集成测试：真实 axum 路由（随机端口）+ reqwest HTTP 客户端。
//!
//! 覆盖：认证流、仓库管理与权限、整块/分块上传、两阶段提交（幂等重放 /
//! OUT_OF_DATE / 路径校验）、下载（Range / HEAD）、tree / log / changes。

use b_artifact_server::api;
use b_artifact_server::auth::password::hash_password;
use b_artifact_server::config::Config;
use b_artifact_server::state::{AppState, SharedState};
use b_artifact_server::storage::{blob::BlobStore, db, pool::DbPool};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

// ============ 测试基建 ============

struct TestServer {
    base: String,
    /// 保活数据目录（drop 即清理）
    _dir: TempDir,
}

/// 起一个随机端口的服务端，预置 admin（管理员）与 member（普通用户）两个本地账号。
async fn spawn_server() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    std::fs::create_dir_all(data.join("uploads")).unwrap();
    let mut conn = db::open(&data.join("b-artifact.db")).unwrap();
    db::migrate(&mut conn).unwrap();
    insert_user(&conn, "admin", "admin-PW-123456", true);
    insert_user(&conn, "member", "member-PW-123456", false);
    drop(conn);

    let blob = BlobStore::new(&data).unwrap();
    let db = DbPool::new(&data.join("b-artifact.db"), 4).unwrap();
    let state = SharedState(Arc::new(AppState::new(
        db,
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
    TestServer { base: format!("http://{addr}"), _dir: dir }
}

fn insert_user(conn: &rusqlite::Connection, name: &str, pw: &str, is_admin: bool) {
    conn.execute(
        "INSERT INTO users (username, source, password_hash, is_admin, disabled, created_at)
         VALUES (?1, 'local', ?2, ?3, 0, ?4)",
        rusqlite::params![
            name,
            hash_password(pw).unwrap(),
            is_admin as i64,
            chrono::Utc::now().to_rfc3339()
        ],
    )
    .unwrap();
}

/// 登录并返回 Bearer token（失败即 panic）。
async fn login(client: &reqwest::Client, base: &str, user: &str, pw: &str) -> String {
    let resp = client
        .post(format!("{base}/api/v1/auth/login"))
        .json(&json!({ "username": user, "password": pw }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "登录 {user} 应成功");
    let body: Value = resp.json().await.unwrap();
    body["token"].as_str().expect("响应含 token").to_string()
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

/// 不可压缩数据（xorshift64，与 blob.rs 单测同款思路）→ 存 raw，支持 Range。
fn incompressible(n: usize) -> Vec<u8> {
    let mut s: u64 = 0x9E3779B97F4A7C15;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 32) as u8
        })
        .collect()
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

/// 简易 UUID 生成（测试用，v4 形态即可，无须密码学随机）。
fn uuid(n: u64) -> String {
    let hi = format!("{n:012x}");
    format!("00000000-0000-4000-8000-{hi}")
}

/// 造一个仓库（admin 身份），返回仓库名。
async fn make_repo(client: &reqwest::Client, base: &str, token: &str, name: &str) {
    let resp = client
        .post(format!("{base}/api/v1/repos"))
        .header("Authorization", auth(token))
        .json(&json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "建仓 {name} 应成功");
}

/// 批量加锁（v0.4.17：§5.2 要求提交前**每个文件**都由本人持锁）。
/// 同一路径重复加锁是幂等的，所以可以在一个用例里反复调。
async fn lock_paths(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    repo: &str,
    paths: &[&str],
) {
    for path in paths {
        let resp = client
            .post(format!("{base}/api/v1/repos/{repo}/locks"))
            .header("Authorization", auth(token))
            .json(&json!({ "path": path }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "加锁 {path} 应成功：{}", resp.status());
    }
}

/// 把已在仓库中的 blob 提交到 `path`（prepare → commit）。
/// M2 起下载要按"引用该 blob 的路径"做 read 校验，故下载前必须先让它被某路径引用。
async fn commit_blob(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    repo: &str,
    path: &str,
    data: &[u8],
    base_rev: i64,
    tag: u64,
) -> i64 {
    let hash = sha256_hex(data);
    let cid = uuid(tag);
    // §5.2：先锁后提交（本人持锁才允许提交）
    lock_paths(client, base, token, repo, &[path]).await;
    let resp: Value = client
        .post(format!("{base}/api/v1/repos/{repo}/commit/prepare"))
        .header("Authorization", auth(token))
        .json(&json!({
            "commit_id": cid, "base_rev": base_rev, "message": "测试提交",
            "changes": [{ "path": path, "op": "add", "kind": "file",
                          "blob_hash": hash, "size": data.len() }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tok = resp["commit_token"].as_str().unwrap().to_string();
    let resp: Value = client
        .post(format!("{base}/api/v1/repos/{repo}/commit"))
        .header("Authorization", auth(token))
        .json(&json!({ "commit_id": cid, "commit_token": tok }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["rev"].as_i64().expect("提交应返回 rev")
}

async fn err_code(resp: reqwest::Response) -> String {
    let body: Value = resp.json().await.unwrap();
    body["error"]["code"].as_str().unwrap_or("无错误码").to_string()
}

// ============ 认证流 ============

#[tokio::test(flavor = "multi_thread")]
async fn auth_flow() {
    let srv = spawn_server().await;
    let client = reqwest::Client::new();

    // providers：本地开、LDAP 关
    let resp: Value = client
        .get(format!("{}/api/v1/auth/providers", srv.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["local"], json!(true));
    assert_eq!(resp["ldap"], json!(false));

    // 未认证访问 → 401
    let resp = client
        .get(format!("{}/api/v1/auth/me", srv.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // 错误密码 → 401
    let resp = client
        .post(format!("{}/api/v1/auth/login", srv.base))
        .json(&json!({ "username": "admin", "password": "wrong-password" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // 正确登录 → token + is_admin
    let resp = client
        .post(format!("{}/api/v1/auth/login", srv.base))
        .json(&json!({ "username": "admin", "password": "admin-PW-123456" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["is_admin"], json!(true));
    let token = body["token"].as_str().unwrap().to_string();

    // me
    let resp: Value = client
        .get(format!("{}/api/v1/auth/me", srv.base))
        .header("Authorization", auth(&token))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["username"], json!("admin"));

    // logout → 吊销当前会话（204 No Content）
    let resp = client
        .post(format!("{}/api/v1/auth/logout", srv.base))
        .header("Authorization", auth(&token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let resp = client
        .get(format!("{}/api/v1/auth/me", srv.base))
        .header("Authorization", auth(&token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "logout 后原 token 应失效");
}

// ============ 仓库管理与权限 ============

#[tokio::test(flavor = "multi_thread")]
async fn repo_management_and_permissions() {
    let srv = spawn_server().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &srv.base, "admin", "admin-PW-123456").await;
    let member = login(&client, &srv.base, "member", "member-PW-123456").await;

    // 非 admin 建仓 → 403
    let resp = client
        .post(format!("{}/api/v1/repos", srv.base))
        .header("Authorization", auth(&member))
        .json(&json!({ "name": "member-repo" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // admin 建仓 → 201
    make_repo(&client, &srv.base, &admin, "assets").await;

    // 重名 → 409 NAME_COLLISION
    let resp = client
        .post(format!("{}/api/v1/repos", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "name": "assets" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(err_code(resp).await, "NAME_COLLISION");

    // 非法仓库名 → 400
    let resp = client
        .post(format!("{}/api/v1/repos", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "name": "../bad" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 列表：member 可见（M1 读 = 任意已登录用户），my_role=member
    let resp: Value = client
        .get(format!("{}/api/v1/repos", srv.base))
        .header("Authorization", auth(&member))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["total"], json!(1));
    assert_eq!(resp["items"][0]["name"], json!("assets"));
    assert_eq!(resp["items"][0]["my_role"], json!("member"));

    // info：head_rev=0，member 只读
    let resp: Value = client
        .get(format!("{}/api/v1/repos/assets/info", srv.base))
        .header("Authorization", auth(&member))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["head_rev"], json!(0));
    assert_eq!(resp["my_permissions"]["read"], json!(true));
    assert_eq!(resp["my_permissions"]["write"], json!(false));

    // 不存在的仓库 → 404
    let resp = client
        .get(format!("{}/api/v1/repos/nope/info", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// ============ 整块上传与下载 ============

#[tokio::test(flavor = "multi_thread")]
async fn blob_upload_download() {
    let srv = spawn_server().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &srv.base, "admin", "admin-PW-123456").await;
    let member = login(&client, &srv.base, "member", "member-PW-123456").await;
    make_repo(&client, &srv.base, &admin, "assets").await;

    // 不可压缩 100KB → raw 存储
    let data = incompressible(100 * 1024);
    let hash = sha256_hex(&data);

    // missing：应报缺失
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/blobs/missing", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "hashes": [hash] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["missing"].as_array().unwrap().len(), 1);

    // member 无写权限 → 403
    let resp = client
        .put(format!("{}/api/v1/repos/assets/blobs/{hash}", srv.base))
        .header("Authorization", auth(&member))
        .body(data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // admin 上传 → 200，codec=raw
    let resp: Value = client
        .put(format!("{}/api/v1/repos/assets/blobs/{hash}", srv.base))
        .header("Authorization", auth(&admin))
        .body(data.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["codec"], json!("raw"));
    assert_eq!(resp["already_present"], json!(false));

    // 重传同内容 → 去重
    let resp: Value = client
        .put(format!("{}/api/v1/repos/assets/blobs/{hash}", srv.base))
        .header("Authorization", auth(&admin))
        .body(data.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["already_present"], json!(true));

    // 内容 hash 不符（合法 hex 但声明错）→ 412
    let wrong = "0".repeat(64);
    let resp = client
        .put(format!("{}/api/v1/repos/assets/blobs/{wrong}", srv.base))
        .header("Authorization", auth(&admin))
        .body(data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 412);
    assert_eq!(err_code(resp).await, "HASH_MISMATCH");

    // 非法 hash（非 hex）→ 400
    let resp = client
        .put(format!("{}/api/v1/repos/assets/blobs/{}", srv.base, "x".repeat(64)))
        .header("Authorization", auth(&admin))
        .body(data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // M2：只有被某条可读路径引用的 blob 才能下载 —— 先提交进去
    let rev = commit_blob(&client, &srv.base, &admin, "assets", "raw.bin", &data, 0, 9001).await;
    assert_eq!(rev, 1);

    // 下载：member 可读，字节一致
    let bytes = client
        .get(format!("{}/api/v1/repos/assets/blobs/{hash}", srv.base))
        .header("Authorization", auth(&member))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&bytes[..], &data[..]);

    // HEAD：200 + Content-Length
    let resp = client
        .head(format!("{}/api/v1/repos/assets/blobs/{hash}", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-length").unwrap().to_str().unwrap(),
        (100 * 1024).to_string()
    );

    // Range 0-15 → 206 + 前缀内容（raw blob）
    let resp = client
        .get(format!("{}/api/v1/repos/assets/blobs/{hash}", srv.base))
        .header("Authorization", auth(&admin))
        .header("Range", "bytes=0-15")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        &"bytes 0-15/102400".parse::<axum::http::HeaderValue>().unwrap()
    );
    let part = resp.bytes().await.unwrap();
    assert_eq!(&part[..], &data[..16]);

    // missing：已就位 → 空
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/blobs/missing", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "hashes": [hash] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["missing"].as_array().unwrap().len(), 0);

    // 可压缩内容 → zstd 存储，取回透明解压一致
    let compressible = vec![b'z'; 64 * 1024];
    let chash = sha256_hex(&compressible);
    let resp: Value = client
        .put(format!("{}/api/v1/repos/assets/blobs/{chash}", srv.base))
        .header("Authorization", auth(&admin))
        .body(compressible.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["codec"], json!("zstd"));
    commit_blob(&client, &srv.base, &admin, "assets", "z.bin", &compressible, 1, 9002).await;
    let bytes = client
        .get(format!("{}/api/v1/repos/assets/blobs/{chash}", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&bytes[..], &compressible[..]);

    // 不存在的 blob 下载 → 404
    let resp = client
        .get(format!("{}/api/v1/repos/assets/blobs/{}", srv.base, "9".repeat(64)))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// ============ 两阶段提交全生命周期 ============

#[tokio::test(flavor = "multi_thread")]
async fn two_phase_commit_lifecycle() {
    let srv = spawn_server().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &srv.base, "admin", "admin-PW-123456").await;
    let member = login(&client, &srv.base, "member", "member-PW-123456").await;
    make_repo(&client, &srv.base, &admin, "assets").await;

    let data = incompressible(4096);
    let hash = sha256_hex(&data);
    let cid1 = uuid(1);

    // member 无写权限 prepare → 403
    let resp = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&member))
        .json(&json!({
            "commit_id": uuid(99), "base_rev": 0, "message": "x",
            "changes": [{ "path": "a.bin", "op": "add", "kind": "file",
                          "blob_hash": hash, "size": data.len() }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // 阶段一（blob 未传）→ need_blobs 含 hash
    lock_paths(&client, &srv.base, &admin, "assets", &["dir1/a.bin"]).await;
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({
            "commit_id": cid1, "base_rev": 0, "message": "首次提交",
            "changes": [{ "path": "dir1/a.bin", "op": "add", "kind": "file",
                          "blob_hash": hash, "size": data.len() }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["need_blobs"].as_array().unwrap().len(), 1);
    assert_eq!(resp["need_blobs"][0], json!(hash));
    let token1 = resp["commit_token"].as_str().unwrap().to_string();
    assert!(!token1.is_empty());

    // blob 未就位直接 commit → 400
    let resp = client
        .post(format!("{}/api/v1/repos/assets/commit", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "commit_id": cid1, "commit_token": token1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "need_blobs 未就位应拒绝提交");

    // 补传 blob 后重 prepare（同 commit_id 幂等，token 刷新）
    let resp: Value = client
        .put(format!("{}/api/v1/repos/assets/blobs/{hash}", srv.base))
        .header("Authorization", auth(&admin))
        .body(data.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["hash"], json!(hash));
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({
            "commit_id": cid1, "base_rev": 0, "message": "首次提交",
            "changes": [{ "path": "dir1/a.bin", "op": "add", "kind": "file",
                          "blob_hash": hash, "size": data.len() }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["need_blobs"].as_array().unwrap().len(), 0);
    let token1 = resp["commit_token"].as_str().unwrap().to_string();

    // 阶段二 → rev 1
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "commit_id": cid1, "commit_token": token1 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["rev"], json!(1));
    assert_eq!(resp["replayed"], json!(false));

    // 幂等重放（网络重试安全）：commit 与 prepare 双双幂等
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "commit_id": cid1, "commit_token": token1 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["rev"], json!(1));
    assert_eq!(resp["replayed"], json!(true));
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({
            "commit_id": cid1, "base_rev": 1, "message": "重放",
            "changes": [{ "path": "dir1/a.bin", "op": "modify", "kind": "file",
                          "blob_hash": hash, "size": data.len() }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["replayed"], json!(true));
    assert_eq!(resp["rev"], json!(1));

    // 过期 base_rev → 409 OUT_OF_DATE
    let resp = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({
            "commit_id": uuid(2), "base_rev": 0, "message": "stale",
            "changes": [{ "path": "x.bin", "op": "add", "kind": "file",
                          "blob_hash": hash, "size": 1 }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(err_code(resp).await, "OUT_OF_DATE");

    // 浏览：tree / log / changes / info
    let resp: Value = client
        .get(format!("{}/api/v1/repos/assets/tree?prefix=dir1", srv.base))
        .header("Authorization", auth(&member))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["items"][0]["path"], json!("dir1/a.bin"));
    assert_eq!(resp["items"][0]["blob_hash"], json!(hash));

    let resp: Value = client
        .get(format!("{}/api/v1/repos/assets/log", srv.base))
        .header("Authorization", auth(&member))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["total"], json!(1));
    assert_eq!(resp["items"][0]["rev"], json!(1));
    assert_eq!(resp["items"][0]["author"], json!("admin"));
    assert_eq!(resp["items"][0]["message"], json!("首次提交"));

    let text = client
        .get(format!("{}/api/v1/repos/assets/changes?from=1&to=1", srv.base))
        .header("Authorization", auth(&member))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(first["op"], json!("add"));
    assert_eq!(first["path"], json!("dir1/a.bin"));

    let resp: Value = client
        .get(format!("{}/api/v1/repos/assets/info", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["head_rev"], json!(1));
    assert_eq!(resp["stats"]["file_count"], json!(1));
    assert_eq!(resp["stats"]["total_size"], json!(data.len()));

    // 第二次提交（modify 换内容）→ rev 2
    let data2 = incompressible(2048);
    let hash2 = sha256_hex(&data2);
    let _ = client
        .put(format!("{}/api/v1/repos/assets/blobs/{hash2}", srv.base))
        .header("Authorization", auth(&admin))
        .body(data2.clone())
        .send()
        .await
        .unwrap();
    let cid3 = uuid(3);
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({
            "commit_id": cid3, "base_rev": 1, "message": "改内容",
            "changes": [{ "path": "dir1/a.bin", "op": "modify", "kind": "file",
                          "blob_hash": hash2, "size": data2.len() }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token3 = resp["commit_token"].as_str().unwrap().to_string();
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "commit_id": cid3, "commit_token": token3 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["rev"], json!(2));

    // 第三次提交：删除 + 加目录 → rev 3
    let cid4 = uuid(4);
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({
            "commit_id": cid4, "base_rev": 2, "message": "删除与目录",
            "changes": [
                { "path": "dir1/a.bin", "op": "delete", "kind": "file" },
                { "path": "dir2", "op": "add", "kind": "dir" }
            ]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token4 = resp["commit_token"].as_str().unwrap().to_string();
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "commit_id": cid4, "commit_token": token4 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["rev"], json!(3));

    // HEAD 树（depth=2）：dir1 唯一文件已删且无目录行 → 消失；仅剩 dir2
    let resp: Value = client
        .get(format!("{}/api/v1/repos/assets/tree?depth=2", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let paths: Vec<&str> = resp["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["dir2"]);

    // 历史树 rev=1 仍可查
    let resp: Value = client
        .get(format!("{}/api/v1/repos/assets/tree?rev=1&prefix=dir1", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["items"][0]["path"], json!("dir1/a.bin"));
    assert_eq!(resp["items"][0]["blob_hash"], json!(hash));

    // 空提交 → 400
    let resp = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "commit_id": uuid(5), "base_rev": 3, "message": "empty", "changes": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 路径穿越 → 400 INVALID_PATH
    let resp = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({
            "commit_id": uuid(6), "base_rev": 3, "message": "bad",
            "changes": [{ "path": "../escape", "op": "add", "kind": "dir" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(err_code(resp).await, "INVALID_PATH");

    // 非法 commit_id → 400
    let resp = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({
            "commit_id": "not-a-uuid", "base_rev": 3, "message": "bad",
            "changes": [{ "path": "ok", "op": "add", "kind": "dir" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 坏 commit_token（新 commit_id）→ 409 COMMIT_TOKEN_EXPIRED
    let resp = client
        .post(format!("{}/api/v1/repos/assets/commit", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "commit_id": uuid(7), "commit_token": "deadbeef" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(err_code(resp).await, "COMMIT_TOKEN_EXPIRED");
}

// ============ 分块上传 ============

#[tokio::test(flavor = "multi_thread")]
async fn chunked_upload_flow() {
    let srv = spawn_server().await;
    let client = reqwest::Client::new();
    let admin = login(&client, &srv.base, "admin", "admin-PW-123456").await;
    make_repo(&client, &srv.base, &admin, "assets").await;

    // 200KB 随机数据，64KB 一块 → 4 块（最后一块 8KB）
    let data = incompressible(200 * 1024);
    let hash = sha256_hex(&data);
    let chunk = 64 * 1024usize;

    // 创建会话
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/blobs/uploads", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "hash": hash, "size": data.len(), "chunk_size": chunk }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let upid = resp["upload_id"].as_str().unwrap().to_string();

    // 乱序上传 2,0,3,1（最后一块不足 64KB）
    for n in [2usize, 0, 3, 1] {
        let start = n * chunk;
        let end = ((n + 1) * chunk).min(data.len());
        let bytes = &data[start..end];
        let resp = client
            .put(format!("{}/api/v1/repos/assets/blobs/uploads/{upid}/{n}", srv.base))
            .header("Authorization", auth(&admin))
            .header("X-Chunk-SHA256", sha256_hex(bytes))
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204, "分块 {n} 上传应成功");
    }

    // 错误块号越界 → 400
    let resp = client
        .put(format!("{}/api/v1/repos/assets/blobs/uploads/{upid}/4", srv.base))
        .header("Authorization", auth(&admin))
        .body(vec![0u8; chunk])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 断点查询：4 块齐
    let resp: Value = client
        .get(format!("{}/api/v1/repos/assets/blobs/uploads/{upid}", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["received_chunks"], json!([0, 1, 2, 3]));

    // 结算：hash 一致
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/blobs/uploads/{upid}/complete", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["hash"], json!(hash));

    // 会话已清理 → 再查 404
    let resp = client
        .get(format!("{}/api/v1/repos/assets/blobs/uploads/{upid}", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // 下载校验 + 提交入库
    let bytes = client
        .get(format!("{}/api/v1/repos/assets/blobs/{hash}", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&bytes[..], &data[..]);

    let cid = uuid(1);
    lock_paths(&client, &srv.base, &admin, "assets", &["big.bin"]).await;
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit/prepare", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({
            "commit_id": cid, "base_rev": 0, "message": "大文件",
            "changes": [{ "path": "big.bin", "op": "add", "kind": "file",
                          "blob_hash": hash, "size": data.len() }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = resp["commit_token"].as_str().unwrap().to_string();
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/commit", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "commit_id": cid, "commit_token": token }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["rev"], json!(1));

    // 第二个会话：只传部分 → complete 报缺块；DELETE 中止 → 204
    let data_b = incompressible(3 * 1024);
    let hash_b = sha256_hex(&data_b);
    let resp: Value = client
        .post(format!("{}/api/v1/repos/assets/blobs/uploads", srv.base))
        .header("Authorization", auth(&admin))
        .json(&json!({ "hash": hash_b, "size": data_b.len(), "chunk_size": 1024 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let upid2 = resp["upload_id"].as_str().unwrap().to_string();

    let _ = client
        .put(format!("{}/api/v1/repos/assets/blobs/uploads/{upid2}/0", srv.base))
        .header("Authorization", auth(&admin))
        .body(data_b[..1024].to_vec())
        .send()
        .await
        .unwrap();
    let resp = client
        .post(format!("{}/api/v1/repos/assets/blobs/uploads/{upid2}/complete", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "缺块结算应失败");
    let body: Value = resp.json().await.unwrap();
    let missing: Vec<i64> = body["error"]["details"]["missing_chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(missing, vec![1, 2]);

    let resp = client
        .delete(format!("{}/api/v1/repos/assets/blobs/uploads/{upid2}", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let resp = client
        .get(format!("{}/api/v1/repos/assets/blobs/uploads/{upid2}", srv.base))
        .header("Authorization", auth(&admin))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "中止后查询应 404");
}
