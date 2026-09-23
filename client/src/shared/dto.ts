/**
 * 跨进程传输的数据形状（IPC 载荷 + 渲染层 DTO）。
 *
 * 这些结构在 `core/` 里已有定义（`StatusItem` / `LockInfo` / `RepoSummary` / …），
 * 这里**不是另起一套**，而是把"要跨 IPC 边界的那几个"抄成不依赖 Node 类型的版本：
 * 渲染层的 tsconfig 不带 `types: ["node"]`，直接 import core 会连带把 `node:fs` 拉进类型检查。
 * 两边的形状一致性由 `tests/dto-parity.test.ts` 双向赋值断言守住——
 * core 那边改了字段而这里没跟上，`tsc` 就会报错。
 */

// ---------- 应用 ----------

export interface AppInfo {
  version: string;
  electron: string;
  node: string;
  chrome: string;
  platform: string;
  arch: string;
}

export interface RecentEntry {
  dir: string;
  repo: string;
  server: string;
  lastOpenedAt: number;
}

export interface AppConfig {
  concurrency: number;
  cacheDir: string;
  servers: string[];
  recent: RecentEntry[];
  /**
   * 检出目录的默认**父目录**（设置里配置）。
   *
   * 检出时默认目录 = `<defaultCheckoutParent>/<仓库名>`。
   * v0.4.21 之前这里是 `lastParentDir`（"上次选过哪个目录"的隐式记忆，任何一次选目录都会写它
   * —— 连"下载旧版本到哪个目录"都会污染检出默认值）。改成显式配置：用户在一个地方决定，
   * 不被别的操作悄悄改掉。
   */
  defaultCheckoutParent: string;
}

export interface AuthState {
  server: string;
  username: string;
}

export interface LoginOutcome extends AuthState {
  username: string;
  is_admin: boolean;
  display_name: string;
}

// ---------- 仓库 ----------

export interface RepoSummary {
  id: number;
  name: string;
  description: string;
  owner: string;
  head_rev: number;
  created_at: string;
  my_role: string;
  my_permissions: { read: boolean; write: boolean; admin: boolean };
}

// ---------- 工作副本 ----------

/**
 * 逐条状态（§6.2）的九个取值 —— 与 `core/scan.ts` 的 `StatusCode` 严格一致
 * （由 `tests/dto-parity.test.ts` 双向赋值守住）。
 *
 * §6.2 的表里还列了 `locked-by-me` / `locked-by-other` 与 `out-of-date`：
 * 前者是**独立的锁轴**（由 `wc:locks` 单独呈现，不并进条目状态）；
 * 后者是**工作副本级**判定（`revision < head_rev`），提交时以错误码 `OUT_OF_DATE` 暴露。
 */
export type StatusCode =
  | 'unversioned'
  | 'ignored'
  | 'normal'
  | 'modified'
  | 'added'
  | 'deleted'
  | 'missing'
  | 'conflicted'
  | 'needs-update';

export interface StatusItem {
  path: string;
  kind: 'file' | 'dir';
  status: StatusCode;
  base_rev: number;
  base_hash: string | null;
  size: number;
  needs_update: boolean;
}

export interface WorkingCopyState {
  root: string;
  repo: string;
  rev: number;
  sparse_paths: string[];
  /**
   * 文件监听是否可用（§6.5 自动同步）。
   *
   * `false` = 已降级：超大目录树或平台限制让 `fs.watch` 起不来，界面**必须**把
   * 手动刷新入口露出来，否则用户会盯着一份过期的状态。
   */
  watching: boolean;
}

/** 三方对比里某一侧的可用形态。 */
export type SideKind = 'text' | 'binary' | 'too-large' | 'missing';

export interface SideContent {
  kind: SideKind;
  size: number;
  /** 只有 `kind === 'text'` 时非空。 */
  text: string | null;
}

/** 冲突成因（决定界面上能做什么）。 */
export type ConflictReason = 'both-modified' | 'deleted-remotely';

export interface ConflictInfo {
  path: string;
  kind: 'file' | 'dir';
  reason: ConflictReason;
  has_mine: boolean;
  has_theirs: boolean;
  /** 三方都是文本才允许合并式解决。 */
  mergeable: boolean;
  theirs_rev: number;
  sides: { base: SideKind; mine: SideKind; theirs: SideKind };
}

export interface ConflictSides {
  path: string;
  mergeable: boolean;
  base: SideContent;
  mine: SideContent;
  theirs: SideContent;
}

/** 冲突解决方式：取本地 / 取服务端 / 用合并结果。 */
export type ConflictResolution = 'mine' | 'theirs' | 'merged';

export interface CommitOutcome {
  rev: number;
  committed: string[];
  replayed: boolean;
}

export interface UpdateOutcome {
  rev: number;
  updated: string[];
  deleted: string[];
  conflicts: string[];
  skipped: string[];
}

// ---------- 历史修订（§6.5 历史视图） ----------

export interface LogEntry {
  rev: number;
  author: string;
  message: string;
  created_at: string;
  file_count: number;
  byte_delta: number;
  manifest_hash: string;
}

export interface TreeEntryDto {
  path: string;
  kind: 'file' | 'dir';
  blob_hash: string | null;
  size: number;
  mode: number;
  mtime: number;
  /** 该条目内容最后一次变化的修订。 */
  changed_rev: number;
}

export interface TreeAtResult {
  repo: string;
  rev: number;
  prefix: string;
  depth: number;
  items: TreeEntryDto[];
  total: number;
}

export interface DownloadOutcome {
  /** 实际落盘路径（重名时自动带修订号后缀，不静默覆盖）。 */
  saved: string;
  size: number;
  rev: number;
  path: string;
}

// ---------- 缓存（§6.5 设置视图） ----------

export interface CacheStats {
  dir: string;
  blobs: number;
  bytes: number;
  tmpFiles: number;
  tmpBytes: number;
}

export interface CacheClearResult {
  removed: number;
  bytes: number;
  clearedTmp: boolean;
}

// ---------- 锁 ----------

export interface LockInfo {
  id: number;
  path: string;
  kind: 'file' | 'dir';
  owner_id: number;
  owner: string;
  comment: string | null;
  created_at: string;
  expires_at: string | null;
  token?: string;
}

// ---------- 进度 ----------

/** 传输/哈希进度（§6.5 传输队列）。 */
export interface ProgressEvent {
  phase: string;
  done: number;
  total: number;
  current?: string;
}
