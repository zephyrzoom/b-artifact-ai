use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

use crate::error::AppError;

const MIGRATION_V1: &str = include_str!("../../migrations/001_init.sql");

/// 元数据库访问。M0 阶段用单连接 + 互斥串行化；
/// 接口收敛在此模块内，后续替换为 r2d2 连接池或 PostgreSQL 不影响上层。
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(MIGRATION_V1)?;
            conn.pragma_update(None, "user_version", 1)?;
        }

        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T, AppError>) -> Result<T, AppError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| AppError::internal("db lock poisoned"))?;
        f(&conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    #[test]
    fn migration_creates_all_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.db")).unwrap();
        db.with(|conn| {
            for table in [
                "users", "sessions", "groups", "group_members", "repos", "revisions",
                "changes", "head_entries", "blobs", "acl_rules", "locks", "gc_queue",
                "pending_commits", "audit_log",
            ] {
                let n: i64 = conn.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )?;
                assert_eq!(n, 1, "表 {table} 应存在");
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        Db::open(&path).unwrap();
        Db::open(&path).unwrap(); // 第二次打开不应重复执行迁移
    }
}
