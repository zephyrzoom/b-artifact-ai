/**
 * IPC 注册器：白名单 + 参数校验 + 统一错误信封。
 *
 * 三条不变量（都有单测钉住）：
 *   1. **只注册白名单里的通道**——渲染层调不到未定义通道；
 *   2. **处理器集合必须与通道表**完全一致——多了（手滑加了通道没加校验）或少了
 *      （通道表加了但没人实现）都在启动时报错，而不是运行到一半才发现；
 *   3. **异常不跨 IPC 边界**——一律转成 `{ ok:false, error:{ code, message } }`，
 *      渲染层靠 code 分支（§7.1 同款约定）。
 */

import { CHANNELS, isChannel, isEventChannel, validateChannelPayload, type Channel, type EventChannel } from '../shared/channels.js';
import { err, ok, toIpcError, type IpcResult } from '../shared/ipc.js';
import type { Handler } from './handlers.js';

export interface IpcMainLike {
  handle(channel: string, listener: (event: unknown, payload: unknown) => Promise<unknown>): void;
}

export interface WebContentsLike {
  send(channel: string, payload: unknown): void;
  isDestroyed?(): boolean;
}

/**
 * 处理一次调用：校验 → 执行 → 包信封。不抛异常。
 * 单独导出是为了能在单测里直接驱动，不必造一个假 ipcMain。
 */
export async function dispatch(
  channel: string,
  handler: Handler,
  payload: unknown,
): Promise<IpcResult<unknown>> {
  const checked = validateChannelPayload(channel, payload);
  if (!checked.ok) return err('BAD_REQUEST', checked.message);
  try {
    return ok(await handler(checked.value));
  } catch (e) {
    return { ok: false, error: toIpcError(e) };
  }
}

/** 校验处理器集合与通道表一致；不一致直接抛（启动期 fail fast）。 */
export function assertHandlersComplete(handlers: Partial<Record<Channel, Handler>>): void {
  const keys = Object.keys(handlers);
  const extra = keys.filter((k) => !isChannel(k));
  if (extra.length > 0) {
    throw new Error(`存在未在通道表中定义的处理器：${extra.join('、')}`);
  }
  const missing = CHANNELS.filter((c) => typeof handlers[c] !== 'function');
  if (missing.length > 0) {
    throw new Error(`通道表中有未实现的处理器：${missing.join('、')}`);
  }
}

export function registerIpc(
  ipcMain: IpcMainLike,
  handlers: Record<Channel, Handler>,
): void {
  assertHandlersComplete(handlers);
  for (const channel of CHANNELS) {
    const handler = handlers[channel];
    ipcMain.handle(channel, async (_event, payload) => dispatch(channel, handler, payload));
  }
}

/** 主进程 → 渲染层推送。事件通道同样白名单校验，防止误用 invoke 通道名推送。 */
export function emitEvent(
  target: WebContentsLike | null,
  channel: EventChannel,
  payload: unknown,
): boolean {
  if (!target) return false;
  if (!isEventChannel(channel)) return false;
  if (target.isDestroyed?.()) return false;
  target.send(channel, payload);
  return true;
}
