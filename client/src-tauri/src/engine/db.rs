/**
 * 工作副本元数据库（§6.1 工作副本布局）。
 *
 * `.b-artifact/wc.db` 是工作副本的唯一真相来源：基线（entries）、待提交集（pending）、
 * 本人持有的锁（locks）、冲突登记（conflicts）。
 *
 * **磁盘格式与 TS 版逐字兼容**：表名/列名/CHECK/索引名/schema_version 一字不改，
 * 也不开 WAL（工作副本目录会被用户直接拷贝/备份，WAL 未 checkpoint 时只拷 wc.db 会丢数据；
 * 客户端本是单进程短事务，默认 journal 更安全）。迁移期两版引擎要能交替打开同一个副本。
 */
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::errors::{ClientError, ClientResult};

pub const WC_DIR: &str = ".b-artifact";
pub const WC_DB: &str = "wc.db";
pub const WC_LOCK: &str = "wc.lock";

/// 迁移：下标 + 1 即 schema 版本号。新增迁移只能追加，不允许改写历史。
const MIGRATIONS: &[&str] = &[
    // v1：基线表 + 待提交集 + 锁 + 元信息
    r#"
    CREATE TABLE meta (
      key   TEXT PRIMARY KEY,
      value TEXT NOT NULL
    );
    CREATE TABLE entries (
      path          TEXT PRIMARY KEY,
      kind          TEXT NOT NULL CHECK (kind IN ('file','dir')),
      base_rev      INTEGER NOT NULL,
      base_hash     TEXT,
      size          INTEGER NOT NULL DEFAULT 0,
      mtime_ms      INTEGER NOT NULL DEFAULT 0,
      mode          INTEGER NOT NULL DEFAULT 0,
      status        TEXT NOT NULL DEFAULT 'normal',
      case_conflict INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_entries_status ON entries(status);
    CREATE TABLE pending (
      path      TEXT PRIMARY KEY,
      op        TEXT NOT NULL CHECK (op IN ('add','modify','delete')),
      staged_at INTEGER NOT NULL
    );
    CREATE TABLE locks (
      path       TEXT PRIMARY KEY,
      kind       TEXT NOT NULL CHECK (kind IN ('file','dir')),
      token      TEXT NOT NULL,
      owner      TEXT,
      comment    TEXT,
      expires_at TEXT
    );
    CREATE TABLE schema_version (version INTEGER NOT NULL);
    INSERT INTO schema_version (version) VALUES (1);
    "#,
    // v2：冲突登记（§6.2 conflicted / §6.5 冲突视图）
    //
    // 为什么必须单独记一张表：update 撞冲突时会把工作文件换成服务端版本，
    // entry.base_hash 也随之被覆盖——**共同祖先（base）的哈希就此丢失**，
    // 而三方对比（本地 / 服务端 / 基线）恰恰需要它。所以要在这里留一份。
    r#"
    CREATE TABLE conflicts (
      path        TEXT PRIMARY KEY,
      kind        TEXT NOT NULL CHECK (kind IN ('file','dir')),
      base_hash   TEXT,
      theirs_hash TEXT,
      mine_hash   TEXT,
      theirs_rev  INTEGER NOT NULL DEFAULT 0,
      reason      TEXT NOT NULL CHECK (reason IN ('both-modified','deleted-remotely')),
      created_at  INTEGER NOT NULL
    );
    "#,
];

pub const SCHEMA_VERSION: usize = MIGRATIONS.len();

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
        }
    }
    pub fn parse(s: &str) -> ClientResult<Self> {
        match s {
            "file" => Ok(EntryKind::File),
            "dir" => Ok(EntryKind::Dir),
            other => Err(ClientError::wc(
                "WC_CORRUPT",
                format!("未知 entry kind：{other}"),
            )),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PendingOp {
    Add,
    Modify,
    Delete,
}

impl PendingOp {
    pub fn as_str(self) -> &'static str {
        match self {
            PendingOp::Add => "add",
            PendingOp::Modify => "modify",
            PendingOp::Delete => "delete",
        }
    }
    pub fn parse(s: &str) -> ClientResult<Self> {
        match s {
            "add" => Ok(PendingOp::Add),
            "modify" => Ok(PendingOp::Modify),
            "delete" => Ok(PendingOp::Delete),
            other => Err(ClientError::wc(
                "WC_CORRUPT",
                format!("未知 pending op：{other}"),
            )),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EntryStatus {
    Normal,
    Modified,
    Added,
    Deleted,
    Missing,
    Conflicted,
    NeedsUpdate,
}

impl EntryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryStatus::Normal => "normal",
            EntryStatus::Modified => "modified",
            EntryStatus::Added => "added",
            EntryStatus::Deleted => "deleted",
            EntryStatus::Missing => "missing",
            EntryStatus::Conflicted => "conflicted",
            EntryStatus::NeedsUpdate => "needs-update",
        }
    }
    pub fn parse(s: &str) -> ClientResult<Self> {
        Ok(match s {
            "normal" => EntryStatus::Normal,
            "modified" => EntryStatus::Modified,
            "added" => EntryStatus::Added,
            "deleted" => EntryStatus::Deleted,
            "missing" => EntryStatus::Missing,
            "conflicted" => EntryStatus::Conflicted,
            "needs-update" => EntryStatus::NeedsUpdate,
            other => {
                return Err(ClientError::wc(
                    "WC_CORRUPT",
                    format!("未知 entry status：{other}"),
                ))
            }
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EntryRow {
    pub path: String,
    pub kind: EntryKind,
    pub base_rev: i64,
    pub base_hash: Option<String>,
    pub size: i64,
    pub mtime_ms: i64,
    pub mode: i64,
    pub status: EntryStatus,
    pub case_conflict: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PendingRow {
    pub path: String,
    pub op: PendingOp,
    pub staged_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LockRow {
    pub path: String,
    pub kind: EntryKind,
    pub token: String,
    pub owner: Option<String>,
    pub comment: Option<String>,
    pub expires_at: Option<String>,
}

/// 冲突成因（决定界面上能做什么）。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictReason {
    /// 双方都改了：本地版本存 `<path>.mine`，工作文件是服务端版本，可三方对比。
    BothModified,
    /// 服务端删了但本地改过：保留本地文件，没有服务端内容可对比，只能"保留 / 接受删除"。
    DeletedRemotely,
}

impl ConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictReason::BothModified => "both-modified",
            ConflictReason::DeletedRemotely => "deleted-remotely",
        }
    }
    pub fn parse(s: &str) -> ClientResult<Self> {
        match s {
            "both-modified" => Ok(ConflictReason::BothModified),
            "deleted-remotely" => Ok(ConflictReason::DeletedRemotely),
            other => Err(ClientError::wc(
                "WC_CORRUPT",
                format!("未知冲突成因：{other}"),
            )),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ConflictRow {
    pub path: String,
    pub kind: EntryKind,
    /// 共同祖先内容哈希（冲突发生前的基线）——三方对比的 base。
    pub base_hash: Option<String>,
    /// 服务端版本哈希（`deleted-remotely` 时为 None）。
    pub theirs_hash: Option<String>,
    /// 本地版本哈希（写 `<path>.mine` 时算得；懒算时为 None）。
    pub mine_hash: Option<String>,
    pub theirs_rev: i64,
    pub reason: ConflictReason,
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct WcMeta {
    /// 服务端地址（含 /api/v1）。
    pub server: String,
    /// 仓库名。
    pub repo: String,
    /// 检出时的 base 修订号。
    pub revision: i64,
    /// 部分检出的前缀列表；空数组表示全量。
    pub sparse_paths: Vec<String>,
    /// 当前登录用户名。
    pub user: String,
    /// 全局缓存目录（pristine 硬链接源）。
    pub cache_dir: String,
}

/// 工作副本句柄。所有写操作应经 `tx()` 以保证扫描/落库原子（§15.2 P0）。
pub struct Wc {
    pub root: PathBuf,
    pub db_path: PathBuf,
    db: Connection,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn opt_to_sql(v: &Option<String>) -> rusqlite::types::Value {
    match v {
        Some(s) => rusqlite::types::Value::Text(s.clone()),
        None => rusqlite::types::Value::Null,
    }
}

impl Wc {
    /// 打开已有工作副本；不存在则抛 `NOT_A_WORKING_COPY`。
    pub fn open(root: &Path) -> ClientResult<Self> {
        let db_path = root.join(WC_DIR).join(WC_DB);
        if !db_path.exists() {
            return Err(ClientError::wc(
                "NOT_A_WORKING_COPY",
                format!("不是 b-artifact 工作副本：{}", root.display()),
            ));
        }
        let db = Connection::open(&db_path)?;
        db.execute_batch("PRAGMA foreign_keys = ON;")?;
        let wc = Self {
            root: root.to_path_buf(),
            db_path,
            db,
        };
        wc.migrate()?;
        Ok(wc)
    }

    /// 初始化新的工作副本（目录不存在则创建）。
    pub fn init(root: &Path, meta: &WcMeta) -> ClientResult<Self> {
        std::fs::create_dir_all(root.join(WC_DIR))?;
        let db_path = root.join(WC_DIR).join(WC_DB);
        let db = Connection::open(&db_path)?;
        db.execute_batch("PRAGMA foreign_keys = ON;")?;
        let mut wc = Self {
            root: root.to_path_buf(),
            db_path,
            db,
        };
        wc.migrate()?;
        wc.tx(|w| w.set_meta(meta))?;
        Ok(wc)
    }

    fn migrate(&self) -> ClientResult<()> {
        let has_version: Option<String> = self
            .db
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let current: i64 = if has_version.is_some() {
            self.db
                .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
                .unwrap_or(0)
        } else {
            0
        };
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            if (i as i64) < current {
                continue;
            }
            self.db.execute_batch("BEGIN")?;
            let r = self
                .db
                .execute_batch(sql)
                .and_then(|_| {
                    self.db
                        .execute_batch(&format!("UPDATE schema_version SET version = {}", i + 1))
                });
            match r {
                Ok(()) => self.db.execute_batch("COMMIT")?,
                Err(e) => {
                    let _ = self.db.execute_batch("ROLLBACK");
                    return Err(ClientError::wc(
                        "WC_CORRUPT",
                        format!("wc.db 迁移 {} 失败：{e}", i + 1),
                    ));
                }
            }
        }
        Ok(())
    }

    /// 事务包装：失败自动回滚并原样抛出。
    pub fn tx<T>(&mut self, f: impl FnOnce(&mut Self) -> ClientResult<T>) -> ClientResult<T> {
        self.db.execute_batch("BEGIN")?;
        match f(self) {
            Ok(v) => {
                self.db.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // ---------- meta ----------

    /// meta 的每一行都以 **JSON 文本** 存（与 TS 版 `JSON.stringify` 一致）。
    pub fn get_meta(&self) -> ClientResult<WcMeta> {
        let mut stmt = self.db.prepare("SELECT key, value FROM meta")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            return Err(ClientError::wc("WC_CORRUPT", "wc.db 缺少 meta"));
        }
        let get = |k: &str| -> Option<Value> {
            rows.iter()
                .find(|(key, _)| key == k)
                .map(|(_, raw)| serde_json::from_str::<Value>(raw).unwrap_or(Value::Null))
        };
        let s = |k: &str| -> String {
            match get(k) {
                Some(Value::String(s)) => s,
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            }
        };
        let n = |k: &str| -> i64 {
            match get(k) {
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
                Some(Value::String(s)) => s.parse().unwrap_or(0),
                _ => 0,
            }
        };
        let sparse = match get("sparse_paths") {
            Some(Value::Array(a)) => a
                .into_iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            _ => Vec::new(),
        };
        Ok(WcMeta {
            server: s("server"),
            repo: s("repo"),
            revision: n("revision"),
            sparse_paths: sparse,
            user: s("user"),
            cache_dir: s("cache_dir"),
        })
    }

    pub fn set_meta(&mut self, meta: &WcMeta) -> ClientResult<()> {
        self.set_meta_json("server", &serde_json::json!(meta.server))?;
        self.set_meta_json("repo", &serde_json::json!(meta.repo))?;
        self.set_meta_json("revision", &serde_json::json!(meta.revision))?;
        self.set_meta_json("sparse_paths", &serde_json::json!(meta.sparse_paths))?;
        self.set_meta_json("user", &serde_json::json!(meta.user))?;
        self.set_meta_json("cache_dir", &serde_json::json!(meta.cache_dir))?;
        Ok(())
    }

    pub fn set_meta_json(&mut self, key: &str, value: &Value) -> ClientResult<()> {
        self.db.execute(
            "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value).unwrap_or_else(|_| "null".into())],
        )?;
        Ok(())
    }

    pub fn revision(&self) -> i64 {
        self.db
            .query_row("SELECT value FROM meta WHERE key = ?", ["revision"], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|v| match v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            })
            .unwrap_or(0)
    }

    pub fn set_revision(&mut self, n: i64) -> ClientResult<()> {
        self.set_meta_json("revision", &serde_json::json!(n))
    }

    pub fn sparse_paths(&self) -> Vec<String> {
        self.get_meta().map(|m| m.sparse_paths).unwrap_or_default()
    }

    // ---------- entries ----------

    pub fn get_entry(&self, path: &str) -> ClientResult<Option<EntryRow>> {
        let r = self
            .db
            .query_row("SELECT * FROM entries WHERE path = ?", [path], row_to_entry)
            .optional()?;
        Ok(r)
    }

    pub fn has_entry(&self, path: &str) -> ClientResult<bool> {
        Ok(self.get_entry(path)?.is_some())
    }

    pub fn upsert_entry(&mut self, row: &EntryRow) -> ClientResult<()> {
        self.db.execute(
            "INSERT INTO entries (path, kind, base_rev, base_hash, size, mtime_ms, mode, status, case_conflict)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET
               kind = excluded.kind, base_rev = excluded.base_rev, base_hash = excluded.base_hash,
               size = excluded.size, mtime_ms = excluded.mtime_ms, mode = excluded.mode,
               status = excluded.status, case_conflict = excluded.case_conflict",
            params![
                row.path,
                row.kind.as_str(),
                row.base_rev,
                row.base_hash,
                row.size,
                row.mtime_ms,
                row.mode,
                row.status.as_str(),
                if row.case_conflict { 1 } else { 0 },
            ],
        )?;
        Ok(())
    }

    pub fn delete_entry(&mut self, path: &str) -> ClientResult<()> {
        self.db
            .execute("DELETE FROM entries WHERE path = ?", [path])?;
        Ok(())
    }

    pub fn all_entries(&self) -> ClientResult<Vec<EntryRow>> {
        let mut stmt = self.db.prepare("SELECT * FROM entries ORDER BY path")?;
        let rows = stmt.query_map([], row_to_entry)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 前缀子树查询（含 prefix 自身）。
    ///
    /// 用半开区间而非 `LIKE 'pfx%'`：SQLite 默认 `case_sensitive_like=OFF`，前缀 LIKE
    /// 不下推索引；而 `'/'` 是 0x2F、`'0'` 是 0x30，区间 `[pfx+'/', pfx+'0')` 正好
    /// 覆盖且仅覆盖 `pfx/` 下的所有路径。
    pub fn entries_under(&self, prefix: &str) -> ClientResult<Vec<EntryRow>> {
        if prefix.is_empty() {
            return self.all_entries();
        }
        let lo = format!("{prefix}/");
        let hi = format!("{prefix}0");
        let mut stmt = self.db.prepare(
            "SELECT * FROM entries WHERE path = ? OR (path >= ? AND path < ?) ORDER BY path",
        )?;
        let rows = stmt
            .query_map(params![prefix, lo, hi], row_to_entry)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---------- pending ----------

    pub fn all_pending(&self) -> ClientResult<Vec<PendingRow>> {
        let mut stmt = self.db.prepare("SELECT * FROM pending ORDER BY path")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PendingRow {
                    path: r.get(0)?,
                    op: PendingOp::parse(&r.get::<_, String>(1)?)
                        .unwrap_or(PendingOp::Modify),
                    staged_at: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn stage(&mut self, path: &str, op: PendingOp) -> ClientResult<()> {
        self.db.execute(
            "INSERT INTO pending (path, op, staged_at) VALUES (?, ?, ?) ON CONFLICT(path) DO UPDATE SET op = excluded.op, staged_at = excluded.staged_at",
            params![path, op.as_str(), now_ms()],
        )?;
        Ok(())
    }

    pub fn unstage(&mut self, path: &str) -> ClientResult<()> {
        self.db
            .execute("DELETE FROM pending WHERE path = ?", [path])?;
        Ok(())
    }

    pub fn clear_pending(&mut self) -> ClientResult<()> {
        self.db.execute("DELETE FROM pending", [])?;
        Ok(())
    }

    pub fn pending_op(&self, path: &str) -> ClientResult<Option<PendingOp>> {
        let raw: Option<String> = self
            .db
            .query_row("SELECT op FROM pending WHERE path = ?", [path], |r| r.get(0))
            .optional()?;
        match raw {
            Some(s) => Ok(Some(PendingOp::parse(&s)?)),
            None => Ok(None),
        }
    }

    // ---------- locks ----------

    pub fn all_locks(&self) -> ClientResult<Vec<LockRow>> {
        let mut stmt = self.db.prepare("SELECT * FROM locks ORDER BY path")?;
        let rows = stmt.query_map([], row_to_lock)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_lock(&self, path: &str) -> ClientResult<Option<LockRow>> {
        Ok(self.all_locks()?.into_iter().find(|l| l.path == path))
    }

    pub fn put_lock(&mut self, row: &LockRow) -> ClientResult<()> {
        self.db.execute(
            "INSERT INTO locks (path, kind, token, owner, comment, expires_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET kind = excluded.kind, token = excluded.token,
               owner = excluded.owner, comment = excluded.comment, expires_at = excluded.expires_at",
            params![
                row.path,
                row.kind.as_str(),
                row.token,
                opt_to_sql(&row.owner),
                opt_to_sql(&row.comment),
                opt_to_sql(&row.expires_at),
            ],
        )?;
        Ok(())
    }

    pub fn remove_lock(&mut self, path: &str) -> ClientResult<()> {
        self.db.execute("DELETE FROM locks WHERE path = ?", [path])?;
        Ok(())
    }

    // ---------- conflicts ----------

    pub fn all_conflicts(&self) -> ClientResult<Vec<ConflictRow>> {
        let mut stmt = self.db.prepare("SELECT * FROM conflicts ORDER BY path")?;
        let rows = stmt.query_map([], row_to_conflict)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_conflict(&self, path: &str) -> ClientResult<Option<ConflictRow>> {
        Ok(self.all_conflicts()?.into_iter().find(|c| c.path == path))
    }

    pub fn put_conflict(&mut self, row: &ConflictRow) -> ClientResult<()> {
        self.db.execute(
            "INSERT INTO conflicts (path, kind, base_hash, theirs_hash, mine_hash, theirs_rev, reason, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET kind = excluded.kind, base_hash = excluded.base_hash,
               theirs_hash = excluded.theirs_hash, mine_hash = excluded.mine_hash,
               theirs_rev = excluded.theirs_rev, reason = excluded.reason, created_at = excluded.created_at",
            params![
                row.path,
                row.kind.as_str(),
                opt_to_sql(&row.base_hash),
                opt_to_sql(&row.theirs_hash),
                opt_to_sql(&row.mine_hash),
                row.theirs_rev,
                row.reason.as_str(),
                row.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn remove_conflict(&mut self, path: &str) -> ClientResult<()> {
        self.db
            .execute("DELETE FROM conflicts WHERE path = ?", [path])?;
        Ok(())
    }

    pub fn clear_conflicts(&mut self) -> ClientResult<()> {
        self.db.execute("DELETE FROM conflicts", [])?;
        Ok(())
    }

    /// 只读连接引用（给需要直接跑 SQL 的测试/工具用）。
    pub fn conn(&self) -> &Connection {
        &self.db
    }
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRow> {
    let kind: String = r.get("kind")?;
    let status: String = r.get("status")?;
    Ok(EntryRow {
        path: r.get("path")?,
        kind: EntryKind::parse(&kind).unwrap_or(EntryKind::File),
        base_rev: r.get("base_rev")?,
        base_hash: r.get("base_hash")?,
        size: r.get("size")?,
        mtime_ms: r.get("mtime_ms")?,
        mode: r.get("mode")?,
        status: EntryStatus::parse(&status).unwrap_or(EntryStatus::Normal),
        case_conflict: r.get::<_, i64>("case_conflict")? != 0,
    })
}

fn row_to_lock(r: &rusqlite::Row<'_>) -> rusqlite::Result<LockRow> {
    let kind: String = r.get("kind")?;
    Ok(LockRow {
        path: r.get("path")?,
        kind: EntryKind::parse(&kind).unwrap_or(EntryKind::File),
        token: r.get("token")?,
        owner: r.get("owner")?,
        comment: r.get("comment")?,
        expires_at: r.get("expires_at")?,
    })
}

fn row_to_conflict(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConflictRow> {
    let kind: String = r.get("kind")?;
    let reason: String = r.get("reason")?;
    Ok(ConflictRow {
        path: r.get("path")?,
        kind: EntryKind::parse(&kind).unwrap_or(EntryKind::File),
        base_hash: r.get("base_hash")?,
        theirs_hash: r.get("theirs_hash")?,
        mine_hash: r.get("mine_hash")?,
        theirs_rev: r.get("theirs_rev")?,
        reason: ConflictReason::parse(&reason).unwrap_or(ConflictReason::BothModified),
        created_at: r.get("created_at")?,
    })
}

// ---------- 进程级排他锁（§6.1 `wc.lock`） ----------

fn lock_refs() -> &'static std::sync::Mutex<HashMap<PathBuf, usize>> {
    static REFS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, usize>>> =
        std::sync::OnceLock::new();
    REFS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn read_lock_pid(p: &Path) -> Option<i32> {
    let raw = std::fs::read_to_string(p).ok()?;
    raw.trim().parse::<i32>().ok().filter(|n| *n > 0)
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    // kill(pid, 0)：只探活，不发信号。
    // **EPERM 也算活着** —— 目标进程存在、只是不归我发信号（非 root 探 pid 1 就是这种），
    // 只看 `== 0` 会把"别人的进程"误判成"已经死了"，于是并发写坏同一个副本。
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(pid: i32) -> bool {
    let _ = pid;
    // 非 Unix 平台退化为"文件存在即认为存活"：宁可报 WC_BUSY 也不并发写坏副本
    true
}

/// 进程级排他锁的守卫；`Drop` 时释放（同 pid 引用计数归零才真正摘锁）。
pub struct ProcessLock {
    path: PathBuf,
    released: bool,
}

impl ProcessLock {
    pub fn release(mut self) {
        self.do_release();
    }

    fn do_release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut refs = lock_refs().lock().unwrap();
        let n = refs.get(&self.path).copied().unwrap_or(1);
        if n > 1 {
            refs.insert(self.path.clone(), n - 1);
            return;
        }
        refs.remove(&self.path);
        drop(refs);
        if read_lock_pid(&self.path) == Some(std::process::id() as i32) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        self.do_release();
    }
}

/// 进程级排他锁（§6.1 `wc.lock`）：防止两个客户端实例同时操作同一工作副本。
///
/// 用 pid 存活检测而非单纯的文件存在——上次崩溃残留的锁文件不应永久阻塞。
pub fn acquire_process_lock(root: &Path) -> ClientResult<ProcessLock> {
    let dir = root.join(WC_DIR);
    std::fs::create_dir_all(&dir)?;
    let lock_path = dir.join(WC_LOCK);

    if let Some(existing) = read_lock_pid(&lock_path) {
        if existing != std::process::id() as i32 && pid_alive(existing) {
            return Err(ClientError::wc(
                "WC_BUSY",
                format!(
                    "工作副本正被另一个进程占用（pid {existing}）：{}",
                    root.display()
                ),
            ));
        }
    }

    std::fs::write(&lock_path, std::process::id().to_string())?;
    let mut refs = lock_refs().lock().unwrap();
    *refs.entry(lock_path.clone()).or_insert(0) += 1;

    Ok(ProcessLock {
        path: lock_path,
        released: false,
    })
}

/// 判断目录是否为工作副本。
pub fn is_working_copy(root: &Path) -> bool {
    root.join(WC_DIR).join(WC_DB).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(repo: &str) -> WcMeta {
        WcMeta {
            server: "http://127.0.0.1:9".into(),
            repo: repo.into(),
            revision: 3,
            sparse_paths: vec!["a".into()],
            user: "u".into(),
            cache_dir: "/tmp/cache".into(),
        }
    }

    fn entry(path: &str, status: EntryStatus) -> EntryRow {
        EntryRow {
            path: path.into(),
            kind: EntryKind::File,
            base_rev: 3,
            base_hash: Some("h".into()),
            size: 10,
            mtime_ms: 1_700_000_000_000,
            mode: 0o644,
            status,
            case_conflict: false,
        }
    }

    #[test]
    fn open_missing_raises_not_a_working_copy() {
        let dir = tempfile::tempdir().unwrap();
        let e = Wc::open(dir.path()).err().expect("应当失败");
        assert_eq!(e.code, "NOT_A_WORKING_COPY");
    }

    #[test]
    fn init_creates_schema_and_meta() {
        let dir = tempfile::tempdir().unwrap();
        let wc = Wc::init(dir.path(), &meta("r1")).unwrap();
        assert!(is_working_copy(dir.path()));
        assert_eq!(wc.revision(), 3);
        let m = wc.get_meta().unwrap();
        assert_eq!(m.repo, "r1");
        assert_eq!(m.sparse_paths, vec!["a".to_string()]);
        let v: i64 = wc
            .conn()
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v as usize, SCHEMA_VERSION);
    }

    #[test]
    fn meta_values_are_json_text() {
        // 这条是**跨版本兼容的命门**：TS 版用 JSON.stringify 逐键写入，
        // Rust 版若改成裸文本，老副本读 revision 会变成 0。
        let dir = tempfile::tempdir().unwrap();
        let wc = Wc::init(dir.path(), &meta("r")).unwrap();
        let raw: String = wc
            .conn()
            .query_row("SELECT value FROM meta WHERE key='revision'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "3");
        let raw: String = wc
            .conn()
            .query_row("SELECT value FROM meta WHERE key='sparse_paths'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, r#"["a"]"#);
    }

    #[test]
    fn reopening_does_not_re_run_migrations() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _wc = Wc::init(dir.path(), &meta("r")).unwrap();
        }
        let wc = Wc::open(dir.path()).unwrap();
        let rows: i64 = wc
            .conn()
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "迁移不能重复插入 schema_version");
    }

    #[test]
    fn entry_upsert_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = Wc::init(dir.path(), &meta("r")).unwrap();
        wc.upsert_entry(&entry("a/b.psd", EntryStatus::Normal)).unwrap();
        let got = wc.get_entry("a/b.psd").unwrap().unwrap();
        assert_eq!(got.size, 10);
        assert_eq!(got.status, EntryStatus::Normal);
        assert!(!got.case_conflict);

        let mut e2 = got.clone();
        e2.status = EntryStatus::Modified;
        e2.size = 99;
        wc.upsert_entry(&e2).unwrap();
        let got2 = wc.get_entry("a/b.psd").unwrap().unwrap();
        assert_eq!(got2.status, EntryStatus::Modified);
        assert_eq!(got2.size, 99);
        assert_eq!(wc.all_entries().unwrap().len(), 1, "upsert 不应产生重复行");
    }

    #[test]
    fn entry_delete() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = Wc::init(dir.path(), &meta("r")).unwrap();
        wc.upsert_entry(&entry("a", EntryStatus::Normal)).unwrap();
        wc.delete_entry("a").unwrap();
        assert!(!wc.has_entry("a").unwrap());
    }

    #[test]
    fn entries_under_uses_half_open_interval() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = Wc::init(dir.path(), &meta("r")).unwrap();
        for p in ["char", "char/a.psd", "char/sub/b.psd", "chars/x.psd", "char2"] {
            wc.upsert_entry(&entry(p, EntryStatus::Normal)).unwrap();
        }
        let got: Vec<String> = wc
            .entries_under("char")
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(got, vec!["char", "char/a.psd", "char/sub/b.psd"]);
    }

    #[test]
    fn entries_under_empty_prefix_is_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = Wc::init(dir.path(), &meta("r")).unwrap();
        wc.upsert_entry(&entry("b", EntryStatus::Normal)).unwrap();
        wc.upsert_entry(&entry("a", EntryStatus::Normal)).unwrap();
        assert_eq!(wc.entries_under("").unwrap().len(), 2);
    }

    #[test]
    fn pending_stage_unstage_clear() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = Wc::init(dir.path(), &meta("r")).unwrap();
        wc.stage("a", PendingOp::Add).unwrap();
        wc.stage("b", PendingOp::Delete).unwrap();
        assert_eq!(wc.pending_op("a").unwrap(), Some(PendingOp::Add));
        wc.stage("a", PendingOp::Modify).unwrap();
        assert_eq!(wc.pending_op("a").unwrap(), Some(PendingOp::Modify));
        assert_eq!(wc.all_pending().unwrap().len(), 2);

        wc.unstage("a").unwrap();
        assert_eq!(wc.pending_op("a").unwrap(), None);

        wc.clear_pending().unwrap();
        assert!(wc.all_pending().unwrap().is_empty());
    }

    #[test]
    fn locks_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = Wc::init(dir.path(), &meta("r")).unwrap();
        let row = LockRow {
            path: "a.psd".into(),
            kind: EntryKind::File,
            token: "t".into(),
            owner: Some("me".into()),
            comment: None,
            expires_at: None,
        };
        wc.put_lock(&row).unwrap();
        assert_eq!(wc.get_lock("a.psd").unwrap().unwrap(), row);
        assert!(wc.get_lock("nope").unwrap().is_none());
        wc.remove_lock("a.psd").unwrap();
        assert!(wc.get_lock("a.psd").unwrap().is_none());
    }

    #[test]
    fn conflicts_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = Wc::init(dir.path(), &meta("r")).unwrap();
        let row = ConflictRow {
            path: "a.psd".into(),
            kind: EntryKind::File,
            base_hash: Some("b".into()),
            theirs_hash: Some("t".into()),
            mine_hash: None,
            theirs_rev: 9,
            reason: ConflictReason::BothModified,
            created_at: 1,
        };
        wc.put_conflict(&row).unwrap();
        assert_eq!(wc.get_conflict("a.psd").unwrap().unwrap(), row);
        wc.clear_conflicts().unwrap();
        assert!(wc.all_conflicts().unwrap().is_empty());
    }

    #[test]
    fn tx_rolls_back_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = Wc::init(dir.path(), &meta("r")).unwrap();
        let r: ClientResult<()> = wc.tx(|w| {
            w.upsert_entry(&entry("a", EntryStatus::Normal))?;
            Err(ClientError::wc("IO", "boom"))
        });
        assert!(r.is_err());
        assert!(!wc.has_entry("a").unwrap());
    }

    #[test]
    fn process_lock_blocks_other_live_pid() {
        let dir = tempfile::tempdir().unwrap();
        let wc = Wc::init(dir.path(), &meta("r")).unwrap();
        drop(wc);
        let lock = dir.path().join(WC_DIR).join(WC_LOCK);
        // 用当前进程的父 pid 冒充"另一个活着的进程"不可靠；用 1（launchd/init）更稳
        std::fs::write(&lock, "1").unwrap();
        let e = acquire_process_lock(dir.path()).err().expect("应当失败");
        assert_eq!(e.code, "WC_BUSY");
        std::fs::remove_file(&lock).unwrap();
    }

    #[test]
    fn process_lock_stale_pid_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let wc = Wc::init(dir.path(), &meta("r")).unwrap();
        drop(wc);
        let lock = dir.path().join(WC_DIR).join(WC_LOCK);
        // 一个必然不存在的 pid
        std::fs::write(&lock, "4294967").unwrap();
        let guard = acquire_process_lock(dir.path()).unwrap();
        assert!(lock.exists());
        guard.release();
        assert!(!lock.exists(), "释放后锁文件应被摘掉");
    }

    #[test]
    fn process_lock_refcount_and_idempotent_release() {
        let dir = tempfile::tempdir().unwrap();
        let wc = Wc::init(dir.path(), &meta("r")).unwrap();
        drop(wc);
        let lock = dir.path().join(WC_DIR).join(WC_LOCK);
        let a = acquire_process_lock(dir.path()).unwrap();
        let b = acquire_process_lock(dir.path()).unwrap();
        drop(a);
        assert!(lock.exists(), "引用计数未归零时不能摘锁");
        b.release();
        assert!(!lock.exists());
    }
}
