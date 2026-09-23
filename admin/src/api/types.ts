// 服务端响应类型（§7.2 / §7.3 / §8）。字段名与 Rust 端 `json!` 一一对应。

export type Level = 'none' | 'read' | 'write' | 'admin'
export type SubjectType = 'everyone' | 'group' | 'user'

export interface Me {
  id: number
  username: string
  display_name: string
  source: string
  is_admin: boolean
}

export interface LoginResp {
  token: string
  expires_at: string
  user: Me
}

export interface Providers {
  local: boolean
  ldap: boolean
}

export interface ListResp<T> {
  items: T[]
  total: number
}

export interface Repo {
  id: number
  name: string
  description: string
  owner: string
  head_rev: number
  created_at: string
  my_role: string
  my_permissions: { read: boolean; write: boolean; admin: boolean }
}

export interface RepoInfo {
  name: string
  description: string
  owner_id: number
  head_rev: number
  my_role: string
  my_permissions: { read: boolean; write: boolean; admin: boolean }
  stats: { file_count: number; total_size: number; rev_count: number }
}

export interface TreeEntry {
  path: string
  kind: 'file' | 'dir'
  blob_hash: string | null
  size: number
  mode: number
  mtime: number
  changed_rev: number
}

export interface TreeResp {
  repo: string
  rev: number
  prefix: string
  depth: number
  items: TreeEntry[]
  total: number
}

export interface Revision {
  rev: number
  author_id: number
  author: string
  message: string
  created_at: string
  file_count: number
  byte_delta: number
  manifest_hash: string
}

export interface Lock {
  id: number
  path: string
  kind: 'file' | 'dir'
  owner_id: number
  owner: string
  comment: string
  created_at: string
  expires_at: string | null
  broken_at: string | null
  broken_by: number | null
  broken_by_name: string | null
  break_reason: string | null
}

export interface AclRule {
  id: number
  path_prefix: string
  subject_type: SubjectType
  subject_id: number
  level: Level
  inherit: boolean
  // 管理端附加字段
  shadowed?: boolean
  subject_label?: string
}

export interface TraceStep {
  prefix: string
  display: string
  outcome: 'hit' | 'barrier' | 'miss' | 'skip'
  level: Level | null
  rule_id?: number
  rules?: AclRule[]
  reason: string
}

export interface AclPreview {
  path: string
  level: Level
  reason: string
  steps: TraceStep[]
  user: {
    id: number
    username: string
    is_admin: boolean
    groups: string[]
  }
}

export interface AclWhoItem {
  id: number
  username: string
  display_name: string
  disabled: boolean
  level: Level
  via: string
}

export interface DirNode {
  path: string
  name: string
  kind: 'dir'
  has_rules: boolean
  has_children: boolean
}

export interface UserRow {
  id: number
  username: string
  display_name: string
  source: string
  ldap_dn: string
  is_admin: boolean
  disabled: boolean
  created_at: string
  last_login_at: string | null
  groups: { id: number; name: string }[]
}

export interface GroupRow {
  id: number
  name: string
  comment: string
  created_at: string
  members: number
}

export interface GroupMember {
  id: number
  username: string
  display_name: string
  source: string
  disabled: boolean
}

export interface AuditRow {
  id: number
  ts: string
  user_id: number | null
  username: string | null
  repo_id: number | null
  repo: string | null
  action: string
  target: string
  detail: string
  ip: string
}

export interface Stats {
  repos: number
  users: number
  users_active: number
  sessions: number
  files: number
  revisions: number
  locks: number
  storage: {
    logical_bytes: number
    unique_bytes: number
    stored_bytes: number
    blob_count: number
    dedup_ratio: number
    compress_ratio: number
  }
  gc: {
    queued: number
    due_now: number
    queued_bytes: number
    orphan_blobs: number
  }
  commit_trend: { date: string; commits: number; files: number }[]
  recent_activity: {
    ts: string
    action: string
    target: string
    detail: string
    username: string | null
  }[]
  top_repos: {
    name: string
    head_rev: number
    files: number
    bytes: number
  }[]
}

export interface Maintenance {
  queued: number
  due_now: number
  queued_bytes: number
  orphan_blobs: number
  grace_secs: number
}

export interface SystemSettings {
  server: { listen: string; data_dir: string; base_url: string }
  storage: {
    compression: string
    max_file_size_mb: number
    chunk_threshold_mb: number
  }
  auth: {
    local_enabled: boolean
    first_user_admin: boolean
    session_ttl_days: number
    login_max_fails: number
    login_lockout_secs: number
    ldap: {
      enabled: boolean
      url: string
      bind_dn: string
      bind_password_set: boolean
      user_base: string
      user_filter: string
      allow_insecure: boolean
    }
  }
  db: { pool_size: number }
  version: string
}

export interface PurgeResult {
  prefix: string
  paths_removed: number
  revisions_affected: number
  rev_range: { from: number; to: number } | null
  blobs_reclaimed: number
  bytes_reclaimed: number
  locks_released: number
  gc_scheduled_at: string
}
