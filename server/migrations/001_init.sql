-- b-artifact 初始 schema（对应架构设计方案 §3.2，v0.4.3）
-- 所有时间字段为 RFC3339 UTC 字符串。

-- ========== 身份 ==========
CREATE TABLE users (
  id            INTEGER PRIMARY KEY,
  username      TEXT NOT NULL UNIQUE,
  display_name  TEXT NOT NULL DEFAULT '',
  email         TEXT NOT NULL DEFAULT '',
  password_hash TEXT,                         -- argon2id；LDAP 用户为 NULL（密码永不落库）
  source        TEXT NOT NULL DEFAULT 'local',-- local | ldap
  ldap_dn       TEXT NOT NULL DEFAULT '',
  external_id   TEXT NOT NULL DEFAULT '',     -- entryUUID / objectGUID
  is_admin      INTEGER NOT NULL DEFAULT 0,
  disabled      INTEGER NOT NULL DEFAULT 0,
  created_at    TEXT NOT NULL,
  last_login_at TEXT
);
CREATE UNIQUE INDEX idx_users_external
  ON users(external_id) WHERE source = 'ldap' AND external_id <> '';

CREATE TABLE sessions (
  token       TEXT PRIMARY KEY,
  user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at  TEXT NOT NULL,
  expires_at  TEXT NOT NULL,
  user_agent  TEXT NOT NULL DEFAULT '',
  last_seen   TEXT NOT NULL
);

CREATE TABLE groups (
  id         INTEGER PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  comment    TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL
);

CREATE TABLE group_members (
  group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
  user_id  INTEGER NOT NULL REFERENCES users(id)  ON DELETE CASCADE,
  PRIMARY KEY (group_id, user_id)
);

-- ========== 仓库 ==========
CREATE TABLE repos (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,          -- URL 片段，[A-Za-z0-9._-]
  description TEXT NOT NULL DEFAULT '',
  head_rev    INTEGER NOT NULL DEFAULT 0,    -- 乐观锁并发控制
  owner_id    INTEGER NOT NULL REFERENCES users(id),
  lock_policy TEXT NOT NULL DEFAULT 'advisory',  -- advisory | strict
  needs_lock  TEXT NOT NULL DEFAULT '',      -- glob 列表，如 "*.psd;*.fbx;*.max"
  allow_anon  INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL
);

-- ========== 修订与文件路径历史 ==========
CREATE TABLE revisions (
  repo_id    INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  rev        INTEGER NOT NULL,
  author_id  INTEGER NOT NULL REFERENCES users(id),
  message    TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  file_count INTEGER NOT NULL DEFAULT 0,
  byte_delta INTEGER NOT NULL DEFAULT 0,
  manifest_hash TEXT NOT NULL DEFAULT '',    -- 仅 HEAD 异步补算（§3.3）
  PRIMARY KEY (repo_id, rev)
);

-- 路径历史：只记录"变更"（§3.3 Path-History）
CREATE TABLE changes (
  repo_id   INTEGER NOT NULL,
  rev       INTEGER NOT NULL,
  path      TEXT    NOT NULL,                -- posix 风格，无前导 '/'，NFC
  op        TEXT    NOT NULL,                -- add | modify | delete | meta
  kind      TEXT    NOT NULL,                -- file | dir
  blob_hash TEXT,                            -- file 时为 sha256(原始内容)
  size      INTEGER NOT NULL DEFAULT 0,
  mode      INTEGER NOT NULL DEFAULT 0644,
  mtime     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (repo_id, rev, path)
);
CREATE INDEX idx_changes_path ON changes(repo_id, path, rev DESC);
CREATE INDEX idx_changes_rev  ON changes(repo_id, rev);

-- HEAD 快路径：随提交就地更新的"当前树"（§3.3）
CREATE TABLE head_entries (
  repo_id   INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  path      TEXT    NOT NULL,
  kind      TEXT    NOT NULL,                -- file | dir（仅显式目录操作产生的行）
  blob_hash TEXT,
  size      INTEGER NOT NULL DEFAULT 0,
  mode      INTEGER NOT NULL DEFAULT 0644,
  mtime     INTEGER NOT NULL DEFAULT 0,
  changed_rev INTEGER NOT NULL,
  PRIMARY KEY (repo_id, path)
);

-- 提交幂等（§5.3）
CREATE TABLE commits (
  commit_id  TEXT PRIMARY KEY,               -- 客户端 UUID v4
  repo_id    INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  rev        INTEGER NOT NULL,
  user_id    INTEGER NOT NULL REFERENCES users(id),
  base_rev   INTEGER NOT NULL,
  changes_hash TEXT NOT NULL,
  created_at TEXT NOT NULL
);

-- prepare 的待提交凭据（5 分钟过期）
CREATE TABLE pending_commits (
  commit_token TEXT PRIMARY KEY,             -- sha256 存储
  commit_id    TEXT NOT NULL,
  repo_id      INTEGER NOT NULL,
  user_id      INTEGER NOT NULL,
  base_rev     INTEGER NOT NULL,
  changes_hash TEXT NOT NULL,
  need_blobs   TEXT NOT NULL DEFAULT '[]',   -- JSON
  created_at   TEXT NOT NULL,
  expires_at   TEXT NOT NULL
);

-- ========== Blob 索引 ==========
CREATE TABLE blobs (
  hash        TEXT PRIMARY KEY,              -- sha256(原始内容)，hex
  size        INTEGER NOT NULL,
  stored_size INTEGER NOT NULL,
  codec       TEXT NOT NULL DEFAULT 'raw',   -- raw | zstd
  refcount    INTEGER NOT NULL DEFAULT 0,    -- 可重建缓存（§3.7）
  created_at  TEXT NOT NULL
);
CREATE INDEX idx_blobs_refcount ON blobs(refcount) WHERE refcount = 0;

-- ========== 目录级权限 ==========
CREATE TABLE acl_rules (
  id           INTEGER PRIMARY KEY,
  repo_id      INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  path_prefix  TEXT NOT NULL DEFAULT '',
  subject_type TEXT NOT NULL,                -- user | group | everyone
  subject_id   INTEGER NOT NULL DEFAULT 0,
  level        TEXT NOT NULL,                -- none | read | write | admin
  inherit      INTEGER NOT NULL DEFAULT 1,   -- 0 = 继承屏障（§4.2）
  created_at   TEXT NOT NULL,
  UNIQUE (repo_id, path_prefix, subject_type, subject_id)
);
CREATE INDEX idx_acl_lookup ON acl_rules(repo_id, path_prefix);

-- ========== 锁 ==========
CREATE TABLE locks (
  id         INTEGER PRIMARY KEY,
  repo_id    INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  path       TEXT NOT NULL,
  kind       TEXT NOT NULL DEFAULT 'file',   -- file | dir（dir 递归覆盖子树）
  owner_id   INTEGER NOT NULL REFERENCES users(id),
  token      TEXT NOT NULL,
  comment    TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  expires_at TEXT,                           -- NULL = 不过期
  UNIQUE (repo_id, path)
);
CREATE INDEX idx_locks_owner ON locks(repo_id, owner_id);
CREATE INDEX idx_locks_path  ON locks(repo_id, path);

-- GC 待删队列（全局，blob 不属于任何仓库，§3.7）
CREATE TABLE gc_queue (
  blob_hash TEXT PRIMARY KEY,
  repo_id   INTEGER,
  reason    TEXT NOT NULL,                   -- purge | refcount-zero
  queued_at TEXT NOT NULL,
  due_at    TEXT NOT NULL
);

-- GC 元信息（启动脏检测，§3.7）
CREATE TABLE gc_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- ========== 审计 ==========
CREATE TABLE audit_log (
  id      INTEGER PRIMARY KEY,
  ts      TEXT NOT NULL,
  user_id INTEGER,
  repo_id INTEGER,
  action  TEXT NOT NULL,
  target  TEXT NOT NULL DEFAULT '',
  detail  TEXT NOT NULL DEFAULT '',
  ip      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_audit_ts ON audit_log(ts DESC);
