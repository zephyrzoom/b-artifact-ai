//! 清除历史（purge，L2）——§3.6。
//!
//! 与 L1（普通删除）的差别必须记牢：purge **不写 Tombstone**，是把该前缀在
//! **所有修订**中的记录连同内容一并抹除，路径如同从未存在过，**不可逆**。
//!
//! 流程（方案 §3.6 步骤 1~5，实现严格对应）：
//!
//! 1. 前置校验：请求者对该目录有 `admin` 权限且是系统管理员（由 API 层负责）；
//!    该前缀下若存在**他人**持有的锁则拒绝（409 `PURGE_BLOCKED`），本人锁自动释放并留痕。
//! 2. 删除路径历史：`changes` + `head_entries` 中该前缀（含子树）全部行。
//! 3. 修订统计修正：受影响的每个修订重算 `file_count` / `byte_delta`；
//!    `manifest_hash` 仅 HEAD 重算。
//! 4. 引用计数回收：按新 `changes` 重建受影响 blob 的 refcount，降为 0 的进 GC 队列。
//! 5. 审计：把完整快照清单（路径数、修订范围、blob 数、回收字节）写审计日志。
//!
//! 成本与"该前缀出现过的修订数"成正比，与仓库总修订数无关。

use crate::error::AppError;
use crate::storage::gc;
use crate::storage::repo::{prefix_bounds, rebuild_head_tree_tx, validate_path};
use chrono::Utc;
use rusqlite::{params, Connection};

#[derive(Debug, Clone)]
pub struct PurgeSummary {
    pub prefix: String,
    /// 被抹除的**不同路径**数
    pub paths_removed: i64,
    /// 受影响的修订数
    pub revisions_affected: i64,
    /// 受影响修订的区间（空 = 没有任何修订受影响）
    pub rev_range: Option<(i64, i64)>,
    /// refcount 降为 0、进入 GC 队列的 blob 数
    pub blobs_reclaimed: i64,
    /// 这些 blob 占的落盘字节（stored_size）
    pub bytes_reclaimed: i64,
    /// 因 purge 被自动释放的本人锁数量
    pub locks_released: i64,
    /// GC 队列项的到期时间（purge 触发的立即可回收）
    pub gc_scheduled_at: String,
}

/// 在**调用方的事务内**执行 purge。
///
/// 与提交共用 `head_rev` 乐观锁通道做写互斥（§3.6 实现要点）：调用方必须在
/// `BEGIN IMMEDIATE` 事务里先读一次 `head_rev` 再调本函数，避免
/// "purge 到一半，新提交又写回同前缀" 的竞态。
pub fn purge_prefix_tx(
    conn: &Connection,
    repo_id: i64,
    prefix: &str,
    actor_id: i64,
) -> Result<PurgeSummary, AppError> {
    if prefix.is_empty() {
        return Err(AppError::InvalidArgument(
            "purge 需要非空路径前缀；要清空整个仓库请直接删除仓库".into(),
        ));
    }
    validate_path(prefix)?;
    let (lo, hi) = prefix_bounds(&format!("{prefix}/"))
        .ok_or_else(|| AppError::InvalidArgument(format!("路径 `{prefix}` 无法计算子树边界")))?;

    // ---- 步骤 1：锁治理（§15.3 决策 #11） ----
    let locks: Vec<(i64, i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, owner_id, path FROM locks
              WHERE repo_id = ?1 AND broken_at IS NULL
                AND (path = ?2 OR (path >= ?3 AND path < ?4))",
        )?;
        let mut out = vec![];
        for row in stmt.query_map(params![repo_id, prefix, lo, hi], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
        })? {
            out.push(row?);
        }
        out
    };
    let others: Vec<String> = locks
        .iter()
        .filter(|(_, owner, _)| *owner != actor_id)
        .map(|(_, _, p)| p.clone())
        .collect();
    if !others.is_empty() {
        return Err(AppError::Conflict {
            code: "PURGE_BLOCKED",
            message: "目标前缀下存在他人持有的锁，请先强制解锁再清除历史".into(),
            details: serde_json::json!({ "paths": others }),
        });
    }
    let now = Utc::now().to_rfc3339();
    for (id, _, _) in &locks {
        conn.execute(
            "UPDATE locks SET broken_at = ?1, broken_by = ?2, break_reason = ?3 WHERE id = ?4",
            params![now, actor_id, "purge: 清除历史自动释放本人锁", id],
        )?;
    }
    let locks_released = locks.len() as i64;

    // ---- 步骤 2：删除路径历史（先统计，再删） ----
    let in_range = "repo_id = ?1 AND (path = ?2 OR (path >= ?3 AND path < ?4))";
    let (paths_removed, rev_min, rev_max, revisions_affected): (i64, Option<i64>, Option<i64>, i64) =
        conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT path), MIN(rev), MAX(rev), COUNT(DISTINCT rev)
                   FROM changes WHERE {in_range}"
            ),
            params![repo_id, prefix, lo, hi],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

    let hashes: Vec<String> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT blob_hash FROM changes
              WHERE {in_range} AND blob_hash IS NOT NULL"
        ))?;
        let mut out = vec![];
        for row in stmt.query_map(params![repo_id, prefix, lo, hi], |r| r.get::<_, String>(0))? {
            out.push(row?);
        }
        out
    };

    let affected_revs: Vec<i64> = {
        let mut stmt = conn.prepare(&format!("SELECT DISTINCT rev FROM changes WHERE {in_range} ORDER BY rev"))?;
        let mut out = vec![];
        for row in stmt.query_map(params![repo_id, prefix, lo, hi], |r| r.get::<_, i64>(0))? {
            out.push(row?);
        }
        out
    };

    conn.execute(&format!("DELETE FROM changes WHERE {in_range}"), params![repo_id, prefix, lo, hi])?;
    conn.execute(
        "DELETE FROM head_entries WHERE repo_id = ?1 AND (path = ?2 OR (path >= ?3 AND path < ?4))",
        params![repo_id, prefix, lo, hi],
    )?;

    // ---- 步骤 3：修订统计修正 ----
    // byte_delta 的语义（与提交路径一致）：本仓库此前未引用过的新 blob 的原始大小之和。
    {
        let mut st = conn.prepare(
            "UPDATE revisions
                SET file_count = (SELECT COUNT(*) FROM changes c WHERE c.repo_id = ?1 AND c.rev = ?2),
                    byte_delta = (
                        SELECT COALESCE(SUM(c.size), 0) FROM changes c
                         WHERE c.repo_id = ?1 AND c.rev = ?2 AND c.kind = 'file'
                           AND c.op IN ('add', 'modify') AND c.blob_hash IS NOT NULL
                           AND NOT EXISTS (SELECT 1 FROM changes p
                                            WHERE p.repo_id = c.repo_id AND p.rev < c.rev
                                              AND p.blob_hash = c.blob_hash)
                    )
              WHERE repo_id = ?1 AND rev = ?2",
        )?;
        for rev in &affected_revs {
            st.execute(params![repo_id, rev])?;
        }
    }

    // HEAD 目录树重算：purge 删掉了 head_entries 的子树行，父目录链要跟着收敛
    // （显式空目录已被删除，不会被 ressurrect——这正是"如同从未存在过"的语义）
    // 注意：本函数运行在调用方的事务内，必须用 `_tx` 版本（其内部不再 BEGIN）
    rebuild_head_tree_tx(conn, repo_id)?;

    // ---- 步骤 4：引用计数回收 ----
    gc::rebuild_refcount_for(conn, &hashes)?;
    let (blobs_reclaimed, bytes_reclaimed): (i64, i64) = if hashes.is_empty() {
        (0, 0)
    } else {
        let ph = placeholders(hashes.len());
        conn.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(stored_size), 0) FROM blobs
                  WHERE refcount = 0 AND hash IN ({ph})"
            ),
            rusqlite::params_from_iter(hashes.iter()),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
    };
    let gc_scheduled_at = (Utc::now() + chrono::Duration::seconds(gc::GC_GRACE_SECS)).to_rfc3339();
    // purge 引发的队列项可立即回收（无 prepare 关联，§3.7 关键语义 3）
    gc::enqueue_zero_ref(conn, "purge", 0)?;

    // manifest_hash 仅 HEAD 重算（§3.6 步骤 3）
    crate::storage::repo::refresh_head_manifest_hash(conn, repo_id)?;

    Ok(PurgeSummary {
        prefix: prefix.to_string(),
        paths_removed,
        revisions_affected,
        rev_range: match (rev_min, rev_max) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        },
        blobs_reclaimed,
        bytes_reclaimed,
        locks_released,
        gc_scheduled_at,
    })
}

fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db;
    use rusqlite::params;

    /// 建好用户与仓库，返回仓库 id（head_entries / revisions 都有外键指向 repos）。
    fn seed(conn: &Connection) -> i64 {
        let uid = crate::storage::repo::create_user(conn, "tester", true).unwrap();
        let repo = crate::storage::repo::create_repo(conn, "demo", uid).unwrap();
        // 三笔修订：rev1 建 a.bin / b.bin，rev2 改 a.bin，rev3 删 b.bin + 加 sub/c.bin
        conn.execute(
            "INSERT INTO revisions (repo_id, rev, author_id, message, created_at, file_count, byte_delta, manifest_hash)
             VALUES (?1, 1, 1, 'r1', '2026-01-01T00:00:00+00:00', 2, 20, '')", params![repo]).unwrap();
        conn.execute(
            "INSERT INTO revisions (repo_id, rev, author_id, message, created_at, file_count, byte_delta, manifest_hash)
             VALUES (?1, 2, 1, 'r2', '2026-01-01T00:00:00+00:00', 1, 0, '')", params![repo]).unwrap();
        conn.execute(
            "INSERT INTO revisions (repo_id, rev, author_id, message, created_at, file_count, byte_delta, manifest_hash)
             VALUES (?1, 3, 1, 'r3', '2026-01-01T00:00:00+00:00', 2, 30, '')", params![repo]).unwrap();
        let rows = [
            (1, "a.bin", "add", "h_a1", 10i64),
            (1, "b.bin", "add", "h_b", 10),
            (2, "a.bin", "modify", "h_a2", 10),
            (3, "b.bin", "delete", "h_b", 10),
            (3, "sub/c.bin", "add", "h_c", 30),
        ];
        for (rev, path, op, hash, size) in rows {
            conn.execute(
                "INSERT INTO changes (repo_id, rev, path, op, kind, blob_hash, size, mode, mtime)
                 VALUES (?1, ?2, ?3, ?4, 'file', ?5, ?6, 420, 0)",
                params![repo, rev, path, op, hash, size],
            )
            .unwrap();
        }
        for (h, size) in [("h_a1", 10i64), ("h_a2", 10), ("h_b", 10), ("h_c", 30)] {
            conn.execute(
                "INSERT INTO blobs (hash, size, stored_size, codec, refcount, created_at)
                 VALUES (?1, ?2, ?2, 'raw', 1, '2026-01-01T00:00:00+00:00')",
                params![h, size],
            )
            .unwrap();
        }
        // head_entries：a.bin / sub / sub/c.bin（b.bin 已删）
        conn.execute(
            "INSERT INTO head_entries (repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, parent_path, is_explicit)
             VALUES (?1, 'a.bin', 'file', 'h_a2', 10, 420, 0, 2, '', 0)", params![repo]).unwrap();
        conn.execute(
            "INSERT INTO head_entries (repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, parent_path, is_explicit)
             VALUES (?1, 'sub', 'dir', NULL, 0, 0, 0, 3, '', 0)", params![repo]).unwrap();
        conn.execute(
            "INSERT INTO head_entries (repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, parent_path, is_explicit)
             VALUES (?1, 'sub/c.bin', 'file', 'h_c', 30, 420, 0, 3, 'sub', 0)", params![repo]).unwrap();
        repo
    }

    fn changes_count(conn: &Connection, repo: i64) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM changes WHERE repo_id = ?1", params![repo], |r| r.get(0)).unwrap()
    }

    #[test]
    fn purge_removes_subtree_from_all_revisions() {
        let (_d, conn) = db::tests::test_db();
        let repo = seed(&conn);

        let s = purge_prefix_tx(&conn, repo, "sub", 1).unwrap();
        assert_eq!(s.paths_removed, 1, "sub/c.bin");
        assert_eq!(s.revisions_affected, 1);
        assert_eq!(s.blobs_reclaimed, 1, "h_c 不再被引用");
        assert_eq!(s.bytes_reclaimed, 30);
        // head_entries 子树连带删除
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM head_entries WHERE repo_id = ?1", params![repo], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1, "只剩 a.bin（sub 是隐式目录，无子项后应被回收）");
        // 队列：h_c 已入队
        let q: i64 = conn.query_row("SELECT COUNT(*) FROM gc_queue WHERE reason = 'purge'", [], |r| r.get(0)).unwrap();
        assert_eq!(q, 1);
    }

    #[test]
    fn purge_recomputes_revision_stats() {
        let (_d, conn) = db::tests::test_db();
        let repo = seed(&conn);

        let s = purge_prefix_tx(&conn, repo, "b.bin", 1).unwrap();
        assert_eq!(s.revisions_affected, 2, "rev1(add) 与 rev3(delete) 都提过 b.bin");
        // rev1 只剩 a.bin → file_count=1、byte_delta=10（h_b 已消失）
        let (fc, bd): (i64, i64) = conn
            .query_row("SELECT file_count, byte_delta FROM revisions WHERE repo_id = ?1 AND rev = 1", params![repo], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(fc, 1);
        assert_eq!(bd, 10);
        // rev3 只剩 sub/c.bin
        let (fc, bd): (i64, i64) = conn
            .query_row("SELECT file_count, byte_delta FROM revisions WHERE repo_id = ?1 AND rev = 3", params![repo], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(fc, 1);
        assert_eq!(bd, 30);
        // rev2 未受影响，保持原值
        let fc: i64 = conn
            .query_row("SELECT file_count FROM revisions WHERE repo_id = ?1 AND rev = 2", params![repo], |r| r.get(0))
            .unwrap();
        assert_eq!(fc, 1);
        assert_eq!(changes_count(&conn, repo), 3);
    }

    #[test]
    fn purge_refuses_when_others_hold_locks() {
        let (_d, conn) = db::tests::test_db();
        let repo = seed(&conn);
        let other = crate::storage::repo::create_user(&conn, "other", false).unwrap();
        conn.execute(
            "INSERT INTO locks (repo_id, path, kind, owner_id, token, comment, created_at, expires_at)
             VALUES (?1, 'sub/c.bin', 'file', ?2, 'tok', '', '2026-01-01T00:00:00+00:00', NULL)",
            params![repo, other],
        )
        .unwrap();

        let err = purge_prefix_tx(&conn, repo, "sub", 1).unwrap_err();
        assert_eq!(err.code(), "PURGE_BLOCKED");
        assert!(err.to_string().contains("他人"));
        // 未改动任何数据
        assert_eq!(changes_count(&conn, repo), 5);
    }

    #[test]
    fn purge_releases_own_locks_with_trace() {
        let (_d, conn) = db::tests::test_db();
        let repo = seed(&conn);
        conn.execute(
            "INSERT INTO locks (repo_id, path, kind, owner_id, token, comment, created_at, expires_at)
             VALUES (?1, 'sub', 'dir', ?2, 'tok', '', '2026-01-01T00:00:00+00:00', NULL)",
            params![repo, 1],
        )
        .unwrap();

        let s = purge_prefix_tx(&conn, repo, "sub", 1).unwrap();
        assert_eq!(s.locks_released, 1);
        let (broken_at, by): (Option<String>, Option<i64>) = conn
            .query_row("SELECT broken_at, broken_by FROM locks WHERE repo_id = ?1", params![repo], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert!(broken_at.is_some(), "自动释放要留痕");
        assert_eq!(by, Some(1));
    }

    #[test]
    fn purge_rejects_empty_prefix() {
        let (_d, conn) = db::tests::test_db();
        let repo = seed(&conn);
        assert!(purge_prefix_tx(&conn, repo, "", 1).is_err());
    }
}
