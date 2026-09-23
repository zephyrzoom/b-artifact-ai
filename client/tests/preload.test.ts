import { describe, expect, it, vi } from 'vitest';

import { buildBridge, type IpcRendererLike } from '../src/preload.js';
import { CHANNELS, EVENT_CHANNELS } from '../src/shared/channels.js';

function fakeIpc() {
  const invoked: string[] = [];
  const listeners = new Map<string, (event: unknown, payload: unknown) => void>();
  const removed: string[] = [];
  const ipc: IpcRendererLike = {
    invoke: async (channel) => {
      invoked.push(channel);
      return { ok: true, data: `data:${channel}` };
    },
    on: (channel, listener) => {
      listeners.set(channel, listener);
    },
    removeListener: (channel, _listener) => {
      removed.push(channel);
      listeners.delete(channel);
    },
  };
  return {
    ipc,
    invoked,
    listeners,
    removed,
    fire: (channel: string, payload: unknown) => listeners.get(channel)?.({}, payload),
  };
}

describe('buildBridge', () => {
  it('expose 的 channels 快照就是白名单', () => {
    const { ipc } = fakeIpc();
    expect(buildBridge(ipc).channels).toEqual(CHANNELS);
  });

  it('白名单内的通道正常转发并回传信封', async () => {
    const h = fakeIpc();
    const bridge = buildBridge(h.ipc);
    const r = await bridge.invoke('wc:status', { forceHash: true });
    expect(r).toEqual({ ok: true, data: 'data:wc:status' });
    expect(h.invoked).toEqual(['wc:status']);
  });

  it('白名单外但"像"通道名的调用被拒，且**不会**打到主进程', async () => {
    const h = fakeIpc();
    const bridge = buildBridge(h.ipc);
    const r = await bridge.invoke('wc:secret');
    expect(r).toMatchObject({ ok: false, error: { code: 'FORBIDDEN' } });
    expect(h.invoked).toEqual([]);
  });

  it('事件通道名不能用 invoke 调用（两套名字空间不混用）', async () => {
    const h = fakeIpc();
    const r = await buildBridge(h.ipc).invoke('wc:progress');
    expect(r).toMatchObject({ ok: false, error: { code: 'FORBIDDEN' } });
    expect(h.invoked).toEqual([]);
  });

  it('原型链上的属性名也调不动（toString / constructor）', async () => {
    const h = fakeIpc();
    const bridge = buildBridge(h.ipc);
    for (const evil of ['toString', 'constructor', '__proto__']) {
      const r = await bridge.invoke(evil);
      expect(r).toMatchObject({ ok: false, error: { code: 'FORBIDDEN' } });
    }
    expect(h.invoked).toEqual([]);
  });

  it('非字符串通道名不会抛异常', async () => {
    const h = fakeIpc();
    const r = await buildBridge(h.ipc).invoke(undefined as unknown as string);
    expect(r).toMatchObject({ ok: false, error: { code: 'FORBIDDEN' } });
  });

  it('可以用 options 收窄白名单（测试/自检场景）', async () => {
    const h = fakeIpc();
    const bridge = buildBridge(h.ipc, { channels: ['app:info'] });
    expect(bridge.channels).toEqual(['app:info']);
    expect(await bridge.invoke('app:info')).toMatchObject({ ok: true });
    expect(await bridge.invoke('wc:status')).toMatchObject({ ok: false });
  });

  it('on 订阅事件通道，回调只收到 payload（不透传 event 对象）', () => {
    const h = fakeIpc();
    const seen: unknown[] = [];
    buildBridge(h.ipc).on('wc:progress', (p) => seen.push(p));
    h.fire('wc:progress', { phase: 'upload', done: 1, total: 3 });
    expect(seen).toEqual([{ phase: 'upload', done: 1, total: 3 }]);
  });

  it('on 返回的退订函数会摘掉监听', () => {
    const h = fakeIpc();
    const off = buildBridge(h.ipc).on('wc:state', () => {});
    off();
    expect(h.removed).toEqual(['wc:state']);
  });

  it('订阅非事件通道（例如 wc:commit）什么也不做', () => {
    const h = fakeIpc();
    const listener = vi.fn();
    const off = buildBridge(h.ipc).on('wc:commit', listener);
    expect(h.listeners.size).toBe(0);
    expect(() => off()).not.toThrow();
  });

  it('事件白名单收窄后越界的 on 也不生效', () => {
    const h = fakeIpc();
    const off = buildBridge(h.ipc, { events: [] }).on('wc:progress', () => {});
    expect(h.listeners.size).toBe(0);
    expect(() => off()).not.toThrow();
  });

  it('暴露出去的能力只有 invoke / on / channels（没有 ipcRenderer 本体）', () => {
    const { ipc } = fakeIpc();
    const bridge = buildBridge(ipc) as unknown as Record<string, unknown>;
    expect(Object.keys(bridge).sort()).toEqual(['channels', 'invoke', 'on']);
    expect(bridge['ipcRenderer']).toBeUndefined();
    expect(bridge['send']).toBeUndefined();
    expect(bridge['require']).toBeUndefined();
  });

  it('事件通道常量本身是白名单的一部分（不会漏在桥上）', () => {
    expect(EVENT_CHANNELS.length).toBeGreaterThan(0);
    for (const e of EVENT_CHANNELS) expect(CHANNELS).not.toContain(e);
  });
});
