-- b-artifact 初始 schema（对应《架构设计方案》§3.2，v0.2）

-- ========== 身份 ==========
CREATE TABLE users (
  id                 INTEGER PRIMARY KEY,
  username           TEXT NOT NULL UNIQUE,
  display_name       TEXT NOT NULL DEFAULT '',
  email              TEXT NOT NULL DEFAULT '',
  password_hash      TEXT NOT NULL,
  is_admin           INTEGER NOT NULL DEFAULT 0,
  disabled           INTEGER NOT NULL DEFAULT 0,
  must_change_pw     INTEGER NOT NULL DEFAULT 0,
  created_at         TEXT NOT NULL
);

CREATE TABLE sessions (
  token       TEXT PRIMARY KEY,             -- 存 sha256(token)
  user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at  TEXT NOT NULL,
  expires_at  TEXT NOT NULL,
  user_agent  TEXT NOT NULL DEFAULT '',
  last_seen   TEXT NOT NULL
);
CREATE INDEX idx_sessions_user ON sessions(user_id);

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
  name        TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT '',
  head_rev    INTEGER NOT NULL DEFAULT 0,
  owner_id    INTEGER NOT NULL REFERENCES users(id),
  lock_policy TEXT NOT NULL DEFAULT 'advisory',
  needs_lock  TEXT NOT NULL DEFAULT '',
  allow_anon  INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL
);

-- ========== 修订与路径历史 ==========
CREATE TABLE revisions (
  repo_id       INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  rev           INTEGER NOT NULL,
  author_id     INTEGER NOT NULL REFERENCES users(id),
  message       TEXT NOT NULL DEFAULT '',
  created_at    TEXT NOT NULL,
  file_count    INTEGER NOT NULL DEFAULT 0,
  byte_delta    INTEGER NOT NULL DEFAULT 0,
  manifest_hash TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (repo_id, rev)
);

CREATE TABLE changes (
  repo_id   INTEGER NOT NULL,
  rev       INTEGER NOT NULL,
  path      TEXT    NOT NULL,
  op        TEXT    NOT NULL,
  kind      TEXT    NOT NULL,
  blob_hash TEXT,
  size      INTEGER NOT NULL DEFAULT 0,
  mode      INTEGER NOT NULL DEFAULT 420,
  mtime     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (repo_id, rev, path)
);
CREATE INDEX idx_changes_path ON changes(repo_id, path, rev DESC);
CREATE INDEX idx_changes_rev  ON changes(repo_id, rev);

CREATE TABLE head_entries (
  repo_id     INTEGER NOT NULL,
  path        TEXT    NOT NULL,
  kind        TEXT    NOT NULL,
  blob_hash   TEXT,
  size        INTEGER NOT NULL DEFAULT 0,
  mode        INTEGER NOT NULL DEFAULT 420,
  mtime       INTEGER NOT NULL DEFAULT 0,
  changed_rev INTEGER NOT NULL,
  PRIMARY KEY (repo_id, path)
);
CREATE INDEX idx_head_prefix ON head_entries(repo_id, path);

-- ========== Blob 索引 ==========
CREATE TABLE blobs (
  hash        TEXT PRIMARY KEY,
  size        INTEGER NOT NULL,
  stored_size INTEGER NOT NULL,
  codec       TEXT NOT NULL DEFAULT 'raw',
  refcount    INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL
);
CREATE INDEX idx_blobs_unref ON blobs(hash) WHERE refcount = 0;

-- ========== 目录级权限 ==========
CREATE TABLE acl_rules (
  id           INTEGER PRIMARY KEY,
  repo_id      INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  path_prefix  TEXT NOT NULL DEFAULT '',
  subject_type TEXT NOT NULL,
  subject_id   INTEGER NOT NULL DEFAULT 0,
  level        TEXT NOT NULL,
  inherit      INTEGER NOT NULL DEFAULT 1,
  created_at   TEXT NOT NULL,
  UNIQUE (repo_id, path_prefix, subject_type, subject_id)
);
CREATE INDEX idx_acl_lookup ON acl_rules(repo_id, path_prefix);

-- ========== 锁 ==========
CREATE TABLE locks (
  id         INTEGER PRIMARY KEY,
  repo_id    INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  path       TEXT NOT NULL,
  kind       TEXT NOT NULL DEFAULT 'file',
  owner_id   INTEGER NOT NULL REFERENCES users(id),
  token      TEXT NOT NULL,
  comment    TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  expires_at TEXT,
  UNIQUE (repo_id, path)
);
CREATE INDEX idx_locks_owner ON locks(repo_id, owner_id);
CREATE INDEX idx_locks_path  ON locks(repo_id, path);

-- ========== GC 待删队列 ==========
CREATE TABLE gc_queue (
  blob_hash TEXT PRIMARY KEY,
  repo_id   INTEGER,
  reason    TEXT NOT NULL,
  queued_at TEXT NOT NULL,
  due_at    TEXT NOT NULL
);

-- ========== 提交准备（两阶段提交的临时凭据） ==========
CREATE TABLE pending_commits (
  token        TEXT PRIMARY KEY,
  repo_id      INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  base_rev     INTEGER NOT NULL,
  payload_json TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  expires_at   TEXT NOT NULL
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
CREATE INDEX idx_audit_ts   ON audit_log(ts DESC);
CREATE INDEX idx_audit_user ON audit_log(user_id);
