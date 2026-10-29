/**
 * 渲染层到 Rust 侧的桥（替代 Electron 的 preload）。
 *
 * 形状刻意与 `src/preload.ts` 的 `buildBridge` 保持一致（`invoke` / `on` / `channels`），
 * 于是 `renderer/api.ts`、store、视图一行都不用改：
 *
 *   Electron:  window.bartifact.invoke(channel, payload)  →  ipcRenderer.invoke
 *   Tauri:     invoke('ipc', { channel, payload })        →  Rust 侧白名单 + schema 校验
 *
 * 三条纪律照旧：
 *   1. 通道名只在白名单里（Rust 侧还会再校验一次，越界直接回信封而不是抛异常）；
 *   2. 只暴露 `invoke` / `on` 两类能力，没有任意命令调用入口；
 *   3. 返回的信封形状不变：`{ ok, data }` / `{ ok, error: { code, message } }`。
 */
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

import { CHANNELS, EVENT_CHANNELS, isChannel, isEventChannel } from '@shared/channels';
import type { IpcResult } from '@shared/ipc';

import type { BartifactBridge } from './env';

/** 与 Rust 侧 `commands::ipc` 的签名对齐。 */
function callRust(channel: string, payload?: unknown): Promise<IpcResult<unknown>> {
  return invoke<IpcResult<unknown>>('ipc', { channel, payload: payload ?? null });
}

export function createTauriBridge(): BartifactBridge {
  return {
    channels: CHANNELS,

    async invoke(channel: string, payload?: unknown): Promise<IpcResult<unknown>> {
      if (!isChannel(channel)) {
        // 与 preload 一致：返回信封而不是抛异常，渲染层只处理一种形状
        return {
          ok: false,
          error: { code: 'FORBIDDEN', message: `拒绝访问未授权通道：${channel}` },
        };
      }
      return callRust(channel, payload);
    },

    on(channel: string, listener: (payload: unknown) => void): () => void {
      if (!isEventChannel(channel)) return () => {};

      // `listen` 是异步的（要去 Rust 侧注册），但 `on` 必须同步返回取消函数：
      // 这里用一个"已取消"标记兜住"还没注册完就取消"的竞态。
      let disposed = false;
      let unlisten: (() => void) | null = null;

      void listen<unknown>(channel, (event) => {
        listener(event.payload);
      }).then((fn) => {
        if (disposed) {
          fn();
          return;
        }
        unlisten = fn;
      });

      return () => {
        disposed = true;
        unlisten?.();
        unlisten = null;
      };
    },
  };
}

/** 事件通道白名单快照（自检 / 测试用）。 */
export const tauriEventChannels: readonly string[] = EVENT_CHANNELS;
