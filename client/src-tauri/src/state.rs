/**
 * 进程内状态（原 `main/handlers.ts` 里 `buildHandlers` 闭包 + `WcSession` 持有的东西）。
 *
 * 同一时刻只打开一个工作副本（单窗口形态）。`wc:close` / `auth:logout` 会清空它。
 */
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::config::ConfigStore;
use crate::engine::api::{ApiClient, ReqwestTransport, Transport};
use crate::engine::errors::ClientResult;
use crate::engine::ignore::{IgnoreRules, IGNORE_FILE};
use crate::engine::wc::WorkingCopy;
use crate::session::SessionStore;
use crate::watch::{self, IgnoreFn, SettledFn, Watcher, SELF_WRITE_MS};

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct WcState {
    pub root: String,
    pub repo: String,
    pub rev: i64,
    pub sparse_paths: Vec<String>,
    /// 文件监听是否可用；`false` = 界面必须露出手动刷新入口。
    pub watching: bool,
}

pub struct AppState {
    pub config: ConfigStore,
    pub session: SessionStore,
    /// 当前工作副本（tokio Mutex：命令是 async 的，操作期间要一直持锁）。
    pub wc: tokio::sync::Mutex<Option<WorkingCopy>>,
    watcher: Mutex<Option<Watcher>>,
    /// 监听用的忽略规则（每次 settle 后重载：用户改了 `.b-artifactignore` 就该按新规则过滤）。
    rules: Arc<RwLock<IgnoreRules>>,
    transport: Arc<dyn Transport>,
    app: OnceLock<AppHandle>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            config: ConfigStore::new(None),
            session: SessionStore::new(None),
            wc: tokio::sync::Mutex::new(None),
            watcher: Mutex::new(None),
            rules: Arc::new(RwLock::new(IgnoreRules::new())),
            transport: Arc::new(ReqwestTransport::default()),
            app: OnceLock::new(),
        }
    }

    /// 桌面壳启动时注入 AppHandle（CLI 不起 Tauri，所以这里允许缺席）。
    pub fn attach(&self, app: AppHandle) {
        let _ = self.app.set(app);
    }

    pub fn app_handle(&self) -> Option<AppHandle> {
        self.app.get().cloned()
    }

    pub fn transport(&self) -> Arc<dyn Transport> {
        self.transport.clone()
    }

    /// 主进程 → 渲染层的事件推送（没有 AppHandle 时静默丢弃 = CLI 场景）。
    pub fn emit(&self, channel: &str, payload: impl Serialize + Clone) {
        if !crate::channels::is_event_channel(channel) {
            return;
        }
        if let Some(app) = self.app.get() {
            let _ = app.emit(channel, payload);
        }
    }

    /// 新建 REST 客户端（token 由 Rust 侧注入，渲染层看不到）。
    pub fn make_client(&self, base_url: &str, token: Option<&str>) -> Arc<ApiClient> {
        Arc::new(ApiClient::new(
            base_url,
            token.map(|s| s.to_string()),
            self.transport.clone(),
        ))
    }

    pub fn snapshot(&self, wc: &WorkingCopy) -> WcState {
        WcState {
            root: wc.wc.root.to_string_lossy().to_string(),
            repo: wc.repo.clone(),
            rev: wc.revision(),
            sparse_paths: wc.sparse_paths(),
            watching: self
                .watcher
                .lock()
                .unwrap()
                .as_ref()
                .map(|w| !w.degraded())
                .unwrap_or(false),
        }
    }

    /// 让监听器忽略接下来这段时间的变化：**我们自己**正在写工作文件。
    ///
    /// 只在真正会写盘的操作上调用（update / revert / 删除 / 检出 / 冲突解决）。
    pub fn suppress_watch(&self) {
        if let Some(w) = self.watcher.lock().unwrap().as_ref() {
            w.suppress(SELF_WRITE_MS);
        }
    }

    fn stop_watch(&self) {
        if let Some(mut w) = self.watcher.lock().unwrap().take() {
            w.stop();
        }
    }

    /// 起监听：变化 → 防抖 → 重算状态 → 推给渲染层。
    pub fn restart_watch(self: &Arc<Self>, wc: &WorkingCopy) {
        self.stop_watch();
        let root = wc.wc.root.clone();
        // 规则先加载一次（`is_ignored` 闭包读它）
        self.reload_rules(&root);

        let rules = self.rules.clone();
        let is_ignored: IgnoreFn = Arc::new(move |rel: &str, is_dir: bool| {
            rules
                .read()
                .map(|r| r.matches(rel, is_dir))
                .unwrap_or(false)
        });

        let state = self.clone();
        let root_for_settle = root.clone();
        let on_settled: SettledFn = Arc::new(move || {
            // 监听线程是普通线程（不是 runtime worker），block_on 是安全的；
            // 状态计算本身是同步的，只是要借 async 去拿 tokio Mutex。
            tauri::async_runtime::block_on(async {
                let guard = state.wc.lock().await;
                if let Some(wc) = guard.as_ref() {
                    match wc.status(false) {
                        Ok(items) => {
                            state.emit("wc:changed", items);
                            // 状态算完之后再刷新规则：本次事件已经处理完了，改规则是为了下一批
                            state.reload_rules(&root_for_settle);
                        }
                        Err(e) => {
                            eprintln!("[main] 监听刷新失败：{} {}", e.code, e.message);
                        }
                    }
                }
            });
        });

        match watch::spawn(&root, Some(is_ignored), on_settled) {
            Ok(w) => {
                *self.watcher.lock().unwrap() = Some(w);
            }
            Err(e) => {
                // 降级：界面露出手动刷新入口，不静默过期
                eprintln!("[main] 文件监听不可用，已降级为手动刷新：{}", e.message);
                *self.watcher.lock().unwrap() = None;
            }
        }
    }

    fn reload_rules(&self, root: &std::path::Path) {
        let mut r = IgnoreRules::new();
        r.add_file("", &root.join(IGNORE_FILE));
        if let Ok(mut guard) = self.rules.write() {
            *guard = r;
        }
    }

    /// 接管工作副本并返回状态快照。
    ///
    /// 关键细节：**同一个实例时不能 close**。提交/更新之后为了把前进的 rev 同步给顶栏，
    /// 会拿当前实例再 adopt 一次——如果无条件 close，就等于把自己关了，
    /// 后续任何操作都会撞 "database is not open"。
    pub async fn adopt(self: &Arc<Self>, wc: WorkingCopy) -> ClientResult<WcState> {
        let mut guard = self.wc.lock().await;
        let same = guard
            .as_ref()
            .map(|old| std::ptr::eq(old as *const _, &wc as *const _))
            .unwrap_or(false);
        if !same {
            if let Some(old) = guard.as_mut() {
                old.close();
            }
        }
        *guard = Some(wc);

        let state = if let Some(cur) = guard.as_ref() {
            self.restart_watch(cur);
            self.snapshot(cur)
        } else {
            unreachable!("刚写入的工作副本不应缺席")
        };
        drop(guard);
        self.emit("wc:state", state.clone());
        Ok(state)
    }

    pub async fn close_wc(&self) {
        self.stop_watch();
        let mut guard = self.wc.lock().await;
        if let Some(mut wc) = guard.take() {
            wc.close();
        }
        drop(guard);
        self.emit("wc:state", Option::<WcState>::None);
    }

    /// 当前快照（供 `wc:state` 之类的只读查询使用）。
    pub async fn current_state(&self) -> Option<WcState> {
        let guard = self.wc.lock().await;
        guard.as_ref().map(|wc| self.snapshot(wc))
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
