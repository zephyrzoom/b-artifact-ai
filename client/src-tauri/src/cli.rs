/**
 * b-artifact CLI（原 `client/src/cli.ts` 的 Rust 重写）。
 *
 * 设计取向与 TS 版一致：
 *   - 手写参数解析（与 `server/main.rs` 同风格，不引 clap）：内网一键分发，依赖越少越好；
 *   - 所有命令支持 `--json`，冒烟脚本直接断言结构化输出，不 scrape 文本；
 *   - 凭据落 `~/.b-artifact/auth.json`，权限 0600。
 *
 * 另外多一个 `channels --json`：把 IPC 通道表导出来，供 `scripts/gen_channels.mjs --check`
 * 与 TS 侧的通道联合类型做防漂移比对（扮演原 `dto-parity.test.ts` 的角色）。
 */
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use b_artifact_client::channels;
use b_artifact_client::engine::api::{ApiClient, ReqwestTransport};
use b_artifact_client::engine::db::{is_working_copy, WC_DIR};
use b_artifact_client::engine::errors::{ClientError, ClientResult};
use b_artifact_client::engine::home::artifact_home;
use b_artifact_client::engine::wc::{CommitOptions, WorkingCopy, WorkingCopyOptions};

// ---------- 参数解析 ----------

#[derive(Default)]
struct Parsed {
    positional: Vec<String>,
    flags: Vec<(String, String)>,
    bools: Vec<String>,
}

impl Parsed {
    fn get(&self, key: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn all(&self, key: &str) -> Vec<String> {
        self.flags
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }
    fn has(&self, key: &str) -> bool {
        self.bools.iter().any(|k| k == key)
    }
}

fn parse_args(argv: &[String]) -> Parsed {
    let mut out = Parsed::default();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            out.positional.extend(argv[i + 1..].iter().cloned());
            break;
        }
        if let Some(body) = a.strip_prefix("--") {
            if let Some((k, v)) = body.split_once('=') {
                out.flags.push((k.to_string(), v.to_string()));
            } else if matches!(body, "json" | "break" | "verbose" | "force-hash") {
                out.bools.push(body.to_string());
            } else {
                match argv.get(i + 1) {
                    Some(next) if !next.starts_with('-') => {
                        out.flags.push((body.to_string(), next.clone()));
                        i += 1;
                    }
                    _ => out.bools.push(body.to_string()),
                }
            }
        } else if a == "-m" || a == "-d" {
            let key = if a == "-m" { "message" } else { "dir" };
            let v = argv.get(i + 1).cloned().unwrap_or_default();
            out.flags.push((key.to_string(), v));
            i += 1;
        } else {
            out.positional.push(a.clone());
        }
        i += 1;
    }
    out
}

// ---------- 凭据 ----------

struct StoredAuth {
    server: String,
    token: String,
    username: String,
}

fn auth_path() -> PathBuf {
    artifact_home().join("auth.json")
}

fn load_auth() -> Option<StoredAuth> {
    let raw = std::fs::read_to_string(auth_path()).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    Some(StoredAuth {
        server: v.get("server")?.as_str()?.to_string(),
        token: v.get("token")?.as_str()?.to_string(),
        username: v.get("username").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

fn save_auth(a: &StoredAuth) -> ClientResult<()> {
    let p = auth_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&json!({
        "server": a.server, "token": a.token, "username": a.username
    }))
    .unwrap_or_default();
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&p)?;
        f.write_all(body.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&p, body)?;
    Ok(())
}

// ---------- 输出 ----------

struct Output {
    json: bool,
}

impl Output {
    fn print(&self, data: &Value) {
        if self.json {
            println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
        } else if let Some(s) = data.as_str() {
            println!("{s}");
        } else {
            println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
        }
    }

    fn fail(&self, msg: &str) -> ! {
        if self.json {
            println!("{}", json!({ "ok": false, "error": msg }));
        } else {
            eprintln!("错误：{msg}");
        }
        std::process::exit(1);
    }
}

const HELP: &str = "b-artifact 客户端 CLI

用法：
  login     --server <url> --username <u> --password <p>
  repos
  checkout  --repo <name> --dir <path> [--sparse <prefix>...]
  status    [--dir <path>]
  add       <paths...>
  remove    <paths...>
  revert    <paths...>
  commit    -m <message> [paths...]
  update    [--dir <path>]
  lock      <path> [-m <comment>]
  unlock    <path> [--break --reason <r>]
  locks     [path]
  log       [--limit n]
  channels  [--json]      # 导出 IPC 通道表（防漂移门禁用）

全局选项：
  --json            以 JSON 输出（供脚本断言）
";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "help" || argv[0] == "--help" {
        print!("{HELP}");
        return;
    }

    let cmd = argv[0].clone();
    // 命令名单独拿出来，其余交给解析器 —— 不能让"路径恰好等于命令名"被误当成关键字
    let parsed = parse_args(&argv[1..]);
    let rest: Vec<String> = parsed.positional.clone();

    let out = Output {
        json: parsed.has("json"),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            out.fail(&format!("运行时初始化失败：{e}"));
        });

    let result = rt.block_on(run(&cmd, &rest, &parsed, &out));
    if let Err(e) = result {
        out.fail(&format!("[{}] {}", e.code, e.message));
    }
}

async fn run(cmd: &str, rest: &[String], p: &Parsed, out: &Output) -> ClientResult<()> {
    let transport: Arc<dyn b_artifact_client::engine::api::Transport> =
        Arc::new(ReqwestTransport::default());

    // 凭据总是从 auth.json 取（渲染层那条路才需要显式传 token）
    let client_for = |server_flag: &str| -> ClientResult<(Arc<ApiClient>, StoredAuth)> {
        let auth = load_auth()
            .ok_or_else(|| ClientError::new("UNAUTHENTICATED", "未登录：先执行 login"))?;
        let server = if server_flag.is_empty() {
            auth.server.clone()
        } else {
            server_flag.to_string()
        };
        let api = Arc::new(ApiClient::new(
            &server,
            Some(auth.token.clone()),
            transport.clone(),
        ));
        Ok((api, auth))
    };

    match cmd {
        "channels" => {
            out.print(&channels::dump_json());
            Ok(())
        }

        "login" => {
            let server = p.get("server").unwrap_or("");
            let username = p.get("username").unwrap_or("");
            let password = p.get("password").unwrap_or("");
            if server.is_empty() || username.is_empty() || password.is_empty() {
                return Err(ClientError::new(
                    "INVALID_ARGUMENT",
                    "login 需要 --server / --username / --password",
                ));
            }
            let mut api = ApiClient::new(server, None, transport.clone());
            let r = api.login(username, password).await?;
            save_auth(&StoredAuth {
                server: server.to_string(),
                token: r.token.clone(),
                username: r.user.username.clone(),
            })?;
            out.print(&json!({
                "ok": true,
                "username": r.user.username,
                "is_admin": r.user.is_admin,
                "expires_at": r.expires_at,
            }));
            Ok(())
        }

        "repos" => {
            let (api, _) = client_for(p.get("server").unwrap_or(""))?;
            let items = api.list_repos().await?;
            if out.json {
                out.print(&json!({ "ok": true, "items": items }));
            } else {
                for r in &items {
                    println!(
                        "{:<24} rev={:<6} role={:<7} rw={}{}  {}",
                        r.name,
                        r.head_rev,
                        r.my_role,
                        if r.my_permissions.read { "r" } else { "-" },
                        if r.my_permissions.write { "w" } else { "-" },
                        r.description
                    );
                }
            }
            Ok(())
        }

        "checkout" => {
            let (api, auth) = client_for(p.get("server").unwrap_or(""))?;
            let repo = p.get("repo").unwrap_or("").to_string();
            if repo.is_empty() {
                return Err(ClientError::new("INVALID_ARGUMENT", "checkout 需要 --repo"));
            }
            let dir = p
                .get("dir")
                .map(PathBuf::from)
                .unwrap_or(std::env::current_dir()?);
            let sparse = p.all("sparse");
            let mut wc = WorkingCopy::checkout(
                b_artifact_client::engine::wc::CheckoutOptions {
                    root: dir.clone(),
                    api,
                    repo: repo.clone(),
                    user: Some(auth.username.clone()),
                    cache_dir: None,
                    concurrency: None,
                    sparse: sparse.clone(),
                },
                None,
            )
            .await?;
            // 先取修订号再 close：close 之后 wc.db 句柄已关，读 revision 会炸
            let rev = wc.revision();
            let sparse_paths = wc.sparse_paths();
            wc.close();
            out.print(&json!({
                "ok": true,
                "repo": repo,
                "dir": dir.to_string_lossy(),
                "rev": rev,
                "sparse_paths": sparse_paths,
                "user": auth.username,
                "meta": WC_DIR,
            }));
            Ok(())
        }

        "status" => {
            let root = find_root(p.get("dir").unwrap_or("."))?;
            let (api, _) = client_for(p.get("server").unwrap_or(""))?;
            let mut wc = open_wc(&root, api)?;
            let meta = wc.wc.get_meta()?;
            let items = wc.status(p.has("force-hash"))?;
            wc.close();
            if out.json {
                out.print(&json!({
                    "ok": true, "root": root.to_string_lossy(), "rev": meta.revision,
                    "repo": meta.repo, "items": items
                }));
            } else if items.is_empty() {
                println!("工作副本干净");
            } else {
                for it in &items {
                    println!("{:<12} {}", it.status.as_str(), it.path);
                }
            }
            Ok(())
        }

        "add" | "remove" | "revert" => {
            let root = find_root(p.get("dir").unwrap_or("."))?;
            let (api, _) = client_for(p.get("server").unwrap_or(""))?;
            let mut wc = open_wc(&root, api)?;
            let mut paths: Vec<String> = rest.to_vec();
            if paths.is_empty() {
                paths = p.all("path");
            }
            if paths.is_empty() {
                return Err(ClientError::new(
                    "INVALID_ARGUMENT",
                    format!("{cmd} 需要至少一个路径"),
                ));
            }
            match cmd {
                "add" => {
                    let n = wc.add(&paths)?;
                    out.print(&json!({ "ok": true, "added": n }));
                }
                "remove" => {
                    wc.remove(&paths).await?;
                    out.print(&json!({ "ok": true, "removed": paths }));
                }
                _ => {
                    wc.revert(&paths).await?;
                    out.print(&json!({ "ok": true, "reverted": paths }));
                }
            }
            wc.close();
            Ok(())
        }

        "commit" => {
            let root = find_root(p.get("dir").unwrap_or("."))?;
            let (api, _) = client_for(p.get("server").unwrap_or(""))?;
            let mut wc = open_wc(&root, api)?;
            let message = p.get("message").unwrap_or("").to_string();
            if message.is_empty() {
                return Err(ClientError::new("INVALID_ARGUMENT", "commit 需要 -m <message>"));
            }
            let paths = if rest.is_empty() {
                None
            } else {
                Some(rest.to_vec())
            };
            let r = wc.commit(CommitOptions { message: Some(message), paths }, None).await?;
            wc.close();
            out.print(&json!({
                "ok": true, "rev": r.rev, "committed": r.committed, "replayed": r.replayed
            }));
            Ok(())
        }

        "update" => {
            let root = find_root(p.get("dir").unwrap_or("."))?;
            let (api, _) = client_for(p.get("server").unwrap_or(""))?;
            let mut wc = open_wc(&root, api)?;
            let r = wc.update(None).await?;
            wc.close();
            let mut v = serde_json::to_value(&r).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("ok".into(), json!(true));
            }
            out.print(&v);
            Ok(())
        }

        "lock" => {
            let root = find_root(p.get("dir").unwrap_or("."))?;
            let (api, _) = client_for(p.get("server").unwrap_or(""))?;
            let mut wc = open_wc(&root, api)?;
            let path = rest
                .first()
                .cloned()
                .or_else(|| p.get("path").map(|s| s.to_string()))
                .ok_or_else(|| ClientError::new("INVALID_ARGUMENT", "lock 需要一个路径"))?;
            let comment = p.get("message").map(|s| s.to_string());
            let info = wc.lock(&path, comment.as_deref()).await?;
            wc.close();
            let mut v = serde_json::to_value(&info).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("ok".into(), json!(true));
            }
            out.print(&v);
            Ok(())
        }

        "unlock" => {
            let root = find_root(p.get("dir").unwrap_or("."))?;
            let (api, _) = client_for(p.get("server").unwrap_or(""))?;
            let mut wc = open_wc(&root, api)?;
            let path = rest
                .first()
                .cloned()
                .or_else(|| p.get("path").map(|s| s.to_string()))
                .ok_or_else(|| ClientError::new("INVALID_ARGUMENT", "unlock 需要一个路径"))?;
            wc.unlock(&path, p.has("break"), p.get("reason")).await?;
            wc.close();
            out.print(&json!({ "ok": true, "path": path }));
            Ok(())
        }

        "locks" => {
            let root = find_root(p.get("dir").unwrap_or("."))?;
            let (api, _) = client_for(p.get("server").unwrap_or(""))?;
            let mut wc = open_wc(&root, api)?;
            let items = wc.list_locks(rest.first().map(|s| s.as_str())).await?;
            wc.close();
            out.print(&json!({ "ok": true, "items": items }));
            Ok(())
        }

        "log" => {
            let root = find_root(p.get("dir").unwrap_or("."))?;
            let (api, _) = client_for(p.get("server").unwrap_or(""))?;
            let mut wc = open_wc(&root, api)?;
            let limit = p
                .get("limit")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(20);
            let (items, total) = wc.api.log(&wc.repo.clone(), None, None, Some(limit), None).await?;
            wc.close();
            if out.json {
                out.print(&json!({ "ok": true, "items": items, "total": total }));
            } else {
                for it in &items {
                    println!(
                        "r{:<6} {:<14} {}  {}",
                        it.rev, it.author, it.created_at, it.message
                    );
                }
            }
            Ok(())
        }

        other => Err(ClientError::new(
            "INVALID_ARGUMENT",
            format!("未知命令：{other}\n\n{HELP}"),
        )),
    }
}

/// 打开工作副本：repo 与服务端地址都取 wc.db 里的记录。
fn open_wc(root: &Path, api: Arc<ApiClient>) -> ClientResult<WorkingCopy> {
    WorkingCopy::open(WorkingCopyOptions {
        root: root.to_path_buf(),
        api,
        repo: None,
        user: None,
        cache_dir: None,
        concurrency: None,
    })
}

/// 定位工作副本根：从 `--dir` 或 cwd 向上找 `.b-artifact/wc.db`。
fn find_root(start: &str) -> ClientResult<PathBuf> {
    let mut dir = std::path::absolute(Path::new(start)).unwrap_or_else(|_| PathBuf::from(start));
    loop {
        if is_working_copy(&dir) {
            return Ok(dir);
        }
        let Some(parent) = dir.parent().map(|p| p.to_path_buf()) else {
            return Err(ClientError::new("NOT_A_WORKING_COPY", "当前目录不在工作副本内"));
        };
        if parent == dir {
            return Err(ClientError::new("NOT_A_WORKING_COPY", "当前目录不在工作副本内"));
        }
        dir = parent;
    }
}
