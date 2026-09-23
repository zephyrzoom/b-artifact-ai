//! 锁服务（方案 §5）：**文件锁**、强制解锁留痕、心跳续期。
//!
//! 三条铁律：
//!
//! 1. **互斥 = 同一路径**（§5.1，v0.4.17 收敛）：一把锁只保护一个文件路径。
//!    原"目录锁递归覆盖子树"已整体删除 —— "路径区间相交"那套判定（祖先链回溯 + 子树扫描）
//!    随之消失，加锁只需一次精确点查。
//! 2. **过期即失效**（§5.5）：有效性判定只看 `expires_at`（NULL = 永不过期），
//!    过期锁不阻塞任何人，行保留由惰性清理回收。
//! 3. **强制解锁留痕**：强制解锁不删行，写 `broken_at` / `break_by` / `break_reason`，
//!    且 reason 必填（§5.5）。
//!
//! `locks.kind` 列保留（存量库与 wc.db 对齐），但**恒为 `'file'`**，代码里不再有该维度。

use crate::error::AppError;
use crate::storage::repo::validate_path;
use chrono::{Duration, Utc};
use rusqlite::{params, Connection};

/// 锁默认 TTL（§5.5：管理页可配，默认 7 天）。
pub const DEFAULT_TTL_SECS: i64 = 7 * 24 * 3600;

/// `locks.kind` 的固定值。列保留只为兼容存量库，写读都走这个常量。
pub const LOCK_KIND: &str = "file";

/// `locks` 一行。
#[derive(Debug, Clone)]
pub struct LockRow {
    pub id: i64,
    pub path: String,
    pub owner_id: i64,
    /// 服务端留的明文 token 的 sha256；明文只在创建响应里回一次。
    pub token_hash: String,
    pub comment: String,
    pub created_at: String,
    /// NULL = 永不过期
    pub expires_at: Option<String>,
    pub broken_at: Option<String>,
    pub broken_by: Option<i64>,
    pub break_reason: String,
    // ---- join users 的展示字段（列表用） ----
    pub owner_name: String,
    pub broken_by_name: Option<String>,
}

impl LockRow {
    /// 有效性（§5.5）：未被强制解锁 且（无到期时间 或 尚未到期）。
    pub fn is_live(&self, now: &str) -> bool {
        if self.broken_at.is_some() {
            return false;
        }
        match &self.expires_at {
            None => true,
            Some(exp) => exp.as_str() > now,
        }
    }
}

/// 冲突项：已存在的、阻止本次加锁的锁。
///
/// v0.4.17 后只剩一种关系（同一路径），`relation` 字段保留以便老客户端不改也能读懂
/// —— 值恒为 `"self"`。
#[derive(Debug, Clone)]
pub struct LockConflict {
    pub path: String,
    pub owner_id: i64,
    pub owner_name: String,
    pub relation: &'static str,
}

// ---------- 读取 ----------

const SELECT_COLS: &str = "
    l.id, l.path, l.owner_id, l.token, l.comment, l.created_at,
    l.expires_at, l.broken_at, l.broken_by, l.break_reason,
    u.username, bu.username";

const SELECT_FROM: &str = "
      FROM locks l
      JOIN users u ON u.id = l.owner_id
      LEFT JOIN users bu ON bu.id = l.broken_by";

fn row_to_lock(r: &rusqlite::Row<'_>) -> rusqlite::Result<LockRow> {
    Ok(LockRow {
        id: r.get(0)?,
        path: r.get(1)?,
        owner_id: r.get(2)?,
        token_hash: r.get(3)?,
        comment: r.get(4)?,
        created_at: r.get(5)?,
        expires_at: r.get(6)?,
        broken_at: r.get(7)?,
        broken_by: r.get(8)?,
        break_reason: r.get(9)?,
        owner_name: r.get(10)?,
        broken_by_name: r.get(11)?,
    })
}

/// 列出仓库的锁。`owner` 为 `Some(uid)` 时只看该用户的（§5.5 `?owner=me` 跨副本管理）。
///
/// `include_broken = true` 时一并返回已强制解锁的历史记录（管理页按"最近被强制解锁"排序）。
pub fn list_locks(
    conn: &Connection,
    repo_id: i64,
    owner: Option<i64>,
    include_broken: bool,
) -> Result<Vec<LockRow>, AppError> {
    // 占位符与参数都按条件拼装（`include_broken` 时没有 `now` 参数），
    // 因此用顺序 `?` 而不是硬编码的 ?2/?3。
    let mut sql = format!("SELECT {SELECT_COLS} {SELECT_FROM} WHERE l.repo_id = ?");
    let mut binds: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Integer(repo_id)];
    if !include_broken {
        sql.push_str(" AND l.broken_at IS NULL AND (l.expires_at IS NULL OR l.expires_at > ?)");
        binds.push(rusqlite::types::Value::Text(Utc::now().to_rfc3339()));
    }
    if let Some(o) = owner {
        sql.push_str(" AND l.owner_id = ?");
        binds.push(rusqlite::types::Value::Integer(o));
    }
    sql.push_str(" ORDER BY l.path");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(binds), row_to_lock)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 精确路径上的锁（不论生效与否，调用方自行判 `is_live`）。
pub fn get_lock(conn: &Connection, repo_id: i64, path: &str) -> Result<Option<LockRow>, AppError> {
    let sql = format!(
        "SELECT {SELECT_COLS} {SELECT_FROM} WHERE l.repo_id = ?1 AND l.path = ?3"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query_map(params![repo_id, Utc::now().to_rfc3339(), path], row_to_lock)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// 某路径上的**有效**锁（§5.2 锁所有权判定）。
///
/// v0.4.17：函数名沿用（调用方不需要知道细节），但实现退化成"精确点查 + 有效期判定"。
/// 原先的"沿祖先链找 dir 锁"随目录锁一起删除 —— 这是一次真实的简化：
/// 旧实现每次对每个变更路径都要做最多 N（目录深度）次点查。
/// 走 `idx_locks_path(repo_id, path)`。
pub fn covering_lock(
    conn: &Connection,
    repo_id: i64,
    path: &str,
) -> Result<Option<LockRow>, AppError> {
    let now = Utc::now().to_rfc3339();
    match get_lock(conn, repo_id, path)? {
        Some(l) if l.is_live(&now) => Ok(Some(l)),
        _ => Ok(None),
    }
}

// ---------- 加锁 ----------

/// 加锁。同一路径被**本人**持锁时幂等返回现有锁（重开客户端重复加锁不应报错）。
///
/// 冲突时返回 `409 LOCKED` + 冲突详情（§5.1）。目录路径不参与锁模型
/// （§5.2：目录条目不是变更路径），但这里不做"路径必须是文件"的判定 ——
/// 服务端手上有 `head_entries` 也未必覆盖未纳管的新文件，判断留给调用方。
pub fn acquire(
    conn: &Connection,
    repo_id: i64,
    path: &str,
    user_id: i64,
    comment: &str,
    ttl_secs: Option<i64>,
) -> Result<Result<LockRow, Vec<LockConflict>>, AppError> {
    validate_path(path)?;
    let now = Utc::now();
    let now_s = now.to_rfc3339();

    // 唯一一种冲突：同一路径已被**他人**持有效锁。
    // 本人持有 → 幂等返回；已失效（过期/已破）→ 清掉旧行，让 UNIQUE(repo_id, path) 可重用。
    if let Some(existing) = get_lock(conn, repo_id, path)? {
        if existing.is_live(&now_s) {
            if existing.owner_id == user_id {
                // 幂等返回，但**备注要跟着更新**：用户重新加锁时写的新备注
                // 被静默丢掉会让人以为"我说了这句话"，而列表里还是旧文案。
                // 空备注不动原值（避免"顺手再调一次"把已有说明抹掉）。
                if !comment.is_empty() && comment != existing.comment {
                    conn.execute(
                        "UPDATE locks SET comment = ?2 WHERE id = ?1",
                        params![existing.id, comment],
                    )?;
                    let mut row = existing;
                    row.comment = comment.to_string();
                    return Ok(Ok(row));
                }
                return Ok(Ok(existing));
            }
            return Ok(Err(vec![LockConflict {
                path: existing.path.clone(),
                owner_id: existing.owner_id,
                owner_name: existing.owner_name.clone(),
                relation: "self",
            }]));
        }
        conn.execute("DELETE FROM locks WHERE id = ?1", params![existing.id])?;
    }

    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|e| AppError::Internal(format!("CSPRNG: {e}")))?;
    let token = hex::encode(raw);
    let expires_at = ttl_secs.map(|s| (now + Duration::seconds(s)).to_rfc3339());
    conn.execute(
        "INSERT INTO locks (repo_id, path, kind, owner_id, token, comment, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            repo_id,
            path,
            LOCK_KIND,
            user_id,
            sha256_hex(&token),
            comment,
            now_s,
            expires_at,
        ],
    )?;
    let id = conn.last_insert_rowid();
    // 明文 token 不落库；回给调用方一次（§5.1：后续提交凭 token 证明所有权）
    let mut row = get_lock(conn, repo_id, path)?.ok_or_else(|| {
        AppError::Internal("加锁后读回失败".into())
    })?;
    row.id = id;
    Ok(Ok(LockRow { token_hash: token, ..row }))
}

// ---------- 释放 / 强制解锁 / 续期 ----------

/// 释放自己的锁（物理删行）。`token` 给定时校验所有权凭据（§5.1）。
pub fn release(
    conn: &Connection,
    repo_id: i64,
    path: &str,
    user_id: i64,
    token: Option<&str>,
) -> Result<(), AppError> {
    let lock = get_lock(conn, repo_id, path)?
        .ok_or_else(|| AppError::NotFound(format!("路径 `{path}` 上没有锁")))?;
    if lock.broken_at.is_some() {
        return Err(AppError::NotFound(format!("路径 `{path}` 上的锁已被强制解锁")));
    }
    if lock.owner_id != user_id {
        return Err(AppError::PermissionDenied(format!(
            "路径 `{path}` 的锁属于 `{}`，不能释放（强制解锁需本目录 admin 权限）",
            lock.owner_name
        )));
    }
    if let Some(t) = token {
        if sha256_hex(t) != lock.token_hash {
            return Err(AppError::PermissionDenied("lock token 不匹配".into()));
        }
    }
    conn.execute("DELETE FROM locks WHERE id = ?1", params![lock.id])?;
    Ok(())
}

/// 强制解锁（§5.5）：**留痕不删行**，且 `reason` 必填。返回被强制解锁的锁数量（v0.4.17 恒为 1）。
///
/// v0.4.17：不再有"破 dir 锁连带释放整个子树"——一次强制解锁只影响一个路径。
pub fn break_lock(
    conn: &Connection,
    repo_id: i64,
    path: &str,
    by_user_id: i64,
    reason: &str,
) -> Result<usize, AppError> {
    if reason.trim().is_empty() {
        return Err(AppError::InvalidArgument("强制解锁必须填写 reason（审计必填字段）".into()));
    }
    let now = Utc::now().to_rfc3339();
    let lock = get_lock(conn, repo_id, path)?
        .ok_or_else(|| AppError::NotFound(format!("路径 `{path}` 上没有锁")))?;
    if lock.broken_at.is_some() {
        return Err(AppError::NotFound(format!("路径 `{path}` 上的锁已被强制解锁")));
    }
    conn.execute(
        "UPDATE locks SET broken_at = ?1, broken_by = ?2, break_reason = ?3 WHERE id = ?4",
        params![now, by_user_id, reason, lock.id],
    )?;
    Ok(1)
}

/// 心跳续期（§5.5）：返回新的 `expires_at`（绝对到期时间，避免客户端时钟漂移）。
///
/// `ttl_secs = None` 表示改为永不过期。只对**本人持有的、未被破的**锁生效。
pub fn refresh(
    conn: &Connection,
    repo_id: i64,
    owner_id: i64,
    ttl_secs: Option<i64>,
) -> Result<(usize, Option<String>), AppError> {
    let now = Utc::now();
    let expires_at = ttl_secs.map(|s| (now + Duration::seconds(s)).to_rfc3339());
    let n = conn.execute(
        "UPDATE locks SET expires_at = ?1
          WHERE repo_id = ?2 AND owner_id = ?3 AND broken_at IS NULL",
        params![expires_at, repo_id, owner_id],
    )?;
    Ok((n, expires_at))
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// 供 API 层把冲突清单转成 409 的 details。
pub fn conflicts_json(conflicts: &[LockConflict]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = conflicts
        .iter()
        .map(|c| {
            serde_json::json!({
                "path": c.path,
                "kind": LOCK_KIND,
                "owner_id": c.owner_id,
                "owner": c.owner_name,
                "relation": c.relation,
            })
        })
        .collect();
    serde_json::json!({ "locks": items })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db;
    use crate::storage::repo::{create_repo, create_user};

    fn setup() -> (tempfile::TempDir, Connection, i64, i64, i64) {
        let (dir, conn) = db::tests::test_db();
        let alice = create_user(&conn, "alice", false).unwrap();
        let bob = create_user(&conn, "bob", false).unwrap();
        let repo = create_repo(&conn, "assets", alice).unwrap();
        (dir, conn, repo, alice, bob)
    }

    fn acq(conn: &Connection, repo: i64, uid: i64, path: &str) -> LockRow {
        match acquire(conn, repo, path, uid, "", Some(DEFAULT_TTL_SECS)).unwrap() {
            Ok(l) => l,
            Err(c) => panic!("加锁 `{path}` 意外冲突: {c:?}"),
        }
    }

    #[test]
    fn acquire_and_read_back() {
        let (_d, conn, repo, alice, _bob) = setup();
        let l = acq(&conn, repo, alice, "art/a.psd");
        assert_eq!(l.path, "art/a.psd");
        assert_eq!(l.token_hash.len(), 64, "回给客户端的是明文 token");

        let covered = covering_lock(&conn, repo, "art/a.psd").unwrap().unwrap();
        assert_eq!(covered.owner_id, alice);
    }

    /// v0.4.17 的核心行为变化：锁只保护自己那一个路径。
    #[test]
    fn lock_is_scoped_to_exact_path_only() {
        let (_d, conn, repo, alice, _bob) = setup();
        acq(&conn, repo, alice, "art/char/a.psd");

        // 兄弟文件、同目录其他文件、目录自身 —— 全都不受保护
        assert!(covering_lock(&conn, repo, "art/char/b.psd").unwrap().is_none());
        assert!(covering_lock(&conn, repo, "art/bg/sky.png").unwrap().is_none());
        assert!(covering_lock(&conn, repo, "art/char").unwrap().is_none());
        assert!(covering_lock(&conn, repo, "art/char/deep/new.psd").unwrap().is_none());
    }

    #[test]
    fn file_lock_conflicts_with_other_user() {
        let (_d, conn, repo, alice, bob) = setup();
        acq(&conn, repo, alice, "art/a.psd");
        let err = acquire(&conn, repo, "art/a.psd", bob, "", None).unwrap();
        let conflicts = err.expect_err("他人持锁应冲突");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].relation, "self");
        assert_eq!(conflicts[0].owner_name, "alice");
    }

    /// 相邻路径互不干扰：这是"没有目录锁"之后最该钉住的一条。
    /// 本人重复加锁是幂等的，但**备注要跟新**：用户第二次写的话不能被静默丢掉。
    #[test]
    fn reacquire_updates_comment_but_empty_keeps_previous() {
        let (_d, conn, repo, alice, _bob) = setup();
        acq(&conn, repo, alice, "art/a.psd"); // 备注为空

        let again = acquire(&conn, repo, "art/a.psd", alice, "改贴图", None)
            .unwrap()
            .unwrap();
        assert_eq!(again.comment, "改贴图");

        // 空备注不动原值（"顺手再调一次"不该把已有说明抹掉）
        let third = acquire(&conn, repo, "art/a.psd", alice, "", None).unwrap().unwrap();
        assert_eq!(third.comment, "改贴图");
    }

    #[test]
    fn neighbour_paths_do_not_conflict() {
        let (_d, conn, repo, alice, bob) = setup();
        acq(&conn, repo, alice, "art/char/a.psd");
        assert!(acquire(&conn, repo, "art/char/b.psd", bob, "", None).unwrap().is_ok());
        // 前缀关系也不行：`art/char/a` 与 `art/char/a.psd` 是两个不同路径
        assert!(acquire(&conn, repo, "art/char/a", bob, "", None).unwrap().is_ok());
    }

    #[test]
    fn same_user_reacquire_is_idempotent() {
        let (_d, conn, repo, alice, _bob) = setup();
        let first = acq(&conn, repo, alice, "art/a.psd");
        let second = acquire(&conn, repo, "art/a.psd", alice, "", None)
            .unwrap()
            .expect("本人重复加锁应幂等返回");
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn expired_lock_does_not_block() {
        let (_d, conn, repo, alice, bob) = setup();
        // 直接插入一把已过期的锁
        conn.execute(
            "INSERT INTO locks (repo_id, path, kind, owner_id, token, comment, created_at, expires_at)
             VALUES (?1, 'art/a.psd', 'file', ?2, 'x', '', '2026-01-01T00:00:00+00:00', '2026-01-02T00:00:00+00:00')",
            params![repo, alice],
        )
        .unwrap();
        assert!(
            covering_lock(&conn, repo, "art/a.psd").unwrap().is_none(),
            "过期即失效（§5.5），不阻塞任何人"
        );
        // 且可以被他人接管（旧行惰性回收）
        assert!(acquire(&conn, repo, "art/a.psd", bob, "", None).unwrap().is_ok());
    }

    #[test]
    fn release_requires_ownership() {
        let (_d, conn, repo, alice, bob) = setup();
        acq(&conn, repo, alice, "art/a.psd");
        assert!(release(&conn, repo, "art/a.psd", bob, None).is_err(), "不能释放他人锁");
        release(&conn, repo, "art/a.psd", alice, None).unwrap();
        assert!(covering_lock(&conn, repo, "art/a.psd").unwrap().is_none());
    }

    #[test]
    fn release_checks_token() {
        let (_d, conn, repo, alice, _bob) = setup();
        let l = acq(&conn, repo, alice, "art/a.psd");
        assert!(release(&conn, repo, "art/a.psd", alice, Some("wrong")).is_err());
        release(&conn, repo, "art/a.psd", alice, Some(&l.token_hash)).unwrap();
    }

    #[test]
    fn break_lock_keeps_record() {
        let (_d, conn, repo, alice, bob) = setup();
        acq(&conn, repo, alice, "art/a.psd");
        let n = break_lock(&conn, repo, "art/a.psd", bob, "离职交接").unwrap();
        assert_eq!(n, 1);

        let row = get_lock(&conn, repo, "art/a.psd").unwrap().unwrap();
        assert!(row.broken_at.is_some(), "强制解锁留痕不删行（§5.5）");
        assert_eq!(row.break_reason, "离职交接");
        assert_eq!(row.broken_by, Some(bob));
        assert_eq!(row.broken_by_name.as_deref(), Some("bob"));
        assert!(!row.is_live(&Utc::now().to_rfc3339()));
        assert!(covering_lock(&conn, repo, "art/a.psd").unwrap().is_none());
    }

    /// 强制解锁不再连带任何别的路径（原"破 dir 锁释放整棵子树"已删除）。
    #[test]
    fn break_lock_touches_only_one_path() {
        let (_d, conn, repo, alice, bob) = setup();
        acq(&conn, repo, alice, "art/a.psd");
        acq(&conn, repo, alice, "art/b.psd");

        assert_eq!(break_lock(&conn, repo, "art/a.psd", bob, "测试").unwrap(), 1);
        assert!(covering_lock(&conn, repo, "art/a.psd").unwrap().is_none());
        assert!(
            covering_lock(&conn, repo, "art/b.psd").unwrap().is_some(),
            "相邻路径的锁不受影响"
        );
    }

    #[test]
    fn break_lock_requires_reason() {
        let (_d, conn, repo, alice, bob) = setup();
        acq(&conn, repo, alice, "art/a.psd");
        assert!(break_lock(&conn, repo, "art/a.psd", bob, "  ").is_err(), "reason 必填");
        assert!(break_lock(&conn, repo, "art/a.psd", bob, "").is_err());
    }

    #[test]
    fn refresh_extends_only_own_live_locks() {
        let (_d, conn, repo, alice, bob) = setup();
        acq(&conn, repo, alice, "art/a.psd");
        acq(&conn, repo, bob, "art/b.psd");
        let (n, exp) = refresh(&conn, repo, alice, Some(3600)).unwrap();
        assert_eq!(n, 1, "只续期本人的锁");
        assert!(exp.is_some());
        assert_eq!(
            get_lock(&conn, repo, "art/a.psd").unwrap().unwrap().expires_at,
            exp
        );
        assert_ne!(
            get_lock(&conn, repo, "art/b.psd").unwrap().unwrap().expires_at,
            exp
        );
    }

    #[test]
    fn list_locks_filters_owner_and_broken() {
        let (_d, conn, repo, alice, bob) = setup();
        acq(&conn, repo, alice, "art/a.psd");
        acq(&conn, repo, bob, "art/b.psd");
        break_lock(&conn, repo, "art/b.psd", alice, "测试").unwrap();

        assert_eq!(list_locks(&conn, repo, None, false).unwrap().len(), 1);
        assert_eq!(list_locks(&conn, repo, None, true).unwrap().len(), 2, "含已强制解锁历史");
        let mine = list_locks(&conn, repo, Some(alice), false).unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].path, "art/a.psd");
    }

    #[test]
    fn acquire_rejects_invalid_path() {
        let (_d, conn, repo, alice, _bob) = setup();
        assert!(acquire(&conn, repo, "../escape", alice, "", None).is_err());
        assert!(acquire(&conn, repo, "/abs/path", alice, "", None).is_err());
    }
}
