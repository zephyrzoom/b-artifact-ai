/**
 * preload：`contextBridge` 暴露**窄接口**（§6.4 安全基线）。
 *
 * 三条纪律：
 *   1. **显式方法表，不用 Proxy**。Proxy 会把"任意属性"变成潜在调用入口，
 *      等于把白名单又打开了；这里每个方法都手写一行。
 *   2. 通道名必须在 `shared/channels` 的白名单里，越界直接 reject，
 *      渲染层即使被注入脚本也调不到别的通道。
 *   3. 只暴露 `invoke` / `on` 两类能力——没有 `ipcRenderer` 本体、没有 `require`。
 *
 * 注意：`sandbox: true` 下 preload 必须是 CommonJS，所以构建产物是 `dist/preload/index.cjs`
 * （见 scripts/build.mjs）。
 */

import { CHANNELS, EVENT_CHANNELS, isChannel, isEventChannel } from './shared/channels.js';
import type { IpcResult } from './shared/ipc.js';

export interface IpcRendererLike {
  invoke(channel: string, payload?: unknown): Promise<unknown>;
  on(channel: string, listener: (event: unknown, payload: unknown) => void): void;
  removeListener(channel: string, listener: (event: unknown, payload: unknown) => void): void;
}

export interface BridgeOptions {
  /** 允许的通道（默认取白名单全集；测试里可收窄）。 */
  channels?: readonly string[];
  /** 允许监听的事件通道。 */
  events?: readonly string[];
}

/** 渲染层看到的 API 形状。 */
export interface BridgeApi {
  invoke(channel: string, payload?: unknown): Promise<IpcResult<unknown>>;
  on(channel: string, listener: (payload: unknown) => void): () => void;
  /** 白名单快照，供界面自检 / 测试断言。 */
  channels: readonly string[];
}

/**
 * 构造桥接对象（不依赖 electron，便于单测）。
 */
export function buildBridge(ipcRenderer: IpcRendererLike, options: BridgeOptions = {}): BridgeApi {
  const allowed = new Set(options.channels ?? CHANNELS);
  const allowedEvents = new Set(options.events ?? EVENT_CHANNELS);

  return {
    channels: [...allowed],

    async invoke(channel: string, payload?: unknown): Promise<IpcResult<unknown>> {
      if (typeof channel !== 'string' || !isChannel(channel) || !allowed.has(channel)) {
        // 返回信封而不是抛异常：渲染层对所有调用都只处理信封，少一条错误路径
        return { ok: false, error: { code: 'FORBIDDEN', message: `拒绝访问未授权通道：${channel}` } };
      }
      return (await ipcRenderer.invoke(channel, payload)) as IpcResult<unknown>;
    },

    on(channel: string, listener: (payload: unknown) => void): () => void {
      if (!isEventChannel(channel) || !allowedEvents.has(channel)) return () => {};
      const wrapped = (_event: unknown, payload: unknown): void => listener(payload);
      ipcRenderer.on(channel, wrapped);
      return () => ipcRenderer.removeListener(channel, wrapped);
    },
  };
}

/** preload 被构建成 CommonJS（`sandbox: true` 的硬要求），所以这里用 require。 */
declare const require: (id: string) => unknown;

interface ElectronPreloadApis {
  contextBridge: { exposeInMainWorld(key: string, api: unknown): void };
  ipcRenderer: IpcRendererLike;
}

function expose(): void {
  const { contextBridge, ipcRenderer } = require('electron') as ElectronPreloadApis;
  contextBridge.exposeInMainWorld('bartifact', buildBridge(ipcRenderer));
}

/**
 * 只有真在 preload 环境里才执行。
 * `process.contextIsolated` 是 Electron 注入的标记——它同时保证了直接 `node` 加载本文件
 * （比如单测或自检脚本）时不会去 require('electron')。
 */
const isPreloadContext = (process as unknown as { contextIsolated?: boolean }).contextIsolated === true;

if (isPreloadContext) {
  expose();
}
