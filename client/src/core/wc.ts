/**
 * 工作副本引擎（§6.3 关键流程）：检出 / 状态 / 提交 / 更新 / 锁。
 *
 * 与 `db.ts` 的分工：这里只负责"把服务端状态与磁盘状态对齐"的流程编排，
 * 所有持久化都走 `Wc`，所有网络都走 `ApiClient`，便于单测用桩替换。
 */

import { randomUUID } from 'node:crypto';
import { createReadStream, createWriteStream, existsSync, readFileSync } from 'node:fs';
import { mkdir, open, rename, rm, stat, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';

import { ApiClient } from './api.js';
import type { ChangeSpec, CommitResult, LockInfo, PrepareResult } from './api.js';
import { acquireProcessLock, Wc, type ConflictReason, type ConflictRow, type EntryRow } from './db.js';
import { toApiError, WcError } from './errors.js';
import { DEFAULT_CHUNK_SIZE, hashFile, needsChunkedUpload } from './hash.js';
import { IGNORE_FILE } from './ignore.js';
import { withRetry } from './ioRetry.js';
import { normalizeNfc, parentOf, validatePath } from './paths.js';
import {
  cachePathFor,
  defaultCacheDir,
  ensurePristine,
  importWorkingFile,
  installWorkingFile,
  pristinePathFor,
  storeToCache,
} from './pristine.js';
import { computeStatus, isLocalChange, MINE_SUFFIX, scanDisk, type StatusItem } from './scan.js';
import { readTextCapped, type TextReadResult } from './text.js';

/** 部分检出时一次拉满的目录深度（服务端 clamp 到 16）。 */
const TREE_DEPTH = 16;
/** 传输并发上限（§6.3：信号量限 4~8 并发）。 */
const DEFAULT_CONCURRENCY = 4;

export interface ProgressEvent {
  phase: 'tree' | 'download' | 'upload' | 'hash' | 'apply' | 'lock';
  done: number;
  total: number;
  current?: string;
}

export interface WorkingCopyOptions {
  root: string;
  client: ApiClient;
  /** 省略时取 wc.db 里记录的仓库名（CLI 场景）。 */
  repo?: string;
  /** 检出者用户名，记进 meta 便于 `status` 展示。 */
  user?: string;
  cacheDir?: string;
  concurrency?: number;
}

export interface CheckoutOptions extends WorkingCopyOptions {
  /** 检出必须明确指定仓库（没有 meta 可回退）。 */
  repo: string;
  /** 部分检出的前缀列表；空数组 = 全量。 */
  sparse?: string[];
  onProgress?: (e: ProgressEvent) => void;
}

export interface CommitOptions {
  message?: string;
  /** 只提交这些路径（默认提交全部本地改动）。 */
  paths?: string[];
  onProgress?: (e: ProgressEvent) => void;
}

export interface CommitOutcome {
  rev: number;
  /** 实际进入这次提交的路径。 */
  committed: string[];
  replayed: boolean;
}

/** 三方对比里某一侧的可用形态。 */
export type SideKind = TextReadResult['kind'];

export interface SideContent {
  kind: SideKind;
  size: number;
  /** 只有 `kind === 'text'` 时非空。 */
  text: string | null;
}

export interface ConflictInfo {
  path: string;
  kind: 'file' | 'dir';
  reason: ConflictReason;
  /** 本地版本（`<path>.mine`）是否还在。 */
  has_mine: boolean;
  /** 服务端版本是否可取得（服务端已删除时为 false）。 */
  has_theirs: boolean;
  /** 三方都是文本才允许做合并式解决。 */
  mergeable: boolean;
  /** 服务端版本对应修订。 */
  theirs_rev: number;
  /** 各侧判定（界面据此给不同提示，不传内容）。 */
  sides: { base: SideKind; mine: SideKind; theirs: SideKind };
}

export interface ConflictSides {
  path: string;
  mergeable: boolean;
  base: SideContent;
  mine: SideContent;
  theirs: SideContent;
}

/** 冲突解决方式。 */
export type ConflictResolution = 'mine' | 'theirs' | 'merged';

export interface UpdateOutcome {
  rev: number;
  updated: string[];
  deleted: string[];
  conflicts: string[];
  skipped: string[];
}

/** 有界并发的任务映射（失败即整体失败）。 */
async function mapLimit<T>(
  items: readonly T[],
  limit: number,
  fn: (item: T, idx: number) => Promise<void>,
): Promise<void> {
  let cursor = 0;
  const workers = Array.from({ length: Math.min(limit, items.length) }, async () => {
    for (;;) {
      const i = cursor++;
      if (i >= items.length) return;
      await fn(items[i]!, i);
    }
  });
  await Promise.all(workers);
}

export class WorkingCopy {
  readonly root: string;
  readonly repo: string;
  readonly api: ApiClient;
  readonly cacheDir: string;
  readonly concurrency: number;
  readonly wc: Wc;
  private releaseLock: (() => void) | null = null;

  private constructor(wc: Wc, opts: WorkingCopyOptions & { repo: string }) {
    this.wc = wc;
    this.root = wc.root;
    this.repo = opts.repo;
    this.api = opts.client;
    this.cacheDir = opts.cacheDir ?? defaultCacheDir();
    this.concurrency = opts.concurrency ?? DEFAULT_CONCURRENCY;
  }

  /** 检出（支持部分检出）。 */
  static async checkout(opts: CheckoutOptions): Promise<WorkingCopy> {
    const { root, client } = opts;
    if (existsSync(join(root, '.b-artifact', 'wc.db'))) {
      throw new WcError('NOT_A_WORKING_COPY', `目录已是工作副本，不能重复检出：${root}`);
    }
    const info = await client.repoInfo(opts.repo);
    const sparse = (opts.sparse ?? []).map(normalizeNfc);
    const cacheDir = opts.cacheDir ?? defaultCacheDir();

    await mkdir(root, { recursive: true });
    const wc = Wc.init(root, {
      server: client.baseUrl,
      repo: opts.repo ?? '',
      revision: info.head_rev,
      sparse_paths: sparse,
      user: opts.user ?? '',
      cache_dir: cacheDir,
    });
    const self = new WorkingCopy(wc, opts);
    self.releaseLock = acquireProcessLock(root);

    // ① 拉目录树（每个部分检出前缀一次；空数组表示全量根）
    const prefixes = sparse.length > 0 ? sparse : [''];
    const all: { path: string; kind: 'file' | 'dir'; blob_hash: string | null; size: number; mode: number }[] = [];
    for (const p of prefixes) {
      const t = await client.tree(opts.repo, { prefix: p, depth: TREE_DEPTH });
      for (const it of t.items) all.push(it);
    }
    opts.onProgress?.({ phase: 'tree', done: all.length, total: all.length });

    // ② 目录先建，避免文件安装时父目录不存在
    //
    // 除树里显式返回的目录外，还要补上**部分检出前缀本身与其祖先**：服务端
    // `tree?prefix=X` 返回的是 X 的**子项**（不含 X），不补的话前缀目录
    // 会出现在磁盘上却没有基线条目，状态机把它报成"未纳管"。
    const dirSet = new Set<string>(all.filter((e) => e.kind === 'dir').map((e) => e.path));
    for (const p of prefixes) {
      for (let a = p; a; a = parentOf(a) as string) dirSet.add(a);
    }
    for (const f of all.filter((e) => e.kind === 'file')) {
      for (let a = parentOf(f.path) as string; a; a = parentOf(a) as string) dirSet.add(a);
    }
    const dirs = [...dirSet].sort((a, b) => a.split('/').length - b.split('/').length);
    for (const d of dirs) await mkdir(join(root, ...d.split('/')), { recursive: true });

    // ③ 文件：下载缺失 blob → 入缓存 → pristine 硬链接 → 安装到工作副本
    const files = all.filter((e) => e.kind === 'file' && e.blob_hash);
    let done = 0;
    await mapLimit(files, self.concurrency, async (f) => {
      // 必须是 installFile 而非 installBlob：后者只保证缓存与 pristine 就位，
      // 不把内容装成工作文件，检出会得到一个空目录。
      await self.installFile(f.path, f.blob_hash!);
      opts.onProgress?.({ phase: 'download', done: ++done, total: files.length, current: f.path });
    });

    // ④ 落基线：stat 必须在事务外做完（`tx` 是同步回调，里面不能有 await）
    const fileStats = await Promise.all(
      files.map(async (f) => ({ f, st: await stat(self.absOf(f.path)) })),
    );
    wc.tx(() => {
      for (const d of dirs) {
        wc.upsertEntry({
          path: d,
          kind: 'dir',
          base_rev: info.head_rev,
          base_hash: null,
          size: 0,
          mtime_ms: 0,
          mode: 0o755,
          status: 'normal',
          case_conflict: false,
        });
      }
      for (const { f, st } of fileStats) {
        wc.upsertEntry({
          path: f.path,
          kind: 'file',
          base_rev: info.head_rev,
          base_hash: f.blob_hash,
          size: st.size,
          mtime_ms: Math.floor(st.mtimeMs),
          mode: st.mode & 0o111,
          status: 'normal',
          case_conflict: false,
        });
      }
    });
    return self;
  }

  /**
   * 打开既有工作副本。
   *
   * `repo` 省略时以 wc.db 里记录的为准；若 meta 里的服务端地址与传入 client 不同，
   * 按 meta 重建 client（工作副本可能是在另一台服务器上检出的）。
   */
  static open(opts: WorkingCopyOptions): WorkingCopy {
    const wc = Wc.open(opts.root);
    const meta = wc.getMeta();
    const repo = opts.repo || meta.repo;
    const client =
      meta.server && meta.server !== opts.client.baseUrl
        ? new ApiClient({
            baseUrl: meta.server,
            token: opts.client.tokenValue ?? undefined,
            fetchImpl: opts.client.fetchImpl,
          })
        : opts.client;
    const self = new WorkingCopy(wc, { ...opts, repo, client });
    self.releaseLock = acquireProcessLock(opts.root);
    return self;
  }

  close(): void {
    this.releaseLock?.();
    this.releaseLock = null;
    this.wc.close();
  }

  get revision(): number {
    return this.wc.revision;
  }

  get sparsePaths(): string[] {
    return this.wc.sparsePaths;
  }

  absOf(rel: string): string {
    return join(this.root, ...rel.split('/'));
  }

  // ---------- 内部：blob 与文件安装 ----------

  /** 确保 blob 在缓存与 pristine 中，然后把内容安装到工作文件。 */
  private async installBlob(hash: string): Promise<void> {
    if (!existsSync(cachePathFor(hash, this.cacheDir))) {
      const tmp = join(this.cacheDir, 'tmp', `${hash}.dl-${process.pid}`);
      await mkdir(join(tmp, '..'), { recursive: true });
      const resp = await this.api.openBlobStream(this.repo, hash);
      if (!resp.body) throw new WcError('IO', `下载 blob 失败：${hash}`);
      await pipeline(Readable.fromWeb(resp.body as never), createWriteStream(tmp));
      await storeToCache(tmp, hash, this.cacheDir);
    }
    await ensurePristine(this.root, hash, this.cacheDir);
  }

  /** 安装服务端某个修订的文件内容到工作副本。 */
  private async installFile(rel: string, hash: string): Promise<void> {
    await this.installBlob(hash);
    await mkdir(join(this.absOf(rel), '..'), { recursive: true });
    await installWorkingFile(this.root, hash, this.absOf(rel));
  }

  /**
   * 补登记隐式祖先目录。
   *
   * 服务端的 changes 流只携带显式变更行——`ensure_dirs` 补的隐式祖先目录只写
   * `head_entries`，不进 changes。所以更新装文件时目录是新建的、却没有基线条目，
   * 会一直显示为 unversioned，用户一 add 就多提交一条重复目录行。这里补上。
   */
  private ensureAncestorEntries(rel: string, rev: number): void {
    for (let p = parentOf(rel); p; p = parentOf(p)) {
      if (this.wc.hasEntry(p)) continue;
      this.wc.upsertEntry({
        path: p,
        kind: 'dir',
        base_rev: rev,
        base_hash: null,
        size: 0,
        mtime_ms: 0,
        mode: 0o755,
        status: 'normal',
        case_conflict: false,
      });
    }
  }

  private async writeEntryFromDisk(rel: string, hash: string, rev: number): Promise<void> {
    const st = await stat(this.absOf(rel));
    this.wc.upsertEntry({
      path: rel,
      kind: 'file',
      base_rev: rev,
      base_hash: hash,
      size: st.size,
      mtime_ms: Math.floor(st.mtimeMs),
      mode: st.mode & 0o111,
      status: 'normal',
      case_conflict: false,
    });
  }

  /**
   * 记录一个"内容与基线**不同**"的本地改动条目。
   *
   * `mtime_ms` 必须置 0：状态判定的快速路径是"mtime + size 都没变 ⇒ 内容没变"（§6.1），
   * 而我们刚把本地内容装回工作文件、磁盘 mtime 是新的；如果照实记进条目，
   * 下一次 status 会直接报 `normal`——**冲突解决完了却提交不了**，用户一头雾水。
   * 置 0 让快速路径必然不命中，从而每次都走内容哈希，判定才是对的。
   */
  private writeLocalChangeEntry(
    rel: string,
    kind: 'file' | 'dir',
    st: { size: number; mode: number },
    baseRev: number,
    baseHash: string | null,
  ): void {
    this.wc.upsertEntry({
      path: rel,
      kind,
      base_rev: baseRev,
      base_hash: baseHash,
      size: st.size,
      mtime_ms: 0,
      mode: st.mode & 0o111,
      status: 'modified',
      case_conflict: false,
    });
  }

  // ---------- 状态 ----------

  async status(opts: { forceHash?: boolean } = {}): Promise<StatusItem[]> {
    return computeStatus(this.wc, opts);
  }

  /** 加入待提交集（新文件必须先 add 才会出现在提交候选里）。 */
  add(paths: string[]): number {
    let n = 0;
    this.wc.tx(() => {
      for (const raw of paths) {
        const p = normalizeNfc(raw);
        const bad = validatePath(p);
        if (bad) throw new WcError('PATH_INVALID', bad, { path: p });
        // 隐式祖先目录随子路径一起纳入版本控制（svn 语义，与 update 端的
        // ensureAncestorEntries 对称）：否则 `add a/b.psd` 之后 `a` 仍是 unversioned，
        // 用户会反复 add 出重复目录。目录本身不进 changes——服务端 ensure_dirs 自补。
        const ancs: string[] = [];
        for (let a = parentOf(p); a; a = parentOf(a)) {
          if (this.wc.hasEntry(a)) break;
          ancs.push(a);
        }
        for (const a of ancs.reverse()) this.wc.stage(a, 'add');
        this.wc.stage(p, this.wc.hasEntry(p) ? 'modify' : 'add');
        n++;
      }
    });
    return n;
  }

  /**
   * 在工作副本里新建目录（§6.5：树上直接增删）。
   *
   * 目录不参与提交（不进 `changes`，服务端 `ensure_dirs` 隐式补建），所以**不 stage**：
   * 新目录会在下一次 `status` 里以 `unversioned` 出现，带文件之后才随文件一起进版本控制。
   */
  async mkdir(path: string): Promise<void> {
    const p = this.requireSafePath(path);
    await withRetry(() => mkdir(this.absOf(p), { recursive: true }));
  }

  /** 路径校验 + NFC 归一化（新建类操作共用的入口）。 */
  private requireSafePath(raw: string): string {
    const p = normalizeNfc(raw);
    const bad = validatePath(p);
    if (bad) throw new WcError('PATH_INVALID', bad, { path: p });
    if (p === '') throw new WcError('PATH_INVALID', '路径不能为空');
    return p;
  }

  /**
   * 删除：纳入待提交集（已纳管），或干脆当作本地文件删掉（未纳管）。
   *
   * 三种情况必须分开（"记一条待提交的删除"只对第一种成立）：
   *
   * 1. **有基线条目**（已纳管）→ 记一条待删除，文件立即移除；
   * 2. **只在待提交集里**（`add` 过、还没提交）→ 撤销那条 `add`（等于从没加过）；
   * 3. **完全陌生**（未纳管）→ 什么都不记，删掉文件即可。
   *
   * 旧实现一律 `stage('delete')`：删一个刚建的新文件，会给服务端发一条
   * "删除一个它从没见过的路径"，提交里平白多出一条变更（v0.4.19 修）。
   */
  async remove(paths: string[]): Promise<void> {
    this.wc.tx(() => {
      for (const raw of paths) {
        const p = normalizeNfc(raw);
        if (this.wc.hasEntry(p)) this.wc.stage(p, 'delete');
        else this.wc.unstage(p); // 撤销待提交的 add（没有就啥也不做）
      }
    });
    for (const raw of paths) {
      const p = normalizeNfc(raw);
      await withRetry(() => rm(this.absOf(p), { recursive: true, force: true }));
    }
  }

  /**
   * 读本副本的忽略规则文件（`.b-artifactignore`，不存在等价于空规则）。
   *
   * **忽略规则是"按副本/按仓库"的**（§6.2）：每个仓库要忽略的东西不一样，所以它跟着工作副本
   * 走，而不是放在全局设置里（真实反馈："每个仓库需要忽略的文件是不同的，设置里是公共的配置"）。
   */
  readIgnoreFile(): string {
    try {
      return readFileSync(join(this.root, IGNORE_FILE), 'utf8');
    } catch {
      return '';
    }
  }

  /**
   * 写本副本的忽略规则文件；**内容清空 = 删除该文件**（留个空文件只会在树上多一条噪音）。
   *
   * 写进去之后它就是副本里的**普通文件**：用户可以像提交别的文件一样提交它，让同仓库的
   * 同事共用同一套规则（git 的 `.gitignore` 就是这么做）。
   */
  async writeIgnoreFile(content: string): Promise<void> {
    const file = join(this.root, IGNORE_FILE);
    if (content.trim() === '') {
      await withRetry(() => rm(file, { force: true }));
      return;
    }
    await withRetry(() => writeFile(file, content, 'utf8'));
  }

  /** 还原：把工作文件恢复成基线内容（离线可用，§6.1）。 */
  async revert(paths: string[]): Promise<void> {
    for (const raw of paths) {
      const p = normalizeNfc(raw);
      const e = this.wc.getEntry(p);
      if (!e) throw new WcError('NOT_FOUND', `路径不在版本控制下：${p}`);
      if (e.kind === 'file' && e.base_hash) {
        await ensurePristine(this.root, e.base_hash, this.cacheDir);
        await installWorkingFile(this.root, e.base_hash, this.absOf(p));
        await this.writeEntryFromDisk(p, e.base_hash, e.base_rev);
      }
      // 还原 = 放弃本地改动，冲突随之解除（`.mine` 与冲突登记一起清掉）
      await withRetry(() => rm(this.absOf(p) + MINE_SUFFIX, { force: true }));
      this.wc.tx(() => {
        this.wc.unstage(p);
        this.wc.removeConflict(p);
      });
    }
  }

  // ---------- 提交（§6.3 / §7.3） ----------

  async commit(opts: CommitOptions = {}): Promise<CommitOutcome> {
    const baseRev = this.wc.revision;
    const status = await this.status();
    const want = new Set(opts.paths?.map(normalizeNfc));
    const matches = (p: string): boolean =>
      want.size === 0 || want.has(p) || [...want].some((w) => p.startsWith(`${w}/`));
    const local = status.filter((s) => isLocalChange(s.status) && matches(s.path));
    // v0.4.17（§6.5）：未纳管文件**自动算新增**，但只有被显式勾选才进提交集。
    // 界面上的默认提交集不含它们（构建产物、素材中间文件误提交的代价太高），
    // 于是"要不要提交这个新文件"仍然是用户的一次明确选择，只是不再需要先点"标记新增"。
    if (want.size > 0) {
      for (const s of status) {
        // 目录不进来：它在服务端由 `ensure_dirs` 随文件隐式补建，本地基线也会在
        // `applyCommittedRevision` 里由 `ensureAncestorEntries` 补齐。放进来只会把
        // "1 个文件"报成 2 个。
        if (s.status === 'unversioned' && s.kind === 'file' && matches(s.path)) {
          local.push({ ...s, status: 'added' });
        }
      }
    }
    // 显式 stage 的删除即使文件已不在磁盘也要带上
    for (const p of this.wc.allPending()) {
      if (p.op === 'delete' && !local.some((s) => s.path === p.path)) {
        const e = this.wc.getEntry(p.path);
        local.push({
          path: p.path,
          kind: e?.kind ?? 'file',
          status: 'deleted',
          base_rev: e?.base_rev ?? baseRev,
          base_hash: e?.base_hash ?? null,
          size: e?.size ?? 0,
          needs_update: false,
        });
      }
    }
    // `missing`（磁盘上被直接删掉、没走 `remove`）按 svn 1.7+ 的语义在提交时记为删除：
    // 用户已经用资源管理器删了文件，再要求他补一次 `remove` 只是添堵。
    // 不归一化的话走到下面组装 changes 会因找不到 uploads 条目而崩。
    for (const s of local) if (s.status === 'missing') s.status = 'deleted';

    if (local.length === 0) throw new WcError('NO_CHANGES', '没有需要提交的变更');
    // 只有目录变更（例如 add 了一个空目录）时 changes 会是空的：空目录在服务端没有
    // 独立实体，提交上去只会换来一句看不懂的服务端报错，这里直接说清楚。
    if (local.every((s) => s.kind === 'dir' && s.status !== 'deleted')) {
      throw new WcError('NO_CHANGES', '只有目录变更：空目录不会单独入库，请先放入文件');
    }
    if (local.some((s) => s.status === 'conflicted')) {
      throw new WcError('CONFLICT', '存在冲突，请先解决冲突再提交', {
        paths: local.filter((s) => s.status === 'conflicted').map((s) => s.path),
      });
    }

    // ② 计算待上传文件的内容哈希（并行）
    const uploads: { path: string; absPath: string; hash: string; size: number; mode: number }[] = [];
    let hashed = 0;
    await mapLimit(
      // 目录不参与哈希：它没有 blob，由服务端 ensure_dirs 隐式补建
      local.filter((s) => (s.status === 'modified' || s.status === 'added') && s.kind === 'file'),
      this.concurrency,
      async (s) => {
        const abs = this.absOf(s.path);
        const st = await stat(abs);
        const hash = await hashFile(abs);
        uploads.push({ path: s.path, absPath: abs, hash, size: st.size, mode: st.mode & 0o111 });
        opts.onProgress?.({ phase: 'hash', done: ++hashed, total: local.length, current: s.path });
      },
    );

    // ③ 变更清单
    const changes: ChangeSpec[] = [];
    for (const s of local) {
      if (s.status === 'deleted') {
        changes.push({ path: s.path, op: 'delete', kind: s.kind });
        continue;
      }
      if (s.kind === 'dir') continue; // 目录由服务端隐式补建，不进 changes
      const u = uploads.find((x) => x.path === s.path)!;
      changes.push({
        path: s.path,
        // 服务端会自动补建隐式祖先目录（storage::repo::ensure_dirs），无需提交目录条目
        op: s.status === 'added' ? 'add' : 'modify',
        kind: 'file',
        blob_hash: u.hash,
        size: u.size,
        mode: u.mode,
        mtime: Math.floor(Date.now() / 1000),
      });
    }

    // 提交涉及的文件路径（目录不参与锁模型）：提交成功后按这批路径解锁
    const committedFiles = local.filter((s) => s.kind === 'file').map((s) => s.path);

    const commitId = randomUUID();
    const { prep } = await this.prepareWithAutoLock(commitId, baseRev, changes, opts);

    if (prep.replayed && prep.rev !== undefined) {
      await this.applyCommittedRevision(prep.rev, local, uploads);
      await this.releaseCommittedLocks(committedFiles);
      return { rev: prep.rev, committed: local.map((s) => s.path), replayed: true };
    }
    if (!prep.commit_token) {
      throw new WcError('IO', '服务端未签发 commit_token');
    }

    // ④ 上传缺失 blob
    const need = new Set(prep.need_blobs ?? []);
    const todo = uploads.filter((u) => need.has(u.hash));
    let uploaded = 0;
    await mapLimit(todo, this.concurrency, async (u) => {
      await this.uploadBlob(u.absPath, u.hash, u.size);
      opts.onProgress?.({ phase: 'upload', done: ++uploaded, total: todo.length, current: u.path });
    });

    // ⑤ 落库（commit_id 幂等，网络重试安全）
    let res: CommitResult;
    try {
      res = await this.api.commit(this.repo, {
        commit_id: commitId,
        commit_token: prep.commit_token,
        message: opts.message,
      });
    } catch (e) {
      // 响应可能在服务端**已提交之后**丢失。此时若换个 commit_id 重来就会产出
      // 一笔重复提交；用同一个 commit_id 再问一次 prepare：已应用则直接拿到
      // replayed + rev，未应用则重新签发 token 后继续。这正是 commit_id 的意义。
      if (toApiError(e).code !== 'NETWORK') throw e;
      const again = await this.api.prepareCommit(this.repo, {
        commit_id: commitId,
        base_rev: baseRev,
        message: opts.message ?? '',
        changes,
      });
      if (again.replayed && again.rev !== undefined) {
        await this.applyCommittedRevision(again.rev, local, uploads);
        await this.releaseCommittedLocks(committedFiles);
        return { rev: again.rev, committed: local.map((s) => s.path), replayed: true };
      }
      if (!again.commit_token) throw e;
      res = await this.api.commit(this.repo, {
        commit_id: commitId,
        commit_token: again.commit_token,
        message: opts.message,
      });
    }

    await this.applyCommittedRevision(res.rev, local, uploads);
    await this.releaseCommittedLocks(committedFiles);
    return { rev: res.rev, committed: local.map((s) => s.path), replayed: res.replayed };
  }

  /**
   * prepare，并在服务端要求"先锁后提交"时**自动补锁**（§5.2 + §6.5）。
   *
   * 为什么不是提交前先查一遍锁：那要为每个变更路径发一次 `GET /locks`（N 次往返），
   * 而服务端在 412 里已经把**未持锁的路径清单**给了我们。于是流程是
   * "prepare → 412 NEEDS_LOCK → 逐个加锁 → 再 prepare"，代价恒定、且天然与
   * 服务端的判定一致（不会出现"客户端以为锁够了、服务端却拒"的偏差）。
   *
   * 加锁**串行**且第一个失败即抛：失败几乎总是"被别人持锁"，此时用户需要知道
   * 是谁占着（`ApiError.details` 带 owner），并发只会把错误顺序搅乱。
   *
   * 只重试一次：第二次再失败就说明清单之外还有别的东西在变，直接让错误冒上去，
   * 比无限重试更容易排查。
   */
  private async prepareWithAutoLock(
    commitId: string,
    baseRev: number,
    changes: ChangeSpec[],
    opts: CommitOptions,
  ): Promise<{ prep: PrepareResult; autoLocked: string[] }> {
    const body = {
      commit_id: commitId,
      base_rev: baseRev,
      message: opts.message ?? '',
      changes,
    };
    try {
      return { prep: await this.api.prepareCommit(this.repo, body), autoLocked: [] };
    } catch (e) {
      const err = toApiError(e);
      if (err.code !== 'NEEDS_LOCK') throw e;
      const missing = (err.details as { paths?: string[] } | undefined)?.paths ?? [];
      if (missing.length === 0) throw e;
      let n = 0;
      for (const p of missing) {
        await this.lock(p);
        opts.onProgress?.({ phase: 'lock', done: ++n, total: missing.length, current: p });
      }
      return { prep: await this.api.prepareCommit(this.repo, body), autoLocked: missing };
    }
  }

  /**
   * 提交成功后自动解锁：**本次提交涉及的文件上、由本人持有的锁，全部释放**（§6.5）。
   *
   * 规则很简单：文件已经提交上去了，编辑告一段落，锁就该交还 —— 无论是提交时自动补的，
   * 还是用户之前手动加的。留着不放的后果不是"多几行脏数据"，而是**下一个想改这个文件的
   * 人永远加不上锁**（锁要 TTL 到期才失效，默认 7 天）。
   *
   * 只动"本次提交涉及的路径"：用户锁着但没提交的文件不受影响。
   * 释放失败不影响提交结果 —— 锁还在，用户可以手动解，或等 TTL。
   */
  private async releaseCommittedLocks(paths: string[]): Promise<void> {
    for (const p of paths) {
      // 只解"我确实持有"的那些（本地锁表是权威：没锁就别白跑一趟网络）
      if (!this.wc.getLock(p)) continue;
      try {
        await this.unlock(p);
      } catch {
        /* 释放失败不影响提交结果 */
      }
    }
  }

  /** 提交成功后：刷新基线、把新内容纳入缓存、清理待提交集。 */
  private async applyCommittedRevision(
    rev: number,
    local: StatusItem[],
    uploads: { path: string; absPath: string; hash: string; size: number; mode: number }[],
  ): Promise<void> {
    for (const u of uploads) {
      await importWorkingFile(u.absPath, this.cacheDir, u.hash);
      await ensurePristine(this.root, u.hash, this.cacheDir);
    }
    this.wc.tx(() => {
      for (const s of local) {
        if (s.status === 'deleted') {
          this.wc.deleteEntry(s.path);
          this.wc.unstage(s.path);
          continue;
        }
        this.ensureAncestorEntries(s.path, rev);
        if (s.kind === 'dir') {
          this.wc.upsertEntry({
            path: s.path,
            kind: 'dir',
            base_rev: rev,
            base_hash: null,
            size: 0,
            mtime_ms: Math.floor(Date.now()),
            mode: 0o755,
            status: 'normal',
            case_conflict: false,
          });
          this.wc.unstage(s.path);
          continue;
        }
        const u = uploads.find((x) => x.path === s.path)!;
        this.wc.upsertEntry({
          path: s.path,
          kind: 'file',
          base_rev: rev,
          base_hash: u.hash,
          size: u.size,
          mtime_ms: Math.floor(Date.now()),
          mode: u.mode,
          status: 'normal',
          case_conflict: false,
        });
        this.wc.unstage(s.path);
      }
      this.wc.revision = rev;
    });
    // 落库后重扫一次，把 mtime/size 校准成磁盘真实值（否则下次 status 必定重算哈希）
    for (const s of local) {
      if (s.status === 'deleted' || s.kind === 'dir') continue;
      const u = uploads.find((x) => x.path === s.path)!;
      if (existsSync(this.absOf(s.path))) await this.writeEntryFromDisk(s.path, u.hash, rev);
    }
  }

  /** 上传：小文件整块，大文件分块（§7.2）。 */
  private async uploadBlob(absPath: string, hash: string, size: number): Promise<void> {
    if (!needsChunkedUpload(size)) {
      const buf = await readAll(absPath);
      await this.api.putBlob(this.repo, hash, buf);
      return;
    }
    const chunkSize = DEFAULT_CHUNK_SIZE;
    const total = Math.ceil(size / chunkSize);
    const session = await this.api.createUpload(this.repo, { hash, size, chunk_size: chunkSize });
    const received = new Set(session.received_chunks ?? []);
    for (let n = 0; n < total; n++) {
      if (received.has(n)) continue;
      const buf = await readRange(absPath, n * chunkSize, chunkSize);
      await this.api.putChunk(this.repo, session.upload_id, n, buf);
    }
    await this.api.completeUpload(this.repo, session.upload_id);
  }

  // ---------- 更新（§6.3） ----------

  async update(opts: { onProgress?: (e: ProgressEvent) => void } = {}): Promise<UpdateOutcome> {
    const info = await this.api.repoInfo(this.repo);
    const from = this.wc.revision;
    if (info.head_rev <= from) {
      return { rev: from, updated: [], deleted: [], conflicts: [], skipped: [] };
    }

    const prefixes = this.sparsePaths.length > 0 ? this.sparsePaths : [''];
    const rows = (
      await Promise.all(
        prefixes.map((p) => this.api.changes(this.repo, { from: from + 1, to: info.head_rev, prefix: p })),
      )
    ).flat();

    // 折叠：同一路径只保留最后一条（changes 已按 rev 升序）
    const latest = new Map<string, (typeof rows)[number]>();
    for (const r of rows) latest.set(r.path, r);

    const localStatus = await this.status();
    const modified = new Set(
      localStatus.filter((s) => isLocalChange(s.status)).map((s) => s.path),
    );

    const outcome: UpdateOutcome = {
      rev: info.head_rev,
      updated: [],
      deleted: [],
      conflicts: [],
      skipped: [],
    };

    let done = 0;
    for (const [path, ch] of latest) {
      done++;
      opts.onProgress?.({ phase: 'apply', done, total: latest.size, current: path });

      if (ch.op === 'delete') {
        if (modified.has(path)) {
          // 本地改过、服务端删了：保留本地文件，标记为冲突交给人处理（绝不静默丢数据）
          const prev = this.wc.getEntry(path);
          const mineHash = await this.hashOfMine(path, /* 文件本身即本地版本 */ false);
          this.wc.tx(() => {
            if (prev) this.wc.upsertEntry({ ...prev, status: 'conflicted' });
            this.wc.putConflict({
              path,
              kind: prev?.kind ?? 'file',
              base_hash: prev?.base_hash ?? null,
              theirs_hash: null,
              mine_hash: mineHash,
              theirs_rev: ch.rev,
              reason: 'deleted-remotely',
              created_at: Date.now(),
            });
          });
          outcome.conflicts.push(path);
          continue;
        }
        await withRetry(() => rm(this.absOf(path), { recursive: true, force: true }));
        this.wc.tx(() => this.wc.deleteEntry(path));
        outcome.deleted.push(path);
        continue;
      }

      if (ch.kind === 'dir') {
        await mkdir(this.absOf(path), { recursive: true });
        this.wc.tx(() =>
          this.wc.upsertEntry({
            path,
            kind: 'dir',
            base_rev: ch.rev,
            base_hash: null,
            size: 0,
            mtime_ms: 0,
            mode: 0o755,
            status: 'normal',
            case_conflict: false,
          }),
        );
        continue;
      }

      if (!ch.blob_hash) {
        outcome.skipped.push(path);
        continue;
      }

      const base = this.wc.getEntry(path);
      if (modified.has(path) && base && base.base_hash !== ch.blob_hash) {
        // 双方都改：把本地版本另存为 `<path>.mine`，服务端版本落到工作文件（§6.2 conflicted）
        await withRetry(() => renameWithRetry(this.absOf(path), this.absOf(path + MINE_SUFFIX)));
        await this.installFile(path, ch.blob_hash);
        await this.writeEntryFromDisk(path, ch.blob_hash, ch.rev);
        // 先算哈希（I/O 不能放在事务回调里）
        const mineHash = await this.hashOfMine(path);
        this.wc.tx(() => {
          this.ensureAncestorEntries(path, ch.rev);
          this.wc.upsertEntry({ ...this.wc.getEntry(path)!, status: 'conflicted' });
          // 登记冲突：entry.base_hash 已经被覆盖成服务端版本，共同祖先的哈希只能留在这里
          this.wc.putConflict({
            path,
            kind: 'file',
            base_hash: base.base_hash,
            theirs_hash: ch.blob_hash,
            mine_hash: mineHash,
            theirs_rev: ch.rev,
            reason: 'both-modified',
            created_at: Date.now(),
          });
        });
        outcome.conflicts.push(path);
        continue;
      }

      try {
        await this.installFile(path, ch.blob_hash);
      } catch (e) {
        // 本机文件系统限制（长路径 / 保留名 / 文件被占用）→ skip + 报告，不阻断整个 update（§6.6 ③）
        outcome.skipped.push(path);
        continue;
      }
      await this.writeEntryFromDisk(path, ch.blob_hash, ch.rev);
      this.wc.tx(() => this.ensureAncestorEntries(path, ch.rev));
      outcome.updated.push(path);
    }

    this.wc.tx(() => {
      this.wc.revision = info.head_rev;
    });
    return outcome;
  }

  // ---------- 锁（§7.2） ----------

  async listLocks(path?: string): Promise<LockInfo[]> {
    return this.api.listLocks(this.repo, path ? { path } : {});
  }

  /**
   * 加锁（§5.2 v0.4.17：只剩文件锁，不再有 `kind`）。
   *
   * `wc.db` 的 `locks.kind` 列保留、恒写 `'file'`（列不能再删，否则存量库要重建表）。
   */
  async lock(path: string, opts: { comment?: string; expiresIn?: number } = {}): Promise<LockInfo> {
    const p = normalizeNfc(path);
    const info = await this.api.acquireLock(this.repo, {
      path: p,
      comment: opts.comment,
      expires_in: opts.expiresIn,
    });
    this.wc.putLock({
      path: p,
      kind: 'file',
      token: info.token ?? '',
      owner: info.owner ?? null,
      comment: opts.comment ?? null,
      expires_at: info.expires_at ?? null,
    });
    return info;
  }

  /**
   * 批量加锁（§6.5 "勾选即提交"的自动补锁）。
   *
   * **串行**逐个加，且第一个失败就抛出 —— 用户真正需要知道的是"哪个文件被别人占着"，
   * 并发打过去会把错误顺序搅乱、还可能把半边成功的结果藏起来。
   * 返回成功加锁的路径；失败时抛 `ApiError`（`LOCKED`，details 里带持锁人）。
   */
  async lockMany(paths: string[]): Promise<string[]> {
    const done: string[] = [];
    for (const p of paths) {
      await this.lock(p);
      done.push(p);
    }
    return done;
  }

  async unlock(path: string, opts: { breakLock?: boolean; reason?: string } = {}): Promise<void> {
    const p = normalizeNfc(path);
    const held = this.wc.getLock(p);
    await this.api.releaseLock(this.repo, p, {
      token: held?.token,
      breakLock: opts.breakLock,
      reason: opts.reason,
    });
    this.wc.removeLock(p);
  }

  myLocks() {
    return this.wc.allLocks();
  }

  // ---------- 冲突（§6.2 / §6.5 冲突视图） ----------

  /** 当前未解决的冲突，带"界面上能做什么"的判定。 */
  async conflicts(): Promise<ConflictInfo[]> {
    const rows = this.wc.allConflicts();
    const out: ConflictInfo[] = [];
    for (const row of rows) {
      const sides = await this.readSides(row);
      out.push({
        path: row.path,
        kind: row.kind,
        reason: row.reason,
        has_mine: sides.mine.kind !== 'missing',
        has_theirs: sides.theirs.kind !== 'missing',
        mergeable: sides.base.kind === 'text' && sides.mine.kind === 'text' && sides.theirs.kind === 'text',
        theirs_rev: row.theirs_rev,
        sides: { base: sides.base.kind, mine: sides.mine.kind, theirs: sides.theirs.kind },
      });
    }
    return out;
  }

  /** 读三方内容（本地 / 服务端 / 基线）用于对比。 */
  async conflictSides(path: string): Promise<ConflictSides> {
    const row = this.wc.getConflict(normalizeNfc(path));
    if (!row) throw new WcError('NOT_FOUND', `该路径没有未解决的冲突：${path}`);
    const sides = await this.readSides(row);
    return {
      path: row.path,
      mergeable: sides.base.kind === 'text' && sides.mine.kind === 'text' && sides.theirs.kind === 'text',
      base: sides.base,
      mine: sides.mine,
      theirs: sides.theirs,
    };
  }

  /**
   * 解决冲突。
   *
   *   - `theirs`：接受服务端版本（工作文件已经是它，删掉 `.mine` 即可 → 回到 `normal`）
   *   - `mine`：取本地版本（`both-modified` 用 `.mine` 覆盖工作文件；
   *     `deleted-remotely` 则是"服务端删了但我保留"，此时服务端已无此路径，
   *     必须退掉基线条目并重新 add，否则提交时会拿 modify 去撞一个已删除的路径）
   *   - `merged`：写入调用方给的合并结果（必须是文本）
   */
  async resolveConflict(
    path: string,
    opts: { choice: ConflictResolution; content?: string },
  ): Promise<void> {
    const rel = normalizeNfc(path);
    const row = this.wc.getConflict(rel);
    if (!row) throw new WcError('NOT_FOUND', `该路径没有未解决的冲突：${path}`);

    const abs = this.absOf(rel);
    const absMine = abs + MINE_SUFFIX;

    switch (opts.choice) {
      case 'theirs': {
        if (row.reason === 'deleted-remotely') {
          // 接受删除：文件还留在磁盘上，删掉并退登记
          await withRetry(() => rm(abs, { recursive: true, force: true }));
          this.wc.tx(() => {
            this.wc.deleteEntry(rel);
            this.wc.unstage(rel);
          });
        } else {
          // 工作文件已经是服务端版本，只需落基线与状态
          if (row.theirs_hash) await this.writeEntryFromDisk(rel, row.theirs_hash, row.theirs_rev);
        }
        await withRetry(() => rm(absMine, { force: true }));
        break;
      }

      case 'mine': {
        if (row.reason === 'both-modified') {
          if (!existsSync(absMine)) {
            throw new WcError('NOT_FOUND', `本地版本已不在：${rel}${MINE_SUFFIX}`);
          }
          await withRetry(() => renameWithRetry(absMine, abs));
          const st = await stat(abs);
          this.wc.tx(() => {
            // base 仍是服务端版本 → 内容与基线不同，状态是 modified，可直接提交
            this.writeLocalChangeEntry(rel, row.kind, st, row.theirs_rev, row.theirs_hash);
          });
        } else {
          // 服务端已删除、本地保留：退掉基线条目并重新纳入（提交时是 add）
          await withRetry(() => rm(absMine, { force: true }));
          const st = await stat(abs);
          this.wc.tx(() => {
            this.wc.deleteEntry(rel);
            this.wc.stage(rel, 'add');
          });
          void st;
        }
        break;
      }

      case 'merged': {
        if (typeof opts.content !== 'string') {
          throw new WcError('IO', '合并结果不能为空');
        }
        if (row.reason !== 'both-modified') {
          throw new WcError('CONFLICT', '服务端已删除该路径，不能做内容合并');
        }
        await mkdir(join(abs, '..'), { recursive: true });
        await writeFile(abs, opts.content, 'utf8');
        await withRetry(() => rm(absMine, { force: true }));
        const stMerged = await stat(abs);
        this.wc.tx(() => {
          this.writeLocalChangeEntry(rel, row.kind, stMerged, row.theirs_rev, row.theirs_hash);
        });
        break;
      }
    }

    this.wc.tx(() => this.wc.removeConflict(rel));
  }

  /** 读一行的三方内容（供 `conflicts()` 与 `conflictSides()` 共用）。 */
  private async readSides(row: ConflictRow): Promise<{
    base: SideContent;
    mine: SideContent;
    theirs: SideContent;
  }> {
    const abs = this.absOf(row.path);
    const absMine = abs + MINE_SUFFIX;

    // base：共同祖先，从 pristine 里取（内容寻址，一直在缓存里）
    let base: SideContent = { kind: 'missing', size: 0, text: null };
    if (row.base_hash) {
      await ensurePristine(this.root, row.base_hash, this.cacheDir);
      base = toSide(await readTextCapped(pristinePathFor(this.root, row.base_hash)));
    }

    // mine：both-modified 在 `<path>.mine`；deleted-remotely 时工作文件本身就是本地版本
    const minePath = row.reason === 'both-modified' ? absMine : abs;
    const mine = toSide(await readTextCapped(minePath));

    // theirs：both-modified 时工作文件已被换成服务端版本；deleted-remotely 没有服务端内容
    const theirs: SideContent =
      row.reason === 'both-modified'
        ? toSide(await readTextCapped(abs))
        : { kind: 'missing', size: 0, text: null };

    return { base, mine, theirs };
  }

  /** 本地版本的内容哈希：优先 `<path>.mine`，退回工作文件；都不存在则为 null。 */
  private async hashOfMine(rel: string, preferMineFile = true): Promise<string | null> {
    const candidates = preferMineFile ? [this.absOf(rel) + MINE_SUFFIX, this.absOf(rel)] : [this.absOf(rel)];
    for (const p of candidates) {
      if (existsSync(p)) {
        try {
          return await hashFile(p);
        } catch {
          return null;
        }
      }
    }
    return null;
  }
}

function toSide(r: TextReadResult): SideContent {
  if (r.kind === 'text') return { kind: 'text', size: r.size, text: r.text };
  return { kind: r.kind, size: r.size, text: null };
}

// ---------- 辅助 ----------

async function readAll(absPath: string): Promise<Uint8Array> {
  const chunks: Buffer[] = [];
  for await (const c of createReadStream(absPath)) chunks.push(c as Buffer);
  return new Uint8Array(Buffer.concat(chunks));
}

async function readRange(absPath: string, offset: number, length: number): Promise<Uint8Array> {
  const fh = await open(absPath, 'r');
  try {
    const buf = Buffer.alloc(length);
    const { bytesRead } = await fh.read(buf, 0, length, offset);
    return new Uint8Array(buf.subarray(0, bytesRead));
  } finally {
    await fh.close();
  }
}

/**
 * Windows 上 Photoshop / Office 会持有文件句柄，导致 rename 失败（§6.6 ④）。
 * 三次退避重试（1s / 2s / 4s），仍失败则抛出，绝不留下半截状态。
 */
async function renameWithRetry(from: string, to: string): Promise<void> {
  return withRetry(() => rename(from, to), { retries: 3, baseDelayMs: 1000 });
}

export type { ConflictReason, EntryRow, StatusItem };

/** 扫描警告（供 CLI 展示）。 */
export async function scanWarnings(root: string): Promise<string[]> {
  const r = await scanDisk(root);
  return r.warnings;
}
