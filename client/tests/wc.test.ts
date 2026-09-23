import { existsSync, readFileSync, statSync } from 'node:fs';
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type {
  ApiClient,
  ChangeRow,
  ChangeSpec,
  LockInfo,
  RepoInfo,
  TreeResult,
} from '../src/core/api.js';
import { ApiError, WcError } from '../src/core/errors.js';
import { hashBuffer } from '../src/core/hash.js';
import { cachePathFor, pristinePathFor } from '../src/core/pristine.js';
import { parentOf, utf8Len } from '../src/core/paths.js';
import { WorkingCopy } from '../src/core/wc.js';

let dir: string;
let cacheDir: string;
let cacheRoot: string;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), 'ba-wc-'));
  // 全局缓存必须在工作副本**之外**——否则会被 scanDisk 当成待提交的未纳管文件。
  cacheRoot = await mkdtemp(join(tmpdir(), 'ba-cache-'));
  cacheDir = join(cacheRoot, 'cache');
  await mkdir(cacheDir, { recursive: true });
});

afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
  await rm(cacheRoot, { recursive: true, force: true });
});

interface ServerFile {
  kind: 'file' | 'dir';
  hash: string | null;
  size: number;
  mode: number;
}

/**
 * 服务端替身：按真实语义维护 head_rev / blob 仓库 / 变更日志 / 锁，
 * 而不是简单返回固定值——否则测不出「提交后 revision 前进」「OUT_OF_DATE」这类行为。
 */
class FakeServer {
  baseUrl = 'http://fake/api/v1';
  headRev = 1;
  files = new Map<string, ServerFile>();
  blobs = new Map<string, Uint8Array>();
  changeLog: ChangeRow[] = [];
  locks = new Map<string, LockInfo>();
  prepared = new Map<string, ChangeSpec[]>();
  committed = new Map<string, number>();
  uploads: string[] = [];
  downloads: string[] = [];
  /** 让下一次 api.commit 在**已应用提交后**丢响应，模拟响应丢失。 */
  loseCommitResponse = false;
  /**
   * 模拟 v0.4.17 的服务端：**提交前每个文件都必须由本人持锁**（§5.2）。
   * 打开后 prepare 会对未持锁的路径回 412 NEEDS_LOCK + 路径清单，
   * 用来验证客户端的"自动补锁再重试"。
   */
  requireLock = false;
  user = 'alice';

  put(path: string, content: string): void {
    const hash = hashBuffer(content);
    this.blobs.set(hash, new TextEncoder().encode(content));
    this.files.set(path, { kind: 'file', hash, size: utf8Len(content), mode: 0 });
    for (let p = parentOf(path); p; p = parentOf(p)) {
      if (!this.files.has(p)) this.files.set(p, { kind: 'dir', hash: null, size: 0, mode: 0o755 });
    }
  }

  mkdir_(path: string): void {
    this.files.set(path, { kind: 'dir', hash: null, size: 0, mode: 0o755 });
    for (let p = parentOf(path); p; p = parentOf(p)) {
      if (!this.files.has(p)) this.files.set(p, { kind: 'dir', hash: null, size: 0, mode: 0o755 });
    }
  }

  /** 模拟他人直接提交（不经过本工作副本）。 */
  otherCommit(changes: ChangeSpec[]): number {
    this.headRev += 1;
    const rev = this.headRev;
    for (const c of changes) {
      if (c.op === 'delete') {
        this.files.delete(c.path);
      } else {
        this.files.set(c.path, {
          kind: 'file',
          hash: c.blob_hash ?? null,
          size: c.size ?? 0,
          mode: c.mode ?? 0,
        });
        if (c.blob_hash) this.blobs.set(c.blob_hash, this.blobs.get(c.blob_hash) ?? new Uint8Array());
      }
      this.changeLog.push({
        rev,
        path: c.path,
        op: c.op,
        kind: c.kind,
        blob_hash: c.blob_hash ?? null,
        size: c.size ?? 0,
        mode: c.mode ?? 0,
        mtime: 0,
      });
    }
    return rev;
  }

  /** 让服务端持有某个 blob 的内容（供他人提交的新版本使用）。 */
  seedBlob(content: string): string {
    const hash = hashBuffer(content);
    this.blobs.set(hash, new TextEncoder().encode(content));
    return hash;
  }

  get api(): ApiClient {
    return {
      baseUrl: this.baseUrl,

      repoInfo: async (): Promise<RepoInfo> =>
        ({
          id: 1,
          name: 'art',
          description: '',
          owner: 'alice',
          head_rev: this.headRev,
          created_at: '',
          my_role: 'writer',
          my_permissions: { read: true, write: true, admin: false },
        }) as RepoInfo,

      tree: async (_repo: string, q: { prefix?: string } = {}): Promise<TreeResult> => {
        const prefix = q.prefix ?? '';
        const items = [...this.files.entries()]
          .filter(([p]) => prefix === '' || p === prefix || p.startsWith(`${prefix}/`))
          .map(([path, f]) => ({
            path,
            kind: f.kind,
            blob_hash: f.hash,
            size: f.size,
            mode: f.mode,
            mtime: 0,
            changed_rev: this.headRev,
          }));
        return { repo: 'art', rev: this.headRev, prefix, depth: 16, items, total: items.length };
      },

      openBlobStream: async (_repo: string, hash: string): Promise<Response> => {
        this.downloads.push(hash);
        const b = this.blobs.get(hash);
        if (!b) throw new Error(`blob 不存在：${hash}`);
        return new Response(new Uint8Array(b), { status: 200 });
      },

      prepareCommit: async (_repo: string, req: { commit_id: string; base_rev: number; changes: ChangeSpec[] }) => {
        const prev = this.committed.get(req.commit_id);
        if (prev !== undefined) return { commit_id: req.commit_id, replayed: true, rev: prev };
        if (req.base_rev !== this.headRev) {
          throw new ApiError({ status: 409, code: 'OUT_OF_DATE', message: '基线已过期' });
        }
        if (this.requireLock) {
          const missing = req.changes
            .filter((c) => c.kind === 'file' && this.locks.get(c.path)?.owner !== this.user)
            .map((c) => c.path);
          if (missing.length > 0) {
            throw new ApiError({
              status: 412,
              code: 'NEEDS_LOCK',
              message: '提交前必须对每个文件加锁',
              details: { paths: missing, locked_by: [] },
            });
          }
        }
        this.prepared.set(req.commit_id, req.changes);
        const need = [
          ...new Set(req.changes.map((c) => c.blob_hash).filter((h): h is string => !!h)),
        ].filter((h) => !this.blobs.has(h));
        return { commit_id: req.commit_id, commit_token: 'tok', need_blobs: need, expires_in: 600 };
      },

      commit: async (_repo: string, req: { commit_id: string; token?: string; commit_token: string }) => {
        const prev = this.committed.get(req.commit_id);
        if (prev !== undefined) return { commit_id: req.commit_id, rev: prev, replayed: true };
        const changes = this.prepared.get(req.commit_id);
        if (!changes) throw new ApiError({ status: 409, code: 'COMMIT_TOKEN_EXPIRED', message: '无此 prepare' });
        this.headRev += 1;
        const rev = this.headRev;
        for (const c of changes) {
          if (c.op === 'delete') {
            this.files.delete(c.path);
          } else {
            this.files.set(c.path, { kind: 'file', hash: c.blob_hash ?? null, size: c.size ?? 0, mode: c.mode ?? 0 });
            if (c.blob_hash) this.blobs.set(c.blob_hash, this.blobs.get(c.blob_hash) ?? new Uint8Array());
            for (let p = parentOf(c.path); p; p = parentOf(p)) {
              if (!this.files.has(p)) this.files.set(p, { kind: 'dir', hash: null, size: 0, mode: 0o755 });
            }
          }
          this.changeLog.push({
            rev,
            path: c.path,
            op: c.op,
            kind: c.kind,
            blob_hash: c.blob_hash ?? null,
            size: c.size ?? 0,
            mode: c.mode ?? 0,
            mtime: 0,
          });
        }
        this.committed.set(req.commit_id, rev);
        if (this.loseCommitResponse) {
          this.loseCommitResponse = false;
          throw new ApiError({ status: 0, code: 'NETWORK', message: '响应丢失' });
        }
        return { commit_id: req.commit_id, rev, replayed: false };
      },

      putBlob: async (_repo: string, hash: string, data: Uint8Array) => {
        this.uploads.push(hash);
        this.blobs.set(hash, data);
      },

      changes: async (_repo: string, q: { from: number; to?: number }) =>
        this.changeLog.filter((r) => r.rev >= q.from && r.rev <= (q.to ?? this.headRev)),

      acquireLock: async (_repo: string, req: { path: string; kind?: 'file' | 'dir'; comment?: string }) => {
        const held = this.locks.get(req.path);
        if (held && held.owner !== this.user) {
          throw new ApiError({ status: 409, code: 'LOCKED', message: '被他人锁定', details: { holder: held.owner } });
        }
        const info: LockInfo = {
          id: this.locks.size + 1,
          path: req.path,
          kind: req.kind ?? 'file',
          owner_id: 1,
          owner: this.user,
          comment: req.comment ?? null,
          created_at: '',
          expires_at: null,
          token: 'locktok',
        };
        this.locks.set(req.path, info);
        return info;
      },

      releaseLock: async (_repo: string, path: string, opts: { breakLock?: boolean } = {}) => {
        const held = this.locks.get(path);
        if (!held) throw new ApiError({ status: 404, code: 'NOT_FOUND', message: '无此锁' });
        if (held.owner !== this.user && !opts.breakLock) {
          throw new ApiError({ status: 409, code: 'LOCKED', message: '不能解他人锁' });
        }
        this.locks.delete(path);
        return { released: 1 };
      },

      listLocks: async () => [...this.locks.values()],
    } as unknown as ApiClient;
  }
}

async function read(rel: string): Promise<string> {
  return readFile(join(dir, ...rel.split('/')), 'utf8');
}

async function write(rel: string, content: string): Promise<void> {
  const abs = join(dir, ...rel.split('/'));
  await mkdir(join(abs, '..'), { recursive: true });
  await writeFile(abs, content, 'utf8');
}

// ---------------------------------------------------------------- 检出

describe('checkout', () => {
  it('全量检出：文件内容、基线条目、revision 都对得上', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    srv.put('sub/b.png', 'BBB');

    const wc = await WorkingCopy.checkout({
      root: dir,
      client: srv.api,
      repo: 'art',
      cacheDir,
    });
    try {
      expect(await read('a.psd')).toBe('AAA');
      expect(await read('sub/b.png')).toBe('BBB');
      expect(wc.revision).toBe(1);
      expect(wc.wc.allEntries().map((e) => e.path)).toEqual(['a.psd', 'sub', 'sub/b.png']);
      const st = await wc.status();
      expect(st.every((i) => i.status === 'normal')).toBe(true);
    } finally {
      wc.close();
    }
  });

  it('部分检出只拉指定前缀', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    srv.put('sub/b.png', 'BBB');

    const wc = await WorkingCopy.checkout({
      root: dir,
      client: srv.api,
      repo: 'art',
      cacheDir,
      sparse: ['sub'],
    });
    try {
      expect(existsSync(join(dir, 'sub', 'b.png'))).toBe(true);
      expect(existsSync(join(dir, 'a.psd'))).toBe(false);
      expect(wc.sparsePaths).toEqual(['sub']);
    } finally {
      wc.close();
    }
  });

  it('重复检出同一目录被拒绝', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    wc.close();
    await expect(
      WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir }),
    ).rejects.toMatchObject({ code: 'NOT_A_WORKING_COPY' });
  });

  it('第二次检出命中全局缓存，不再下载', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const h = hashBuffer('AAA');
    expect(srv.downloads).toEqual([]);

    const w1 = await WorkingCopy.checkout({ root: join(dir, 'w1'), client: srv.api, repo: 'art', cacheDir });
    w1.close();
    expect(srv.downloads).toEqual([h]);

    const w2 = await WorkingCopy.checkout({ root: join(dir, 'w2'), client: srv.api, repo: 'art', cacheDir });
    w2.close();
    expect(srv.downloads).toEqual([h]); // 缓存命中，没有第二次下载
    expect(existsSync(cachePathFor(h, cacheDir))).toBe(true);
  });
});

// ---------------------------------------------------------------- pristine 不变量

describe('状态判定的 racy 边界（§6.1 的已知取舍）', () => {
  it('同大小 + 同 mtime ⇒ 快速路径判为未改动；forceHash 能发现（记录该已知边界）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'BASE');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    try {
      // 构造"基线记录与内容变更落在同一毫秒"的场面：先改内容（字节数相同），
      // 再把条目的 mtime 对齐到磁盘此刻的 mtime。
      // （不用 utimes 反推时间戳：浮点秒会丢亚毫秒精度，测试会变成看运气。）
      await write('a.psd', 'MINE');
      const st = await stat(join(dir, 'a.psd'));
      const entry = wc.wc.getEntry('a.psd')!;
      wc.wc.upsertEntry({ ...entry, mtime_ms: Math.floor(st.mtimeMs), size: st.size });

      // 快速路径：mtime + size 都没变 ⇒ 认定未改动（省掉一次哈希，这是 §6.1 的取舍）
      const fast = (await wc.status()).find((s) => s.path === 'a.psd')!;
      expect(fast.status).toBe('normal');

      // 需要确定答案时用 forceHash —— 界面上的"刷新状态"走的就是这条
      const forced = (await wc.status({ forceHash: true })).find((s) => s.path === 'a.psd')!;
      expect(forced.status).toBe('modified');
    } finally {
      wc.close();
    }
  });
});

describe('部分检出的基线条目', () => {
  it('前缀目录本身与祖先都要有基线（服务端的 tree?prefix=X 不含 X，回归：曾漏登记）', async () => {
    const srv = new FakeServer();
    srv.put('characters/hero.psd', 'HERO');
    srv.put('characters/props/table.png', 'TABLE');
    srv.put('docs/readme.txt', 'DOC');

    const wc = await WorkingCopy.checkout({
      root: dir,
      client: srv.api,
      repo: 'art',
      cacheDir,
      sparse: ['characters'],
    });
    try {
      // 磁盘上只应有 characters 子树
      expect(existsSync(join(dir, 'characters', 'hero.psd'))).toBe(true);
      expect(existsSync(join(dir, 'docs'))).toBe(false);

      // 关键：characters 自己有基线条目，否则它会一直被报成"未纳管"
      expect(wc.wc.getEntry('characters')?.kind).toBe('dir');
      expect(wc.wc.getEntry('characters/props')?.kind).toBe('dir');
      expect((await wc.status()).filter((s) => s.status === 'unversioned')).toEqual([]);
    } finally {
      wc.close();
    }
  });

  it('部分检出前缀是个空目录时也要建出来并登记', async () => {
    const srv = new FakeServer();
    srv.put('a/x.psd', 'X');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir, sparse: ['empty'] });
    try {
      expect(existsSync(join(dir, 'empty'))).toBe(true);
      expect(wc.wc.getEntry('empty')?.kind).toBe('dir');
      expect((await wc.status()).filter((s) => s.status === 'unversioned')).toEqual([]);
    } finally {
      wc.close();
    }
  });

  it('全量检出后也没有"未纳管"的目录（根一层的目录由树返回）', async () => {
    const srv = new FakeServer();
    srv.put('characters/hero.psd', 'HERO');
    srv.put('docs/readme.txt', 'DOC');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    try {
      expect((await wc.status()).filter((s) => s.status === 'unversioned')).toEqual([]);
    } finally {
      wc.close();
    }
  });
});

describe('pristine 三层布局（§6.1）', () => {
  it('改工作文件不会污染 pristine 与全局缓存', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    const h = hashBuffer('AAA');
    const cache = cachePathFor(h, cacheDir);
    const pristine = pristinePathFor(dir, h);
    expect(existsSync(cache)).toBe(true);
    expect(existsSync(pristine)).toBe(true);

    await write('a.psd', 'XXX'); // 用户原地保存

    expect(readFileSync(cache, 'utf8')).toBe('AAA');
    expect(readFileSync(pristine, 'utf8')).toBe('AAA');
    wc.close();
  });

  it('revert 从 pristine 恢复基线内容（离线可用）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('a.psd', 'XXX');
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('modified');

    await wc.revert(['a.psd']);

    expect(await read('a.psd')).toBe('AAA');
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('normal');
    wc.close();
  });

  it('revert 未纳管路径抛 NOT_FOUND', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await expect(wc.revert(['nope.psd'])).rejects.toMatchObject({ code: 'NOT_FOUND' });
    wc.close();
  });
});

// ---------------------------------------------------------------- 提交

describe('commit', () => {
  it('新增文件：add → commit → 服务端前进一版，本地基线刷新', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await write('new.psd', 'NEW');
    expect(wc.add(['new.psd'])).toBe(1);

    const out = await wc.commit({ message: 'add new' });
    expect(out.rev).toBe(2);
    expect(out.replayed).toBe(false);
    expect(out.committed).toEqual(['new.psd']);
    expect(srv.uploads).toEqual([hashBuffer('NEW')]);
    expect(srv.files.get('new.psd')?.hash).toBe(hashBuffer('NEW'));

    expect(wc.revision).toBe(2);
    expect(wc.wc.allPending()).toEqual([]);
    expect((await wc.status()).every((s) => s.status === 'normal')).toBe(true);
    wc.close();
  });

  it('add 会把尚无基线的父目录一并纳入版本控制（svn 语义）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await write('chars/hero/hero.psd', 'HERO');
    // 计数只算显式 add 的路径，祖先目录是隐式带上的
    expect(wc.add(['chars/hero/hero.psd'])).toBe(1);

    const before = await wc.status();
    expect(before.find((s) => s.path === 'chars')!.status).toBe('added');
    expect(before.find((s) => s.path === 'chars/hero')!.status).toBe('added');

    const out = await wc.commit({ message: 'add hero' });
    expect(out.rev).toBe(2);
    // 目录不进 changes——服务端 ensure_dirs 会自补，提交目录行只会换来重复条目
    const changes = [...srv.prepared.values()].at(-1)!;
    expect(changes.map((c) => c.path)).toEqual(['chars/hero/hero.psd']);
    expect(changes.every((c) => c.kind === 'file')).toBe(true);

    // 提交后目录也要有基线条目，否则下一次 status 又变成 unversioned
    const after = await wc.status();
    expect(after.every((s) => s.status === 'normal')).toBe(true);
    expect(after.find((s) => s.path === 'chars')!.kind).toBe('dir');
    wc.close();
  });

  it('只 add 空目录时提交抛 NO_CHANGES（空目录不单独入库）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await mkdir(join(dir, 'empty'), { recursive: true });
    expect(wc.add(['empty'])).toBe(1);
    expect((await wc.status()).find((s) => s.path === 'empty')!.status).toBe('added');

    await expect(wc.commit({ message: 'dir only' })).rejects.toMatchObject({ code: 'NO_CHANGES' });
    wc.close();
  });

  it('改动文件只上传新 blob（内容寻址去重）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    srv.put('b.psd', 'BBB');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    // b.psd 改成与 a.psd 相同内容 → 该 blob 服务端已有，不应重复上传
    await write('b.psd', 'AAA');
    expect((await wc.status()).find((s) => s.path === 'b.psd')!.status).toBe('modified');

    await wc.commit({ message: 'dedupe' });
    expect(srv.uploads).toEqual([]);
    expect(srv.files.get('b.psd')?.hash).toBe(hashBuffer('AAA'));
    wc.close();
  });

  it('无改动提交抛 NO_CHANGES', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await expect(wc.commit()).rejects.toMatchObject({ code: 'NO_CHANGES' });
    wc.close();
  });

  it('未 add 的新文件不进提交（unversioned 被跳过）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('scratch.psd', 'X');
    await expect(wc.commit()).rejects.toMatchObject({ code: 'NO_CHANGES' });
    wc.close();
  });

  it('删除提交：remove 后服务端条目消失，本地基线同步', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    srv.put('sub/b.png', 'BBB');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await wc.remove(['sub/b.png']);
    expect(existsSync(join(dir, 'sub', 'b.png'))).toBe(false);

    const out = await wc.commit({ message: 'rm' });
    expect(out.rev).toBe(2);
    expect(srv.files.has('sub/b.png')).toBe(false);
    expect(wc.wc.hasEntry('sub/b.png')).toBe(false);
    wc.close();
  });

  it('磁盘上直接删掉的文件（missing）提交时记为删除', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await rm(join(dir, 'a.psd')); // 没走 remove，直接删
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('missing');

    const out = await wc.commit({ message: 'deleted by os' });
    expect(out.rev).toBe(2);
    expect(srv.files.has('a.psd')).toBe(false);
    expect(wc.wc.hasEntry('a.psd')).toBe(false);
    wc.close();
  });

  it('基线过期（他人先提交）→ OUT_OF_DATE，本地不被改动', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await write('a.psd', 'MINE');
    srv.otherCommit([{ path: 'z.psd', op: 'add', kind: 'file', blob_hash: srv.seedBlob('Z'), size: 1 }]);

    await expect(wc.commit()).rejects.toMatchObject({ code: 'OUT_OF_DATE' });
    expect(wc.revision).toBe(1);
    expect(await read('a.psd')).toBe('MINE');
    wc.close();
  });

  it('响应丢失后重试：同一 commit_id 拿到 replayed，不产生重复提交', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await write('a.psd', 'V2');
    srv.loseCommitResponse = true; // 服务端已提交，但响应没回来

    const out = await wc.commit({ message: 'v2' });
    expect(out.replayed).toBe(true);
    expect(out.rev).toBe(2);
    expect(srv.headRev).toBe(2); // 关键：没有变成 3
    expect(wc.revision).toBe(2);
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('normal');
    wc.close();
  });

  it('add 非法路径抛 PATH_INVALID，且整批回滚', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('ok.psd', 'X');
    // add 是同步方法，直接抛而不是返回 rejected promise
    let caught: unknown;
    try {
      wc.add(['ok.psd', '../escape.psd']);
    } catch (e) {
      caught = e;
    }
    expect(caught).toBeInstanceOf(WcError);
    expect((caught as WcError).code).toBe('PATH_INVALID');
    expect(wc.wc.allPending()).toEqual([]); // 整批回滚
    wc.close();
  });
});

// ---------------------------------------------------------------- 更新

describe('update', () => {
  it('服务端新增/修改 → 落到本地，revision 前进', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    const h2 = srv.seedBlob('AAA2');
    srv.otherCommit([
      { path: 'a.psd', op: 'modify', kind: 'file', blob_hash: h2, size: 4 },
      { path: 'new/z.psd', op: 'add', kind: 'file', blob_hash: srv.seedBlob('ZZ'), size: 2 },
    ]);

    const out = await wc.update();
    expect(out.rev).toBe(2);
    expect(out.updated.sort()).toEqual(['a.psd', 'new/z.psd']);
    expect(await read('a.psd')).toBe('AAA2');
    expect(await read('new/z.psd')).toBe('ZZ');
    expect(wc.revision).toBe(2);
    expect((await wc.status()).every((s) => s.status === 'normal')).toBe(true);
    wc.close();
  });

  it('服务端删除 + 本地未改 → 文件被移除', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    srv.put('b.psd', 'BBB');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    srv.otherCommit([{ path: 'a.psd', op: 'delete', kind: 'file' }]);
    const out = await wc.update();

    expect(out.deleted).toEqual(['a.psd']);
    expect(existsSync(join(dir, 'a.psd'))).toBe(false);
    expect(wc.wc.hasEntry('a.psd')).toBe(false);
    wc.close();
  });

  it('双方都改 → 本地另存 .mine，工作文件取服务端版本，状态 conflicted', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await write('a.psd', 'MINE');
    srv.otherCommit([{ path: 'a.psd', op: 'modify', kind: 'file', blob_hash: srv.seedBlob('THEIRS'), size: 6 }]);

    const out = await wc.update();
    expect(out.conflicts).toEqual(['a.psd']);
    expect(await read('a.psd')).toBe('THEIRS');
    expect(await read('a.psd.mine')).toBe('MINE'); // 绝不静默丢本地改动
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('conflicted');

    await expect(wc.commit()).rejects.toMatchObject({ code: 'CONFLICT' });
    wc.close();
  });

  it('服务端删 + 本地已改 → 保留本地并标冲突', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await write('a.psd', 'MINE');
    srv.otherCommit([{ path: 'a.psd', op: 'delete', kind: 'file' }]);

    const out = await wc.update();
    expect(out.conflicts).toEqual(['a.psd']);
    expect(await read('a.psd')).toBe('MINE');
    wc.close();
  });

  it('已最新时 update 是空操作', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    const out = await wc.update();
    expect(out).toEqual({ rev: 1, updated: [], deleted: [], conflicts: [], skipped: [] });
    wc.close();
  });
});

// ---------------------------------------------------------------- 锁

describe('lock', () => {
  it('加锁落到本地 locks 表，解锁后清空', async () => {
    const srv = new FakeServer();
    srv.put('char/hero.psd', 'H');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    const info = await wc.lock('char/hero.psd', { comment: '改贴图' });
    expect(info.token).toBe('locktok');
    expect(wc.myLocks().map((l) => l.path)).toEqual(['char/hero.psd']);
    expect(await wc.listLocks()).toHaveLength(1);

    await wc.unlock('char/hero.psd');
    expect(wc.myLocks()).toEqual([]);
    expect(await wc.listLocks()).toEqual([]);
    wc.close();
  });

  it('他人持有的锁：409 LOCKED 原样抛到上层', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    srv.user = 'bob';
    await srv.api.acquireLock('art', { path: 'a.psd' });
    srv.user = 'alice';

    await expect(wc.lock('a.psd')).rejects.toMatchObject({ code: 'LOCKED', status: 409 });
    wc.close();
  });

  it('强制解锁需要 break 参数（服务端侧校验）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    srv.user = 'bob';
    await srv.api.acquireLock('art', { path: 'a.psd' });
    srv.user = 'alice';

    await expect(wc.unlock('a.psd')).rejects.toMatchObject({ code: 'LOCKED' });
    await wc.unlock('a.psd', { breakLock: true, reason: '人已离职' });
    expect(await wc.listLocks()).toEqual([]);
    wc.close();
  });

  // ---- v0.4.17：先锁后提交 + 客户端自动补锁（§5.2 / §6.5） ----

  it('服务端回 412 NEEDS_LOCK 时自动补锁并重试 prepare，一次点击完成提交', async () => {
    const srv = new FakeServer();
    srv.put('char/hero.psd', 'v1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('char/hero.psd', 'v2');
    srv.requireLock = true;

    const phases: string[] = [];
    const out = await wc.commit({ message: '', onProgress: (e) => phases.push(e.phase) });

    expect(out.rev).toBeGreaterThan(0);
    expect(out.committed).toContain('char/hero.psd');
    expect(phases).toContain('lock');
    // **提交成功即释放自动补的锁**：留着不放会让下一个想改这个文件的人加不上锁
    expect(srv.locks.has('char/hero.psd')).toBe(false);
    expect(wc.myLocks()).toEqual([]);
    wc.close();
  });

  it('提交后自动解锁：**手动加的锁**也一并释放（文件已经交上去了）', async () => {
    const srv = new FakeServer();
    srv.put('char/hero.psd', 'v1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await wc.lock('char/hero.psd', { comment: '手动持锁' });
    await write('char/hero.psd', 'v2');
    srv.requireLock = true;

    const out = await wc.commit({ message: '' });

    expect(out.rev).toBeGreaterThan(0);
    expect(srv.locks.has('char/hero.psd')).toBe(false);
    expect(wc.myLocks()).toEqual([]);
    wc.close();
  });

  it('提交只解锁"本次提交的文件"：锁着但没提交的不动', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    srv.put('b.psd', 'BBB');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await wc.lock('a.psd');
    await wc.lock('b.psd');
    await write('a.psd', 'AAA2'); // 只改 a
    srv.requireLock = true;

    const out = await wc.commit({ message: '' });

    expect(out.committed).toEqual(['a.psd']);
    expect(srv.locks.has('a.psd')).toBe(false); // 提交掉了 → 自动解锁
    expect(srv.locks.has('b.psd')).toBe(true); // 没提交 → 锁留着
    wc.close();
  });

  it('自动补锁撞上他人的锁：原样抛 LOCKED（带持锁人），不静默吞掉', async () => {
    const srv = new FakeServer();
    srv.put('char/hero.psd', 'v1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('char/hero.psd', 'v2');
    srv.user = 'bob';
    await srv.api.acquireLock('art', { path: 'char/hero.psd' });
    srv.user = 'alice';
    srv.requireLock = true;

    await expect(wc.commit({ message: '' })).rejects.toMatchObject({ code: 'LOCKED' });
    wc.close();
  });
});

// ---------------------------------------------------------------- 树上直接增删

describe('mkdir（§6.5 树上直接增删；新建文件已去掉）', () => {
  it('新建目录：落盘、且**不**进待提交集（目录由服务端 ensure_dirs 隐式补建）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await wc.mkdir('characters/new');
    expect(statSync(join(dir, 'characters', 'new')).isDirectory()).toBe(true);
    const st = await wc.status();
    const node = st.find((x) => x.path === 'characters/new');
    expect(node?.status).toBe('unversioned');
    expect(wc.myLocks()).toEqual([]);
    wc.close();
  });

  it('删除**未纳管**的新文件：只删文件，不产生待提交的删除', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('scratch.txt', 'scratch'); // 用户自己造的文件（版本工具不提供"新建文件"）

    await wc.remove(['scratch.txt']);

    expect(existsSync(join(dir, 'scratch.txt'))).toBe(false);
    // 关键：没有留下一条"待提交的删除"（服务端从没见过这个路径）——状态里干干净净
    const st = await wc.status();
    expect(st.find((x) => x.path === 'scratch.txt')).toBeUndefined();
    // 也因此没有可提交的变更
    await expect(wc.commit({ message: '' })).rejects.toMatchObject({ code: 'NO_CHANGES' });
    wc.close();
  });

  it('删除**已 add 待提交**的新文件：撤销那条 add，而不是记一条删除', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('scratch.txt', 'scratch');
    wc.add(['scratch.txt']);

    await wc.remove(['scratch.txt']);

    const st = await wc.status();
    expect(st.find((x) => x.path === 'scratch.txt')).toBeUndefined();
    await expect(wc.commit({ message: '' })).rejects.toMatchObject({ code: 'NO_CHANGES' });
    wc.close();
  });

  it('忽略规则按**副本**读写：写进 .b-artifactignore，清空即删除该文件', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    expect(wc.readIgnoreFile()).toBe(''); // 一开始没有规则

    await wc.writeIgnoreFile('*.tmp\nbuild/\n');
    expect(wc.readIgnoreFile()).toBe('*.tmp\nbuild/\n');
    expect(existsSync(join(dir, '.b-artifactignore'))).toBe(true);

    // 清空 = 删掉文件（留个空文件只会在树上多一条噪音）
    await wc.writeIgnoreFile('   \n');
    expect(existsSync(join(dir, '.b-artifactignore'))).toBe(false);
    wc.close();
  });

  it('忽略规则立即生效：命中的文件从「新增」变成「忽略」', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('scratch.tmp', 'noise');
    await write('keep.psd', 'KEEP');

    let st = await wc.status();
    expect(st.find((x) => x.path === 'scratch.tmp')?.status).toBe('unversioned');

    await wc.writeIgnoreFile('*.tmp\n');
    st = await wc.status();
    expect(st.find((x) => x.path === 'scratch.tmp')?.status).toBe('ignored');
    expect(st.find((x) => x.path === 'keep.psd')?.status).toBe('unversioned'); // 没命中的不受影响
    wc.close();
  });

  it('非法路径（穿越 / 空）被拦下', async () => {
    const srv = new FakeServer();
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await expect(wc.mkdir('../escape')).rejects.toMatchObject({ code: 'PATH_INVALID' });
    wc.close();
  });

  it('显式勾选的未纳管文件按"新增"提交（默认不勾，勾了才算）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('brand-new.psd', 'NEW');

    // 不传 paths：未纳管不进提交集
    await expect(wc.commit({ message: '' })).rejects.toMatchObject({ code: 'NO_CHANGES' });
    // 显式勾选：按新增提交。**只报 1 个文件**——目录由服务端隐式补建，
    // 把它算进 `committed` 会让"提交了 2 个文件"与事实不符。
    const out = await wc.commit({ message: '', paths: ['brand-new.psd'] });
    expect(out.committed).toEqual(['brand-new.psd']);
    // 提交后该文件有了基线，父目录也随之有了目录基线条目
    const st = await wc.status();
    expect(st.find((x) => x.path === 'brand-new.psd')?.status).toBe('normal');
    wc.close();
  });
});

// ---------------------------------------------------------------- 进程锁

describe('进程锁', () => {
  it('close 后锁释放，可以重新打开', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const root = join(dir, 'w');
    const w1 = await WorkingCopy.checkout({ root, client: srv.api, repo: 'art', cacheDir });
    w1.close();
    const w2 = WorkingCopy.open({ root, client: srv.api, repo: 'art', cacheDir });
    expect(w2.revision).toBe(1);
    w2.close();
  });

  it('同进程连续 checkout → open 不被自己的锁挡住', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const root = join(dir, 'w');
    const w1 = await WorkingCopy.checkout({ root, client: srv.api, repo: 'art', cacheDir });
    // 还没 close 就再开一次：同 pid 不是真冲突（db 层引用计数保证）
    const w2 = WorkingCopy.open({ root, client: srv.api, repo: 'art', cacheDir });
    expect(w2.revision).toBe(1);
    w2.close();
    expect(existsSync(join(root, '.b-artifact', 'wc.lock'))).toBe(true); // 外层仍持有
    w1.close();
    expect(existsSync(join(root, '.b-artifact', 'wc.lock'))).toBe(false);
  });
});

describe('错误类型', () => {
  it('WcError 与 ApiError 类型可用且可区分', () => {
    const w = new WcError('CONFLICT', 'x');
    const a = new ApiError({ status: 500, code: 'INTERNAL', message: 'y' });
    expect(w.name).toBe('WcError');
    expect(a.name).toBe('ApiError');
    expect(w instanceof WcError).toBe(true);
    expect(a instanceof WcError).toBe(false);
  });
});

// ---------------------------------------------------------------- 冲突解决（§6.5）

describe('冲突登记与三方对比', () => {
  /** 造一个"双方都改"的冲突工作副本。 */
  async function bothModified(srv: FakeServer, wc: Awaited<ReturnType<typeof WorkingCopy.checkout>>) {
    await write('a.psd', 'MINE');
    srv.otherCommit([{ path: 'a.psd', op: 'modify', kind: 'file', blob_hash: srv.seedBlob('THEIRS'), size: 6 }]);
    await wc.update();
  }

  it('双方都改 → conflicts() 给出 both-modified，且三方内容都能读到', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'BASE-1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await bothModified(srv, wc);

    const list = await wc.conflicts();
    expect(list).toHaveLength(1);
    const c = list[0]!;
    expect(c.path).toBe('a.psd');
    expect(c.reason).toBe('both-modified');
    expect(c.has_mine).toBe(true);
    expect(c.has_theirs).toBe(true);
    expect(c.mergeable).toBe(true);
    expect(c.sides).toEqual({ base: 'text', mine: 'text', theirs: 'text' });

    const sides = await wc.conflictSides('a.psd');
    expect(sides.base.text).toBe('BASE-1'); // 共同祖先（冲突前的基线）
    expect(sides.mine.text).toBe('MINE');
    expect(sides.theirs.text).toBe('THEIRS');
    wc.close();
  });

  it('共同祖先的哈希必须被留住（entry.base_hash 已被服务端版本覆盖）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'BASE-1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await bothModified(srv, wc);

    // 这正是要单独建 conflicts 表的原因：base 不能从 entries 里读
    expect(wc.wc.getEntry('a.psd')!.base_hash).toBe(hashBuffer('THEIRS'));
    expect(wc.wc.getConflict('a.psd')!.base_hash).toBe(hashBuffer('BASE-1'));
    wc.close();
  });

  it('没有冲突时列表为空，读不存在路径的三方内容抛 NOT_FOUND', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    expect(await wc.conflicts()).toEqual([]);
    await expect(wc.conflictSides('a.psd')).rejects.toMatchObject({ code: 'NOT_FOUND' });
    wc.close();
  });

  it('二进制冲突不可合并（美术资源库的常态）', async () => {
    const srv = new FakeServer();
    const bin = (s: string) => `\u0000\u0001BIN-${s}`; // 含 NUL 即视为二进制
    srv.put('hero.psd', bin('base'));
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await write('hero.psd', bin('mine'));
    srv.otherCommit([
      { path: 'hero.psd', op: 'modify', kind: 'file', blob_hash: srv.seedBlob(bin('theirs')), size: 12 },
    ]);
    await wc.update();

    const c = (await wc.conflicts())[0]!;
    expect(c.mergeable).toBe(false);
    expect(c.sides.mine).toBe('binary');
    expect(c.sides.theirs).toBe('binary');
    expect((await wc.conflictSides('hero.psd')).mine.text).toBeNull();
    wc.close();
  });

  it('服务端删除 + 本地改 → deleted-remotely，没有服务端内容可对比', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'BASE-1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });

    await write('a.psd', 'MINE');
    srv.otherCommit([{ path: 'a.psd', op: 'delete', kind: 'file' }]);
    await wc.update();

    const c = (await wc.conflicts())[0]!;
    expect(c.reason).toBe('deleted-remotely');
    expect(c.has_theirs).toBe(false);
    expect(c.mergeable).toBe(false);
    expect(c.sides.theirs).toBe('missing');

    const sides = await wc.conflictSides('a.psd');
    expect(sides.mine.text).toBe('MINE'); // 工作文件本身就是本地版本
    expect(sides.base.text).toBe('BASE-1');
    wc.close();
  });
});

describe('冲突解决', () => {
  async function makeConflict(content = { base: 'BASE-1', mine: 'MINE', theirs: 'THEIRS' }) {
    const srv = new FakeServer();
    srv.put('a.psd', content.base);
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('a.psd', content.mine);
    srv.otherCommit([
      { path: 'a.psd', op: 'modify', kind: 'file', blob_hash: srv.seedBlob(content.theirs), size: content.theirs.length },
    ]);
    await wc.update();
    return { srv, wc };
  }

  it('取服务端版本 → 回到 normal，`.mine` 与冲突登记都清掉', async () => {
    const { wc } = await makeConflict();
    await wc.resolveConflict('a.psd', { choice: 'theirs' });

    expect(await read('a.psd')).toBe('THEIRS');
    expect(existsSync(join(dir, 'a.psd.mine'))).toBe(false);
    expect(await wc.conflicts()).toEqual([]);
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('normal');
    wc.close();
  });

  it('取本地版本 → 工作文件变成本地内容，状态 modified，可以直接提交', async () => {
    const { wc } = await makeConflict();
    await wc.resolveConflict('a.psd', { choice: 'mine' });

    expect(await read('a.psd')).toBe('MINE');
    expect(existsSync(join(dir, 'a.psd.mine'))).toBe(false);
    expect(await wc.conflicts()).toEqual([]);
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('modified');

    const out = await wc.commit({ message: 'keep mine' });
    expect(out.committed).toEqual(['a.psd']);
    wc.close();
  });

  it('写入合并结果 → 状态 modified，提交内容就是合并结果', async () => {
    const { wc } = await makeConflict();
    await wc.resolveConflict('a.psd', { choice: 'merged', content: 'MERGED' });

    expect(await read('a.psd')).toBe('MERGED');
    expect(existsSync(join(dir, 'a.psd.mine'))).toBe(false);
    expect(await wc.conflicts()).toEqual([]);
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('modified');
    wc.close();
  });

  it('merged 必须带内容（空合并结果是误用）', async () => {
    const { wc } = await makeConflict();
    await expect(wc.resolveConflict('a.psd', { choice: 'merged' })).rejects.toMatchObject({ code: 'IO' });
    wc.close();
  });

  it('`.mine` 被删掉后再取本地版本 → 明确报错，而不是悄悄解决', async () => {
    const { wc } = await makeConflict();
    await rm(join(dir, 'a.psd.mine'), { force: true });
    await expect(wc.resolveConflict('a.psd', { choice: 'mine' })).rejects.toMatchObject({
      code: 'NOT_FOUND',
    });
    wc.close();
  });

  it('解决不存在的冲突 → NOT_FOUND', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'AAA');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await expect(wc.resolveConflict('a.psd', { choice: 'theirs' })).rejects.toMatchObject({
      code: 'NOT_FOUND',
    });
    wc.close();
  });

  it('服务端已删除时：取服务端 = 接受删除（文件与登记一起消失）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'BASE-1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('a.psd', 'MINE');
    srv.otherCommit([{ path: 'a.psd', op: 'delete', kind: 'file' }]);
    await wc.update();

    await wc.resolveConflict('a.psd', { choice: 'theirs' });
    expect(existsSync(join(dir, 'a.psd'))).toBe(false);
    expect(wc.wc.getEntry('a.psd')).toBeUndefined();
    expect(await wc.conflicts()).toEqual([]);
    wc.close();
  });

  it('服务端已删除时：取本地 = 退掉基线并重新纳入（提交时是 add）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'BASE-1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('a.psd', 'MINE');
    srv.otherCommit([{ path: 'a.psd', op: 'delete', kind: 'file' }]);
    await wc.update();

    await wc.resolveConflict('a.psd', { choice: 'mine' });
    expect(await read('a.psd')).toBe('MINE');
    expect(wc.wc.getEntry('a.psd')).toBeUndefined(); // 基线已退掉
    expect(wc.wc.pendingOp('a.psd')).toBe('add'); // 重新纳入，提交 op=add
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('added');

    const out = await wc.commit({ message: 'resurrect' });
    expect(out.committed).toEqual(['a.psd']);
    expect(srv.files.get('a.psd')!.hash).toBe(hashBuffer('MINE'));
    wc.close();
  });

  it('服务端已删除时不允许"合并"（无从合并）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'BASE-1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('a.psd', 'MINE');
    srv.otherCommit([{ path: 'a.psd', op: 'delete', kind: 'file' }]);
    await wc.update();

    await expect(
      wc.resolveConflict('a.psd', { choice: 'merged', content: 'X' }),
    ).rejects.toMatchObject({ code: 'CONFLICT' });
    wc.close();
  });

  it('revert 也解除冲突（并清掉 `.mine`）', async () => {
    const srv = new FakeServer();
    srv.put('a.psd', 'BASE-1');
    const wc = await WorkingCopy.checkout({ root: dir, client: srv.api, repo: 'art', cacheDir });
    await write('a.psd', 'MINE');
    srv.otherCommit([
      { path: 'a.psd', op: 'modify', kind: 'file', blob_hash: srv.seedBlob('THEIRS'), size: 6 },
    ]);
    await wc.update();
    expect(await wc.conflicts()).toHaveLength(1);

    await wc.revert(['a.psd']);
    expect(await wc.conflicts()).toEqual([]);
    expect(existsSync(join(dir, 'a.psd.mine'))).toBe(false);
    expect((await wc.status()).find((s) => s.path === 'a.psd')!.status).toBe('normal');
    wc.close();
  });

  it('解决后可以正常提交（不再被 CONFLICT 挡住）', async () => {
    const { wc } = await makeConflict();
    await expect(wc.commit()).rejects.toMatchObject({ code: 'CONFLICT' });

    await wc.resolveConflict('a.psd', { choice: 'mine' });
    const out = await wc.commit({ message: 'resolved' });
    expect(out.committed).toEqual(['a.psd']);
    expect(wc.revision).toBe(out.rev);
    wc.close();
  });
});
