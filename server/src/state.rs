//! AppState（§3.5）。

use crate::auth::LoginLimiter;
use crate::config::Config;
use crate::error::AppError;
use crate::storage::blob::BlobStore;
use crate::storage::pool::{DbPool, Pooled};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 取 DB 连接时是否包 `tokio::task::block_in_place`（默认 false，理由见 `lock_db`）。
/// 运行期开关，便于压测 A/B：`B_ARTIFACT_DB_BLOCK_IN_PLACE=1`。
static DB_BLOCK_IN_PLACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn db_block_in_place() -> bool {
    *DB_BLOCK_IN_PLACE.get_or_init(|| {
        std::env::var("B_ARTIFACT_DB_BLOCK_IN_PLACE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

pub struct AppState {
    /// 连接池（M1.5 B6）：WAL 下多读单写，写事务走 BEGIN IMMEDIATE 串行。
    pub db: DbPool,
    pub blob: BlobStore,
    pub config: Arc<Config>,
    /// 登录失败退避限流（内存态）
    pub limiter: LoginLimiter,
    /// 分块上传暂存目录 data/uploads/（§7.2）
    pub uploads_dir: PathBuf,
    /// 每个 upload_id 一把锁：串行化 meta.json/分块文件写（避免交错覆盖）
    upload_locks: Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
}

impl AppState {
    pub fn new(db: DbPool, blob: BlobStore, config: Config, uploads_dir: PathBuf) -> AppState {
        let limiter = LoginLimiter::new(config.auth.login_max_fails, config.auth.login_lockout_secs);
        AppState {
            db,
            blob,
            config: Arc::new(config),
            limiter,
            uploads_dir,
            upload_locks: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 取一条 DB 连接（池满则阻塞等待）。
    ///
    /// 池满时 `get()` 会睡在 Condvar 上。是否用 `block_in_place` 包裹见
    /// `DB_BLOCK_IN_PLACE`——实测在高并发下 `block_in_place` **反而更慢**
    /// （8 worker 全部进入 block_in_place 时 runtime 无法腾挪任务，
    /// p95 从 11ms 涨到 65ms、吞吐掉一个数量级），故默认关闭。
    pub fn lock_db(&self) -> Result<Pooled, AppError> {
        if db_block_in_place() {
            Ok(tokio::task::block_in_place(|| self.db.get())?)
        } else {
            self.db.get()
        }
    }

    /// 取（或建）某 upload_id 的互斥锁；complete/delete 后调 forget_upload 清理表项。
    pub fn lock_upload(&self, upload_id: &str) -> Arc<Mutex<()>> {
        let mut map = self.upload_locks.lock().unwrap();
        Arc::clone(map.entry(upload_id.to_string()).or_default())
    }

    pub fn forget_upload(&self, upload_id: &str) {
        self.upload_locks.lock().unwrap().remove(upload_id);
    }
}

/// axum state 需要 Clone；内部全部为 Arc/Mutex。
#[derive(Clone)]
pub struct SharedState(pub Arc<AppState>);

impl std::ops::Deref for SharedState {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.0
    }
}
