//! Path-History：修订与清单模型（§3.3）。
//!
//! 只存增量变更（changes 表），由变更序列推导任意修订状态：
//! - 目录 Tombstone：删除目录只写一行 delete-dir，不展开逐文件
//! - 恢复 = 新提交（restore-as-commit）
//! - HEAD 快路径：head_entries 随提交就地更新，目录删除物理删子树行
//! - manifest_hash 仅 HEAD 异步补算

use crate::error::AppError;
use chrono::Utc;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub type Result<T> = std::result::Result<T, AppError>;

const MAX_PATH_BYTES: usize = 1024;
const MAX_SEGMENT_BYTES: usize = 255;

// ============ 类型 ============

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Add,
    Modify,
    Delete,
    Meta,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Add => "add",
            Op::Modify => "modify",
            Op::Delete => "delete",
            Op::Meta => "meta",
        }
    }
    pub fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "add" => Op::Add,
            "modify" => Op::Modify,
            "delete" => Op::Delete,
            "meta" => Op::Meta,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "dir",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "file" => Kind::File,
            "dir" => Kind::Dir,
            _ => return None,
        })
    }
}

/// 一次提交中的一条变更。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Change {
    pub path: String,
    pub op: Op,
    pub kind: Kind,
    pub blob_hash: Option<String>,
    pub size: i64,
    pub mode: i64,
    pub mtime: i64,
}

impl Change {
    pub fn add_file(path: impl Into<String>, blob_hash: impl Into<String>, size: i64) -> Change {
        Change { path: path.into(), op: Op::Add, kind: Kind::File, blob_hash: Some(blob_hash.into()), size, mode: 0o644, mtime: 0 }
    }
    pub fn modify_file(path: impl Into<String>, blob_hash: impl Into<String>, size: i64) -> Change {
        Change { path: path.into(), op: Op::Modify, kind: Kind::File, blob_hash: Some(blob_hash.into()), size, mode: 0o644, mtime: 0 }
    }
    pub fn delete_file(path: impl Into<String>) -> Change {
        Change { path: path.into(), op: Op::Delete, kind: Kind::File, blob_hash: None, size: 0, mode: 0o644, mtime: 0 }
    }
    pub fn add_dir(path: impl Into<String>) -> Change {
        Change { path: path.into(), op: Op::Add, kind: Kind::Dir, blob_hash: None, size: 0, mode: 0o644, mtime: 0 }
    }
    /// 目录 Tombstone：删除目录只写一行（§3.3）。
    pub fn delete_dir(path: impl Into<String>) -> Change {
        Change { path: path.into(), op: Op::Delete, kind: Kind::Dir, blob_hash: None, size: 0, mode: 0o644, mtime: 0 }
    }
}

/// 某修订下一个路径的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryInfo {
    pub path: String,
    pub kind: Kind,
    pub blob_hash: Option<String>,
    pub size: i64,
    pub mode: i64,
    pub mtime: i64,
    pub changed_rev: i64,
}

#[derive(Debug, Clone)]
pub struct CommitResult {
    pub rev: i64,
    pub file_count: i64,
    pub byte_delta: i64,
}

// ============ 路径合法性（§5.3 步骤 5 / §6.6） ============

/// 入库前路径校验：posix 相对路径、NFC、无穿越、无 Windows 保留名/非法字符、长度上限。
pub fn validate_path(path: &str) -> Result<()> {
    let bad = |msg: &str| Err(AppError::InvalidPath(format!("路径 `{path}`: {msg}")));
    if path.is_empty() {
        return bad("不能为空（仓库根用空字符串表示，但不可作为变更路径）");
    }
    if path.starts_with('/') {
        return bad("不能以 / 开头");
    }
    if path.len() > MAX_PATH_BYTES {
        return bad("全路径超过 1024 字节上限");
    }
    if path.contains('\\') || path.contains('\0') {
        return bad("不允许反斜杠或 NUL");
    }
    if path.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
        return bad("不允许控制字符");
    }
    for seg in path.split('/') {
        if seg.is_empty() {
            return bad("不允许空路径段");
        }
        if seg == "." || seg == ".." {
            return bad("不允许 . 或 .. 段");
        }
        if seg.len() > MAX_SEGMENT_BYTES {
            return bad("单段超过 255 字节上限");
        }
        if seg == ".b-artifact" {
            return bad(".b-artifact 为保留目录名");
        }
        if seg.chars().any(|c| "<>:\"|?*".contains(c)) {
            return bad("包含 Windows 非法字符 < > : \" | ? *");
        }
        if seg.ends_with(' ') || seg.ends_with('.') {
            return bad("段名不能以空格或点结尾（Windows 不支持）");
        }
        // Windows 保留名（不区分大小写，含带扩展名形式 CON.txt 等）
        let base = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9");
        if reserved {
            return bad("Windows 保留设备名");
        }
    }
    // NFC 规范化（§6.6 ①）：服务端入库唯一形态 = NFC
    if path.nfc().collect::<String>() != path {
        return Err(AppError::PathNotNormalized(format!("路径 `{path}` 非 NFC 形态")));
    }
    Ok(())
}

/// 父目录路径："a/b/c.txt" → "a/b"；"c.txt" → ""（仓库根）。
pub fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// 前缀 → 半开区间 `[lo, hi)`，让 `path >= ? AND path < ?` 下推 `(repo_id, path)` 索引。
///
/// M1.5 瓶颈 B1：前缀 `LIKE 'pfx%'` 在默认 `case_sensitive_like=OFF` 下不会下推索引，
/// 且空前缀（仓库根）连 GLOB 也救不了。改半开区间后**不依赖任何 pragma**，
/// 代价是空前缀退化为无界（此时调用方必须走 parent_path 等值快路径，见 `list_dir_head`）。
///
/// 入参必须是 `normalize_prefix` 的结果（"" 或以 '/' 结尾）。
pub fn prefix_bounds(pfx: &str) -> Option<(String, String)> {
    if pfx.is_empty() {
        return None;
    }
    let mut hi = pfx.as_bytes().to_vec();
    let last = hi.len() - 1;
    if hi[last] == u8::MAX {
        return None; // 理论不可达：前缀以 '/'（0x2F）结尾
    }
    hi[last] += 1; // '/' 0x2F → '0' 0x30，故 "art/" 的严格上界是 "art0"
    Some((
        pfx.to_string(),
        String::from_utf8(hi).expect("末字节 +1 后仍是合法 UTF-8"),
    ))
}

/// 走语句缓存的单行查询（M1.5 瓶颈 B2）：无行返回 None。
fn cached_row<T, F>(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
    f: F,
) -> rusqlite::Result<Option<T>>
where
    F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(p)?;
    match rows.next()? {
        Some(row) => Ok(Some(f(row)?)),
        None => Ok(None),
    }
}

/// 规范化目录前缀："" 或 "art" 或 "art/" → "" 或 "art/"。
fn normalize_prefix(prefix: &str) -> String {
    if prefix.is_empty() {
        String::new()
    } else {
        let p = prefix.trim_matches('/');
        if p.is_empty() {
            String::new()
        } else {
            format!("{p}/")
        }
    }
}

/// 祖先目录链："a/b/c.txt" → ["a", "a/b"]。
fn ancestors(path: &str) -> Vec<String> {
    let segs: Vec<&str> = path.split('/').collect();
    (1..segs.len()).map(|i| segs[..i].join("/")).collect()
}

// ============ 基础写入：用户 / 仓库 ============

pub fn create_user(conn: &Connection, username: &str, is_admin: bool) -> Result<i64> {
    conn.execute(
        "INSERT INTO users (username, is_admin, source, created_at) VALUES (?1, ?2, 'local', ?3)",
        params![username, is_admin as i64, Utc::now().to_rfc3339()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 建仓：默认写入 '' + everyone + read 基线 ACL（§4.2 要点 4）。
///
/// v0.4.17：不再接受锁策略参数（advisory 与 needs_lock 白名单都已删除，§5.1/§5.2）。
/// `lock_policy` / `needs_lock` 两列保留但恒写 `'strict'` / `''` —— **必须显式写**，
/// 因为 001 迁移给 `lock_policy` 留的 DEFAULT 还是 `'advisory'`，省略就会写回旧值。
pub fn create_repo(conn: &Connection, name: &str, owner_id: i64) -> Result<i64> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(AppError::InvalidArgument(format!("仓库名 `{name}` 只允许 [A-Za-z0-9._-]")));
    }
    conn.execute(
        "INSERT INTO repos (name, owner_id, lock_policy, needs_lock, created_at)
         VALUES (?1, ?2, 'strict', '', ?3)",
        params![name, owner_id, Utc::now().to_rfc3339()],
    )?;
    let repo_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO acl_rules (repo_id, path_prefix, subject_type, subject_id, level, inherit, created_at)
         VALUES (?1, '', 'everyone', 0, 'read', 1, ?2)",
        params![repo_id, Utc::now().to_rfc3339()],
    )?;
    Ok(repo_id)
}

/// 仓库摘要（API 层用）。
///
/// v0.4.17：`lock_policy` / `needs_lock` 已从结构体移除（连同 API 响应），
/// 列仍在库里但恒为 `'strict'` / `''`，不再有任何读路径。
#[derive(Debug, Clone)]
pub struct RepoRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub owner_id: i64,
    pub head_rev: i64,
}

/// 按名字查仓库；名字规则 [A-Za-z0-9._-]{1,64}。
pub fn repo_by_name(conn: &Connection, name: &str) -> Result<Option<RepoRow>> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Err(AppError::InvalidArgument(format!("仓库名 `{name}` 不合法（[A-Za-z0-9._-]，长度 1~64）")));
    }
    let row = conn.query_row(
        "SELECT id, name, description, owner_id, head_rev
           FROM repos WHERE name = ?1",
        params![name],
        |r| {
            Ok(RepoRow {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                owner_id: r.get(3)?,
                head_rev: r.get(4)?,
            })
        },
    );
    match row {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn repo_head_rev(conn: &Connection, repo_id: i64) -> Result<i64> {
    match conn.query_row(
        "SELECT head_rev FROM repos WHERE id = ?1",
        params![repo_id],
        |r| r.get(0),
    ) {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            Err(AppError::NotFound(format!("仓库 {repo_id} 不存在")))
        }
        Err(e) => Err(e.into()),
    }
}

// ============ 提交（单事务，乐观锁，§5.3 阶段二的落库部分） ============

/// 应用一次提交（自开事务包装，供 restore / 单测使用）。
/// manifest_hash 不在此计算（由 refresh_head_manifest_hash 异步补算，§3.3）。
pub fn apply_commit(
    conn: &mut Connection,
    repo_id: i64,
    user_id: i64,
    message: &str,
    changes: &[Change],
) -> Result<CommitResult> {
    // §5.3：changes 为零则回滚——空提交必须拒绝
    if changes.is_empty() {
        return Err(AppError::InvalidArgument("提交必须包含至少一条变更".to_string()));
    }
    for c in changes {
        validate_path(&c.path)?;
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let head_rev: i64 = tx
        .query_row("SELECT head_rev FROM repos WHERE id = ?1", params![repo_id], |r| r.get(0))
        .map_err(|_| AppError::NotFound(format!("仓库 {repo_id} 不存在")))?;
    let result = apply_commit_tx(&tx, repo_id, user_id, message, changes, head_rev)?;
    tx.commit()?;
    Ok(result)
}

/// 补建路径的**隐式**祖先目录行（已存在则跳过，不动显式行）。
fn ensure_dirs(
    repo_id: i64,
    path: &str,
    new_rev: i64,
    st: &mut rusqlite::CachedStatement<'_>,
) -> Result<()> {
    for anc in ancestors(path) {
        st.execute(params![repo_id, anc, new_rev, parent_of(&anc)])?;
    }
    Ok(())
}

/// 自深向浅回收"因本次删除而变空的隐式目录"（显式目录即使为空也保留）。
fn prune_dirs_upward(
    repo_id: i64,
    path: &str,
    st_flag: &mut rusqlite::CachedStatement<'_>,
    st_has_child: &mut rusqlite::CachedStatement<'_>,
    st_del: &mut rusqlite::CachedStatement<'_>,
) -> Result<()> {
    for anc in ancestors(path).into_iter().rev() {
        let explicit: i64 = {
            let mut rows = st_flag.query(params![repo_id, &anc])?;
            match rows.next()? {
                Some(r) => r.get(0)?,
                None => 0,
            }
        };
        if explicit != 0 {
            break;
        }
        let has_child = {
            let mut rows = st_has_child.query(params![repo_id, &anc])?;
            rows.next()?.is_some()
        };
        if has_child {
            break; // 仍有子项 → 它的祖先必然也非空，无需继续
        }
        st_del.execute(params![repo_id, &anc])?;
    }
    Ok(())
}

/// 事务内应用提交（commit 服务在同一事务内还需写 commits/pending/审计，§5.3 步骤 4）。
/// expected_head_rev = 客户端 base_rev；不等 → 409 OUT_OF_DATE（严格相等语义）。
/// 调用方负责事务的开启与提交。
pub fn apply_commit_tx(
    tx: &rusqlite::Transaction<'_>,
    repo_id: i64,
    user_id: i64,
    message: &str,
    changes: &[Change],
    expected_head_rev: i64,
) -> Result<CommitResult> {
    // §5.3：changes 为零则回滚——空提交必须拒绝
    if changes.is_empty() {
        return Err(AppError::InvalidArgument("提交必须包含至少一条变更".to_string()));
    }
    for c in changes {
        validate_path(&c.path)?;
    }

    let head_rev: i64 = cached_row(
        tx,
        "SELECT head_rev FROM repos WHERE id = ?1",
        params![repo_id],
        |r| r.get(0),
    )?
    .ok_or_else(|| AppError::NotFound(format!("仓库 {repo_id} 不存在")))?;
    if head_rev != expected_head_rev {
        return Err(AppError::Conflict {
            code: "OUT_OF_DATE",
            message: "工作副本落后于服务端，请先 update".into(),
            details: serde_json::json!({ "head_rev": head_rev, "paths": [] }),
        });
    }
    let new_rev = head_rev + 1;

    // 语句全部预编译并缓存（M1.5 B2：热路径 prepare 一次 ≈ 7µs，缓存后 ≈ 0.3µs）
    let mut st_known = tx.prepare_cached(
        "SELECT 1 FROM changes WHERE repo_id = ?1 AND blob_hash = ?2 LIMIT 1",
    )?;
    let mut st_ins_change = tx.prepare_cached(
        "INSERT INTO changes (repo_id, rev, path, op, kind, blob_hash, size, mode, mtime)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut st_ins_dir = tx.prepare_cached(
        "INSERT OR REPLACE INTO head_entries
             (repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, parent_path, is_explicit)
         VALUES (?1, ?2, 'dir', NULL, 0, 0, 0, ?3, ?4, 1)",
    )?;
    let mut st_ins_file = tx.prepare_cached(
        "INSERT OR REPLACE INTO head_entries
             (repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, parent_path, is_explicit)
         VALUES (?1, ?2, 'file', ?3, ?4, ?5, ?6, ?7, ?8, 0)",
    )?;
    let mut st_ensure_dir = tx.prepare_cached(
        "INSERT OR IGNORE INTO head_entries
             (repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, parent_path, is_explicit)
         VALUES (?1, ?2, 'dir', NULL, 0, 0, 0, ?3, ?4, 0)",
    )?;
    let mut st_del_exact = tx.prepare_cached(
        "DELETE FROM head_entries WHERE repo_id = ?1 AND path = ?2",
    )?;
    let mut st_del_subtree = tx.prepare_cached(
        "DELETE FROM head_entries WHERE repo_id = ?1 AND path >= ?2 AND path < ?3",
    )?;
    let mut st_has_child = tx.prepare_cached(
        "SELECT 1 FROM head_entries WHERE repo_id = ?1 AND parent_path = ?2 LIMIT 1",
    )?;
    let mut st_flag = tx.prepare_cached(
        "SELECT is_explicit FROM head_entries WHERE repo_id = ?1 AND path = ?2",
    )?;
    let mut st_meta = tx.prepare_cached(
        "UPDATE head_entries SET mode = ?3, mtime = ?4 WHERE repo_id = ?1 AND path = ?2",
    )?;

    // byte_delta：本仓库此前未引用过的新 blob 的原始大小之和（去重后新增存储字节）
    let mut byte_delta: i64 = 0;
    for c in changes {
        if c.kind == Kind::File && c.op != Op::Delete {
            if let Some(h) = &c.blob_hash {
                let known = {
                    let mut rows = st_known.query(params![repo_id, h])?;
                    rows.next()?.is_some()
                };
                if !known {
                    byte_delta += c.size;
                }
            }
        }
    }

    tx.execute(
        "INSERT INTO revisions (repo_id, rev, author_id, message, created_at, file_count, byte_delta, manifest_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '')",
        params![
            repo_id,
            new_rev,
            user_id,
            message,
            Utc::now().to_rfc3339(),
            changes.len() as i64,
            byte_delta
        ],
    )?;

    for c in changes {
        st_ins_change.execute(params![
            repo_id,
            new_rev,
            c.path,
            c.op.as_str(),
            c.kind.as_str(),
            c.blob_hash,
            c.size,
            c.mode,
            c.mtime
        ])?;
        match (c.op, c.kind) {
            (Op::Add, Kind::Dir) => {
                // 先补祖先（隐式），再写自身（显式，空目录也保留）
                ensure_dirs(repo_id, &c.path, new_rev, &mut st_ensure_dir)?;
                st_ins_dir.execute(params![
                    repo_id,
                    c.path,
                    new_rev,
                    parent_of(&c.path)
                ])?;
            }
            (Op::Delete, Kind::Dir) => {
                // HEAD 的目录删除：物理删除自身 + 子树行（Tombstone 只在 changes 留痕，§3.3）
                let (lo, hi) = prefix_bounds(&format!("{}/", c.path))
                    .expect("目录路径非空，前缀必定有界");
                st_del_exact.execute(params![repo_id, c.path])?;
                st_del_subtree.execute(params![repo_id, lo, hi])?;
                // 祖先若因此变空且是隐式目录 → 一并回收
                prune_dirs_upward(
                    repo_id,
                    &c.path,
                    &mut st_flag,
                    &mut st_has_child,
                    &mut st_del_exact,
                )?;
            }
            (Op::Add | Op::Modify, Kind::File) => {
                ensure_dirs(repo_id, &c.path, new_rev, &mut st_ensure_dir)?;
                st_ins_file.execute(params![
                    repo_id,
                    c.path,
                    c.blob_hash,
                    c.size,
                    c.mode,
                    c.mtime,
                    new_rev,
                    parent_of(&c.path)
                ])?;
            }
            (Op::Delete, Kind::File) => {
                st_del_exact.execute(params![repo_id, c.path])?;
                prune_dirs_upward(
                    repo_id,
                    &c.path,
                    &mut st_flag,
                    &mut st_has_child,
                    &mut st_del_exact,
                )?;
            }
            (Op::Meta, _) => {
                // 属性变更：仅更新 head_entries 的 mode/mtime
                st_meta.execute(params![repo_id, c.path, c.mode, c.mtime])?;
            }
            // 目录 modify 无意义（目录行无内容），head_entries 不动
            (Op::Modify, Kind::Dir) => {}
        }
    }

    let n = tx.execute(
        "UPDATE repos SET head_rev = ?2 WHERE id = ?1 AND head_rev = ?3",
        params![repo_id, new_rev, head_rev],
    )?;
    if n != 1 {
        return Err(AppError::Conflict {
            code: "OUT_OF_DATE",
            message: "仓库 head_rev 已被并发修改".into(),
            details: serde_json::json!({ "head_rev": head_rev }),
        });
    }

    // 不 commit：由调用方决定事务边界
    Ok(CommitResult { rev: new_rev, file_count: changes.len() as i64, byte_delta })
}

// ============ 存在性判定与查询（§3.3） ============

#[derive(Debug, Clone)]
struct ChangeRec {
    rev: i64,
    op: Op,
    kind: Kind,
    blob_hash: Option<String>,
    size: i64,
    mode: i64,
    mtime: i64,
}

impl ChangeRec {
    fn into_entry(self, path: &str) -> EntryInfo {
        EntryInfo {
            path: path.to_string(),
            kind: self.kind,
            blob_hash: self.blob_hash,
            size: self.size,
            mode: self.mode,
            mtime: self.mtime,
            changed_rev: self.rev,
        }
    }
}

/// 路径自身的最近一次 change（rev ≤ max_rev）。
fn last_change(
    conn: &Connection,
    repo_id: i64,
    path: &str,
    max_rev: i64,
    kind: Option<Kind>,
) -> Result<Option<ChangeRec>> {
    let sql = match kind {
        Some(_) => "SELECT rev, op, kind, blob_hash, size, mode, mtime FROM changes
                     WHERE repo_id = ?1 AND path = ?2 AND rev <= ?3 AND kind = ?4
                     ORDER BY rev DESC LIMIT 1",
        None => "SELECT rev, op, kind, blob_hash, size, mode, mtime FROM changes
                  WHERE repo_id = ?1 AND path = ?2 AND rev <= ?3
                  ORDER BY rev DESC LIMIT 1",
    };
    let row = match kind {
        Some(k) => cached_row(conn, sql, params![repo_id, path, max_rev, k.as_str()], map_change),
        None => cached_row(conn, sql, params![repo_id, path, max_rev], map_change),
    };
    row.map_err(Into::into)
}

fn map_change(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChangeRec> {
    Ok(ChangeRec {
        rev: r.get(0)?,
        op: Op::parse(&r.get::<_, String>(1)?).unwrap_or(Op::Modify),
        kind: Kind::parse(&r.get::<_, String>(2)?).unwrap_or(Kind::File),
        blob_hash: r.get(3)?,
        size: r.get(4)?,
        mode: r.get(5)?,
        mtime: r.get(6)?,
    })
}

/// 祖先链上是否存在覆盖该路径的 delete-dir Tombstone（晚于 own_rev）。
/// 返回 (祖先路径, tombstone rev)。
fn covering_tombstone(
    conn: &Connection,
    repo_id: i64,
    path: &str,
    own_rev: i64,
    max_rev: i64,
) -> Result<Option<(String, i64)>> {
    for anc in ancestors(path) {
        if let Some(rec) = last_change(conn, repo_id, &anc, max_rev, Some(Kind::Dir))? {
            if rec.op == Op::Delete && rec.rev > own_rev {
                return Ok(Some((anc, rec.rev)));
            }
        }
    }
    Ok(None)
}

/// 修订 N 时某文件的内容（最高频，O(log n)，§3.3 核心查询）。
pub fn file_at(conn: &Connection, repo_id: i64, path: &str, rev: i64) -> Result<Option<EntryInfo>> {
    let Some(rec) = last_change(conn, repo_id, path, rev, Some(Kind::File))? else {
        return Ok(None);
    };
    if rec.op == Op::Delete {
        return Ok(None);
    }
    if covering_tombstone(conn, repo_id, path, rec.rev, rev)?.is_some() {
        return Ok(None);
    }
    Ok(Some(rec.into_entry(path)))
}

/// 显式目录行是否存活（mkdir 后未 rmdir、未被祖先 Tombstone 覆盖）。
fn dir_explicit_alive(
    conn: &Connection,
    repo_id: i64,
    path: &str,
    rev: i64,
) -> Result<Option<i64>> {
    let Some(rec) = last_change(conn, repo_id, path, rev, Some(Kind::Dir))? else {
        return Ok(None);
    };
    if rec.op == Op::Delete {
        return Ok(None);
    }
    if covering_tombstone(conn, repo_id, path, rec.rev, rev)?.is_some() {
        return Ok(None);
    }
    Ok(Some(rec.rev))
}

/// 前缀下是否存在存活条目（文件走 file_at，显式目录行走 dir_explicit_alive）。
fn has_alive_descendant(conn: &Connection, repo_id: i64, prefix: &str, rev: i64) -> Result<bool> {
    for (path, kind) in last_changes_under(conn, repo_id, prefix, rev)? {
        let alive = match kind {
            Kind::File => file_at(conn, repo_id, &path, rev)?.is_some(),
            Kind::Dir => dir_explicit_alive(conn, repo_id, &path, rev)?.is_some(),
        };
        if alive {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 修订 N 时目录是否存在：显式行存活 或 由"存活子文件 + 未被 Tombstone 覆盖"推导（§3.3）。
pub fn dir_exists_at(conn: &Connection, repo_id: i64, path: &str, rev: i64) -> Result<bool> {
    if path.is_empty() {
        return Ok(true); // 仓库根恒存在
    }
    if dir_explicit_alive(conn, repo_id, path, rev)?.is_some() {
        return Ok(true);
    }
    has_alive_descendant(conn, repo_id, path, rev)
}

/// 前缀下（任意深度）每个路径的最近一次 change（rev ≤ max_rev 且 op != delete 的候选）。
/// 返回 (path, kind) 列表，存活性由调用方按需复核。
///
/// 写法要点：**用 `GROUP BY path` + 裸列配 `MAX(rev)`，不要用窗口函数**。
/// `ROW_NUMBER() OVER (PARTITION BY path ORDER BY rev DESC)` 会让 SQLite 把整个
/// 候选区间物化再排序（O(n log n)，n = 前缀下的全部历史行）；而 `GROUP BY path`
/// 可以直接顺着索引 `idx_changes_path(repo_id, path, rev DESC)` 的顺序分组，**不需要排序**。
/// 裸列与 `MAX()` 搭配时取的是取到最大值的**那一行**的字段（SQLite 的确定性行为），
/// 所以我们能同时拿到"最后一次变更的 kind 与 op"。
const LAST_CHANGES_UNDER_RANGE: &str = "
    SELECT path, kind, op, MAX(rev) AS mrev
      FROM changes
     WHERE repo_id = ?1 AND rev <= ?2 AND path >= ?3 AND path < ?4
     GROUP BY path";

/// 空前缀（仓库根）：path 无法被界住，退化为全仓扫描。
const LAST_CHANGES_UNDER_ALL: &str = "
    SELECT path, kind, op, MAX(rev) AS mrev
      FROM changes
     WHERE repo_id = ?1 AND rev <= ?2
     GROUP BY path";

fn last_changes_under(
    conn: &Connection,
    repo_id: i64,
    prefix: &str,
    max_rev: i64,
) -> Result<Vec<(String, Kind)>> {
    let pfx = normalize_prefix(prefix);
    // 第 3 列是 op：`GROUP BY ... MAX(rev)` 拿到的就是"最后一次变更"，delete 的直接跳过，
    // 省掉调用方一次多余的存在性复核。
    let map = |r: &rusqlite::Row<'_>| {
        Ok((
            r.get::<_, String>(0)?,
            Kind::parse(&r.get::<_, String>(1)?).unwrap_or(Kind::File),
            Op::parse(&r.get::<_, String>(2)?).unwrap_or(Op::Modify),
        ))
    };
    let mut out = vec![];
    let mut push = |r: rusqlite::Result<(String, Kind, Op)>| -> Result<()> {
        let (path, kind, op) = r?;
        if op != Op::Delete {
            out.push((path, kind));
        }
        Ok(())
    };

    match prefix_bounds(&pfx) {
        Some((lo, hi)) => {
            let mut stmt = conn.prepare_cached(LAST_CHANGES_UNDER_RANGE)?;
            for row in stmt.query_map(params![repo_id, max_rev, lo, hi], map)? {
                push(row)?;
            }
        }
        None => {
            let mut stmt = conn.prepare_cached(LAST_CHANGES_UNDER_ALL)?;
            for row in stmt.query_map(params![repo_id, max_rev], map)? {
                push(row)?;
            }
        }
    }
    Ok(out)
}

// ============ 列目录 ============

/// 修订 N 时某目录的直接子项（历史推导路径）。
pub fn list_dir_at(conn: &Connection, repo_id: i64, prefix: &str, rev: i64) -> Result<Vec<EntryInfo>> {
    // 快路径：`rev >= head_rev` 与 HEAD 完全等价，没有理由去回放历史。
    // 客户端历史视图传的是**具体修订号**，所以"选中最新修订"会经常落到这里——
    // 走 `head_entries` 的 parent_path 等值查询是 O(直接子项)，而历史路径是 O(该前缀下的全部历史)。
    let head_rev = repo_head_rev(conn, repo_id)?;
    if rev >= head_rev {
        return list_dir_head(conn, repo_id, prefix);
    }
    let pfx = normalize_prefix(prefix);

    // 候选直接子项 = HEAD 的直接子项（O(直接子项)）+ (rev, head] 内被删掉的一级子段。
    //
    // 为什么不是"枚举整棵子树"：那需要扫该前缀下的**全部历史行**，再对每条候选做一次
    // 存在性点查（`file_at` + 祖先 Tombstone 链）。24 万文件档位实测 158 ms 全花在这里，
    // 而真正要输出的只有几十个直接子项。改成本形式后复杂度是
    // O(直接子项 + (rev, head] 的增量)，与子树大小无关。
    let mut cands: std::collections::BTreeMap<String, Option<EntryInfo>> = std::collections::BTreeMap::new();
    for e in list_dir_head(conn, repo_id, &pfx)? {
        cands.insert(first_segment(&e.path, &pfx), Some(e));
    }
    for p in deleted_between(conn, repo_id, &pfx, rev, head_rev)? {
        cands.entry(first_segment(&p, &pfx)).or_insert(None);
    }

    let mut out: Vec<EntryInfo> = vec![];
    for (seg, head_row) in cands {
        let path = format!("{pfx}{seg}");

        // ① 文件：HEAD 行就是它、且最后变更在 rev 之前 → 当时就是这个状态，直接采用。
        if let Some(e) = &head_row {
            if e.kind == Kind::File && e.changed_rev <= rev {
                out.push(e.clone());
                continue;
            }
        }
        // 否则按路径点查 rev 时的文件状态（索引点查，含祖先 Tombstone 判定）。
        if let Some(f) = file_at(conn, repo_id, &path, rev)? {
            out.push(f);
            continue;
        }

        // ② 目录：需要判断"该子树在 rev 时是否有存活内容"。
        if let Some(changed_rev) = dir_alive_at(conn, repo_id, &path, rev, head_row.as_ref(), head_rev)? {
            out.push(EntryInfo {
                path: path.clone(),
                kind: Kind::Dir,
                blob_hash: None,
                size: 0,
                mode: 0o644,
                mtime: 0,
                changed_rev,
            });
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// `prefix` 之下的一级子段（`assets/0000/x` + `assets` → `0000`）。
fn first_segment(path: &str, prefix: &str) -> String {
    match path[prefix.len()..].split_once('/') {
        Some((c, _)) => c.to_string(),
        None => path[prefix.len()..].to_string(),
    }
}

/// `(rev, head_rev]` 之间被删除的路径（限定在 prefix 之下）。
///
/// 这是"HEAD 快照 + 增量回滚"的增量来源：只有这些路径可能在 rev 时存在、如今已不在 HEAD。
/// 走 `idx_changes_rev(repo_id, rev)` 的 rev 区间扫描，与仓库规模无关，只与**这段区间内的变更数**有关。
fn deleted_between(
    conn: &Connection,
    repo_id: i64,
    prefix: &str,
    rev: i64,
    head_rev: i64,
) -> Result<Vec<String>> {
    let map = |r: &rusqlite::Row<'_>| r.get::<_, String>(0);
    let mut out = vec![];
    match prefix_bounds(prefix) {
        Some((lo, hi)) => {
            let sql = "SELECT DISTINCT path FROM changes
                        WHERE repo_id = ?1 AND op = 'delete' AND rev > ?2 AND rev <= ?3
                          AND path >= ?4 AND path < ?5";
            let mut stmt = conn.prepare_cached(sql)?;
            for row in stmt.query_map(params![repo_id, rev, head_rev, lo, hi], map)? {
                out.push(row?);
            }
        }
        None => {
            let sql = "SELECT DISTINCT path FROM changes
                        WHERE repo_id = ?1 AND op = 'delete' AND rev > ?2 AND rev <= ?3";
            let mut stmt = conn.prepare_cached(sql)?;
            for row in stmt.query_map(params![repo_id, rev, head_rev], map)? {
                out.push(row?);
            }
        }
    }
    Ok(out)
}

/// 该目录在 rev 时是否存在；存在则连同"最后一次变化的修订"一起返回。
fn dir_alive_at(
    conn: &Connection,
    repo_id: i64,
    path: &str,
    rev: i64,
    head_row: Option<&EntryInfo>,
    head_rev: i64,
) -> Result<Option<i64>> {
    // ① HEAD 的目录行最后变更在 rev 之前 → 当时就是这个状态。
    //    （HEAD 里现存 ⟹ 此后没被删过：真被删过又重建的话，changed_rev 会是重建的那次。）
    if let Some(hr) = head_row {
        if hr.kind == Kind::Dir && hr.changed_rev <= rev {
            return Ok(Some(hr.changed_rev));
        }
    }
    // ② 子树里存在"rev 之前就已存在"的条目 → 当时有内容。
    //    一次索引范围扫描 + LIMIT 1，命中即停；命中不了才说明该子树全是 rev 之后新建的。
    if let Some(changed_rev) = oldest_head_entry_before(conn, repo_id, path, rev)? {
        return Ok(Some(changed_rev));
    }
    // ③ 兜底：该子树在 (rev, head] 被删过的路径里，有在 rev 时仍然存活的。
    //    只在 ② 落空（子树全是新建的）时才走，代价与这段增量的规模成正比。
    for p in deleted_between(conn, repo_id, path, rev, head_rev)? {
        if file_at(conn, repo_id, &p, rev)?.is_some() || dir_explicit_alive(conn, repo_id, &p, rev)?.is_some()
        {
            return Ok(Some(rev));
        }
    }
    // ④ 显式空目录（mkdir 建出来、没被 rmdir 或祖先 Tombstone 覆盖）。
    dir_explicit_alive(conn, repo_id, path, rev)
}

/// 该子树里是否存在 `changed_rev <= rev` 的现存条目；有则返回那个 changed_rev。
fn oldest_head_entry_before(
    conn: &Connection,
    repo_id: i64,
    path: &str,
    rev: i64,
) -> Result<Option<i64>> {
    let (lo, hi) = prefix_bounds(path).expect("非空路径必有前缀边界");
    // 只看**文件行**：文件行的 changed_rev 语义明确（= changes 里该路径的 MAX(rev)），
    // 而目录行是物化时顺带记下的子项 rev，不足以拿来判断"这个目录在 rev 时是否已存在"。
    let sql = "SELECT changed_rev FROM head_entries
                WHERE repo_id = ?1 AND path >= ?2 AND path < ?3
                  AND kind = 'file' AND changed_rev <= ?4
                LIMIT 1";
    cached_row(conn, sql, params![repo_id, lo, hi, rev], |r| r.get::<_, i64>(0))
        .map_err(Into::into)
}

/// 列目录入参 → 目录路径本身："art/" 与 "art" 都归一为 "art"；"" 为仓库根。
fn normalize_dir(prefix: &str) -> String {
    prefix.trim_matches('/').to_string()
}

/// HEAD 目录列表（快路径：`parent_path` 等值查询，§3.3 + M1.5 B4）。
///
/// `head_entries` 物化了**全部**目录行——既有显式 mkdir 的（is_explicit=1，允许为空），
/// 也有文件隐式产生的父目录（is_explicit=0，最后一个子项消失即回收）。
/// 于是"直接子项"= `parent_path = 该目录` 的行，代价 O(直接子项)，
/// 无需再读整棵子树（M1.5 实测 full 档列仓库根要读 100 万行 / 762 ms）。
pub fn list_dir_head(conn: &Connection, repo_id: i64, prefix: &str) -> Result<Vec<EntryInfo>> {
    let dir = normalize_dir(prefix);
    let sql = "SELECT path, kind, blob_hash, size, mode, mtime, changed_rev
                 FROM head_entries
                WHERE repo_id = ?1 AND parent_path = ?2";
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params![repo_id, dir], |r| {
        Ok(EntryInfo {
            path: r.get(0)?,
            kind: Kind::parse(&r.get::<_, String>(1)?).unwrap_or(Kind::File),
            blob_hash: r.get(2)?,
            size: r.get(3)?,
            mode: r.get(4)?,
            mtime: r.get(5)?,
            changed_rev: r.get(6)?,
        })
    })?;

    let mut entries: Vec<EntryInfo> = vec![];
    for row in rows {
        entries.push(row?);
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// 重建 HEAD 目录树（parent_path + 物化目录行）。
///
/// 用途：① schema 迁移 003 的存量回填；② 与 `rebuild_refcount()` 同类的自愈入口。
///
/// 隐式目录行（文件行反推）可以放心重算；**显式目录行必须原样保留**——"这个空目录
/// 是有意为之"这件事只记录在 `head_entries.is_explicit` 上，文件行里推不出来，
/// 一旦丢掉，空目录就会在下次 `prune_dirs_upward` 时被误回收。
/// 事务内版本——**调用方已持有事务时必须用这个**（purge / 提交路径）。
///
/// 它不再自己 BEGIN：`unchecked_transaction()` 在已有事务上会直接执行 `BEGIN`，
/// SQLite 报 "cannot start a transaction within a transaction"（purge 曾踩此坑）。
pub fn rebuild_head_tree_tx(conn: &Connection, repo_id: i64) -> Result<usize> {
    let dirs: usize = {
        // ① 先记住显式目录行，重建后原样恢复
        let mut explicit: Vec<(String, i64)> = vec![];
        {
            let mut stmt = conn.prepare(
                "SELECT path, changed_rev FROM head_entries
                  WHERE repo_id = ?1 AND kind = 'dir' AND is_explicit = 1",
            )?;
            for row in stmt.query_map(params![repo_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })? {
                explicit.push(row?);
            }
        }
        // ② 文件行是目录树的唯一事实来源
        let mut files: Vec<(String, i64)> = vec![];
        {
            let mut stmt = conn.prepare(
                "SELECT path, changed_rev FROM head_entries WHERE repo_id = ?1 AND kind = 'file'",
            )?;
            for row in stmt.query_map(params![repo_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })? {
                files.push(row?);
            }
        }
        // ③ 清空目录行后重算：文件行补 parent_path，祖先目录补隐式行
        conn.execute(
            "DELETE FROM head_entries WHERE repo_id = ?1 AND kind = 'dir'",
            params![repo_id],
        )?;
        let mut upd = conn.prepare_cached(
            "UPDATE head_entries SET parent_path = ?3 WHERE repo_id = ?1 AND path = ?2",
        )?;
        let mut ins = conn.prepare_cached(
            "INSERT OR IGNORE INTO head_entries
                 (repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, parent_path, is_explicit)
             VALUES (?1, ?2, 'dir', NULL, 0, 0, 0, ?3, ?4, 0)",
        )?;
        let mut mk_explicit = conn.prepare_cached(
            "UPDATE head_entries SET is_explicit = 1 WHERE repo_id = ?1 AND path = ?2",
        )?;
        for (path, changed_rev) in &files {
            upd.execute(params![repo_id, path, parent_of(path)])?;
            for anc in ancestors(path) {
                ins.execute(params![repo_id, anc, changed_rev, parent_of(&anc)])?;
            }
        }
        // ④ 显式目录即使一个子项也没有，也必须回到树上
        for (path, changed_rev) in &explicit {
            ins.execute(params![repo_id, path, changed_rev, parent_of(path)])?;
            mk_explicit.execute(params![repo_id, path])?;
        }
        conn.query_row(
            "SELECT COUNT(*) FROM head_entries WHERE repo_id = ?1 AND kind = 'dir'",
            params![repo_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }; // 语句在此归还缓存
    Ok(dirs)
}

/// 独立自愈入口（迁移 003 回填 / 离线修复）：自己开事务后转调 `_tx` 版本。
pub fn rebuild_head_tree(conn: &Connection, repo_id: i64) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let n = rebuild_head_tree_tx(&tx, repo_id)?;
    tx.commit()?;
    Ok(n)
}

// ============ 恢复 = 新提交（§3.3） ============

/// 把 source_rev 时该路径的内容以 add/modify 写入新修订。
/// 不产生任何"反向变更"；恢复本身可追溯、可再次恢复。
pub fn restore(
    conn: &mut Connection,
    repo_id: i64,
    user_id: i64,
    path: &str,
    source_rev: i64,
    message: &str,
) -> Result<CommitResult> {
    let head = repo_head_rev(conn, repo_id)?;
    let mut changes: Vec<Change> = vec![];

    if let Some(e) = file_at(conn, repo_id, path, source_rev)? {
        // 恢复单个文件
        let op = if file_at(conn, repo_id, path, head)?.is_some() {
            Op::Modify
        } else {
            Op::Add
        };
        changes.push(Change {
            path: path.to_string(),
            op,
            kind: Kind::File,
            blob_hash: e.blob_hash,
            size: e.size,
            mode: e.mode,
            mtime: e.mtime,
        });
    } else if dir_exists_at(conn, repo_id, path, source_rev)? {
        let pfx = format!("{}/", path.trim_matches('/'));
        // 该前缀下所有存活文件
        for (p, kind) in last_changes_under(conn, repo_id, &pfx, source_rev)? {
            if kind == Kind::File {
                if let Some(e) = file_at(conn, repo_id, &p, source_rev)? {
                    let op = if file_at(conn, repo_id, &p, head)?.is_some() {
                        Op::Modify
                    } else {
                        Op::Add
                    };
                    changes.push(Change {
                        path: p,
                        op,
                        kind: Kind::File,
                        blob_hash: e.blob_hash,
                        size: e.size,
                        mode: e.mode,
                        mtime: e.mtime,
                    });
                }
            }
        }
        // source_rev 时显式存活的目录（保留空目录语义）；路径自身若是显式目录也补一行
        if dir_explicit_alive(conn, repo_id, path, source_rev)?.is_some() {
            changes.push(Change::add_dir(path.to_string()));
        }
        for (p, kind) in last_changes_under(conn, repo_id, &pfx, source_rev)? {
            if kind == Kind::Dir && dir_explicit_alive(conn, repo_id, &p, source_rev)?.is_some() {
                changes.push(Change::add_dir(p));
            }
        }
    } else {
        return Err(AppError::NotFound(format!(
            "路径 `{path}` 在修订 {source_rev} 不存在"
        )));
    }

    if changes.is_empty() {
        return Err(AppError::NotFound(format!(
            "路径 `{path}` 在修订 {source_rev} 无可恢复内容"
        )));
    }
    apply_commit(conn, repo_id, user_id, message, &changes)
}

// ============ manifest_hash（仅 HEAD，异步补算，§3.3） ============

/// sha256(排序后的 "path\0hash\n" 拼接)，基于 head_entries。
pub fn compute_manifest_hash(conn: &Connection, repo_id: i64) -> Result<String> {
    let mut stmt = conn.prepare(
        "SELECT path, blob_hash FROM head_entries
          WHERE repo_id = ?1 AND kind = 'file' ORDER BY path",
    )?;
    let rows = stmt.query_map(params![repo_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut hasher = Sha256::new();
    for row in rows {
        let (path, hash) = row?;
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.unwrap_or_default().as_bytes());
        hasher.update(b"\n");
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 提交后异步调用：为 HEAD 修订补写 manifest_hash。返回计算值。
pub fn refresh_head_manifest_hash(conn: &Connection, repo_id: i64) -> Result<Option<String>> {
    let head = repo_head_rev(conn, repo_id)?;
    if head == 0 {
        return Ok(None);
    }
    let h = compute_manifest_hash(conn, repo_id)?;
    conn.execute(
        "UPDATE revisions SET manifest_hash = ?1 WHERE repo_id = ?2 AND rev = ?3",
        params![h, repo_id, head],
    )?;
    Ok(Some(h))
}

// ============ 单元测试（§12.2 path_history 必测语义） ============

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db;

    fn setup() -> (tempfile::TempDir, Connection, i64, i64) {
        let (dir, conn) = db::tests::test_db();
        let uid = create_user(&conn, "alice", true).unwrap();
        let rid = create_repo(&conn, "assets", uid).unwrap();
        (dir, conn, rid, uid)
    }

    fn commit(conn: &mut Connection, rid: i64, uid: i64, msg: &str, changes: &[Change]) -> CommitResult {
        apply_commit(conn, rid, uid, msg, changes).unwrap()
    }

    fn file_hash(conn: &Connection, rid: i64, path: &str, rev: i64) -> Option<String> {
        file_at(conn, rid, path, rev).unwrap().map(|e| e.blob_hash.unwrap())
    }

    fn names(entries: &[EntryInfo]) -> Vec<String> {
        entries.iter().map(|e| e.path.clone()).collect()
    }

    // ① 目录 Tombstone：删除目录一行覆盖整棵子树；Tombstone 后新文件不受旧覆盖影响
    #[test]
    fn tombstone_covers_subtree() {
        let (_dir, mut conn, rid, uid) = setup();
        commit(&mut conn, rid, uid, "init", &[
            Change::add_file("art/a.psd", "h1", 100),
            Change::add_file("art/sub/b.psd", "h2", 200),
            Change::add_file("keep.txt", "h3", 10),
        ]);
        assert_eq!(repo_head_rev(&conn, rid).unwrap(), 1);
        assert!(file_at(&conn, rid, "art/a.psd", 1).unwrap().is_some());
        assert!(file_at(&conn, rid, "art/sub/b.psd", 1).unwrap().is_some());
        assert!(dir_exists_at(&conn, rid, "art", 1).unwrap());
        assert!(dir_exists_at(&conn, rid, "art/sub", 1).unwrap());

        // rev2：目录删除 = 一行 Tombstone
        commit(&mut conn, rid, uid, "rmdir art", &[Change::delete_dir("art")]);
        assert!(file_at(&conn, rid, "art/a.psd", 2).unwrap().is_none(), "Tombstone 应覆盖子树文件");
        assert!(file_at(&conn, rid, "art/sub/b.psd", 2).unwrap().is_none(), "Tombstone 应递归覆盖深层文件");
        assert!(!dir_exists_at(&conn, rid, "art", 2).unwrap());
        assert!(!dir_exists_at(&conn, rid, "art/sub", 2).unwrap());
        assert_eq!(names(&list_dir_at(&conn, rid, "art", 2).unwrap()), Vec::<String>::new());
        assert_eq!(names(&list_dir_at(&conn, rid, "", 2).unwrap()), vec!["keep.txt"]);

        // 历史修订仍可取回（L1 删除可逆）
        assert!(file_at(&conn, rid, "art/a.psd", 1).unwrap().is_some());

        // rev3：Tombstone 后新增文件 → 存活（dir 锁/删除对新路径不追溯）
        commit(&mut conn, rid, uid, "re-add", &[Change::add_file("art/new.psd", "h4", 50)]);
        assert!(file_at(&conn, rid, "art/new.psd", 3).unwrap().is_some());
        assert!(dir_exists_at(&conn, rid, "art", 3).unwrap(), "新文件使目录隐式复活");
        assert!(file_at(&conn, rid, "art/a.psd", 3).unwrap().is_none(), "旧文件仍被 Tombstone 覆盖");

        // head_entries 快路径同步：物理删除子树行
        assert_eq!(names(&list_dir_head(&conn, rid, "art").unwrap()), vec!["art/new.psd"]);
    }

    // ② 恢复 = 新提交：restore 把 source_rev 内容以 add/modify 写入新修订，可再次恢复
    #[test]
    fn restore_is_a_new_commit() {
        let (_dir, mut conn, rid, uid) = setup();
        commit(&mut conn, rid, uid, "init", &[
            Change::add_file("sub/b.txt", "hb", 20),
            Change::add_file("sub/deep/c.txt", "hc", 30),
            Change::add_dir("sub/empty"),
        ]);
        commit(&mut conn, rid, uid, "rmdir sub", &[Change::delete_dir("sub")]);
        assert!(file_at(&conn, rid, "sub/b.txt", 2).unwrap().is_none());

        // 从 rev1 恢复目录
        let r3 = restore(&mut conn, rid, uid, "sub", 1, "restore sub from r1").unwrap();
        assert_eq!(r3.rev, 3, "恢复产生新修订");
        assert_eq!(file_hash(&conn, rid, "sub/b.txt", 3).as_deref(), Some("hb"));
        assert_eq!(file_hash(&conn, rid, "sub/deep/c.txt", 3).as_deref(), Some("hc"));
        assert!(dir_exists_at(&conn, rid, "sub/empty", 3).unwrap(), "显式空目录也被恢复");

        // 恢复本身在 changes 里是 add 行（新提交，非"反向变更"）
        let ops: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT op FROM changes WHERE repo_id=?1 AND rev=3 ORDER BY path")
                .unwrap();
            stmt.query_map(params![rid], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(ops.iter().all(|o| o == "add"), "恢复应写 add 行: {ops:?}");

        // 恢复的恢复：从 rev3 再恢复（历史永远线性只增）
        commit(&mut conn, rid, uid, "rmdir again", &[Change::delete_dir("sub")]);
        let r5 = restore(&mut conn, rid, uid, "sub", 3, "restore from restore").unwrap();
        assert_eq!(r5.rev, 5);
        assert_eq!(file_hash(&conn, rid, "sub/b.txt", 5).as_deref(), Some("hb"));

        // 恢复单个文件
        commit(&mut conn, rid, uid, "del file", &[Change::delete_file("sub/b.txt")]);
        let r7 = restore(&mut conn, rid, uid, "sub/b.txt", 5, "restore one file").unwrap();
        assert_eq!(r7.rev, 7);
        assert_eq!(file_hash(&conn, rid, "sub/b.txt", 7).as_deref(), Some("hb"));

        // 恢复不存在的路径/修订 → NotFound
        assert!(matches!(
            restore(&mut conn, rid, uid, "no/such/path", 1, "x"),
            Err(AppError::NotFound(_))
        ));
    }

    // ③ HEAD 快路径与历史回溯结果一致
    #[test]
    fn head_fast_path_matches_history() {
        let (_dir, mut conn, rid, uid) = setup();
        commit(&mut conn, rid, uid, "r1", &[
            Change::add_file("art/a.psd", "h1", 10),
            Change::add_file("art/sub/b.psd", "h2", 20),
            Change::add_file("root.txt", "h3", 5),
            Change::add_dir("docs"),
        ]);
        commit(&mut conn, rid, uid, "r2", &[Change::modify_file("art/a.psd", "h1b", 15)]);
        commit(&mut conn, rid, uid, "r3", &[Change::delete_file("root.txt")]);
        commit(&mut conn, rid, uid, "r4", &[Change::delete_dir("art/sub")]);
        commit(&mut conn, rid, uid, "r5", &[
            Change::add_file("art/sub/c.psd", "h4", 7), // Tombstone 后重建子目录
        ]);
        let head = repo_head_rev(&conn, rid).unwrap();
        assert_eq!(head, 5);

        for prefix in ["", "art", "art/sub", "docs", "nope"] {
            let via_head = list_dir_head(&conn, rid, prefix).unwrap();
            let via_hist = list_dir_at(&conn, rid, prefix, head).unwrap();
            let key = |v: &Vec<EntryInfo>| {
                v.iter()
                    .map(|e| (e.path.clone(), e.kind, e.blob_hash.clone()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(key(&via_head), key(&via_hist), "prefix={prefix:?} HEAD 与历史推导不一致");
        }

        // 各中间修订的目录内容也自洽
        assert_eq!(names(&list_dir_at(&conn, rid, "", 1).unwrap()).len(), 3, "r1: art + docs + root.txt");
        assert_eq!(names(&list_dir_at(&conn, rid, "", 3).unwrap()), vec!["art", "docs"], "r3: root.txt 已删");
        assert_eq!(names(&list_dir_at(&conn, rid, "art", 4).unwrap()), vec!["art/a.psd"], "r4: art/sub 被 Tombstone");
        assert_eq!(names(&list_dir_at(&conn, rid, "art", 5).unwrap()), vec!["art/a.psd", "art/sub"], "r5: art/sub 复活");
    }

    // ④ 目录存在性由"存活子文件 + 未被 Tombstone 覆盖"推导（显式空目录除外）
    #[test]
    fn dir_existence_is_derived() {
        let (_dir, mut conn, rid, uid) = setup();
        // 无显式目录行：文件 add 隐式建立目录
        commit(&mut conn, rid, uid, "r1", &[Change::add_file("sub/x.txt", "h1", 1)]);
        assert!(dir_exists_at(&conn, rid, "sub", 1).unwrap(), "子文件存活 → 目录存在");

        // 文件删除后目录随之消失（无显式行）
        commit(&mut conn, rid, uid, "r2", &[Change::delete_file("sub/x.txt")]);
        assert!(!dir_exists_at(&conn, rid, "sub", 2).unwrap(), "子文件删除且无显式行 → 目录不存在");
        assert!(dir_exists_at(&conn, rid, "sub", 1).unwrap(), "历史修订仍存在");

        // 显式 mkdir 的空目录独立存活
        commit(&mut conn, rid, uid, "r3", &[Change::add_dir("empty")]);
        assert!(dir_exists_at(&conn, rid, "empty", 3).unwrap(), "显式空目录存在");
        assert!(list_dir_at(&conn, rid, "", 3).unwrap().iter().any(|e| e.path == "empty"));

        // rmdir 显式删除
        commit(&mut conn, rid, uid, "r4", &[Change::delete_dir("empty")]);
        assert!(!dir_exists_at(&conn, rid, "empty", 4).unwrap());

        // 祖先 Tombstone 覆盖目录存在性
        commit(&mut conn, rid, uid, "r5", &[
            Change::add_file("top/mid/leaf.txt", "h2", 2),
        ]);
        assert!(dir_exists_at(&conn, rid, "top/mid", 5).unwrap());
        commit(&mut conn, rid, uid, "r6", &[Change::delete_dir("top")]);
        assert!(!dir_exists_at(&conn, rid, "top/mid", 6).unwrap(), "祖先 Tombstone 覆盖深层目录");
        assert!(dir_exists_at(&conn, rid, "top/mid", 5).unwrap());
    }

    // ⑤ 任意修订取单文件 = 路径历史倒序第一条
    #[test]
    fn file_at_any_rev() {
        let (_dir, mut conn, rid, uid) = setup();
        commit(&mut conn, rid, uid, "r1", &[Change::add_file("f.txt", "H1", 11)]);
        commit(&mut conn, rid, uid, "r2", &[Change::add_file("g.txt", "HG", 22)]); // 无关提交
        commit(&mut conn, rid, uid, "r3", &[Change::modify_file("f.txt", "H2", 33)]);
        commit(&mut conn, rid, uid, "r4", &[Change::add_file("h.txt", "HH", 44)]);
        commit(&mut conn, rid, uid, "r5", &[Change::delete_file("f.txt")]);

        assert_eq!(file_hash(&conn, rid, "f.txt", 0).as_deref(), None, "rev0 无历史");
        assert_eq!(file_hash(&conn, rid, "f.txt", 1).as_deref(), Some("H1"));
        assert_eq!(file_hash(&conn, rid, "f.txt", 2).as_deref(), Some("H1"), "无关修订沿用旧内容");
        assert_eq!(file_hash(&conn, rid, "f.txt", 3).as_deref(), Some("H2"));
        assert_eq!(file_hash(&conn, rid, "f.txt", 4).as_deref(), Some("H2"));
        assert_eq!(file_hash(&conn, rid, "f.txt", 5).as_deref(), None, "删除后不存在");
        assert!(file_at(&conn, rid, "never.txt", 5).unwrap().is_none());
    }

    // manifest_hash：提交后为空，异步刷新后为正确值（仅 HEAD，§3.3）
    #[test]
    fn manifest_hash_async_refresh() {
        let (_dir, mut conn, rid, uid) = setup();
        commit(&mut conn, rid, uid, "r1", &[
            Change::add_file("b.txt", "HB", 2),
            Change::add_file("a.txt", "HA", 1),
        ]);
        commit(&mut conn, rid, uid, "r2", &[Change::add_file("c.txt", "HC", 3)]);

        let head = repo_head_rev(&conn, rid).unwrap();
        let mh: String = conn
            .query_row(
                "SELECT manifest_hash FROM revisions WHERE repo_id=?1 AND rev=?2",
                params![rid, head],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mh, "", "提交事务内不计算 manifest_hash（异步补算）");

        let computed = refresh_head_manifest_hash(&conn, rid).unwrap().unwrap();
        // 独立复算：sha256(排序后的 "path\0hash\n" 拼接)
        let mut hasher = Sha256::new();
        for (p, h) in [("a.txt", "HA"), ("b.txt", "HB"), ("c.txt", "HC")] {
            hasher.update(p.as_bytes());
            hasher.update(b"\0");
            hasher.update(h.as_bytes());
            hasher.update(b"\n");
        }
        let expect = hex::encode(hasher.finalize());
        assert_eq!(computed, expect);

        let mh2: String = conn
            .query_row(
                "SELECT manifest_hash FROM revisions WHERE repo_id=?1 AND rev=?2",
                params![rid, head],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mh2, expect, "刷新后落库");

        // 新提交后 HEAD 的 manifest_hash 又变空，历史修订保持（只补 HEAD）
        commit(&mut conn, rid, uid, "r3", &[Change::delete_file("c.txt")]);
        let head3 = repo_head_rev(&conn, rid).unwrap();
        let mh3: String = conn
            .query_row(
                "SELECT manifest_hash FROM revisions WHERE repo_id=?1 AND rev=?2",
                params![rid, head3],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mh3, "");
        refresh_head_manifest_hash(&conn, rid).unwrap();
        let mh_old: String = conn
            .query_row(
                "SELECT manifest_hash FROM revisions WHERE repo_id=?1 AND rev=?2",
                params![rid, head],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mh_old, expect, "历史修订值不被覆盖");
    }

    // 提交统计与完整性：file_count / byte_delta（去重后）/ 重复路径拒绝
    #[test]
    fn commit_stats_and_integrity() {
        let (_dir, mut conn, rid, uid) = setup();
        let r1 = commit(&mut conn, rid, uid, "r1", &[
            Change::add_file("a.psd", "h1", 100),
            Change::add_file("b.psd", "h2", 200),
        ]);
        assert_eq!(r1.rev, 1);
        assert_eq!(r1.file_count, 2);
        assert_eq!(r1.byte_delta, 300);

        // 同 blob 跨修订去重：byte_delta 不重复计
        let r2 = commit(&mut conn, rid, uid, "r2", &[
            Change::modify_file("a.psd", "h1", 100), // 未变内容
            Change::add_file("c.psd", "h1", 100),    // 复用 h1
        ]);
        assert_eq!(r2.byte_delta, 0, "已引用 blob 不再计入新增字节");

        // 同一提交中重复路径 → 主键冲突拒绝
        let err = apply_commit(&mut conn, rid, uid, "dup", &[
            Change::add_file("x.txt", "h9", 1),
            Change::add_file("x.txt", "h9", 1),
        ]);
        assert!(err.is_err(), "重复路径必须整单拒绝");
        assert_eq!(repo_head_rev(&conn, rid).unwrap(), 2, "失败提交不推进 head_rev");

        // 空提交 → 拒绝（§5.3 changes 为零则回滚），head_rev 不推进
        assert!(
            matches!(
                apply_commit(&mut conn, rid, uid, "empty", &[]),
                Err(AppError::InvalidArgument(_))
            ),
            "空提交必须拒绝"
        );
        assert_eq!(repo_head_rev(&conn, rid).unwrap(), 2);
    }

    // ④b HEAD 目录树：隐式目录被物化、最后一个子项消失即回收、显式空目录保留（M1.5 B4）
    #[test]
    fn head_dirs_materialized_and_pruned() {
        let (_dir, mut conn, rid, uid) = setup();
        commit(&mut conn, rid, uid, "r1", &[
            Change::add_file("a/b/c.txt", "h1", 1),
            Change::add_dir("keep"),
        ]);
        assert_eq!(names(&list_dir_head(&conn, rid, "").unwrap()), vec!["a", "keep"], "文件隐式产生的祖先必须被物化");
        assert_eq!(names(&list_dir_head(&conn, rid, "a").unwrap()), vec!["a/b"]);
        assert_eq!(names(&list_dir_head(&conn, rid, "a/b").unwrap()), vec!["a/b/c.txt"]);

        // 唯一子项删除 → 隐式祖先 a/b、a 一并回收；显式空目录 keep 保留
        commit(&mut conn, rid, uid, "r2", &[Change::delete_file("a/b/c.txt")]);
        assert_eq!(names(&list_dir_head(&conn, rid, "").unwrap()), vec!["keep"], "隐式目录随最后一个子项消失");
        assert!(list_dir_head(&conn, rid, "a").unwrap().is_empty());

        commit(&mut conn, rid, uid, "r3", &[Change::delete_dir("keep")]);
        assert!(list_dir_head(&conn, rid, "").unwrap().is_empty(), "显式目录 rmdir 后消失");

        // 深层目录删除：子树行全清，祖先随之回收
        commit(&mut conn, rid, uid, "r4", &[
            Change::add_file("x/y/z1.txt", "h2", 2),
            Change::add_file("x/y/z2.txt", "h3", 3),
        ]);
        assert_eq!(names(&list_dir_head(&conn, rid, "x").unwrap()), vec!["x/y"]);
        commit(&mut conn, rid, uid, "r5", &[Change::delete_dir("x/y")]);
        assert!(list_dir_head(&conn, rid, "x").unwrap().is_empty(), "x/y 子树清空");
        assert!(list_dir_head(&conn, rid, "").unwrap().is_empty(), "x 随之回收");
    }

    // ④c rebuild_head_tree 自愈：破坏 parent_path 与目录行后可重建（迁移回填 / 自愈入口）
    #[test]
    fn rebuild_head_tree_repairs_tree() {
        let (_dir, mut conn, rid, uid) = setup();
        commit(&mut conn, rid, uid, "r1", &[
            Change::add_file("a/b/c.txt", "h1", 1),
            Change::add_dir("keep"),
        ]);
        // 只破坏"可推导"的部分：parent_path 归零 + 隐式目录行消失。
        // 显式目录行不能删——"空目录是有意为之"这件事只存在那里，删了就推不回来。
        conn.execute("UPDATE head_entries SET parent_path = ''", []).unwrap();
        conn.execute("DELETE FROM head_entries WHERE kind = 'dir' AND is_explicit = 0", []).unwrap();
        assert_eq!(list_dir_head(&conn, rid, "a").unwrap().len(), 0, "破坏后列不出子项");

        let n = rebuild_head_tree(&conn, rid).unwrap();
        assert!(n >= 3, "应重建 a、a/b 并保留显式目录 keep，实际 {n}");
        assert_eq!(names(&list_dir_head(&conn, rid, "").unwrap()), vec!["a", "keep"]);
        assert_eq!(names(&list_dir_head(&conn, rid, "a").unwrap()), vec!["a/b"]);
        let head = repo_head_rev(&conn, rid).unwrap();
        assert_eq!(
            names(&list_dir_head(&conn, rid, "").unwrap()),
            names(&list_dir_at(&conn, rid, "", head).unwrap()),
            "重建后应与历史推导一致"
        );
    }

    // B1：前缀 → 半开区间的边界（索引能否下推取决于这两个边界是否正确）
    #[test]
    fn prefix_bounds_cover_exactly_the_subtree() {
        assert_eq!(prefix_bounds(""), None, "空前缀无界，调用方必须走 parent_path 快路径");
        let (lo, hi) = prefix_bounds("art/").unwrap();
        assert_eq!(lo, "art/");
        assert_eq!(hi, "art0");
        let inside = |p: &str| p.as_bytes() >= lo.as_bytes() && p.as_bytes() < hi.as_bytes();
        assert!(inside("art/a.txt"));
        assert!(inside("art/sub/b.txt"));
        assert!(!inside("art"), "目录自身不在区间内（需额外 path = ? 条件）");
        assert!(!inside("art0/z"));
        assert!(!inside("arts/z"));
        assert!(!inside("z/art/a.txt"));
    }

    // 路径合法性（§6.6）：保留名/非法字符/超长/非 NFC
    #[test]
    fn path_validation() {
        assert!(validate_path("art/char/boss.psd").is_ok());
        assert!(validate_path("a/b/c.txt").is_ok());
        for bad in [
            "", "/abs.txt", "a//b.txt", "a/./b.txt", "../esc.txt", "a/../b",
            "a\\b.txt", "a/b\0c", "con.txt", "AUX", "COM1.dat", "nul",
            "trailing. ", "trailing.", "with<gt.txt", "with:colon",
            ".b-artifact/x", "a/.b-artifact",
        ] {
            assert!(matches!(validate_path(bad), Err(AppError::InvalidPath(_))), "应拒绝: {bad:?}");
        }
        // 超长：段 > 255B / 全路径 > 1024B
        let long_seg = "x".repeat(256);
        assert!(matches!(validate_path(&long_seg), Err(AppError::InvalidPath(_))));
        let long_path = format!("{}/{}", "d".repeat(100), "f".repeat(1000));
        assert!(matches!(validate_path(&long_path), Err(AppError::InvalidPath(_))));
        // 非 NFC（NFD 形态的 "é" = e + U+0301）
        let nfd = "caf\u{0065}\u{0301}.txt";
        assert!(matches!(validate_path(nfd), Err(AppError::PathNotNormalized(_))));
        // NFC 形态合法
        assert!(validate_path("caf\u{00e9}.txt").is_ok());

        // 非法路径进提交 → 整单拒绝且不落库
        let (_dir, mut conn, rid, uid) = setup();
        assert!(matches!(
            apply_commit(&mut conn, rid, uid, "bad", &[Change::add_file("CON", "h", 1)]),
            Err(AppError::InvalidPath(_))
        ));
        assert!(matches!(
            apply_commit(&mut conn, rid, uid, "bad", &[Change::add_file(nfd, "h", 1)]),
            Err(AppError::PathNotNormalized(_))
        ));
        assert_eq!(repo_head_rev(&conn, rid).unwrap(), 0);
    }
}
