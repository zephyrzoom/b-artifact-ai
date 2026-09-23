//! 存储层：SQLite 连接与迁移（§3.1 / §3.2）。

use rusqlite::{params, Connection};
use std::path::Path;

/// 迁移清单：按序应用，已应用过的跳过。
const MIGRATIONS: &[(&str, &str)] = &[
    ("001_init", include_str!("../../migrations/001_init.sql")),
    ("002_m1", include_str!("../../migrations/002_m1.sql")),
    ("003_perf", include_str!("../../migrations/003_perf.sql")),
    ("004_m2", include_str!("../../migrations/004_m2.sql")),
    ("005_m3", include_str!("../../migrations/005_m3.sql")),
    (
        "006_fix_changed_rev",
        include_str!("../../migrations/006_fix_changed_rev.sql"),
    ),
    (
        "007_lock_model",
        include_str!("../../migrations/007_lock_model.sql"),
    ),
];

/// 打开 SQLite 连接：WAL + 外键 + busy timeout（§3.1、§10.1）。
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

/// 应用全部未执行的迁移（事务内，可重复执行）。
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    TEXT PRIMARY KEY,
             applied_at TEXT NOT NULL
         )",
        [],
    )?;
    let tx = conn.transaction()?;
    let mut applied_now: Vec<&str> = vec![];
    for (version, sql) in MIGRATIONS {
        let applied = match tx.query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            params![version],
            |_| Ok(true),
        ) {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(e) => return Err(e),
        };
        if !applied {
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                params![version, chrono::Utc::now().to_rfc3339()],
            )?;
            applied_now.push(version);
        }
    }
    tx.commit()?;

    // 003_perf 新增 parent_path / 目录物化：存量库需要回填（空库跳过，代价为 0）。
    if applied_now.contains(&"003_perf") {
        backfill_head_tree(conn)?;
    }
    Ok(())
}

/// 003_perf 的存量回填：逐个仓库重建 HEAD 目录树（parent_path + 物化目录行）。
fn backfill_head_tree(conn: &mut Connection) -> rusqlite::Result<()> {
    // 003 之前只有 mkdir 会往 head_entries 写目录行（隐式祖先不入表），所以存量目录行
    // 一律是显式的——先把它们标记好，再重建，否则会被重算成隐式、清空后遭误回收。
    conn.execute("UPDATE head_entries SET is_explicit = 1 WHERE kind = 'dir'", [])?;
    let ids: Vec<i64> = {
        let mut out = vec![];
        let mut stmt = conn.prepare("SELECT id FROM repos")?;
        for row in stmt.query_map([], |r| r.get(0))? {
            out.push(row?);
        }
        out
    };
    for id in ids {
        crate::storage::repo::rebuild_head_tree(conn, id)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// 每个用例独立的 tempdir + 独立 SQLite 文件（§12.2 测试环境约定）。
    pub fn test_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut conn = open(&dir.path().join("b-artifact.db")).expect("open db");
        migrate(&mut conn).expect("migrate");
        (dir, conn)
    }
}
