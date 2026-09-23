import { describe, expect, it, vi } from 'vitest';

import {
  buildWindowOptions,
  hardenWebContents,
  isNavigationAllowed,
  rendererTarget,
  type HardenedSession,
  type HardenedWebContents,
} from '../src/main/window.js';

describe('buildWindowOptions（§6.4 安全基线）', () => {
  const opts = buildWindowOptions({ preloadPath: '/app/preload/index.cjs', rendererFile: '/app/index.html' });

  it('contextIsolation / sandbox 打开，nodeIntegration 关闭', () => {
    expect(opts.webPreferences['contextIsolation']).toBe(true);
    expect(opts.webPreferences['nodeIntegration']).toBe(false);
    expect(opts.webPreferences['sandbox']).toBe(true);
  });

  it('webSecurity 不被关掉，也不允许混跑不安全内容', () => {
    expect(opts.webPreferences['webSecurity']).toBe(true);
    expect(opts.webPreferences['allowRunningInsecureContent']).toBe(false);
    expect(opts.webPreferences['experimentalFeatures']).toBe(false);
  });

  it('关闭后台节流（15s 锁轮询与自动同步推送靠它按时反映，E2E 也靠它不被"卡住"）', () => {
    expect(opts.webPreferences['backgroundThrottling']).toBe(false);
  });

  it('preload 路径原样传入（少了它渲染层就调不到任何通道）', () => {
    expect(opts.webPreferences['preload']).toBe('/app/preload/index.cjs');
  });

  it('初始不显示，等 ready-to-show（避免白屏闪烁）', () => {
    expect(opts.show).toBe(false);
    expect(buildWindowOptions({ preloadPath: 'p', rendererFile: 'f', show: true }).show).toBe(true);
  });
});

describe('rendererTarget', () => {
  it('有 devUrl 时走 dev server', () => {
    expect(rendererTarget({ rendererFile: '/app/index.html', devUrl: 'http://127.0.0.1:5273' })).toBe(
      'http://127.0.0.1:5273',
    );
  });

  it('devUrl 为空/未设时走本地文件', () => {
    expect(rendererTarget({ rendererFile: '/app/index.html' })).toBe('/app/index.html');
    expect(rendererTarget({ rendererFile: '/app/index.html', devUrl: '' })).toBe('/app/index.html');
  });
});

describe('isNavigationAllowed', () => {
  it('只放行白名单前缀', () => {
    expect(isNavigationAllowed('file:///app/index.html', ['file://'])).toBe(true);
    expect(isNavigationAllowed('https://evil.example/x', ['file://'])).toBe(false);
    expect(isNavigationAllowed('http://127.0.0.1:5273/x', ['http://127.0.0.1:5273'])).toBe(true);
    expect(isNavigationAllowed('http://127.0.0.1:5273.evil/x', ['http://127.0.0.1:5273'])).toBe(true);
  });
});

describe('hardenWebContents', () => {
  function harness() {
    let navigate: ((e: { preventDefault(): void }, url: string) => void) | null = null;
    let openHandler: ((d: { url: string }) => { action: 'deny' | 'allow' }) | null = null;
    let permHandler:
      | ((c: unknown, p: string, cb: (granted: boolean) => void) => void)
      | null = null;

    const contents: HardenedWebContents = {
      on: (_ev, listener) => {
        navigate = listener;
      },
      setWindowOpenHandler: (h) => {
        openHandler = h;
      },
    };
    const session: HardenedSession = {
      setPermissionRequestHandler: (h) => {
        permHandler = h;
      },
    };
    const result = hardenWebContents(contents, session, ['file://']);
    return {
      result,
      navigate: (url: string) => {
        const preventDefault = vi.fn();
        navigate!({ preventDefault }, url);
        return preventDefault;
      },
      open: (url: string) => openHandler!({ url }),
      ask: (permission: string) => {
        const cb = vi.fn();
        permHandler!(null, permission, cb);
        return cb;
      },
    };
  }

  it('白名单内导航放行（不调用 preventDefault）', () => {
    const h = harness();
    expect(h.navigate('file:///app/index.html')).not.toHaveBeenCalled();
    expect(h.result.blockedNavigations).toEqual([]);
  });

  it('外部导航被拦下并记账（便于冒烟断言"外链打不开"）', () => {
    const h = harness();
    const prevent = h.navigate('https://evil.example/phish');
    expect(prevent).toHaveBeenCalled();
    expect(h.result.blockedNavigations).toEqual(['https://evil.example/phish']);
  });

  it('window.open / 新窗口一律拒绝', () => {
    const h = harness();
    expect(h.open('https://evil.example')).toEqual({ action: 'deny' });
    expect(h.result.blockedNavigations).toContain('https://evil.example');
  });

  it('所有权限请求一律拒绝（本应用一项都不需要）', () => {
    const h = harness();
    for (const p of ['media', 'geolocation', 'notifications']) {
      expect(h.ask(p)).toHaveBeenCalledWith(false);
    }
    expect(h.result.deniedPermissions).toEqual(['media', 'geolocation', 'notifications']);
  });
});
