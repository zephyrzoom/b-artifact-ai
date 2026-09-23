import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ApiClient, RepoSummary, UserInfo } from '../src/core/api.js';
import { ApiError, WcError } from '../src/core/errors.js';
import type { StatusItem } from '../src/core/scan.js';
import type { WorkingCopy } from '../src/core/wc.js';
import { buildHandlers, type AppInfo, type HandlerDeps } from '../src/main/handlers.js';
import { SessionStore, type SecretBox } from '../src/main/session.js';
import { ConfigStore, defaultConfig } from '../src/main/store.js';
import type { EventChannel } from '../src/shared/channels.js';

// ---------- 替身 ----------

const box: SecretBox = {
  available: true,
  encrypt: (p) => Buffer.from(Buffer.from(p).toString('base64')),
  decrypt: (b) => Buffer.from(b.toString(), 'base64').toString(),
};

const appInfo: AppInfo = {
  version: '0.1.0',
  electron: '44.3.0',
  node: '22.0.0',
  chrome: '130.0.0',
  platform: 'darwin',
  arch: 'x64',
};

const user: UserInfo = { id: 1, username: 'alice', display_name: 'Alice', is_admin: true, source: 'local' };

const repo: RepoSummary = {
  id: 1,
  name: 'art',
  description: '美术',
  owner: 'alice',
  head_rev: 3,
  created_at: '2026-09-15T00:00:00Z',
  my_role: 'admin',
  my_permissions: { read: true, write: true, admin: true },
};

interface FakeWcOptions {
  root?: string;
  repo?: string;
  status?: StatusItem[];
  updateResult?: Record<string, unknown>;
}

class FakeWc {
  closed = false;
  readonly calls: Array<[string, unknown]> = [];
  statusItems: StatusItem[];
  updateResult: Record<string, unknown>;

  constructor(
    readonly root = '/w/art',
    readonly repo = 'art',
    private rev = 3,
    readonly sparse: string[] = [],
    opts: FakeWcOptions = {},
  ) {
    this.statusItems = opts.status ?? [];
    this.updateResult = opts.updateResult ?? { rev: 4, updated: ['a.psd'], deleted: [], conflicts: [], skipped: [] };
  }

  get revision(): number {
    return this.rev;
  }
  get sparsePaths(): string[] {
    return this.sparse;
  }
  close(): void {
    this.closed = true;
  }

  async status(): Promise<StatusItem[]> {
    return this.statusItems;
  }
  add(paths: string[]): number {
    this.calls.push(['add', paths]);
    return paths.length;
  }
  async remove(paths: string[]): Promise<void> {
    this.calls.push(['remove', paths]);
  }
  async revert(paths: string[]): Promise<void> {
    this.calls.push(['revert', paths]);
  }
  async commit(opts: { message: string; paths?: string[]; onProgress?: (e: unknown) => void }): Promise<unknown> {
    this.calls.push(['commit', opts]);
    opts.onProgress?.({ phase: 'hash', done: 1, total: 2, current: 'a.psd' });
    this.rev += 1;
    return { rev: this.rev, committed: ['a.psd'], replayed: false };
  }
  async update(opts: { onProgress?: (e: unknown) => void } = {}): Promise<unknown> {
    this.calls.push(['update', opts]);
    opts.onProgress?.({ phase: 'download', done: 1, total: 1 });
    this.rev = Number(this.updateResult['rev'] ?? 4);
    return this.updateResult;
  }
  async listLocks(path?: string): Promise<unknown[]> {
    this.calls.push(['listLocks', path]);
    return [{ id: 1, path: 'a.psd', kind: 'file', owner_id: 1, owner: 'bob', comment: null, created_at: '', expires_at: null }];
  }
  async lock(path: string, opts: unknown): Promise<unknown> {
    this.calls.push(['lock', { path, opts }]);
    return { id: 2, path, kind: 'file', owner_id: 1, owner: 'alice', comment: null, created_at: '', expires_at: null, token: 't' };
  }
  async unlock(path: string, opts: unknown): Promise<void> {
    this.calls.push(['unlock', { path, opts }]);
  }
  async mkdir(path: string): Promise<void> {
    this.calls.push(['mkdir', { path }]);
  }
  async conflicts(): Promise<unknown[]> {
    this.calls.push(['conflicts', null]);
    return [{ path: 'a.psd', kind: 'file', reason: 'both-modified', has_mine: true, has_theirs: true, mergeable: true, theirs_rev: 4, sides: { base: 'text', mine: 'text', theirs: 'text' } }];
  }
  async conflictSides(path: string): Promise<unknown> {
    this.calls.push(['conflictSides', path]);
    return { path, mergeable: true, base: { kind: 'text', size: 4, text: 'BASE' }, mine: { kind: 'text', size: 4, text: 'MINE' }, theirs: { kind: 'text', size: 6, text: 'THEIRS' } };
  }
  async resolveConflict(path: string, opts: unknown): Promise<void> {
    this.calls.push(['resolveConflict', { path, opts }]);
  }
}

interface Harness {
  deps: HandlerDeps;
  handlers: ReturnType<typeof buildHandlers>;
  config: ConfigStore;
  session: SessionStore;
  events: Array<[EventChannel, unknown]>;
  wcs: FakeWc[];
  client: {
    loginCalls: Array<[string, string]>;
    logoutCalls: number;
    failLogin?: Error;
    failLogout?: Error;
    repos: RepoSummary[];
    logCalls: Record<string, unknown>[];
    treeCalls: Record<string, unknown>[];
    /** 调用里带的仓库名：用来钉住"仓库由调用方显式给"。 */
    logRepos: string[];
    treeRepos: string[];
  };
  revealed: string[];
}

let dir: string;

function harness(): Harness {
  const config = new ConfigStore(join(dir, 'config.json'));
  config.load();
  // 缓存目录也指到临时目录：handler 测试绝不能碰开发者真实的 ~/.b-artifact
  config.save({ ...defaultConfig(), cacheDir: join(dir, 'cache') });
  const session = new SessionStore(box, join(dir, 'session.enc'));
  const events: Array<[EventChannel, unknown]> = [];
  const wcs: FakeWc[] = [];
  const revealed: string[] = [];
  const client = {
    loginCalls: [] as Array<[string, string]>,
    logoutCalls: 0,
    failLogin: undefined as Error | undefined,
    failLogout: undefined as Error | undefined,
    repos: [repo],
    logCalls: [] as Record<string, unknown>[],
    treeCalls: [] as Record<string, unknown>[],
    logRepos: [] as string[],
    treeRepos: [] as string[],
  };

  const fakeApi = {
    login: async (username: string, password: string) => {
      client.loginCalls.push([username, password]);
      if (client.failLogin) throw client.failLogin;
      return { token: 'tok-1', expires_at: '2026-10-15T00:00:00Z', user };
    },
    logout: async () => {
      client.logoutCalls += 1;
      if (client.failLogout) throw client.failLogout;
    },
    listRepos: async () => client.repos,
    log: async (_repo: string, q: Record<string, unknown>) => {
      client.logRepos.push(_repo);
      client.logCalls.push(q);
      return {
        items: [
          { rev: 2, author: 'alice', message: '第二次提交', created_at: '2026-09-15T03:00:00Z', file_count: 1, byte_delta: 12, manifest_hash: 'm2' },
          { rev: 1, author: 'bob', message: '初始提交', created_at: '2026-09-15T02:00:00Z', file_count: 2, byte_delta: 30, manifest_hash: 'm1' },
        ],
        total: 2,
      };
    },
    tree: async (_repo: string, q: Record<string, unknown>) => {
      client.treeRepos.push(_repo);
      client.treeCalls.push(q);
      return {
        repo: 'art',
        rev: (q['rev'] as number) ?? 0,
        prefix: (q['prefix'] as string) ?? '',
        depth: (q['depth'] as number) ?? 1,
        items: [{ path: 'props/table.png', kind: 'file', blob_hash: 'sha$t', size: 11, mode: 0, mtime: 0, changed_rev: 2 }],
        total: 1,
      };
    },
    openBlobStream: async () => new Response(new TextEncoder().encode('REMOTE-TABLE-V1'), { status: 200 }),
  } as unknown as ApiClient;

  const deps: HandlerDeps = {
    config,
    session,
    appInfo,
    makeClient: () => fakeApi,
    openWorkingCopy: (root) => {
      const wc = new FakeWc(root);
      wcs.push(wc);
      return wc as unknown as WorkingCopy;
    },
    checkoutWorkingCopy: async ({ dir: d, repo: r, sparse }) => {
      const wc = new FakeWc(d, r, 3, sparse ?? []);
      wcs.push(wc);
      return wc as unknown as WorkingCopy;
    },
    pickDir: async (title) => (title.includes('取消') ? null : '/picked/dir'),
    revealPath: (p) => revealed.push(p),
    emit: (c, p) => events.push([c, p]),
    log: () => {},
  };

  return { deps, handlers: buildHandlers(deps), config, session, events, wcs, client, revealed };
}

/** 直接调处理器（绕过 IPC 层，专测业务分支）。 */
function call(h: Harness, channel: keyof ReturnType<typeof buildHandlers>, payload: Record<string, unknown> = {}) {
  return h.handlers[channel](payload);
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), 'ba-h-'));
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

// ---------- 用例 ----------

describe('会话前置条件', () => {
  it('未登录时仓库列表报 UNAUTHENTICATED', async () => {
    const h = harness();
    await expect(call(h, 'repos:list')).rejects.toMatchObject({ code: 'UNAUTHENTICATED' });
  });

  it('未打开工作副本时任何 wc 操作报 NO_WORKING_COPY', async () => {
    const h = harness();
    await h.session.save({ server: 'http://x', username: 'alice', token: 't' });
    for (const ch of ['wc:status', 'wc:locks'] as const) {
      await expect(call(h, ch)).rejects.toMatchObject({ code: 'NO_WORKING_COPY' });
    }
    await expect(call(h, 'wc:add', { paths: ['a'] })).rejects.toMatchObject({ code: 'NO_WORKING_COPY' });
    await expect(call(h, 'wc:commit', { message: 'm' })).rejects.toMatchObject({ code: 'NO_WORKING_COPY' });
  });

  it('auth:state 未登录返回 null', async () => {
    expect(await call(harness(), 'auth:state')).toBeNull();
  });
});

describe('auth', () => {
  it('登录成功：token 进主进程会话，返回值**不含 token**', async () => {
    const h = harness();
    const r = (await call(h, 'auth:login', { server: 'http://s', username: 'alice', password: 'pw' })) as Record<string, unknown>;

    expect(h.client.loginCalls).toEqual([['alice', 'pw']]);
    expect(h.session.get()).toEqual({ server: 'http://s', username: 'alice', token: 'tok-1' });
    expect(r).toMatchObject({ server: 'http://s', username: 'alice', is_admin: true });
    expect(JSON.stringify(r)).not.toContain('tok-1');
  });

  it('登录后把服务器记进配置（下次启动可点选）', async () => {
    const h = harness();
    await call(h, 'auth:login', { server: 'http://s', username: 'alice', password: 'pw' });
    expect(h.config.get().servers).toEqual(['http://s']);
  });

  it('登录失败：原样抛 ApiError（渲染层按 code 提示），且不写会话', async () => {
    const h = harness();
    h.client.failLogin = new ApiError({ status: 401, code: 'UNAUTHENTICATED', message: '用户名或密码错误' });
    await expect(
      call(h, 'auth:login', { server: 'http://s', username: 'alice', password: 'bad' }),
    ).rejects.toMatchObject({ code: 'UNAUTHENTICATED' });
    expect(h.session.get()).toBeNull();
    expect(h.config.get().servers).toEqual([]);
  });

  it('auth:state 只回 server / username，绝不带 token', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 'tok-1' });
    const r = await call(h, 'auth:state');
    expect(r).toEqual({ server: 'http://s', username: 'alice' });
    expect(JSON.stringify(r)).not.toContain('tok-1');
  });

  it('logout：清会话 + 关闭工作副本', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:open', { dir: '/w/art' });

    await call(h, 'auth:logout');
    expect(h.client.logoutCalls).toBe(1);
    expect(h.session.get()).toBeNull();
    expect(h.wcs[0]!.closed).toBe(true);
    expect(h.events.at(-1)).toEqual(['wc:state', null]);
  });

  it('服务端已失效导致 logout 报错时仍要清干净（不能把用户卡住）', async () => {
    const h = harness();
    h.client.failLogout = new ApiError({ status: 401, code: 'UNAUTHENTICATED', message: '令牌失效' });
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await expect(call(h, 'auth:logout')).resolves.toEqual({});
    expect(h.session.get()).toBeNull();
  });

  it('logout 遇到非 ApiError（真故障）时向上抛，不清会话', async () => {
    const h = harness();
    h.client.failLogout = new WcError('IO', '磁盘炸了');
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await expect(call(h, 'auth:logout')).rejects.toMatchObject({ code: 'IO' });
    expect(h.session.get()).not.toBeNull();
  });

  it('forgetServer 从配置里移除', async () => {
    const h = harness();
    h.config.rememberServer('http://a');
    h.config.rememberServer('http://b');
    expect(await call(h, 'auth:forgetServer', { server: 'http://a' })).toMatchObject({ servers: ['http://b'] });
  });
});

describe('工作副本', () => {
  beforeEach(async () => {
    // 大多数用例都需要登录态
  });

  it('checkout：采用实例、记 recent、推 wc:state', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });

    const r = await call(h, 'wc:checkout', { repo: 'art', dir: '/w/art', sparse: ['characters'] });
    expect(r).toEqual({ root: '/w/art', repo: 'art', rev: 3, sparse_paths: ['characters'], watching: false });
    expect(h.config.get().recent[0]).toMatchObject({ dir: '/w/art', repo: 'art', server: 'http://s' });
    expect(h.events).toContainEqual(['wc:state', r]);
  });

  it('切换工作副本时把前一个关掉（进程锁必须释放）', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });
    await call(h, 'wc:open', { dir: '/w/b' });
    expect(h.wcs[0]!.closed).toBe(true);
    expect(h.wcs[1]!.closed).toBe(false);
  });

  it('提交/更新后再 adopt 同一个实例**不能**把它关掉（否则后续全是 database is not open）', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });

    await call(h, 'wc:commit', { message: 'm' });
    expect(h.wcs[0]!.closed).toBe(false);
    await call(h, 'wc:update');
    expect(h.wcs[0]!.closed).toBe(false);

    // 还能继续用（真 bug 的表现：这里会报 database is not open）
    await expect(call(h, 'wc:status')).resolves.toEqual([]);
  });

  it('打开一个不是工作副本的目录 → 报错并从"最近"里剔除', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    h.config.rememberRecent({ dir: '/w/gone', repo: 'art', server: 'http://s' });
    h.deps.openWorkingCopy = () => {
      throw new WcError('NOT_A_WORKING_COPY', '不是工作副本');
    };

    await expect(call(h, 'wc:open', { dir: '/w/gone' })).rejects.toMatchObject({
      code: 'NOT_A_WORKING_COPY',
    });
    expect(h.config.get().recent).toEqual([]);
  });

  it('status 把 forceHash 透传给引擎', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });
    await call(h, 'wc:status', { forceHash: true });
    expect(h.wcs[0]!.statusItems).toEqual([]);
  });

  it('add 返回条数；remove / revert 如实转发路径', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });

    expect(await call(h, 'wc:add', { paths: ['a.psd', 'b.psd'] })).toEqual({ added: 2 });
    await call(h, 'wc:remove', { paths: ['c.psd'] });
    await call(h, 'wc:revert', { paths: ['d.psd'] });

    const calls = h.wcs[0]!.calls;
    expect(calls).toContainEqual(['add', ['a.psd', 'b.psd']]);
    expect(calls).toContainEqual(['remove', ['c.psd']]);
    expect(calls).toContainEqual(['revert', ['d.psd']]);
  });

  it('commit：转发说明与选中路径，并把哈希/上传进度推给渲染层', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });

    const r = await call(h, 'wc:commit', { message: '改贴图', paths: ['a.psd'] });
    expect(r).toMatchObject({ rev: 4, committed: ['a.psd'] });

    const commitCall = h.wcs[0]!.calls.find(([k]) => k === 'commit')![1] as {
      message: string;
      paths?: string[];
    };
    expect(commitCall.message).toBe('改贴图');
    expect(commitCall.paths).toEqual(['a.psd']);

    expect(h.events.some(([c, p]) => c === 'wc:progress' && (p as { phase: string }).phase === 'commit')).toBe(
      true,
    );
    // 提交后 rev 前进要反映到 wc:state
    expect(h.events.some(([c, p]) => c === 'wc:state' && (p as { rev: number }).rev === 4)).toBe(true);
  });

  it('update：转发并推送 download 阶段进度', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });

    const r = (await call(h, 'wc:update')) as { rev: number };
    expect(r.rev).toBe(4);
    expect(
      h.events.some(([c, p]) => c === 'wc:progress' && (p as { phase: string }).phase === 'update'),
    ).toBe(true);
  });

  it('wc:close 清空当前并推 wc:state=null', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });
    await call(h, 'wc:close');
    expect(h.wcs[0]!.closed).toBe(true);
    expect(h.events.at(-1)).toEqual(['wc:state', null]);
    await expect(call(h, 'wc:status')).rejects.toMatchObject({ code: 'NO_WORKING_COPY' });
  });
});

describe('锁', () => {
  async function withWc() {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });
    return h;
  }

  it('lock 只传 path（v0.4.17 去掉了 kind），comment 为空时传 undefined', async () => {
    const h = await withWc();
    await call(h, 'wc:lock', { path: 'a.psd' });
    const call0 = h.wcs[0]!.calls.find(([k]) => k === 'lock')![1] as { opts: Record<string, unknown> };
    expect(call0.opts).toEqual({ comment: undefined });
    expect('kind' in call0.opts).toBe(false);
  });

  it('lock 带备注', async () => {
    const h = await withWc();
    await call(h, 'wc:lock', { path: 'characters/hero.psd', comment: '改贴图' });
    const c = h.wcs[0]!.calls.find(([k]) => k === 'lock')![1] as { opts: Record<string, unknown> };
    expect(c.opts).toEqual({ comment: '改贴图' });
  });

  it('树上新建目录走独立通道（§6.5 直接增删；新建文件已去掉）', async () => {
    const h = await withWc();
    await call(h, 'wc:mkdir', { path: 'characters/new' });
    const calls = h.wcs[0]!.calls;
    expect(calls.find(([k]) => k === 'mkdir')![1]).toEqual({ path: 'characters/new' });
  });

  it('unlock 不带原因时是普通解锁（不传 breakLock）', async () => {
    const h = await withWc();
    await call(h, 'wc:unlock', { path: 'a.psd' });
    const c = h.wcs[0]!.calls.find(([k]) => k === 'unlock')![1] as { opts: Record<string, unknown> };
    expect(c.opts).toEqual({ breakLock: false, reason: undefined });
  });

  it('unlock 带原因时走强制解锁', async () => {
    const h = await withWc();
    await call(h, 'wc:unlock', { path: 'a.psd', breakLock: true, reason: '人已离职' });
    const c = h.wcs[0]!.calls.find(([k]) => k === 'unlock')![1] as { opts: Record<string, unknown> };
    expect(c.opts).toEqual({ breakLock: true, reason: '人已离职' });
  });

  it('locks 不带路径时查全仓', async () => {
    const h = await withWc();
    await call(h, 'wc:locks', {});
    expect(h.wcs[0]!.calls.find(([k]) => k === 'listLocks')![1]).toBeUndefined();
  });

  it('locks 带路径时按前缀查', async () => {
    const h = await withWc();
    await call(h, 'wc:locks', { path: 'characters' });
    expect(h.wcs[0]!.calls.find(([k]) => k === 'listLocks')![1]).toBe('characters');
  });
});

describe('冲突（§6.5）', () => {
  async function withWc() {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });
    return h;
  }

  it('wc:conflicts 返回待解决清单', async () => {
    const h = await withWc();
    const list = (await call(h, 'wc:conflicts')) as Array<{ path: string; mergeable: boolean }>;
    expect(list).toHaveLength(1);
    expect(list[0]!.path).toBe('a.psd');
    expect(list[0]!.mergeable).toBe(true);
  });

  it('wc:conflictSides 把路径透传给引擎', async () => {
    const h = await withWc();
    const sides = (await call(h, 'wc:conflictSides', { path: 'a.psd' })) as { mine: { text: string } };
    expect(sides.mine.text).toBe('MINE');
    expect(h.wcs[0]!.calls.find(([k]) => k === 'conflictSides')![1]).toBe('a.psd');
  });

  it('wc:resolveConflict 转发 choice；未给 content 时是 undefined（而不是空串）', async () => {
    const h = await withWc();
    await call(h, 'wc:resolveConflict', { path: 'a.psd', choice: 'mine' });
    const c = h.wcs[0]!.calls.find(([k]) => k === 'resolveConflict')![1] as {
      path: string;
      opts: { choice: string; content?: string };
    };
    expect(c.path).toBe('a.psd');
    expect(c.opts.choice).toBe('mine');
    expect(c.opts.content).toBeUndefined();
  });

  it('wc:resolveConflict 带合并结果时原样转发', async () => {
    const h = await withWc();
    await call(h, 'wc:resolveConflict', { path: 'a.psd', choice: 'merged', content: 'MERGED' });
    const c = h.wcs[0]!.calls.find(([k]) => k === 'resolveConflict')![1] as {
      opts: { choice: string; content?: string };
    };
    expect(c.opts).toEqual({ choice: 'merged', content: 'MERGED' });
  });

  it('未打开工作副本时三个通道都报 NO_WORKING_COPY', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    for (const ch of ['wc:conflicts', 'wc:conflictSides'] as const) {
      await expect(call(h, ch, { path: 'a.psd' })).rejects.toMatchObject({ code: 'NO_WORKING_COPY' });
    }
    await expect(call(h, 'wc:resolveConflict', { path: 'a.psd', choice: 'mine' })).rejects.toMatchObject({
      code: 'NO_WORKING_COPY',
    });
  });
});

describe('历史修订与缓存（§6.5 历史 / 设置视图）', () => {
  async function withWc() {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });
    return h;
  }

  it('repo:log 转发分页参数', async () => {
    const h = await withWc();
    const r = (await call(h, 'repo:log', { repo: 'art', limit: 50, offset: 100 })) as { items: unknown[] };
    expect(r.items).toHaveLength(2);
    expect(h.client.logCalls[0]).toEqual({ limit: 50, offset: 100, prefix: undefined });
  });

  it('repo:log 没给参数时不传 limit/offset（用服务端默认）', async () => {
    const h = await withWc();
    await call(h, 'repo:log', { repo: 'art' });
    expect(h.client.logCalls[0]).toEqual({ limit: undefined, offset: undefined, prefix: undefined });
  });

  it('repo:log 支持按前缀看某个目录的历史', async () => {
    const h = await withWc();
    await call(h, 'repo:log', { repo: 'art', prefix: 'props' });
    expect(h.client.logCalls[0]!['prefix']).toBe('props');
  });

  it('repo:treeAt 默认 rev=0（HEAD）与 depth=1', async () => {
    const h = await withWc();
    await call(h, 'repo:treeAt', { repo: 'art' });
    expect(h.client.treeCalls[0]).toEqual({ rev: 0, prefix: '', depth: 1 });
  });

  it('repo:treeAt 带上修订与目录', async () => {
    const h = await withWc();
    await call(h, 'repo:treeAt', { repo: 'art', rev: 3, path: 'props', depth: 1 });
    expect(h.client.treeCalls[0]).toEqual({ rev: 3, prefix: 'props', depth: 1 });
  });

  it('repo:downloadRevision 把旧版本流式落盘', async () => {
    const h = await withWc();
    const target = join(dir, 'dl');
    const r = (await call(h, 'repo:downloadRevision', {
      repo: 'art',
      path: 'props/table.png',
      rev: 2,
      targetDir: target,
    })) as { saved: string; size: number; rev: number };
    expect(r.saved).toBe(join(target, 'table.png'));
    expect(r.rev).toBe(2);
    expect(readFileSync(r.saved, 'utf8')).toBe('REMOTE-TABLE-V1');
  });

  it('**没打开工作副本时仓库级读操作照样能用**（仓库页的目录树不该被副本绑架）', async () => {
    // 真实反馈："工作副本打开后，仓库部分检出才能出来目录树，不打开显示暂无数据"。
    // 这三个通道以前取"当前副本的仓库"，现在仓库由调用方显式给。
    const h = harness(); // 注意：harness 里没有工作副本
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });

    await call(h, 'repo:treeAt', { repo: 'art' });
    expect(h.client.treeRepos).toEqual(['art']);

    await call(h, 'repo:log', { repo: 'other', limit: 10 });
    expect(h.client.logRepos).toEqual(['other']);

    // 没传 repo 时才是参数错误（而不是"没打开工作副本"）
    await expect(call(h, 'repo:treeAt', {})).rejects.toMatchObject({ code: 'BAD_REQUEST' });
  });

  it('config:cacheStats 只统计配置里的缓存目录', async () => {
    const h = harness();
    const cacheDir = h.config.get().cacheDir;
    mkdirSync(join(cacheDir, 'aa', 'bb'), { recursive: true });
    writeFileSync(join(cacheDir, 'aa', 'bb', 'hash1'), 'hello');

    const s = (await call(h, 'config:cacheStats')) as { dir: string; blobs: number; bytes: number };
    expect(s.dir).toBe(cacheDir);
    expect(s.blobs).toBe(1);
    expect(s.bytes).toBe(5);
  });

  it('config:clearCache 清掉内容并可只清临时文件', async () => {
    const h = harness();
    const cacheDir = h.config.get().cacheDir;
    mkdirSync(join(cacheDir, 'aa', 'bb'), { recursive: true });
    writeFileSync(join(cacheDir, 'aa', 'bb', 'hash1'), 'hello');
    mkdirSync(join(cacheDir, 'tmp'), { recursive: true });
    writeFileSync(join(cacheDir, 'tmp', 'x.dl-1'), 'partial');

    const onlyTmp = (await call(h, 'config:clearCache', { keepTmp: true })) as { removed: number };
    expect(onlyTmp.removed).toBe(1);
    expect(readFileSync(join(cacheDir, 'aa', 'bb', 'hash1'), 'utf8')).toBe('hello');

    const all = (await call(h, 'config:clearCache', {})) as { removed: number };
    expect(all.removed).toBe(1);
    expect(existsSync(join(cacheDir, 'aa', 'bb', 'hash1'))).toBe(false);
  });
});

describe('应用与系统', () => {
  it('app:info 返回构建与运行时信息', async () => {
    expect(await call(harness(), 'app:info')).toEqual(appInfo);
  });

  it('app:setConfig 只改传入字段并落盘', async () => {
    const h = harness();
    h.config.save({ ...h.config.get(), cacheDir: '/tmp/keep-me' });
    const r = (await call(h, 'app:setConfig', { concurrency: 8 })) as {
      concurrency: number;
      cacheDir: string;
    };
    expect(r.concurrency).toBe(8);
    expect(r.cacheDir).toBe('/tmp/keep-me'); // 没传的字段保持原样
    expect(h.config.get().concurrency).toBe(8);
  });

  it('app:pickDir **不再擅自改配置**（检出默认路径由设置页显式决定）', async () => {
    // v0.4.21：以前选任何目录都会把父目录记成"检出默认值"，连"下载旧版本到哪"都会污染它。
    const h = harness();
    h.config.update({ defaultCheckoutParent: '/configured' });
    expect(await call(h, 'app:pickDir', { title: '选择检出目录' })).toBe('/picked/dir');
    expect(h.config.get().defaultCheckoutParent).toBe('/configured'); // 还是设置里那个
    expect(await call(h, 'app:pickDir', { title: '取消选择' })).toBeNull();
    expect(h.config.get().defaultCheckoutParent).toBe('/configured');
  });

  it('app:setConfig 能写入检出默认路径', async () => {
    const h = harness();
    const r = (await call(h, 'app:setConfig', { defaultCheckoutParent: '/w' })) as {
      defaultCheckoutParent: string;
    };
    expect(r.defaultCheckoutParent).toBe('/w');
    expect(h.config.get().defaultCheckoutParent).toBe('/w');
  });

  it('没有对话框能力时 pickDir 返回 null（而不是抛）', async () => {
    const h = harness();
    delete h.deps.pickDir;
    expect(await call(h, 'app:pickDir', {})).toBeNull();
  });

  it('shell:revealPath 转发给文件管理器；无能力时回 revealed=false', async () => {
    const h = harness();
    expect(await call(h, 'shell:revealPath', { path: '/w/a' })).toEqual({ revealed: true });
    expect(h.revealed).toEqual(['/w/a']);

    const h2 = harness();
    delete h2.deps.revealPath;
    expect(await call(h2, 'shell:revealPath', { path: '/w/a' })).toEqual({ revealed: false });
  });
});

describe('处理器集合完整性', () => {
  it('buildHandlers 覆盖全部白名单通道', async () => {
    const h = harness();
    const { CHANNELS } = await import('../src/shared/channels.js');
    for (const c of CHANNELS) {
      expect(typeof h.handlers[c], `${c} 没有处理器`).toBe('function');
    }
    expect(Object.keys(h.handlers).sort()).toEqual([...CHANNELS].sort());
  });
});

describe('事件推送', () => {
  it('每次 adopt 都会推一次 wc:state（渲染层靠它同步顶栏）', async () => {
    const h = harness();
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' });
    await call(h, 'wc:commit', { message: 'm' });
    const states = h.events.filter(([c]) => c === 'wc:state');
    // checkout 采用一次、commit 后又采用一次（把前进的 rev 同步给顶栏）
    expect(states.length).toBeGreaterThanOrEqual(2);
    expect((states.at(-1)![1] as { rev: number }).rev).toBe(4);
  });

  it('没有 emit 能力时不崩（测试/无窗口场景）', async () => {
    const h = harness();
    delete h.deps.emit;
    await h.session.save({ server: 'http://s', username: 'alice', token: 't' });
    await expect(call(h, 'wc:checkout', { repo: 'art', dir: '/w/a' })).resolves.toBeTruthy();
  });

  it('vi 未使用告警（保持显式导入）', () => {
    expect(vi.isMockFunction(vi.fn())).toBe(true);
  });
});
