//! SQLite 连接池（M1.5 瓶颈 B6）。
//!
//! M1 的 `Mutex<Connection>` 让数据库并发度**结构性恒为 1**：并发 8→32（4×）时
//! DB 端点吞吐只有 +0.2%~+8%（`repo_info` 甚至 −7%），p50 涨约 4 倍。
//! WAL 下 SQLite 支持"多读 + 单写"，因此改为 N 条连接的池：
//!   - 读：N 条连接真正并行；
//!   - 写：靠 `BEGIN IMMEDIATE` + `busy_timeout` 串行（延迟事务先读后写会撞
//!     SQLITE_BUSY_SNAPSHOT，而 busy handler 不重试该错误，故写事务一律 IMMEDIATE）。
//!
//! 实现为**阻塞式**池，与既有代码一致（handler 在 `block_in_place` 内取连接），
//! 不引入 r2d2：省一条外部依赖，也避免与 rusqlite 版本耦合。

use crate::error::AppError;
use crate::storage::db;
use rusqlite::Connection;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// 连接池：句柄可 Clone（内部 Arc），连接按需惰性打开。
#[derive(Clone)]
pub struct DbPool {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    max: usize,
    idle: Mutex<Vec<Connection>>,
    /// 已打开的连接总数（idle + 已借出）；持锁时递增，保证不超额。
    live: AtomicUsize,
    cv: Condvar,
}

/// 池中借出的连接；Drop 时归还（替换掉原来的 `MutexGuard<'_, Connection>`，
/// 语义一致：`Deref<Target = Connection>`）。
pub struct Pooled {
    conn: Option<Connection>,
    inner: Arc<Inner>,
}

impl Deref for Pooled {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("Pooled 已归还，不该再被解引用")
    }
}

impl DerefMut for Pooled {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("Pooled 已归还，不该再被解引用")
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut idle = self.inner.idle.lock().unwrap_or_else(|e| e.into_inner());
            idle.push(conn);
            drop(idle);
            self.inner.cv.notify_one();
        }
    }
}

impl DbPool {
    /// 建池并预热一条连接（顺带验证路径可打开、migration 已应用）。
    pub fn new(path: impl AsRef<Path>, max: usize) -> Result<DbPool, AppError> {
        let max = max.max(1);
        let first = db::open(path.as_ref())?;
        Ok(DbPool {
            inner: Arc::new(Inner {
                path: path.as_ref().to_path_buf(),
                max,
                idle: Mutex::new(vec![first]),
                live: AtomicUsize::new(1),
                cv: Condvar::new(),
            }),
        })
    }

    /// 取一条连接；池满时阻塞等待（busy_timeout 之外的另一层排队）。
    pub fn get(&self) -> Result<Pooled, AppError> {
        let mut idle = self.inner.idle.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(conn) = idle.pop() {
                return Ok(Pooled {
                    conn: Some(conn),
                    inner: Arc::clone(&self.inner),
                });
            }
            if self.inner.live.load(Ordering::SeqCst) < self.inner.max {
                self.inner.live.fetch_add(1, Ordering::SeqCst);
                drop(idle);
                return match db::open(&self.inner.path) {
                    Ok(conn) => Ok(Pooled {
                        conn: Some(conn),
                        inner: Arc::clone(&self.inner),
                    }),
                    Err(e) => {
                        self.inner.live.fetch_sub(1, Ordering::SeqCst);
                        self.inner.cv.notify_one();
                        Err(e.into())
                    }
                };
            }
            idle = self.inner.cv.wait(idle).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 已打开的连接数（监控 / 测试用）。
    pub fn size(&self) -> usize {
        self.inner.live.load(Ordering::SeqCst)
    }

    pub fn max_size(&self) -> usize {
        self.inner.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// M1.5 B6 的验收点：并发取连接不再串行化到 1，且连接数不超过池上限。
    #[test]
    fn pool_serves_concurrent_borrowers_within_max() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b-artifact.db");
        {
            let mut conn = db::open(&path).unwrap();
            db::migrate(&mut conn).unwrap();
        }
        let pool = DbPool::new(&path, 4).unwrap();
        assert_eq!(pool.max_size(), 4);

        let mut handles = vec![];
        for _ in 0..8 {
            let p = pool.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..20 {
                    let conn = p.get().expect("取连接失败");
                    let v: i64 = conn.query_row("SELECT 1", [], |r| r.get(0)).unwrap();
                    assert_eq!(v, 1);
                }
            }));
        }
        for h in handles {
            h.join().expect("并发取连接的线程 panic");
        }
        assert!(pool.size() <= 4, "连接数不应超过池上限，实际 {}", pool.size());
        assert!(pool.size() >= 2, "并发应真正复用/新建多条连接，实际 {}", pool.size());
    }
}
