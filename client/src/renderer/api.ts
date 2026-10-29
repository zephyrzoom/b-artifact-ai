/**
 * 渲染层对 IPC 的唯一出口。
 *
 * 渲染层**不直接用 fetch**、也拿不到 token：所有能力都从 `window.bartifact.invoke` 过桥，
 * 主进程按 `shared/channels` 的白名单校验后再落到引擎。
 * 这里只做两件事：把错误信封还原成异常、把 payload 的类型标好。
 */

import type { ConflictResolution } from '@shared/dto';

import { createTauriBridge } from './tauri-bridge';
import type { BridgeCallMap, BridgeEventMap, BartifactBridge } from './env';

export class IpcError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly details?: unknown,
  ) {
    super(message);
    this.name = 'IpcError';
  }
}

/**
 * 是否跑在 Tauri 的 WebView 里。
 *
 * `__TAURI_INTERNALS__` 由 Tauri 注入到 window；happy-dom 单测里不存在，
 * 于是"没有桥"的原有用例仍然成立。
 */
function inTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

let tauriBridge: BartifactBridge | null = null;

/**
 * 取桥接对象。
 *
 * 两种来源，形状完全一致：
 *   1. `window.bartifact` —— Electron 时代的 preload 注入（单测替身也走这条路）；
 *   2. Tauri 桥（`invoke('ipc', {channel, payload})` + `listen`）。
 *
 * 两者都没有时**明确报错**而不是静默降级成空操作。
 */
export function bridge(): BartifactBridge {
  const fromPreload = window.bartifact;
  if (fromPreload) return fromPreload;
  if (inTauri()) {
    tauriBridge ??= createTauriBridge();
    return tauriBridge;
  }
  throw new IpcError(
    'NO_BRIDGE',
    'preload/bridge 未注入（window.bartifact 不存在，且不在 Tauri 环境里）',
  );
}

export function hasBridge(): boolean {
  return !!window.bartifact || inTauri();
}

/**
 * IPC 载荷必须是**可结构化克隆的纯数据**。
 *
 * Vue 的 `ref([...])` 取出来的是响应式 Proxy，直接丢给 `ipcRenderer.invoke` 会被
 * Chromium 拒掉，错误信息只有一句 "An object could not be cloned."——排查成本极高。
 * 我们的载荷本来就全是 JSON 形状（主进程侧还有 schema 校验），所以统一做一次
 * JSON 归一化，把 Proxy 变成纯数组/对象。载荷里出现函数或循环引用会在这里就暴露。
 */
function toPlainPayload(payload: unknown): unknown {
  if (payload === undefined || payload === null) return payload;
  if (typeof payload !== 'object') return payload;
  return JSON.parse(JSON.stringify(payload));
}

/** 调一个通道；失败时抛 `IpcError`（code 来自主进程，可直接用于分支）。 */
export async function call<K extends keyof BridgeCallMap>(
  channel: K,
  payload?: unknown,
): Promise<BridgeCallMap[K]> {
  const res = await bridge().invoke(channel, toPlainPayload(payload));
  if (res.ok) return res.data as BridgeCallMap[K];
  const e = res.error;
  throw new IpcError(e.code, e.message, e.details);
}

/** 订阅主进程推送，返回取消订阅函数。 */
export function on<K extends keyof BridgeEventMap>(
  channel: K,
  listener: (payload: BridgeEventMap[K]) => void,
): () => void {
  return bridge().on(channel, (payload) => listener(payload as BridgeEventMap[K]));
}

export const api = {
  appInfo: () => call('app:info'),
  config: () => call('app:config'),
  setConfig: (patch: {
    concurrency?: number;
    cacheDir?: string;
    /** 检出目录的默认父目录（设置页配置）。 */
    defaultCheckoutParent?: string;
  }) => call('app:setConfig', patch),
  pickDir: (title?: string) => call('app:pickDir', { title }),

  login: (server: string, username: string, password: string) =>
    call('auth:login', { server, username, password }),
  logout: () => call('auth:logout'),
  authState: () => call('auth:state'),
  forgetServer: (server: string) => call('auth:forgetServer', { server }),

  listRepos: () => call('repos:list'),

  recent: () => call('wc:recent'),
  checkout: (repo: string, dir: string, sparse?: string[]) =>
    call('wc:checkout', { repo, dir, sparse }),
  open: (dir: string) => call('wc:open', { dir }),
  close: () => call('wc:close'),
  status: (forceHash?: boolean) => call('wc:status', { forceHash }),
  add: (paths: string[]) => call('wc:add', { paths }),
  remove: (paths: string[]) => call('wc:remove', { paths }),
  revert: (paths: string[]) => call('wc:revert', { paths }),
  mkdir: (path: string) => call('wc:mkdir', { path }),
  ignoreRules: () => call('wc:ignoreRules', {}),
  setIgnoreRules: (content: string) => call('wc:setIgnoreRules', { content }),
  commit: (message: string, paths?: string[]) => call('wc:commit', { message, paths }),
  update: () => call('wc:update'),

  conflicts: () => call('wc:conflicts'),
  conflictSides: (path: string) => call('wc:conflictSides', { path }),
  resolveConflict: (path: string, choice: ConflictResolution, content?: string) =>
    call('wc:resolveConflict', { path, choice, content }),

  /** 历史修订列表。**仓库由调用方给**：仓库页在没有工作副本时也要能看历史。 */
  log: (repo: string, opts: { limit?: number; offset?: number; prefix?: string } = {}) =>
    call('repo:log', { repo, ...opts }),
  treeAt: (repo: string, rev: number, path = '', depth = 1) =>
    call('repo:treeAt', { repo, rev, path, depth }),
  downloadRevision: (repo: string, path: string, rev: number, targetDir: string) =>
    call('repo:downloadRevision', { repo, path, rev, targetDir }),

  cacheStats: () => call('config:cacheStats'),
  clearCache: (keepTmp = false) => call('config:clearCache', { keepTmp }),

  locks: (path?: string) => call('wc:locks', { path }),
  lock: (path: string, opts: { comment?: string } = {}) => call('wc:lock', { path, ...opts }),
  unlock: (path: string, opts: { breakLock?: boolean; reason?: string } = {}) =>
    call('wc:unlock', { path, ...opts }),

  revealPath: (path: string) => call('shell:revealPath', { path }),
};
