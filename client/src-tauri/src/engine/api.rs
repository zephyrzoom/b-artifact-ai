/**
 * 服务端 REST 客户端（§7 传输协议）。
 *
 * 三条硬规则来自 §7.1：
 *   1. 分支判断只看 `code`，不看 HTTP 状态、不看 message；
 *   2. 只有"可安全重试"的操作才重试（GET/HEAD、PUT blob、分块 PUT、POST /commit）；
 *   3. 429 / 503 按 `Retry-After` 退避，5xx 指数退避最多 3 次。
 *
 * 传输层是可注入的 `Transport`（对应 TS 版的 `fetchImpl` 注入）：单测用桩替换即可，
 * 不必起真实服务端；`ReqwestTransport` 是生产实现，下载走流式落盘。
 */
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::engine::errors::{ClientError, ClientResult};

pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_RETRIES: u32 = 3;
/// 先问后传的单批上限（§7.2）。
const MISSING_BATCH: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// HTTP 传输抽象（生产 = reqwest，测试 = 桩）。
pub trait Transport: Send + Sync {
    fn send<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, ClientResult<HttpResponse>>;

    /// 下载到文件（大文件不驻留内存）。
    fn download<'a>(&'a self, req: HttpRequest, dest: &'a Path) -> BoxFuture<'a, ClientResult<u64>>;
}

// ---------- DTO（字段名与 TS 版逐一对应，均为 snake_case 线上格式） ----------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct UserInfo {
    pub id: i64,
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub is_admin: bool,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LoginResult {
    pub token: String,
    pub expires_at: String,
    pub user: UserInfo,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub admin: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RepoSummary {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub head_rev: i64,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub my_role: String,
    #[serde(default)]
    pub my_permissions: Permissions,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TreeEntry {
    pub path: String,
    pub kind: String,
    #[serde(default)]
    pub blob_hash: Option<String>,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub mode: i64,
    #[serde(default)]
    pub mtime: i64,
    #[serde(default)]
    pub changed_rev: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TreeResult {
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub rev: i64,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub depth: i64,
    #[serde(default)]
    pub items: Vec<TreeEntry>,
    #[serde(default)]
    pub total: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogEntry {
    pub rev: i64,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub file_count: i64,
    #[serde(default)]
    pub byte_delta: i64,
    #[serde(default)]
    pub manifest_hash: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ChangeRow {
    #[serde(default)]
    pub rev: i64,
    pub path: String,
    pub op: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub blob_hash: Option<String>,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub mode: i64,
    #[serde(default)]
    pub mtime: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ChangeSpec {
    pub path: String,
    pub op: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PrepareResult {
    #[serde(default)]
    pub commit_id: String,
    /// 幂等重放时为 true，且带 rev、无 token。
    #[serde(default)]
    pub replayed: bool,
    #[serde(default)]
    pub rev: Option<i64>,
    #[serde(default)]
    pub commit_token: Option<String>,
    #[serde(default)]
    pub need_blobs: Option<Vec<String>>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommitResult {
    #[serde(default)]
    pub commit_id: String,
    pub rev: i64,
    #[serde(default)]
    pub replayed: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UploadSession {
    pub upload_id: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub chunk_size: i64,
    #[serde(default)]
    pub received_chunks: Vec<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LockInfo {
    #[serde(default)]
    pub id: i64,
    pub path: String,
    /// v0.4.17：服务端仍回这个字段，但**恒为 `file`**（§5.2 只剩文件锁）。
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub owner_id: i64,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// 百分比编码（等价 encodeURIComponent）。
fn enc_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        let keep = c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')');
        if keep {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 路径段编码：分段编码、保留 `/`（§7.1 路径本身保持 posix）。
fn enc_path(s: &str) -> String {
    s.split('/')
        .map(enc_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// query 值编码（等价 URLSearchParams：空格用 `+`）。
fn enc_query(s: &str) -> String {
    enc_component(s).replace("%20", "+")
}

/// 解析 `Retry-After`（秒数或 HTTP 日期）。
fn parse_retry_after(v: Option<&str>) -> Option<u64> {
    let v = v?.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(secs) = v.parse::<f64>() {
        if secs.is_finite() {
            return Some((secs * 1000.0).max(0.0) as u64);
        }
    }
    // HTTP 日期：形如 "Wed, 21 Oct 2015 07:28:00 GMT"
    parse_http_date(v).map(|t| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        ((t - now).max(0) as u64) * 1000
    })
}

/// 只支持 IMF-fixdate（服务端只会发这个）。
fn parse_http_date(s: &str) -> Option<i64> {
    // "Wed, 21 Oct 2015 07:28:00 GMT"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<&str> = parts[4].split(':').collect();
    if hms.len() != 3 {
        return None;
    }
    let (h, m, sec): (i64, i64, i64) = (
        hms[0].parse().ok()?,
        hms[1].parse().ok()?,
        hms[2].parse().ok()?,
    );
    Some(days_from_civil(year, month, day) * 86400 + h * 3600 + m * 60 + sec)
}

/// 公历 → 自 1970-01-01 起的天数（Howard Hinnant 的算法）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub struct ApiClient {
    pub base_url: String,
    token: Option<String>,
    transport: std::sync::Arc<dyn Transport>,
    timeout_ms: u64,
    max_retries: u32,
}

#[derive(Default, Clone)]
pub struct RequestOptions {
    pub query: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub headers: Vec<(String, String)>,
    /// 该请求是否可安全重试（§7.1「重试语义」）。缺省 = GET/HEAD 才重试。
    pub idempotent: Option<bool>,
    /// 原始二进制请求体（决定 Content-Type）。**必须显式给**：
    /// 用"首字节是不是 `{`"来猜会把恰好以 `{` 开头的二进制文件误判成 JSON。
    pub raw: bool,
}

impl ApiClient {
    pub fn new(base_url: &str, token: Option<String>, transport: std::sync::Arc<dyn Transport>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            transport,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_retries: MAX_RETRIES,
        }
    }

    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    pub fn set_token(&mut self, t: Option<String>) {
        self.token = t;
    }

    pub fn token_value(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// 传输层句柄（按 meta 重建 client 时用：token 换掉、底层 HTTP 实现不变）。
    pub fn transport(&self) -> std::sync::Arc<dyn Transport> {
        self.transport.clone()
    }

    fn url(&self, path: &str, query: &[(String, String)]) -> String {
        let mut u = format!("{}/api/v1{}", self.base_url, path);
        let qs: Vec<String> = query
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{}={}", enc_query(k), enc_query(v)))
            .collect();
        if !qs.is_empty() {
            u.push('?');
            u.push_str(&qs.join("&"));
        }
        u
    }

    fn headers(&self, extra: &[(String, String)]) -> Vec<(String, String)> {
        let mut h: Vec<(String, String)> = extra.to_vec();
        if let Some(t) = &self.token {
            h.push(("Authorization".to_string(), format!("Bearer {t}")));
        }
        h
    }

    fn to_error(resp: &HttpResponse) -> ClientError {
        let mut code = "UNKNOWN".to_string();
        let mut message = format!("HTTP {}", resp.status);
        let mut details = None;
        if !resp.body.is_empty() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&resp.body) {
                if let Some(err) = v.get("error") {
                    if let Some(c) = err.get("code").and_then(|c| c.as_str()) {
                        code = c.to_string();
                    }
                    if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
                        message = m.to_string();
                    }
                    details = err.get("details").filter(|d| !d.is_null()).cloned();
                }
            }
        }
        let mut e = ClientError::api(resp.status, code, message);
        e.details = details;
        if let Some(ms) = parse_retry_after(resp.header("retry-after")) {
            e = e.with_retry_after(ms);
        }
        e
    }

    /// 发一次请求（含重试）。返回原始响应。
    pub async fn request(
        &self,
        method: &'static str,
        path: &str,
        opts: RequestOptions,
    ) -> ClientResult<HttpResponse> {
        let retryable = opts
            .idempotent
            .unwrap_or(method == "GET" || method == "HEAD");
        let mut last: Option<ClientError> = None;

        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let backoff = std::cmp::min(2000u64, 200u64 * 2u64.pow(attempt - 1));
                let ms = last
                    .as_ref()
                    .and_then(|e| e.retry_after_ms)
                    .unwrap_or(backoff);
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }

            let body = opts.body.clone();
            let has_ct = opts
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
            let content_type = if has_ct {
                None
            } else if opts.raw {
                Some((
                    "Content-Type".to_string(),
                    "application/octet-stream".to_string(),
                ))
            } else {
                Some((
                    "Content-Type".to_string(),
                    "application/json; charset=utf-8".to_string(),
                ))
            };

            let mut headers = self.headers(&opts.headers);
            if let Some(h) = content_type {
                headers.push(h);
            }

            let req = HttpRequest {
                method,
                url: self.url(path, &opts.query),
                headers,
                body,
            };

            let fut = self.transport.send(req);
            let result = tokio::time::timeout(Duration::from_millis(self.timeout_ms), fut).await;

            let outcome = match result {
                Err(_) => Err(ClientError::new("NETWORK", "请求超时")),
                Ok(Err(e)) => Err(e),
                Ok(Ok(resp)) => {
                    if (200..300).contains(&resp.status) {
                        return Ok(resp);
                    }
                    Err(Self::to_error(&resp))
                }
            };

            match outcome {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if !retryable || !e.retryable() {
                        return Err(e);
                    }
                    last = Some(e);
                }
            }
        }

        Err(last.unwrap_or_else(|| ClientError::new("UNKNOWN", "请求失败")))
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        path: &str,
        opts: RequestOptions,
    ) -> ClientResult<T> {
        let resp = self.request(method, path, opts).await?;
        if resp.body.is_empty() {
            return serde_json::from_slice(b"{}")
                .map_err(|e| ClientError::new("INTERNAL", e.to_string()));
        }
        serde_json::from_slice(&resp.body).map_err(|e| ClientError::new("INTERNAL", e.to_string()))
    }

    fn json_body(body: &impl Serialize) -> Option<Vec<u8>> {
        Some(serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec()))
    }

    // ---------- 认证（§7.2） ----------

    pub async fn login(&mut self, username: &str, password: &str) -> ClientResult<LoginResult> {
        let body = serde_json::json!({ "username": username, "password": password });
        let r: LoginResult = self
            .json(
                "POST",
                "/auth/login",
                RequestOptions {
                    body: Self::json_body(&body),
                    idempotent: Some(false),
                    ..Default::default()
                },
            )
            .await?;
        self.token = Some(r.token.clone());
        Ok(r)
    }

    pub async fn me(&self) -> ClientResult<UserInfo> {
        self.json("GET", "/auth/me", RequestOptions::default()).await
    }

    pub async fn logout(&mut self) -> ClientResult<()> {
        self.request(
            "POST",
            "/auth/logout",
            RequestOptions {
                idempotent: Some(true),
                ..Default::default()
            },
        )
        .await?;
        self.token = None;
        Ok(())
    }

    // ---------- 仓库与浏览 ----------

    pub async fn list_repos(&self) -> ClientResult<Vec<RepoSummary>> {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            items: Vec<RepoSummary>,
        }
        let r: Wrap = self.json("GET", "/repos", RequestOptions::default()).await?;
        Ok(r.items)
    }

    pub async fn repo_info(&self, repo: &str) -> ClientResult<serde_json::Value> {
        self.json(
            "GET",
            &format!("/repos/{}/info", enc_component(repo)),
            RequestOptions::default(),
        )
        .await
    }

    pub async fn tree(
        &self,
        repo: &str,
        rev: Option<i64>,
        prefix: Option<&str>,
        depth: Option<i64>,
    ) -> ClientResult<TreeResult> {
        self.json(
            "GET",
            &format!("/repos/{}/tree", enc_component(repo)),
            RequestOptions {
                query: vec![
                    ("rev".into(), rev.unwrap_or(0).to_string()),
                    ("prefix".into(), prefix.unwrap_or("").to_string()),
                    ("depth".into(), depth.unwrap_or(1).to_string()),
                ],
                ..Default::default()
            },
        )
        .await
    }

    pub async fn log(
        &self,
        repo: &str,
        prefix: Option<&str>,
        from: Option<i64>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> ClientResult<(Vec<LogEntry>, i64)> {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            items: Vec<LogEntry>,
            #[serde(default)]
            total: i64,
        }
        let mut query = Vec::new();
        if let Some(p) = prefix {
            query.push(("prefix".to_string(), p.to_string()));
        }
        if let Some(f) = from {
            query.push(("from".to_string(), f.to_string()));
        }
        if let Some(l) = limit {
            query.push(("limit".to_string(), l.to_string()));
        }
        if let Some(o) = offset {
            query.push(("offset".to_string(), o.to_string()));
        }
        let r: Wrap = self
            .json(
                "GET",
                &format!("/repos/{}/log", enc_component(repo)),
                RequestOptions {
                    query,
                    ..Default::default()
                },
            )
            .await?;
        Ok((r.items, r.total))
    }

    /// 增量变更（JSONL 流式，§7.1：大响应不走分页信封）。返回按 (rev, path) 升序的原始行。
    pub async fn changes(
        &self,
        repo: &str,
        from: i64,
        to: Option<i64>,
        prefix: Option<&str>,
    ) -> ClientResult<Vec<ChangeRow>> {
        let mut query = vec![("from".to_string(), from.to_string())];
        if let Some(t) = to {
            query.push(("to".to_string(), t.to_string()));
        }
        if let Some(p) = prefix {
            query.push(("prefix".to_string(), p.to_string()));
        }
        let resp = self
            .request(
                "GET",
                &format!("/repos/{}/changes", enc_component(repo)),
                RequestOptions {
                    query,
                    ..Default::default()
                },
            )
            .await?;
        let text = String::from_utf8_lossy(&resp.body);
        let mut out = Vec::new();
        for line in text.split('\n') {
            let s = line.trim();
            if s.is_empty() {
                continue;
            }
            let row: ChangeRow = serde_json::from_str(s)
                .map_err(|e| ClientError::new("INTERNAL", format!("changes 行解析失败：{e}")))?;
            out.push(row);
        }
        Ok(out)
    }

    // ---------- 传输（§7.2） ----------

    /// 先问后传：返回服务端缺失的 hash（一次最多 1000 个）。
    pub async fn blobs_missing(&self, repo: &str, hashes: &[String]) -> ClientResult<Vec<String>> {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            missing: Vec<String>,
        }
        let mut out = Vec::new();
        for chunk in hashes.chunks(MISSING_BATCH) {
            let body = serde_json::json!({ "hashes": chunk });
            let r: Wrap = self
                .json(
                    "POST",
                    &format!("/repos/{}/blobs/missing", enc_component(repo)),
                    RequestOptions {
                        body: Self::json_body(&body),
                        idempotent: Some(true),
                        ..Default::default()
                    },
                )
                .await?;
            out.extend(r.missing);
        }
        Ok(out)
    }

    /// 整块上传（< 64 MB，幂等）。
    pub async fn put_blob(&self, repo: &str, hash: &str, data: Vec<u8>) -> ClientResult<()> {
        self.request(
            "PUT",
            &format!("/repos/{}/blobs/{}", enc_component(repo), hash),
            RequestOptions {
                body: Some(data),
                idempotent: Some(true),
                raw: true,
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    pub async fn create_upload(
        &self,
        repo: &str,
        hash: &str,
        size: i64,
        chunk_size: i64,
    ) -> ClientResult<UploadSession> {
        let body = serde_json::json!({ "hash": hash, "size": size, "chunk_size": chunk_size });
        self.json(
            "POST",
            &format!("/repos/{}/blobs/uploads", enc_component(repo)),
            RequestOptions {
                body: Self::json_body(&body),
                idempotent: Some(false),
                ..Default::default()
            },
        )
        .await
    }

    pub async fn put_chunk(
        &self,
        repo: &str,
        upload_id: &str,
        n: i64,
        data: Vec<u8>,
    ) -> ClientResult<()> {
        self.request(
            "PUT",
            &format!(
                "/repos/{}/blobs/uploads/{}/{}",
                enc_component(repo),
                enc_component(upload_id),
                n
            ),
            RequestOptions {
                body: Some(data),
                idempotent: Some(true),
                raw: true,
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    pub async fn upload_status(&self, repo: &str, upload_id: &str) -> ClientResult<UploadSession> {
        self.json(
            "GET",
            &format!(
                "/repos/{}/blobs/uploads/{}",
                enc_component(repo),
                enc_component(upload_id)
            ),
            RequestOptions::default(),
        )
        .await
    }

    pub async fn complete_upload(&self, repo: &str, upload_id: &str) -> ClientResult<()> {
        self.request(
            "POST",
            &format!(
                "/repos/{}/blobs/uploads/{}/complete",
                enc_component(repo),
                enc_component(upload_id)
            ),
            RequestOptions {
                body: Some(b"{}".to_vec()),
                idempotent: Some(false),
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    pub async fn abort_upload(&self, repo: &str, upload_id: &str) -> ClientResult<()> {
        self.request(
            "DELETE",
            &format!(
                "/repos/{}/blobs/uploads/{}",
                enc_component(repo),
                enc_component(upload_id)
            ),
            RequestOptions {
                idempotent: Some(true),
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    /// 下载 blob 原始字节（小文件）。
    pub async fn download_blob(&self, repo: &str, hash: &str) -> ClientResult<Vec<u8>> {
        let resp = self
            .request(
                "GET",
                &format!("/repos/{}/blobs/{}", enc_component(repo), hash),
                RequestOptions::default(),
            )
            .await?;
        Ok(resp.body)
    }

    /// 下载到文件（大文件走这里，避免整块驻留内存）。
    pub async fn download_blob_to(
        &self,
        repo: &str,
        hash: &str,
        dest: &Path,
    ) -> ClientResult<u64> {
        let req = HttpRequest {
            method: "GET",
            url: self.url(
                &format!("/repos/{}/blobs/{}", enc_component(repo), hash),
                &[],
            ),
            headers: self.headers(&[]),
            body: None,
        };
        self.transport.download(req, dest).await
    }

    // ---------- 提交（§5.3 / §7.3） ----------

    pub async fn prepare_commit(
        &self,
        repo: &str,
        commit_id: &str,
        base_rev: i64,
        message: &str,
        changes: &[ChangeSpec],
    ) -> ClientResult<PrepareResult> {
        let body = serde_json::json!({
            "commit_id": commit_id,
            "base_rev": base_rev,
            "message": message,
            "changes": changes,
        });
        self.json(
            "POST",
            &format!("/repos/{}/commit/prepare", enc_component(repo)),
            RequestOptions {
                body: Self::json_body(&body),
                idempotent: Some(true),
                ..Default::default()
            },
        )
        .await
    }

    pub async fn commit(
        &self,
        repo: &str,
        commit_id: &str,
        commit_token: &str,
        message: &str,
    ) -> ClientResult<CommitResult> {
        let body = serde_json::json!({
            "commit_id": commit_id,
            "commit_token": commit_token,
            "message": message,
        });
        self.json(
            "POST",
            &format!("/repos/{}/commit", enc_component(repo)),
            RequestOptions {
                body: Self::json_body(&body),
                idempotent: Some(true),
                ..Default::default()
            },
        )
        .await
    }

    // ---------- 锁（§7.2） ----------

    pub async fn list_locks(
        &self,
        repo: &str,
        path: Option<&str>,
        owner: Option<&str>,
    ) -> ClientResult<Vec<LockInfo>> {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            items: Vec<LockInfo>,
        }
        let mut query = Vec::new();
        if let Some(p) = path {
            query.push(("path".to_string(), p.to_string()));
        }
        if let Some(o) = owner {
            query.push(("owner".to_string(), o.to_string()));
        }
        let r: Wrap = self
            .json(
                "GET",
                &format!("/repos/{}/locks", enc_component(repo)),
                RequestOptions {
                    query,
                    ..Default::default()
                },
            )
            .await?;
        Ok(r.items)
    }

    /// v0.4.17：锁只剩文件级，服务端不再接受 kind（§5.2）。
    pub async fn acquire_lock(
        &self,
        repo: &str,
        path: &str,
        comment: Option<&str>,
        expires_in: Option<i64>,
    ) -> ClientResult<LockInfo> {
        let mut body = serde_json::Map::new();
        body.insert("path".into(), serde_json::json!(path));
        if let Some(c) = comment {
            body.insert("comment".into(), serde_json::json!(c));
        }
        if let Some(e) = expires_in {
            body.insert("expires_in".into(), serde_json::json!(e));
        }
        self.json(
            "POST",
            &format!("/repos/{}/locks", enc_component(repo)),
            RequestOptions {
                body: Self::json_body(&serde_json::Value::Object(body)),
                idempotent: Some(false),
                ..Default::default()
            },
        )
        .await
    }

    pub async fn refresh_locks(
        &self,
        repo: &str,
        ttl_secs: Option<i64>,
    ) -> ClientResult<(i64, Option<String>)> {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            refreshed: i64,
            #[serde(default)]
            expires_at: Option<String>,
        }
        let body = serde_json::json!({ "ttl_secs": ttl_secs });
        let r: Wrap = self
            .json(
                "POST",
                &format!("/repos/{}/locks/refresh", enc_component(repo)),
                RequestOptions {
                    body: Self::json_body(&body),
                    idempotent: Some(true),
                    ..Default::default()
                },
            )
            .await?;
        Ok((r.refreshed, r.expires_at))
    }

    pub async fn release_lock(
        &self,
        repo: &str,
        path: &str,
        token: Option<&str>,
        break_lock: bool,
        reason: Option<&str>,
    ) -> ClientResult<serde_json::Value> {
        let mut query = Vec::new();
        if let Some(t) = token {
            query.push(("token".to_string(), t.to_string()));
        }
        if break_lock {
            query.push(("break".to_string(), "true".to_string()));
        }
        if let Some(r) = reason {
            query.push(("reason".to_string(), r.to_string()));
        }
        self.json(
            "DELETE",
            &format!("/repos/{}/locks/{}", enc_component(repo), enc_path(path)),
            RequestOptions {
                query,
                idempotent: Some(true),
                ..Default::default()
            },
        )
        .await
    }
}

// ---------- 生产实现：reqwest ----------

pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> ClientResult<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ClientError::new("NETWORK", e.to_string()))?;
        Ok(Self { client })
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new().expect("reqwest 客户端初始化失败")
    }
}

impl Transport for ReqwestTransport {
    fn send<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, ClientResult<HttpResponse>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(req.method.as_bytes())
                .map_err(|e| ClientError::new("INTERNAL", e.to_string()))?;
            let mut rb = self.client.request(method, &req.url);
            for (k, v) in &req.headers {
                rb = rb.header(k, v);
            }
            if let Some(body) = req.body {
                rb = rb.body(body);
            }
            let resp = rb
                .send()
                .await
                .map_err(|e| ClientError::new("NETWORK", e.to_string()))?;
            let status = resp.status().as_u16();
            let headers: Vec<(String, String)> = resp
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_string(),
                        v.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect();
            let body = resp
                .bytes()
                .await
                .map_err(|e| ClientError::new("NETWORK", e.to_string()))?
                .to_vec();
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }

    fn download<'a>(&'a self, req: HttpRequest, dest: &'a Path) -> BoxFuture<'a, ClientResult<u64>> {
        Box::pin(async move {
            let mut rb = self.client.request(reqwest::Method::GET, &req.url);
            for (k, v) in &req.headers {
                rb = rb.header(k, v);
            }
            let resp = rb
                .send()
                .await
                .map_err(|e| ClientError::new("NETWORK", e.to_string()))?;
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let headers: Vec<(String, String)> = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_string(),
                            v.to_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect();
                let body = resp.bytes().await.unwrap_or_default().to_vec();
                return Err(ApiClient::to_error(&HttpResponse {
                    status,
                    headers,
                    body,
                }));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ClientError::new("IO", e.to_string()))?;
            }
            let mut file = tokio::fs::File::create(dest)
                .await
                .map_err(|e| ClientError::new("IO", e.to_string()))?;
            let mut stream = resp.bytes_stream();
            let mut total: u64 = 0;
            use futures_util::StreamExt;
            use tokio::io::AsyncWriteExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| ClientError::new("NETWORK", e.to_string()))?;
                file.write_all(&chunk)
                    .await
                    .map_err(|e| ClientError::new("IO", e.to_string()))?;
                total += chunk.len() as u64;
            }
            file.flush()
                .await
                .map_err(|e| ClientError::new("IO", e.to_string()))?;
            Ok(total)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// 记录请求并按脚本返回的桩传输。
    struct Stub {
        calls: AtomicUsize,
        seen: Mutex<Vec<HttpRequest>>,
        script: Mutex<Vec<HttpResponse>>,
    }

    impl Stub {
        fn new(responses: Vec<HttpResponse>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
                script: Mutex::new(responses),
            })
        }
        fn push(&self, r: HttpResponse) {
            self.script.lock().unwrap().push(r);
        }
    }

    impl Transport for Stub {
        fn send<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, ClientResult<HttpResponse>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.seen.lock().unwrap().push(req);
                let mut s = self.script.lock().unwrap();
                if s.is_empty() {
                    return Err(ClientError::new("NETWORK", "桩没有更多响应"));
                }
                let r = s.remove(0);
                Ok(r)
            })
        }
        fn download<'a>(
            &'a self,
            _req: HttpRequest,
            dest: &'a Path,
        ) -> BoxFuture<'a, ClientResult<u64>> {
            Box::pin(async move {
                std::fs::write(dest, b"blob").unwrap();
                Ok(4)
            })
        }
    }

    fn resp(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        }
    }

    fn client(stub: std::sync::Arc<Stub>) -> ApiClient {
        ApiClient::new("http://127.0.0.1:9/", None, stub)
    }

    #[tokio::test]
    async fn base_url_trailing_slash_is_trimmed_and_api_v1_prefixed() {
        let stub = Stub::new(vec![resp(200, r#"{"items":[]}"#)]);
        let c = client(stub.clone());
        c.list_repos().await.unwrap();
        assert_eq!(stub.seen.lock().unwrap()[0].url, "http://127.0.0.1:9/api/v1/repos");
    }

    #[tokio::test]
    async fn bearer_token_is_attached() {
        let stub = Stub::new(vec![resp(200, r#"{"id":1,"username":"u","is_admin":false}"#)]);
        let c = ApiClient::new(
            "http://h",
            Some("tok".into()),
            stub.clone(),
        );
        let me = c.me().await.unwrap();
        assert_eq!(me.username, "u");
        let h = &stub.seen.lock().unwrap()[0];
        assert!(h
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer tok"));
    }

    #[tokio::test]
    async fn login_sets_token() {
        let stub = Stub::new(vec![resp(
            200,
            r#"{"token":"t1","expires_at":"x","user":{"id":1,"username":"u","is_admin":true}}"#,
        )]);
        let mut c = client(stub);
        let r = c.login("u", "p").await.unwrap();
        assert_eq!(r.token, "t1");
        assert_eq!(c.token_value(), Some("t1"));
    }

    #[tokio::test]
    async fn get_is_retried_on_5xx_but_post_is_not() {
        let stub = Stub::new(vec![
            resp(500, r#"{"error":{"code":"INTERNAL","message":"boom"}}"#),
            resp(200, r#"{"items":[]}"#),
        ]);
        let c = client(stub.clone());
        assert!(c.list_repos().await.is_ok());
        assert_eq!(stub.calls.load(Ordering::SeqCst), 2);

        // POST /auth/login 不可重试：一次失败即返回
        let stub2 = Stub::new(vec![resp(500, r#"{"error":{"code":"INTERNAL","message":"x"}}"#)]);
        let mut c2 = client(stub2.clone());
        let e = c2.login("u", "p").await.err().expect("应当失败");
        assert_eq!(e.code, "INTERNAL");
        assert_eq!(stub2.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn non_retryable_code_stops_immediately() {
        let stub = Stub::new(vec![resp(
            403,
            r#"{"error":{"code":"PERMISSION_DENIED","message":"nope"}}"#,
        )]);
        let c = client(stub.clone());
        let e = c.list_repos().await.err().expect("应当失败");
        assert_eq!(e.code, "PERMISSION_DENIED");
        assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn error_body_is_parsed_into_code_and_message() {
        let stub = Stub::new(vec![resp(
            409,
            r#"{"error":{"code":"OUT_OF_DATE","message":"落后了","details":{"head":3}}}"#,
        )]);
        let c = client(stub);
        let e = c.list_repos().await.err().expect("应当失败");
        assert_eq!(e.code, "OUT_OF_DATE");
        assert_eq!(e.message, "落后了");
        assert_eq!(e.details.unwrap(), serde_json::json!({"head":3}));
    }

    #[tokio::test]
    async fn changes_parses_jsonl() {
        let body = "{\"rev\":1,\"path\":\"a\",\"op\":\"add\",\"kind\":\"file\"}\n\n{\"rev\":2,\"path\":\"b\",\"op\":\"modify\",\"kind\":\"file\"}\n";
        let stub = Stub::new(vec![resp(200, body)]);
        let c = client(stub);
        let rows = c.changes("r", 0, None, None).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, "a");
        assert_eq!(rows[1].op, "modify");
    }

    #[tokio::test]
    async fn path_segments_are_percent_encoded_but_slash_kept() {
        let stub = Stub::new(vec![resp(200, "{}")]);
        let c = client(stub.clone());
        c.release_lock("my repo", "a b/c?.psd", None, false, None)
            .await
            .unwrap();
        let url = &stub.seen.lock().unwrap()[0].url;
        assert!(url.contains("/repos/my%20repo/locks/a%20b/c%3F.psd"), "实际：{url}");
    }

    #[tokio::test]
    async fn query_uses_plus_for_space() {
        let stub = Stub::new(vec![resp(200, r#"{"items":[]}"#)]);
        let c = client(stub.clone());
        c.list_locks("r", Some("a b"), None).await.unwrap();
        let url = &stub.seen.lock().unwrap()[0].url;
        assert!(url.contains("path=a+b"), "实际：{url}");
    }

    #[tokio::test]
    async fn retry_after_is_parsed_from_seconds() {
        let mut r = resp(429, r#"{"error":{"code":"RATE_LIMITED","message":"slow"}}"#);
        r.headers.push(("retry-after".into(), "0".into()));
        let stub = Stub::new(vec![r]);
        let c = client(stub).with_max_retries(0);
        let e = c.list_repos().await.err().expect("应当失败");
        assert_eq!(e.retry_after_ms, Some(0));
    }

    #[tokio::test]
    async fn http_date_is_parsed() {
        let t = parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        assert_eq!(t, 1_445_412_480);
    }

    #[tokio::test]
    async fn blob_download_writes_file() {
        let stub = Stub::new(vec![]);
        let c = client(stub);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("blob");
        let n = c.download_blob_to("r", "abc", &p).await.unwrap();
        assert_eq!(n, 4);
        assert_eq!(std::fs::read(&p).unwrap(), b"blob");
    }

    #[tokio::test]
    async fn json_body_is_not_marked_as_raw() {
        let stub = Stub::new(vec![resp(200, r#"{"commit_id":"c","rev":1,"replayed":false}"#)]);
        let c = client(stub.clone());
        c.commit("r", "c", "t", "m").await.unwrap();
        let req = &stub.seen.lock().unwrap()[0];
        let ct = req
            .headers
            .iter()
            .find(|(k, _)| k == "Content-Type")
            .map(|(_, v)| v.as_str());
        assert_eq!(ct, Some("application/json; charset=utf-8"));
    }

    #[tokio::test]
    async fn raw_body_gets_octet_stream() {
        let stub = Stub::new(vec![resp(200, "{}")]);
        let c = client(stub.clone());
        c.put_blob("r", "h", vec![0xde, 0xad]).await.unwrap();
        let req = &stub.seen.lock().unwrap()[0];
        let ct = req
            .headers
            .iter()
            .find(|(k, _)| k == "Content-Type")
            .map(|(_, v)| v.as_str());
        assert_eq!(ct, Some("application/octet-stream"));
    }
}
