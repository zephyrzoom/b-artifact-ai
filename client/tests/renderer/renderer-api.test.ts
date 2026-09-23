import { reactive, ref } from 'vue';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { IpcResult } from '../../src/shared/ipc.js';

/** 重装 window.bartifact 并把模块重置，避免模块级状态串场。 */
async function loadApi(bridge: unknown) {
  vi.resetModules();
  if (bridge === undefined) {
    delete (window as unknown as Record<string, unknown>)['bartifact'];
  } else {
    (window as unknown as Record<string, unknown>)['bartifact'] = bridge;
  }
  return import('../../src/renderer/api.js');
}

afterEach(() => {
  delete (window as unknown as Record<string, unknown>)['bartifact'];
});

describe('桥接探测', () => {
  it('preload 注入缺失时明确报 NO_BRIDGE（不静默降级成"空操作"）', async () => {
    const { call, IpcError } = await loadApi(undefined);
    await expect(call('app:info')).rejects.toBeInstanceOf(IpcError);
    await expect(call('app:info')).rejects.toMatchObject({ code: 'NO_BRIDGE' });
  });

  it('hasBridge 反映注入情况', async () => {
    const { hasBridge } = await loadApi(undefined);
    expect(hasBridge()).toBe(false);
    const mod = await loadApi({ invoke: async () => ({ ok: true, data: 1 }), on: () => () => {}, channels: [] });
    expect(mod.hasBridge()).toBe(true);
  });
});

describe('call', () => {
  it('成功时直接返回 data（渲染层只关心业务值）', async () => {
    const { call } = await loadApi({
      invoke: async () => ({ ok: true, data: { version: '0.1.0' } }),
      on: () => () => {},
      channels: [],
    });
    await expect(call('app:info')).resolves.toEqual({ version: '0.1.0' });
  });

  it('失败时抛 IpcError，code 原样保留（可据此分支）', async () => {
    const { call, IpcError } = await loadApi({
      invoke: async () => ({ ok: false, error: { code: 'OUT_OF_DATE', message: '基线过期' } }),
      on: () => () => {},
      channels: [],
    });
    const err = await call('wc:commit', {}).catch((e) => e);
    expect(err).toBeInstanceOf(IpcError);
    expect(err.code).toBe('OUT_OF_DATE');
    expect(err.message).toBe('基线过期');
  });

  it('details 一并带回（冲突清单等）', async () => {
    const { call } = await loadApi({
      invoke: async () => ({
        ok: false,
        error: { code: 'CONFLICT', message: '有冲突', details: { paths: ['a.psd'] } },
      }),
      on: () => () => {},
      channels: [],
    });
    const err = await call('wc:commit', {}).catch((e) => e);
    expect(err.details).toEqual({ paths: ['a.psd'] });
  });

  it('载荷原样透传（渲染层不自己加工参数）', async () => {
    const invoke = vi.fn(async (): Promise<IpcResult<unknown>> => ({ ok: true, data: [] }));
    const { api } = await loadApi({ invoke, on: () => () => {}, channels: [] });
    await api.checkout('art', '/w/art', ['characters']);
    expect(invoke).toHaveBeenCalledWith('wc:checkout', {
      repo: 'art',
      dir: '/w/art',
      sparse: ['characters'],
    });
  });

  it('响应式数组载荷被归一化成纯数据（否则 Electron 只报一句 could not be cloned）', async () => {
    const invoke = vi.fn(
      async (_channel: string, _payload?: unknown): Promise<IpcResult<unknown>> => ({
        ok: true,
        data: { added: 1 },
      }),
    );
    const { api } = await loadApi({ invoke, on: () => () => {}, channels: [] });

    // ref 取出来的值是响应式 Proxy —— 这正是踩过的坑
    const reactivePaths = ref(['a.psd', 'b.psd']).value;
    expect(() => structuredClone(reactivePaths)).toThrow(/could not be cloned/);

    await api.add(reactivePaths);

    const payload = invoke.mock.calls[0]![1];
    // ① 必须是可结构化克隆的；② 不能把 Proxy 原样透过去
    expect(() => structuredClone(payload)).not.toThrow();
    expect(payload).toEqual({ paths: ['a.psd', 'b.psd'] });
    expect((payload as { paths: unknown }).paths).not.toBe(reactivePaths);
  });

  it('深层嵌套的响应式对象同样被归一化', async () => {
    const invoke = vi.fn(
      async (_channel: string, _payload?: unknown): Promise<IpcResult<unknown>> => ({
        ok: true,
        data: {},
      }),
    );
    const { call } = await loadApi({ invoke, on: () => () => {}, channels: [] });
    const nested = reactive({ a: [{ b: 1 }] });
    await call('wc:status', nested);
    expect(() => structuredClone(invoke.mock.calls[0]![1])).not.toThrow();
  });

  it('api 覆盖常用通道（名字与白名单一致）', async () => {
    const { api } = await loadApi({ invoke: async () => ({ ok: true, data: null }), on: () => () => {}, channels: [] });
    expect(Object.keys(api).sort()).toEqual(
      [
        'add',
        'appInfo',
        'cacheStats',
        'clearCache',
        'conflictSides',
        'conflicts',
        'downloadRevision',
        'authState',
        'checkout',
        'close',
        'commit',
        'config',
        'forgetServer',
        'listRepos',
        'ignoreRules',
        'lock',
        'log',
        'mkdir',
        'locks',
        'login',
        'logout',
        'open',
        'pickDir',
        'recent',
        'remove',
        'revealPath',
        'resolveConflict',
        'revert',
        'setConfig',
        'setIgnoreRules',
        'status',
        'treeAt',
        'unlock',
        'update',
      ].sort(),
    );
  });
});

describe('on（主进程推送）', () => {
  it('转发 payload 并返回退订函数', async () => {
    let captured: ((p: unknown) => void) | null = null;
    const off = vi.fn();
    const { on } = await loadApi({
      invoke: async () => ({ ok: true, data: null }),
      on: (_c: string, l: (p: unknown) => void) => {
        captured = l;
        return off;
      },
      channels: [],
    });

    const seen: unknown[] = [];
    const unsubscribe = on('wc:progress', (p) => seen.push(p));
    captured!({ phase: 'upload', done: 1, total: 2 });
    expect(seen).toEqual([{ phase: 'upload', done: 1, total: 2 }]);

    unsubscribe();
    expect(off).toHaveBeenCalled();
  });

  it('没有桥时订阅同样抛 NO_BRIDGE（不静默变成空订阅）', async () => {
    const { on } = await loadApi(undefined);
    expect(() => on('wc:state', () => {})).toThrow(/preload/);
  });
});
