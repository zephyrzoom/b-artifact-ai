/**
 * 主进程层错误码。
 *
 * 与 `core/errors.ts` 分开：那些是**引擎/服务端**的错误（`OUT_OF_DATE`、`LOCKED`…），
 * 这些是**主进程会话状态**相关的错误（还没登录、还没打开工作副本）。
 * 混进 `WcError` 会让"引擎错了"和"你还没打开目录"这两类问题看起来一样。
 */

export const MAIN_ERROR_CODES = [
  /** 渲染层要求操作工作副本，但主进程还没打开任何一个。 */
  'NO_WORKING_COPY',
  /** 没有有效登录态。 */
  'UNAUTHENTICATED',
  /** 参数不合法（IPC 层 schema 之外再校验一次）。 */
  'BAD_REQUEST',
  /** 被白名单/权限拒绝。 */
  'FORBIDDEN',
] as const;

export type MainErrorCode = (typeof MAIN_ERROR_CODES)[number];

export class MainError extends Error {
  readonly code: MainErrorCode;
  readonly details: unknown;

  constructor(code: MainErrorCode, message: string, details?: unknown) {
    super(message);
    this.name = 'MainError';
    this.code = code;
    this.details = details ?? null;
  }
}
