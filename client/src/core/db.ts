/**
 * 工作副本元数据库（§6.1 工作副本布局）。
 *
 * `.b-artifact/wc.db` 是工作副本的唯一真相来源：基线（entries）、待提交集（pending）、
 * 本人持有的锁（locks）。磁盘上的文件只是基线的「工作态」投影。
 *
 * 存储选型：`node:sqlite`（Node 内置，零原生依赖）。这里封一层薄适配，
 * 主要目的有两个——
 *   1. `node:sqlite` 不支持 boolean 绑定，统一转成 1/0；
 *   2. 日后 Electron 内置 Node 若不带该模块，可在此单点替换为其它绑定。
 */

import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';
// 只取类型：真正的加载在下面走 createRequire。
import type { DatabaseSync as DatabaseSyncHandle } from 'node:sqlite';
import { WcError } from './errors.js';

/**
 * 运行时加载 `node:sqlite`。
 *
 * 不用静态 import 的原因：Vite 5 的内置模块名单里没有 `node:sqlite`（Node 22.5+
 * 才加入），静态 import 会被当成第三方包去 resolve 而失败。走 createRequire 既
 * 绕开了打包器的静态分析，也让"这一层是唯一需要替换的适配点"落到实处——将来
 * Electron 内置 Node 若不带该模块，只改这一处即可换成其它绑定。
 */
const nodeRequire = createRequire(import.meta.url);
const DatabaseSync: new (path: string) => DatabaseSyncHandle = (
  nodeRequire('node:sqlite') as { DatabaseSync: new (path: string) => DatabaseSyncHandle }
).DatabaseSync;

export const WC_DIR = '.b-artifact';
export const WC_DB = 'wc.db';
export const WC_LOCK = 'wc.lock';

/** 迁移：下标 + 1 即 schema 版本号。新增迁移只能追加，不允许改写历史。 */
const MIGRATIONS: readonly string[] = [
  // v1：基线表 + 待提交集 + 锁 + 元信息
  `
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
  `,

  // v2：冲突登记（§6.2 conflicted / §6.5 冲突视图）
  //
  // 为什么必须单独记一张表：update 撞冲突时会把工作文件换成服务端版本，
  // entry.base_hash 也随之被覆盖——**共同祖先（base）的哈希就此丢失**，
  // 而三方对比（本地 / 服务端 / 基线）恰恰需要它。所以要在这里留一份。
  `
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
  `,
];

export const SCHEMA_VERSION = MIGRATIONS.length;

export type EntryKind = 'file' | 'dir';
export type PendingOp = 'add' | 'modify' | 'delete';

/** 文件状态（§6.2 状态机）。计算态（`unversioned`/`ignored`）不落库。 */
export type EntryStatus =
  | 'normal'
  | 'modified'
  | 'added'
  | 'deleted'
  | 'missing'
  | 'conflicted'
  | 'needs-update';

export interface EntryRow {
  path: string;
  kind: EntryKind;
  base_rev: number;
  base_hash: string | null;
  size: number;
  mtime_ms: number;
  mode: number;
  status: EntryStatus;
  case_conflict: boolean;
}

export interface PendingRow {
  path: string;
  op: PendingOp;
  staged_at: number;
}

export interface LockRow {
  path: string;
  kind: 'file' | 'dir';
  token: string;
  owner: string | null;
  comment: string | null;
  expires_at: string | null;
}

/** 冲突成因（决定界面上能做什么）。 */
export type ConflictReason =
  /** 双方都改了：本地版本存 `<path>.mine`，工作文件是服务端版本，可三方对比。 */
  | 'both-modified'
  /** 服务端删了但本地改过：保留本地文件，没有服务端内容可对比，只能"保留 / 接受删除"。 */
  | 'deleted-remotely';

export interface ConflictRow {
  path: string;
  kind: EntryKind;
  /** 共同祖先内容哈希（冲突发生前的基线）——三方对比的 base。 */
  base_hash: string | null;
  /** 服务端版本哈希（`deleted-remotely` 时为 null）。 */
  theirs_hash: string | null;
  /** 本地版本哈希（写 `<path>.mine` 时算得；懒算时为 null）。 */
  mine_hash: string | null;
  theirs_rev: number;
  reason: ConflictReason;
  created_at: number;
}

export interface WcMeta {
  /** 服务端地址（含 /api/v1）。 */
  server: string;
  /** 仓库名。 */
  repo: string;
  /** 检出时的 base 修订号。 */
  revision: number;
  /** 部分检出的前缀列表；空数组表示全量。 */
  sparse_paths: string[];
  /** 当前登录用户名。 */
  user: string;
  /** 全局缓存目录（pristine 硬链接源）。 */
  cache_dir: string;
}

/** SQLite 只接受 null / number / bigint / string / Uint8Array。 */
function bind(v: unknown): null | number | bigint | string | Uint8Array {
  if (v === undefined || v === null) return null;
  if (typeof v === 'boolean') return v ? 1 : 0;
  if (typeof v === 'number' || typeof v === 'bigint' || typeof v === 'string') return v;
  if (v instanceof Uint8Array) return v;
  throw new WcError('IO', `不支持的 SQL 绑定类型：${typeof v}`);
}

type Row = Record<string, unknown>;

function asString(v: unknown): string {
  return typeof v === 'string' ? v : String(v ?? '');
}
function asNumber(v: unknown): number {
  return typeof v === 'number' ? v : Number(v ?? 0);
}

/** 工作副本句柄。所有写操作必须经 `tx()` 以保证扫描/落库原子（§15.2 P0）。 */
export class Wc {
  readonly root: string;
  readonly dbPath: string;
  private readonly db: DatabaseSyncHandle;
  private readonly stmts = new Map<string, ReturnType<DatabaseSyncHandle['prepare']>>();

  private constructor(root: string, dbPath: string, db: DatabaseSyncHandle) {
    this.root = root;
    this.dbPath = dbPath;
    this.db = db;
  }

  /** 打开已有工作副本；不存在或不是工作副本则抛 `NOT_A_WORKING_COPY`。 */
  static open(root: string): Wc {
    const dbPath = join(root, WC_DIR, WC_DB);
    if (!existsSync(dbPath)) {
      throw new WcError('NOT_A_WORKING_COPY', `不是 b-artifact 工作副本：${root}`);
    }
    const db = new DatabaseSync(dbPath);
    // 刻意不开 WAL：工作副本目录会被用户直接拷贝/备份，WAL 未 checkpoint 时
    // 只拷 wc.db 会丢数据；客户端本是单进程短事务，默认 journal 更安全。
    db.exec('PRAGMA foreign_keys = ON;');
    const wc = new Wc(root, dbPath, db);
    wc.migrate();
    return wc;
  }

  /** 初始化新的工作副本（目录不存在则创建）。 */
  static init(root: string, meta: WcMeta): Wc {
    mkdirSync(join(root, WC_DIR), { recursive: true });
    const dbPath = join(root, WC_DIR, WC_DB);
    const db = new DatabaseSync(dbPath);
    // 刻意不开 WAL：工作副本目录会被用户直接拷贝/备份，WAL 未 checkpoint 时
    // 只拷 wc.db 会丢数据；客户端本是单进程短事务，默认 journal 更安全。
    db.exec('PRAGMA foreign_keys = ON;');
    const wc = new Wc(root, dbPath, db);
    wc.migrate();
    wc.tx(() => {
      wc.setMeta(meta);
    });
    return wc;
  }

  close(): void {
    this.db.close();
  }

  // ---------- 迁移 ----------

  private migrate(): void {
    const hasVersion = this.db
      .prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='schema_version'",
      )
      .get();
    const current = hasVersion
      ? asNumber(this.db.prepare('SELECT version FROM schema_version').get()?.['version'])
      : 0;
    for (let i = current; i < MIGRATIONS.length; i++) {
      const sql = MIGRATIONS[i]!;
      this.db.exec('BEGIN');
      try {
        this.db.exec(sql);
        this.db.exec(`UPDATE schema_version SET version = ${i + 1}`);
        this.db.exec('COMMIT');
      } catch (e) {
        this.db.exec('ROLLBACK');
        throw new WcError('WC_CORRUPT', `wc.db 迁移 ${i + 1} 失败`, e);
      }
    }
  }

  // ---------- 语句缓存 ----------

  private stmt(sql: string): ReturnType<DatabaseSyncHandle['prepare']> {
    let s = this.stmts.get(sql);
    if (!s) {
      s = this.db.prepare(sql);
      this.stmts.set(sql, s);
    }
    return s;
  }

  run(sql: string, ...params: unknown[]): void {
    this.stmt(sql).run(...params.map(bind));
  }

  get<T = Row>(sql: string, ...params: unknown[]): T | undefined {
    return this.stmt(sql).get(...params.map(bind)) as T | undefined;
  }

  all<T = Row>(sql: string, ...params: unknown[]): T[] {
    return this.stmt(sql).all(...params.map(bind)) as T[];
  }

  /** 事务包装：失败自动回滚并原样抛出。 */
  tx<T>(fn: () => T): T {
    this.db.exec('BEGIN');
    try {
      const r = fn();
      this.db.exec('COMMIT');
      return r;
    } catch (e) {
      this.db.exec('ROLLBACK');
      throw e;
    }
  }

  // ---------- meta ----------

  getMeta(): WcMeta {
    const rows = this.all<{ key: string; value: string }>('SELECT key, value FROM meta');
    if (rows.length === 0) throw new WcError('WC_CORRUPT', 'wc.db 缺少 meta');
    const m: Record<string, unknown> = {};
    for (const r of rows) {
      const raw = r.value;
      try {
        m[r.key] = JSON.parse(raw);
      } catch {
        m[r.key] = raw;
      }
    }
    return m as unknown as WcMeta;
  }

  setMeta(meta: WcMeta): void {
    const entries = Object.entries(meta) as [string, unknown][];
    for (const [k, v] of entries) {
      this.run(
        'INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value',
        k,
        JSON.stringify(v),
      );
    }
  }

  setMetaValue(key: keyof WcMeta, value: unknown): void {
    this.run(
      'INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value',
      key,
      JSON.stringify(value),
    );
  }

  get revision(): number {
    return asNumber(this.get('SELECT value FROM meta WHERE key = ?', 'revision')?.['value']);
  }

  set revision(n: number) {
    this.setMetaValue('revision', n);
  }

  get sparsePaths(): string[] {
    const raw = this.get('SELECT value FROM meta WHERE key = ?', 'sparse_paths')?.['value'];
    try {
      const v = JSON.parse(asString(raw));
      return Array.isArray(v) ? (v as string[]) : [];
    } catch {
      return [];
    }
  }

  // ---------- entries ----------

  getEntry(path: string): EntryRow | undefined {
    const r = this.get('SELECT * FROM entries WHERE path = ?', path);
    return r ? toEntry(r) : undefined;
  }

  hasEntry(path: string): boolean {
    return this.getEntry(path) !== undefined;
  }

  upsertEntry(row: EntryRow): void {
    this.run(
      `INSERT INTO entries (path, kind, base_rev, base_hash, size, mtime_ms, mode, status, case_conflict)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(path) DO UPDATE SET
         kind = excluded.kind, base_rev = excluded.base_rev, base_hash = excluded.base_hash,
         size = excluded.size, mtime_ms = excluded.mtime_ms, mode = excluded.mode,
         status = excluded.status, case_conflict = excluded.case_conflict`,
      row.path,
      row.kind,
      row.base_rev,
      row.base_hash,
      row.size,
      row.mtime_ms,
      row.mode,
      row.status,
      row.case_conflict,
    );
  }

  deleteEntry(path: string): void {
    this.run('DELETE FROM entries WHERE path = ?', path);
  }

  allEntries(): EntryRow[] {
    return this.all('SELECT * FROM entries ORDER BY path').map(toEntry);
  }

  /**
   * 前缀子树查询（含 prefix 自身）。
   *
   * 用半开区间而非 `LIKE 'pfx%'`：SQLite 默认 `case_sensitive_like=OFF`，前缀 LIKE
   * 不下推索引；而 `'/'` 是 0x2F、`'0'` 是 0x30，区间 `[pfx+'/', pfx+'0')` 正好
   * 覆盖且仅覆盖 `pfx/` 下的所有路径（比 `'/'` 小的字节都落在区间左侧）。
   */
  entriesUnder(prefix: string): EntryRow[] {
    if (prefix === '') return this.allEntries();
    return this.all(
      'SELECT * FROM entries WHERE path = ? OR (path >= ? AND path < ?) ORDER BY path',
      prefix,
      prefix + '/',
      prefix + '0',
    ).map(toEntry);
  }

  // ---------- pending ----------

  allPending(): PendingRow[] {
    return this.all('SELECT * FROM pending ORDER BY path').map((r) => ({
      path: asString(r['path']),
      op: asString(r['op']) as PendingOp,
      staged_at: asNumber(r['staged_at']),
    }));
  }

  stage(path: string, op: PendingOp): void {
    this.run(
      'INSERT INTO pending (path, op, staged_at) VALUES (?, ?, ?) ON CONFLICT(path) DO UPDATE SET op = excluded.op, staged_at = excluded.staged_at',
      path,
      op,
      Date.now(),
    );
  }

  unstage(path: string): void {
    this.run('DELETE FROM pending WHERE path = ?', path);
  }

  clearPending(): void {
    this.run('DELETE FROM pending');
  }

  pendingOp(path: string): PendingOp | undefined {
    const r = this.get('SELECT op FROM pending WHERE path = ?', path);
    return r ? (asString(r['op']) as PendingOp) : undefined;
  }

  // ---------- locks ----------

  allLocks(): LockRow[] {
    return this.all('SELECT * FROM locks ORDER BY path').map((r) => ({
      path: asString(r['path']),
      kind: asString(r['kind']) as 'file' | 'dir',
      token: asString(r['token']),
      owner: (r['owner'] as string | null) ?? null,
      comment: (r['comment'] as string | null) ?? null,
      expires_at: (r['expires_at'] as string | null) ?? null,
    }));
  }

  getLock(path: string): LockRow | undefined {
    return this.allLocks().find((l) => l.path === path);
  }

  putLock(row: LockRow): void {
    this.run(
      `INSERT INTO locks (path, kind, token, owner, comment, expires_at)
       VALUES (?, ?, ?, ?, ?, ?)
       ON CONFLICT(path) DO UPDATE SET kind = excluded.kind, token = excluded.token,
         owner = excluded.owner, comment = excluded.comment, expires_at = excluded.expires_at`,
      row.path,
      row.kind,
      row.token,
      row.owner,
      row.comment,
      row.expires_at,
    );
  }

  removeLock(path: string): void {
    this.run('DELETE FROM locks WHERE path = ?', path);
  }

  // ---------- conflicts ----------

  allConflicts(): ConflictRow[] {
    return this.all('SELECT * FROM conflicts ORDER BY path').map((r) => ({
      path: asString(r['path']),
      kind: asString(r['kind']) as EntryKind,
      base_hash: (r['base_hash'] as string | null) ?? null,
      theirs_hash: (r['theirs_hash'] as string | null) ?? null,
      mine_hash: (r['mine_hash'] as string | null) ?? null,
      theirs_rev: asNumber(r['theirs_rev']),
      reason: asString(r['reason']) as ConflictReason,
      created_at: asNumber(r['created_at']),
    }));
  }

  getConflict(path: string): ConflictRow | undefined {
    return this.allConflicts().find((c) => c.path === path);
  }

  putConflict(row: ConflictRow): void {
    this.run(
      `INSERT INTO conflicts (path, kind, base_hash, theirs_hash, mine_hash, theirs_rev, reason, created_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(path) DO UPDATE SET kind = excluded.kind, base_hash = excluded.base_hash,
         theirs_hash = excluded.theirs_hash, mine_hash = excluded.mine_hash,
         theirs_rev = excluded.theirs_rev, reason = excluded.reason, created_at = excluded.created_at`,
      row.path,
      row.kind,
      row.base_hash,
      row.theirs_hash,
      row.mine_hash,
      row.theirs_rev,
      row.reason,
      row.created_at,
    );
  }

  removeConflict(path: string): void {
    this.run('DELETE FROM conflicts WHERE path = ?', path);
  }

  clearConflicts(): void {
    this.run('DELETE FROM conflicts');
  }
}

function toEntry(r: Row): EntryRow {
  return {
    path: asString(r['path']),
    kind: asString(r['kind']) as EntryKind,
    base_rev: asNumber(r['base_rev']),
    base_hash: (r['base_hash'] as string | null) ?? null,
    size: asNumber(r['size']),
    mtime_ms: asNumber(r['mtime_ms']),
    mode: asNumber(r['mode']),
    status: asString(r['status']) as EntryStatus,
    case_conflict: asNumber(r['case_conflict']) !== 0,
  };
}

/**
 * 同进程对同一工作副本的持锁次数。
 * 同 pid 重入不是真冲突（就是自己），必须允许，否则一次会话里连续
 * checkout → status 会被自己的锁挡住；用引用计数保证最内层释放不提前摘锁。
 */
const lockRefs = new Map<string, number>();

/**
 * 进程级排他锁（§6.1 `wc.lock`）：防止两个客户端实例同时操作同一工作副本。
 * 用 pid 存活检测而非单纯的文件存在——上次崩溃残留的锁文件不应永久阻塞。
 */
export function acquireProcessLock(root: string): () => void {
  const lockPath = join(root, WC_DIR, WC_LOCK);
  mkdirSync(join(root, WC_DIR), { recursive: true });
  const existing = readLockPid(lockPath);
  if (existing !== null && existing !== process.pid && pidAlive(existing)) {
    throw new WcError('WC_BUSY', `工作副本正被另一个进程占用（pid ${existing}）：${root}`);
  }
  const refs = lockRefs.get(lockPath) ?? 0;
  writeFileSync(lockPath, String(process.pid), 'utf8');
  lockRefs.set(lockPath, refs + 1);

  let released = false;
  return () => {
    if (released) return; // 幂等：重复调用 close() 不应误删他人锁
    released = true;
    const n = (lockRefs.get(lockPath) ?? 1) - 1;
    if (n > 0) {
      lockRefs.set(lockPath, n);
      return;
    }
    lockRefs.delete(lockPath);
    try {
      if (readLockPid(lockPath) === process.pid) rmSync(lockPath, { force: true });
    } catch {
      /* 释放失败不影响主流程 */
    }
  };
}

function readLockPid(p: string): number | null {
  try {
    const raw = readFileSync(p, 'utf8').trim();
    const n = Number(raw);
    return Number.isInteger(n) && n > 0 ? n : null;
  } catch {
    return null;
  }
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** 判断目录是否为工作副本。 */
export function isWorkingCopy(root: string): boolean {
  return existsSync(join(root, WC_DIR, WC_DB));
}
