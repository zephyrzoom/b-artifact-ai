/**
 * IPC 统一信封与错误映射。
 *
 * 所有 `invoke` 一律返回信封，主进程**从不**把异常直接抛过 IPC 边界：
 * Electron 序列化异常只会留下一句 "Error invoking remote method"，错误码与上下文全丢，
 * 渲染层没法据此分支。所以这里统一成 `{ ok, data }` / `{ ok, error: { code, message, details } }`，
 * 与 §7.1 服务端错误码的用法一致（code 机器可读，message 仅展示）。
 */

export interface IpcErrorPayload {
  code: string;
  message: string;
  details?: unknown;
}

export type IpcResult<T> = { ok: true; data: T } | { ok: false; error: IpcErrorPayload };

export function ok<T>(data: T): IpcResult<T> {
  return { ok: true, data };
}

export function err(code: string, message: string, details?: unknown): IpcResult<never> {
  return { ok: false, error: details === undefined ? { code, message } : { code, message, details } };
}

interface CodedError extends Error {
  code?: unknown;
  details?: unknown;
}

/**
 * 把任意异常映射成错误信封。
 *
 * `WcError` / `ApiError` 都带 `code`，原样透传——渲染层靠它区分
 * `OUT_OF_DATE` / `CONFLICT` / `LOCKED` / `PERMISSION_DENIED` 等分支，
 * 而不是去匹配中文文案。
 */
export function toIpcError(e: unknown): IpcErrorPayload {
  if (e && typeof e === 'object') {
    const it = e as CodedError;
    const code = typeof it.code === 'string' && it.code ? it.code : 'INTERNAL';
    const message = typeof it.message === 'string' && it.message ? it.message : String(e);
    // `WcError` / `ApiError` 的 details 缺省是 null —— 别把 `details: null` 塞进信封，
    // 让渲染层的 `if (error.details)` 这类判断保持干净
    const { details } = it;
    return details === undefined || details === null ? { code, message } : { code, message, details };
  }
  return { code: 'INTERNAL', message: String(e) };
}

export function isIpcResult(v: unknown): v is IpcResult<unknown> {
  return typeof v === 'object' && v !== null && 'ok' in (v as Record<string, unknown>);
}
