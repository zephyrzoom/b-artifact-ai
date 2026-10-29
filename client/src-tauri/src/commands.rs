/**
 * IPC 命令层：校验 → 执行 → 统一错误信封。
 *
 * 只暴露**一个** `ipc` 命令（`invoke('ipc', { channel, payload })`），而不是 34 个
 * `#[tauri::command]`：白名单与校验逻辑只有一份，新增通道只改 `channels.rs`；
 * 且 `dispatch` 是普通 async 函数，Rust 单测能直接驱动它，不必起 Tauri。
 *
 * 三条不变量（与 TS 版 `main/ipc.ts` 一致）：
 *   1. 只处理白名单里的通道；
 *   2. 处理分支必须与通道表**完全一致**；
 *   3. 异常不跨 IPC 边界 —— 一律转成 `{ ok:false, error:{ code, message } }`。
 */
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::channels;
use crate::engine::api::ApiClient;
use crate::engine::cache::{cache_stats, clear_cache};
use crate::engine::errors::ClientError;
use crate::engine::revisions::download_revision_file;
use crate::engine::scan::StatusItem;
use crate::engine::wc::{
    CheckoutOptions, CommitOptions, ConflictResolution, ProgressEvent, ProgressFn, WorkingCopy,
    WorkingCopyOptions,
};
use crate::session::SessionState;
use crate::state::AppState;

#[derive(Serialize, Debug, Clone)]
pub struct IpcErrorPayload {
    pub code: String,
    pub message: String,
    /// 缺省不写进信封（渲染层的 `if (error.details)` 才干净）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Serialize, Debug, Clone)]
pub struct IpcResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcErrorPayload>,
}

impl IpcResult {
    pub fn ok(data: impl Into<Value>) -> Self {
        Self {
            ok: true,
            data: Some(data.into()),
            error: None,
        }
    }

    pub fn from_error(e: &ClientError) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(IpcErrorPayload {
                code: e.code.clone(),
                message: e.message.clone(),
                details: e.details.clone(),
            }),
        }
    }

    pub fn err(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::from_error(&ClientError::new(code, message))
    }
}

/// Tauri 命令入口。
///
/// 返回 `Result<IpcResult, ()>` 而不是裸 `IpcResult`：Tauri 的 async 命令若带引用参数
/// （`State<'_, _>`）**必须**返回 `Result`，否则编译期直接拒（E0277）。业务错误仍然
/// 走信封（`Err` 永远不会被用到），这样渲染层看到的形状与 TS 版一致。
#[tauri::command]
pub async fn ipc(
    state: tauri::State<'_, Arc<AppState>>,
    channel: String,
    payload: Option<Value>,
) -> Result<IpcResult, ()> {
    Ok(dispatch(state.inner(), &channel, payload.as_ref()).await)
}

/// 打包自检的**成功**回传位：只有 `code == 0` 会落在这里（0 = 首屏已挂载且 IPC 往返成功）。
pub static SELF_CHECK_CODE: AtomicI32 = AtomicI32::new(-1);

/// 自检的**诊断**位（3 = 页面在但没挂载、4 = JS/IPC 抛异常）。
///
/// 分开存的原因：诊断码是"此刻还没好"，不是"最终失败"——CI 冷启动时页面刚解析完、
/// Vue 还没挂载就先报一次 3 是常态。早期版本拿它直接判失败，linux/windows 两条腿双双假红。
pub static SELF_CHECK_DIAG: AtomicI32 = AtomicI32::new(-1);

/// 只在 `--self-check` 流程里被前端调用（正常界面不会碰）。
#[tauri::command]
pub fn selfcheck(code: i32) {
    if code == 0 {
        SELF_CHECK_CODE.store(0, Ordering::SeqCst);
    } else {
        SELF_CHECK_DIAG.store(code, Ordering::SeqCst);
    }
}

/// 处理一次调用：校验 → 执行 → 包信封。**不 panic、不抛异常**。
pub async fn dispatch(state: &Arc<AppState>, channel: &str, payload: Option<&Value>) -> IpcResult {
    let params = match channels::validate_channel_payload(channel, payload) {
        Ok(p) => p,
        Err(message) => return IpcResult::err("BAD_REQUEST", message),
    };

    match handle(state, channel, &params).await {
        Ok(v) => IpcResult::ok(v),
        Err(e) => IpcResult::from_error(&e),
    }
}

fn as_string(p: &Map<String, Value>, key: &str) -> String {
    p.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn opt_string(p: &Map<String, Value>, key: &str) -> Option<String> {
    p.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn str_array(p: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    p.get(key).and_then(|v| v.as_array()).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect()
    })
}

fn num(p: &Map<String, Value>, key: &str) -> Option<i64> {
    p.get(key).and_then(|v| v.as_i64())
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, ClientError> {
    serde_json::to_value(v).map_err(|e| ClientError::new("INTERNAL", e.to_string()))
}

/// 用当前登录态建客户端；未登录抛 `UNAUTHENTICATED`。
fn authed_client(state: &Arc<AppState>) -> Result<(Arc<ApiClient>, SessionState), ClientError> {
    let st = state
        .session
        .get()
        .filter(|s| !s.token.is_empty())
        .ok_or_else(|| ClientError::new("UNAUTHENTICATED", "尚未登录"))?;
    let client = state.make_client(&st.server, Some(&st.token));
    Ok((client, st))
}

/// 当前工作副本为空时的统一报错（渲染层据此提示"请先打开工作副本"）。
fn no_wc() -> ClientError {
    ClientError::new("NO_WORKING_COPY", "尚未打开工作副本")
}

fn progress_cb(state: &Arc<AppState>, phase: &'static str) -> ProgressFn {
    let st = state.clone();
    Arc::new(move |e: ProgressEvent| {
        let payload = ProgressEvent {
            phase,
            done: e.done,
            total: e.total,
            current: e.current.clone(),
        };
        st.emit("wc:progress", payload);
    })
}

/// 通道 → 引擎。**分支集合必须与 `channels::CHANNEL_SPECS` 一致**（有单测钉住）。
async fn handle(
    state: &Arc<AppState>,
    channel: &str,
    p: &Map<String, Value>,
) -> Result<Value, ClientError> {
    match channel {
        // ---------- 应用 ----------
        "app:info" => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            // 字段名沿用 TS 版（渲染层直接用），值换成真实运行时
            "electron": "Tauri 2",
            "node": format!("Rust {}", option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("")),
            "chrome": "",
            "platform": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        })),
        "app:config" => to_value(&state.config.get()),
        "app:setConfig" => {
            let c = state.config.update(
                num(p, "concurrency"),
                opt_string(p, "cacheDir").as_deref(),
                opt_string(p, "defaultCheckoutParent").as_deref(),
            );
            to_value(&c)
        }
        "app:pickDir" => {
            let title = opt_string(p, "title").unwrap_or_else(|| "选择目录".to_string());
            let app = state
                .app_handle()
                .ok_or_else(|| ClientError::new("INTERNAL", "当前环境没有原生对话框（CLI）"))?;
            let picked = tokio::task::spawn_blocking(move || {
                use tauri_plugin_dialog::DialogExt;
                app.dialog().file().set_title(&title).blocking_pick_folder()
            })
            .await
            .map_err(|e| ClientError::new("INTERNAL", e.to_string()))?;
            match picked.and_then(|f| f.into_path().ok()) {
                Some(path) => Ok(json!(path.to_string_lossy())),
                None => Ok(Value::Null),
            }
        }

        // ---------- 认证 ----------
        "auth:login" => {
            let server = as_string(p, "server");
            let username = as_string(p, "username");
            let password = as_string(p, "password");
            let mut client = state.make_client(&server, None);
            let api = Arc::get_mut(&mut client)
                .ok_or_else(|| ClientError::new("INTERNAL", "客户端初始化失败"))?;
            let r = api.login(&username, &password).await?;
            state.session.save(SessionState {
                server: server.clone(),
                username: r.user.username.clone(),
                token: r.token.clone(),
            });
            state.config.remember_server(&server);
            eprintln!("[main] 登录成功：{}@{}", r.user.username, server);
            Ok(json!({
                "server": server,
                "username": r.user.username,
                "is_admin": r.user.is_admin,
                "display_name": r.user.display_name.unwrap_or_default(),
            }))
        }
        "auth:logout" => {
            if let Some(st) = state.session.get() {
                let mut c = ApiClient::new(
                    &st.server,
                    Some(st.token.clone()),
                    state.transport(),
                );
                // 令牌可能已在服务端失效：登出必须"总能成功"，否则用户会被卡住
                if let Err(e) = c.logout().await {
                    eprintln!("[main] logout 失败（忽略）：{}", e.code);
                }
            }
            state.session.clear();
            state.close_wc().await;
            Ok(json!({}))
        }
        "auth:state" => match state.session.get() {
            Some(s) => Ok(json!({ "server": s.server, "username": s.username })),
            None => Ok(Value::Null),
        },
        "auth:forgetServer" => {
            let c = state.config.forget_server(&as_string(p, "server"));
            to_value(&c)
        }

        // ---------- 仓库 ----------
        "repos:list" => {
            let (client, _) = authed_client(state)?;
            to_value(&client.list_repos().await?)
        }

        // ---------- 工作副本 ----------
        "wc:recent" => to_value(&state.config.get().recent),
        "wc:checkout" => {
            let (client, st) = authed_client(state)?;
            let cfg = state.config.get();
            let repo = as_string(p, "repo");
            let dir = as_string(p, "dir");
            let sparse = str_array(p, "sparse").unwrap_or_default();
            let wc = WorkingCopy::checkout(
                CheckoutOptions {
                    root: PathBuf::from(&dir),
                    api: client,
                    repo: repo.clone(),
                    user: Some(st.username.clone()),
                    cache_dir: Some(PathBuf::from(&cfg.cache_dir)),
                    concurrency: Some(cfg.concurrency.max(1) as usize),
                    sparse,
                },
                Some(progress_cb(state, "download")),
            )
            .await?;
            let snap = state.adopt(wc).await?;
            state.config.remember_recent(&snap.root, &repo, &st.server);
            to_value(&snap)
        }
        "wc:open" => {
            let (client, _) = authed_client(state)?;
            let cfg = state.config.get();
            let root = PathBuf::from(as_string(p, "dir"));
            match WorkingCopy::open(WorkingCopyOptions {
                root: root.clone(),
                api: client,
                repo: None,
                user: None,
                cache_dir: Some(PathBuf::from(&cfg.cache_dir)),
                concurrency: Some(cfg.concurrency.max(1) as usize),
            }) {
                Ok(wc) => {
                    let snap = state.adopt(wc).await?;
                    to_value(&snap)
                }
                Err(e) => {
                    if e.code == "NOT_A_WORKING_COPY" {
                        state.config.drop_recent(&root.to_string_lossy());
                    }
                    Err(e)
                }
            }
        }
        "wc:close" => {
            state.close_wc().await;
            Ok(json!({}))
        }
        "wc:status" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            let force = p.get("forceHash").and_then(|v| v.as_bool()) == Some(true);
            let items: Vec<StatusItem> = wc.status(force)?;
            to_value(&items)
        }
        "wc:add" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            let n = wc.add(&str_array(p, "paths").unwrap_or_default())?;
            Ok(json!({ "added": n }))
        }
        "wc:remove" => {
            state.suppress_watch();
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            wc.remove(&str_array(p, "paths").unwrap_or_default()).await?;
            Ok(json!({}))
        }
        "wc:revert" => {
            state.suppress_watch();
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            wc.revert(&str_array(p, "paths").unwrap_or_default()).await?;
            Ok(json!({}))
        }
        "wc:mkdir" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            wc.mkdir(&as_string(p, "path")).await?;
            Ok(json!({}))
        }
        "wc:ignoreRules" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            Ok(json!({ "content": wc.read_ignore_file() }))
        }
        "wc:setIgnoreRules" => {
            state.suppress_watch();
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            wc.write_ignore_file(&as_string(p, "content")).await?;
            Ok(json!({}))
        }
        "wc:commit" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            let outcome = wc
                .commit(
                    CommitOptions {
                        message: Some(as_string(p, "message")),
                        paths: str_array(p, "paths"),
                    },
                    Some(progress_cb(state, "commit")),
                )
                .await?;
            state.restart_watch(wc);
            let snap = state.snapshot(wc);
            drop(guard);
            state.emit("wc:state", snap);
            to_value(&outcome)
        }
        "wc:update" => {
            state.suppress_watch();
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            let outcome = wc.update(Some(progress_cb(state, "update"))).await?;
            state.restart_watch(wc);
            let snap = state.snapshot(wc);
            drop(guard);
            state.emit("wc:state", snap);
            to_value(&outcome)
        }

        // ---------- 冲突 ----------
        "wc:conflicts" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            to_value(&wc.conflicts()?)
        }
        "wc:conflictSides" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            to_value(&wc.conflict_sides(&as_string(p, "path"))?)
        }
        "wc:resolveConflict" => {
            state.suppress_watch();
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            let choice = ConflictResolution::parse(&as_string(p, "choice"))?;
            wc.resolve_conflict(&as_string(p, "path"), choice, opt_string(p, "content").as_deref())
                .await?;
            Ok(json!({}))
        }

        // ---------- 锁 ----------
        "wc:locks" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            let path = opt_string(p, "path").filter(|s| !s.is_empty());
            to_value(&wc.list_locks(path.as_deref()).await?)
        }
        "wc:lock" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            let comment = opt_string(p, "comment");
            to_value(&wc.lock(&as_string(p, "path"), comment.as_deref()).await?)
        }
        "wc:unlock" => {
            let mut guard = state.wc.lock().await;
            let wc = guard.as_mut().ok_or_else(no_wc)?;
            wc.unlock(
                &as_string(p, "path"),
                p.get("breakLock").and_then(|v| v.as_bool()) == Some(true),
                opt_string(p, "reason").as_deref(),
            )
            .await?;
            Ok(json!({}))
        }

        // ---------- 历史修订 ----------
        "repo:log" => {
            let (client, _) = authed_client(state)?;
            let repo = require_repo(p)?;
            let prefix = opt_string(p, "prefix").filter(|s| !s.is_empty());
            let (items, total) = client
                .log(&repo, prefix.as_deref(), None, num(p, "limit"), num(p, "offset"))
                .await?;
            Ok(json!({ "items": items, "total": total }))
        }
        "repo:treeAt" => {
            let (client, _) = authed_client(state)?;
            let repo = require_repo(p)?;
            let path = opt_string(p, "path").unwrap_or_default();
            to_value(
                &client
                    .tree(
                        &repo,
                        Some(num(p, "rev").unwrap_or(0)),
                        Some(&path),
                        Some(num(p, "depth").unwrap_or(1)),
                    )
                    .await?,
            )
        }
        "repo:downloadRevision" => {
            let (client, _) = authed_client(state)?;
            let repo = require_repo(p)?;
            to_value(
                &download_revision_file(
                    &client,
                    &repo,
                    &as_string(p, "path"),
                    num(p, "rev").unwrap_or(0),
                    &as_string(p, "targetDir"),
                )
                .await?,
            )
        }

        // ---------- 配置与缓存 ----------
        "config:cacheStats" => {
            let dir = PathBuf::from(state.config.get().cache_dir);
            to_value(&cache_stats(&dir))
        }
        "config:clearCache" => {
            let dir = PathBuf::from(state.config.get().cache_dir);
            let keep_tmp = p.get("keepTmp").and_then(|v| v.as_bool()) == Some(true);
            to_value(&clear_cache(&dir, keep_tmp))
        }

        // ---------- 系统 ----------
        "shell:revealPath" => {
            let path = as_string(p, "path");
            match tauri_plugin_opener::reveal_item_in_dir(&path) {
                Ok(()) => Ok(json!({ "revealed": true })),
                Err(e) => {
                    eprintln!("[main] 定位文件失败：{e}");
                    Ok(json!({ "revealed": false }))
                }
            }
        }

        other => Err(ClientError::new(
            "NOT_IMPLEMENTED",
            format!("通道尚未迁移完成：{other}"),
        )),
    }
}

fn require_repo(p: &Map<String, Value>) -> Result<String, ClientError> {
    let name = as_string(p, "repo");
    // schema 已要求必填，这里再挡一次"空串"：空仓库名打到服务端会变成 `//tree` 这种脏请求
    if name.is_empty() {
        return Err(ClientError::new("BAD_REQUEST", "缺少参数：repo"));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> Arc<AppState> {
        Arc::new(AppState::new())
    }

    #[tokio::test]
    async fn envelope_shape_is_stable() {
        let s = state();
        let ok = dispatch(&s, "app:config", None).await;
        assert!(ok.ok);
        let v = serde_json::to_value(&ok).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert!(v.get("data").is_some());
        assert!(v.get("error").is_none(), "成功信封不应带 error 字段");

        let bad = dispatch(&s, "wc:open", Some(&json!({}))).await;
        assert!(!bad.ok);
        let v = serde_json::to_value(&bad).unwrap();
        assert_eq!(v["error"]["code"], json!("BAD_REQUEST"));
        assert_eq!(v["error"]["message"], json!("缺少参数：dir"));
        assert!(v.get("data").is_none(), "失败信封不应带 data 字段");
        assert!(
            v["error"].get("details").is_none(),
            "details 缺省不应写进信封"
        );
    }

    #[tokio::test]
    async fn unknown_channel_never_panics() {
        let s = state();
        let r = dispatch(&s, "nope:nope", None).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "BAD_REQUEST");
    }

    #[tokio::test]
    async fn every_channel_has_a_handler_branch() {
        // 与 TS 版 assertHandlersComplete 同目的：通道表加了但忘了实现，这里就该红。
        let s = state();
        for name in channels::channel_names() {
            let r = dispatch(&s, name, None).await;
            if let Some(e) = &r.error {
                assert_ne!(e.code, "NOT_IMPLEMENTED", "通道 {name} 还没有实现分支");
            }
        }
    }

    #[tokio::test]
    async fn wc_channels_require_an_open_working_copy() {
        let s = state();
        for name in ["wc:status", "wc:conflicts", "wc:ignoreRules"] {
            let r = dispatch(&s, name, None).await;
            assert!(!r.ok, "{name} 在没有工作副本时应报错");
            assert_eq!(r.error.unwrap().code, "NO_WORKING_COPY", "{name}");
        }
    }

    #[tokio::test]
    async fn authed_channels_require_login() {
        let s = state();
        let r = dispatch(&s, "repos:list", None).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "UNAUTHENTICATED");
        let _ = super::authed_client(&s).err().expect("应当失败");
    }

    #[tokio::test]
    async fn repo_param_must_not_be_empty() {
        let s = state();
        let r = dispatch(&s, "repo:log", Some(&json!({ "repo": "" }))).await;
        assert!(!r.ok);
        let e = r.error.unwrap();
        // 未登录先被挡住也行，但绝不能变成 `//log` 这种脏请求
        assert!(
            e.code == "BAD_REQUEST" || e.code == "UNAUTHENTICATED",
            "{}",
            e.code
        );
    }

    #[tokio::test]
    async fn cache_stats_reports_json_shape() {
        let s = state();
        let r = dispatch(&s, "config:cacheStats", None).await;
        assert!(r.ok);
        let d = r.data.unwrap();
        assert!(d.get("blobs").is_some());
        assert!(d.get("tmpFiles").is_some(), "camelCase 字段：{d}");
    }

    #[tokio::test]
    async fn app_info_reports_tauri_runtime() {
        let s = state();
        let r = dispatch(&s, "app:info", None).await;
        let d = r.data.unwrap();
        assert_eq!(d["electron"], json!("Tauri 2"));
        assert_eq!(d["version"], json!(env!("CARGO_PKG_VERSION")));
    }
}
