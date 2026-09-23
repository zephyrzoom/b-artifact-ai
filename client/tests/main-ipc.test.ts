import { describe, expect, it, vi } from 'vitest';

import { ApiError, WcError } from '../src/core/errors.js';
import { MainError } from '../src/main/errors.js';
import type { Handler } from '../src/main/handlers.js';
import { assertHandlersComplete, dispatch, emitEvent, registerIpc } from '../src/main/ipc.js';
import { CHANNELS, type Channel } from '../src/shared/channels.js';

describe('dispatch（校验 → 执行 → 包信封）', () => {
  const handler: Handler = async (p) => ({ echo: p['x'] });

  it('成功时返回 { ok:true, data }', async () => {
    const r = await dispatch('app:pickDir', async () => 'picked', {});
    expect(r).toEqual({ ok: true, data: 'picked' });
  });

  it('参数不合法时返回 BAD_REQUEST，且**不调用**处理器', async () => {
    const fn = vi.fn(async () => 'never');
    const r = await dispatch('wc:add', fn, { paths: 'not-an-array' });
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.error.code).toBe('BAD_REQUEST');
    expect(fn).not.toHaveBeenCalled();
  });

  it('未知通道也返回 BAD_REQUEST（而非抛异常）', async () => {
    const r = await dispatch('wc:nope', handler, {});
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.error.code).toBe('BAD_REQUEST');
  });

  it('WcError 的 code 原样透传，渲染层靠它分支', async () => {
    const r = await dispatch('wc:status', async () => {
      throw new WcError('CONFLICT', '存在冲突');
    }, {});
    expect(r).toEqual({ ok: false, error: { code: 'CONFLICT', message: '存在冲突' } });
  });

  it('ApiError 的 code / details 一并保留', async () => {
    const r = await dispatch('auth:login', async () => {
      throw new ApiError({ status: 401, code: 'UNAUTHENTICATED', message: '密码错误' });
    }, { server: 'http://x', username: 'a', password: 'b' });
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.error.code).toBe('UNAUTHENTICATED');
  });

  it('MainError 的 code 也透传（会话状态类错误）', async () => {
    const r = await dispatch('wc:status', async () => {
      throw new MainError('NO_WORKING_COPY', '尚未打开工作副本');
    }, {});
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.error.code).toBe('NO_WORKING_COPY');
  });

  it('普通异常归一成 INTERNAL，不泄漏堆栈到渲染层', async () => {
    const r = await dispatch('app:info', async () => {
      throw new TypeError('undefined is not a function');
    }, {});
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.error.code).toBe('INTERNAL');
      expect(r.error.message).toBe('undefined is not a function');
      expect(JSON.stringify(r)).not.toContain('at ');
    }
  });

  it('抛非 Error 也兜得住', async () => {
    const r = await dispatch('app:info', async () => {
      throw 'plain string';
    }, {});
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.error.code).toBe('INTERNAL');
      expect(r.error.message).toBe('plain string');
    }
  });
});

describe('assertHandlersComplete', () => {
  function fullHandlers(): Record<Channel, Handler> {
    const out = {} as Record<Channel, Handler>;
    for (const c of CHANNELS) out[c] = async () => null;
    return out;
  }

  it('齐全时通过', () => {
    expect(() => assertHandlersComplete(fullHandlers())).not.toThrow();
  });

  it('少了处理器 → 启动期报错（而不是运行到一半才发现）', () => {
    const h = fullHandlers();
    delete (h as Partial<Record<Channel, Handler>>)['wc:commit'];
    expect(() => assertHandlersComplete(h)).toThrow(/未实现的处理器/);
  });

  it('多了未在通道表声明的处理器 → 报错（防止绕过 schema 校验加通道）', () => {
    const h = { ...fullHandlers(), 'wc:secret': async () => 'boom' } as Record<string, Handler>;
    expect(() => assertHandlersComplete(h as Record<Channel, Handler>)).toThrow(
      /未在通道表中定义/,
    );
  });
});

describe('registerIpc', () => {
  it('只注册白名单里的通道，数量与通道表一致', () => {
    const handled: string[] = [];
    const ipcMain = {
      handle: (channel: string) => {
        handled.push(channel);
      },
    };
    const handlers = {} as Record<Channel, Handler>;
    for (const c of CHANNELS) handlers[c] = async () => null;

    registerIpc(ipcMain, handlers);
    expect(handled).toEqual([...CHANNELS]);
  });

  it('注册的监听器走的是同一条 dispatch 路径（校验生效）', async () => {
    let registered: ((event: unknown, payload: unknown) => Promise<unknown>) | null = null;
    const ipcMain = {
      handle: (_c: string, listener: (event: unknown, payload: unknown) => Promise<unknown>) => {
        registered = listener;
      },
    };
    const handlers = {} as Record<Channel, Handler>;
    for (const c of CHANNELS) handlers[c] = async () => 'ok';
    registerIpc(ipcMain, handlers);

    const r = await registered!(null, 'not-an-object');
    expect(r).toMatchObject({ ok: false, error: { code: 'BAD_REQUEST' } });
  });
});

describe('emitEvent（主进程 → 渲染层）', () => {
  it('白名单事件通道可推送', () => {
    const sent: Array<[string, unknown]> = [];
    const target = { send: (c: string, p: unknown) => sent.push([c, p]) };
    expect(emitEvent(target, 'wc:progress', { phase: 'hash', done: 1, total: 2 })).toBe(true);
    expect(sent).toEqual([['wc:progress', { phase: 'hash', done: 1, total: 2 }]]);
  });

  it('非事件通道拒绝推送（防止误用 invoke 通道名）', () => {
    const target = { send: vi.fn() };
    expect(emitEvent(target, 'wc:commit' as never, {})).toBe(false);
    expect(target.send).not.toHaveBeenCalled();
  });

  it('窗口不存在或已销毁时不推（也不抛）', () => {
    expect(emitEvent(null, 'wc:state', null)).toBe(false);
    expect(emitEvent({ send: vi.fn(), isDestroyed: () => true }, 'wc:state', null)).toBe(false);
  });
});
