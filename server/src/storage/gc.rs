//! Blob 引用计数重建与 GC（§3.7）。
//!
//! 关键语义（与方案一致，实现不可偏离）：
//!
//! 1. **`changes` 表是引用事实来源**，`blobs.refcount` 只是**可重建的缓存**。
//!    任何不一致（崩溃、bug、purge 事故）都靠 [`rebuild_refcount`] 自愈，不需要人工修库。
//! 2. **orphan blob 是正常的**：`PUT /blobs` 成功但提交从未落地 → refcount=0，
//!    走统一的 GC 队列，宽限期后回收。
//! 3. **GC 最终复查必须排除 pending_commits**：refcount=0 的 blob 可能正被某个
//!    5 分钟内未过期的 prepare 等待上传，物理删除前必须查一次（§3.7 关键语义 3）。
//! 4. **宽限期可撤销**：任何提交重新引用该 blob 时出队（由 commit 路径负责）。

use crate::error::AppError;
use crate::storage::blob::BlobStore;
use chrono::Utc;
use rusqlite::{params, Connection};

/// 宽限期：refcount 归零后多久才允许物理删除（默认 24h）。
pub const GC_GRACE_SECS: i64 = 24 * 60 * 60;

/// refcount 定义：引用该 blob 的 **(repo_id, rev) 组合数**。
///
/// 与提交路径一致——`commit` 对每个 commit 内的去重 blob 各 +1
/// （见 `api/commit.rs` 的 `referenced_blobs`），因此同一修订里
/// 同一 blob 出现 100 次也只算 1。
const REFCOUNT_EXPR: &str =
    "(SELECT COUNT(DISTINCT c.repo_id || ':' || c.rev) FROM changes c WHERE c.blob_hash = blobs.hash)";

#[derive(Debug, Clone, Default)]
pub struct RebuildStats {
    pub blobs: i64,
    /// 重建后 refcount = 0 的 blob 数（= 可回收候选）
    pub zero_ref: i64,
    /// 这些 blob 占的**落盘**字节（stored_size，压缩后）
    pub reclaimable_bytes: i64,
}

/// 全量重建 refcount（自愈入口，§3.7 `rebuild_refcount()`）。
///
/// 成本 O(blobs × log changes)，靠 `idx_changes_blob_hash`（迁移 005）走索引。
pub fn rebuild_refcount(conn: &Connection) -> Result<RebuildStats, AppError> {
    let blobs: i64 = conn.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))?;
    conn.execute(
        &format!("UPDATE blobs SET refcount = {REFCOUNT_EXPR}"),
        [],
    )?;
    let (zero_ref, reclaimable_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(stored_size), 0) FROM blobs WHERE refcount = 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(RebuildStats { blobs, zero_ref, reclaimable_bytes })
}

/// 只重建给定 hash 集合的 refcount（purge 场景：受影响 blob 是少数）。
pub fn rebuild_refcount_for(conn: &Connection, hashes: &[String]) -> Result<usize, AppError> {
    let mut n = 0;
    let mut stmt = conn.prepare_cached(&format!(
        "UPDATE blobs SET refcount = {REFCOUNT_EXPR} WHERE hash = ?1"
    ))?;
    for h in hashes {
        n += stmt.execute(params![h])?;
    }
    Ok(n)
}

/// 把当前 refcount=0 的 blob 放入 GC 队列；同时把"复活"的出队（§3.7 关键语义 4）。
///
/// `grace_secs` = 0 表示立即可回收（purge 场景，§3.7 关键语义 3 末句）。
pub fn enqueue_zero_ref(conn: &Connection, reason: &str, grace_secs: i64) -> Result<i64, AppError> {
    let now = Utc::now();
    let due = now + chrono::Duration::seconds(grace_secs);
    let n = conn.execute(
        "INSERT OR IGNORE INTO gc_queue (blob_hash, repo_id, reason, queued_at, due_at)
         SELECT b.hash, NULL, ?1, ?2, ?3 FROM blobs b WHERE b.refcount = 0",
        params![reason, now.to_rfc3339(), due.to_rfc3339()],
    )?;
    // 重新被引用 → 出队（宽限期可撤销）
    conn.execute(
        "DELETE FROM gc_queue WHERE blob_hash IN (SELECT hash FROM blobs WHERE refcount > 0)",
        [],
    )?;
    // blobs 行已不存在（上一轮 GC 已删）→ 队列项是垃圾
    conn.execute(
        "DELETE FROM gc_queue WHERE blob_hash NOT IN (SELECT hash FROM blobs)",
        [],
    )?;
    Ok(n as i64)
}

#[derive(Debug, Clone, Default)]
pub struct GcStats {
    /// 物理删除的 blob 数
    pub deleted: i64,
    /// 因 pending_commits 仍引用而跳过本轮的数（§3.7 关键语义 3）
    pub skipped_pending: i64,
    /// 释放的落盘字节
    pub bytes: i64,
}

/// 处理已到期的队列项：最终复查 → 删磁盘文件 + 删 blobs 行 + 出队。
///
/// 删除是**幂等**的（文件不存在视为成功）。每次最多处理 `limit` 项，避免长事务。
pub fn run_due(store: &BlobStore, conn: &Connection, limit: usize) -> Result<GcStats, AppError> {
    let now = Utc::now().to_rfc3339();
    let due: Vec<(String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT q.blob_hash, COALESCE(b.stored_size, 0)
               FROM gc_queue q LEFT JOIN blobs b ON b.hash = q.blob_hash
              WHERE q.due_at <= ?1
              LIMIT ?2",
        )?;
        let mut out = vec![];
        for row in stmt.query_map(params![now, limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })? {
            out.push(row?);
        }
        out
    };

    let mut stats = GcStats::default();
    let mut st_pending = conn.prepare_cached(
        "SELECT 1 FROM pending_commits
          WHERE expires_at > ?1 AND need_blobs LIKE '%' || ?2 || '%' LIMIT 1",
    )?;
    let mut st_del_blob = conn.prepare_cached("DELETE FROM blobs WHERE hash = ?1")?;
    let mut st_del_q = conn.prepare_cached("DELETE FROM gc_queue WHERE blob_hash = ?1")?;

    for (hash, stored_size) in due {
        let pending = {
            let mut rows = st_pending.query(params![now, hash])?;
            rows.next()?.is_some()
        };
        if pending {
            stats.skipped_pending += 1;
            continue;
        }
        // 幂等：文件可能已被上一轮删掉
        let _ = std::fs::remove_file(store.blob_path(&hash));
        st_del_blob.execute(params![hash])?;
        st_del_q.execute(params![hash])?;
        stats.deleted += 1;
        stats.bytes += stored_size;
    }
    Ok(stats)
}

/// 队列与磁盘的当前状态（管理页 §8.2「系统设置 / 维护」展示用）。
#[derive(Debug, Clone, Default)]
pub struct GcStatus {
    pub queued: i64,
    pub due_now: i64,
    pub queued_bytes: i64,
    pub orphan_blobs: i64,
}

pub fn status(conn: &Connection) -> Result<GcStatus, AppError> {
    let now = Utc::now().to_rfc3339();
    let (queued, due_now): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(CASE WHEN due_at <= ?1 THEN 1 ELSE 0 END), 0)
           FROM gc_queue",
        params![now],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let queued_bytes: i64 = conn.query_row(
        "SELECT COALESCE(SUM(b.stored_size), 0)
           FROM gc_queue q JOIN blobs b ON b.hash = q.blob_hash",
        [],
        |r| r.get(0),
    )?;
    let orphan_blobs: i64 = conn.query_row("SELECT COUNT(*) FROM blobs WHERE refcount = 0", [], |r| r.get(0))?;
    Ok(GcStatus { queued, due_now, queued_bytes, orphan_blobs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::blob::BlobStore;
    use crate::storage::db;
    use rusqlite::params;

    fn put_blob(conn: &Connection, store: &BlobStore, data: &[u8]) -> String {
        let sb = store.put(conn, data).unwrap();
        conn.execute("UPDATE blobs SET refcount = 0 WHERE hash = ?1", params![sb.hash])
            .unwrap();
        sb.hash
    }

    fn reference(conn: &Connection, repo: i64, rev: i64, hash: &str) {
        conn.execute(
            "INSERT INTO changes (repo_id, rev, path, op, kind, blob_hash, size, mode, mtime)
             VALUES (?1, ?2, 'a.bin', 'add', 'file', ?3, 10, 420, 0)",
            params![repo, rev, hash],
        )
        .unwrap();
    }

    #[test]
    fn rebuild_counts_distinct_repo_rev_pairs() {
        let (_d, conn) = db::tests::test_db();
        conn.execute("INSERT INTO blobs (hash, size, stored_size, codec, refcount, created_at)
                      VALUES ('h1', 10, 10, 'raw', 99, '2026-01-01T00:00:00+00:00')", [])
            .unwrap();
        // 仓库 1 的 rev 1 引用两次（同一修订内重复出现只算 1 —— 主键是 (repo,rev,path)，
        // 所以"引用两次"体现为两条不同路径指向同一 blob）
        reference(&conn, 1, 1, "h1");
        conn.execute(
            "INSERT INTO changes (repo_id, rev, path, op, kind, blob_hash, size, mode, mtime)
             VALUES (1, 1, 'dup.bin', 'add', 'file', 'h1', 10, 420, 0)", [])
            .unwrap();
        // 仓库 1 的 rev 2、仓库 2 的 rev 1 各引用一次
        conn.execute(
            "INSERT INTO changes (repo_id, rev, path, op, kind, blob_hash, size, mode, mtime)
             VALUES (1, 2, 'b.bin', 'add', 'file', 'h1', 10, 420, 0)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO changes (repo_id, rev, path, op, kind, blob_hash, size, mode, mtime)
             VALUES (2, 1, 'c.bin', 'add', 'file', 'h1', 10, 420, 0)", [])
            .unwrap();

        let st = rebuild_refcount(&conn).unwrap();
        assert_eq!(st.blobs, 1);
        let rc: i64 = conn
            .query_row("SELECT refcount FROM blobs WHERE hash = 'h1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rc, 3, "(1,1) (1,2) (2,1) 三个组合；同修订内重复只算一次");
    }

    #[test]
    fn enqueue_and_run_due_respects_grace_and_pending() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = db::open(&dir.path().join("db.sqlite")).unwrap();
        db::migrate(&mut conn).unwrap();
        let store = BlobStore::new(dir.path()).unwrap();

        let h = put_blob(&conn, &store, b"hello gc");
        // 宽限期 24h → 不到期
        enqueue_zero_ref(&conn, "test", GC_GRACE_SECS).unwrap();
        assert_eq!(status(&conn).unwrap().queued, 1);
        let st = run_due(&store, &conn, 100).unwrap();
        assert_eq!(st.deleted, 0, "宽限期内不删");
        assert_eq!(st.skipped_pending, 0);

        // 到期：把 due_at 改到过去
        conn.execute("UPDATE gc_queue SET due_at = '2000-01-01T00:00:00+00:00'", [])
            .unwrap();

        // 最终复查：pending_commits 未过期且引用该 blob → 跳过
        conn.execute(
            "INSERT INTO pending_commits (commit_token, commit_id, repo_id, user_id, base_rev,
                 changes_hash, need_blobs, created_at, expires_at)
             VALUES ('t', 'c', 1, 1, 0, 'h', ?1, '2026-01-01T00:00:00+00:00',
                     '2099-01-01T00:00:00+00:00')",
            params![serde_json::json!([h]).to_string()],
        )
        .unwrap();
        let st = run_due(&store, &conn, 100).unwrap();
        assert_eq!(st.skipped_pending, 1);
        assert_eq!(st.deleted, 0);

        // 清掉 pending → 这次真删
        conn.execute("DELETE FROM pending_commits", []).unwrap();
        let st = run_due(&store, &conn, 100).unwrap();
        assert_eq!(st.deleted, 1);
        assert!(!store.blob_path(&h).exists(), "磁盘文件应已删除");
        assert_eq!(status(&conn).unwrap().queued, 0);
    }
}
