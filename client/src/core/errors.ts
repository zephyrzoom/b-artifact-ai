/**
 * 错误模型（§7.1 错误响应统一结构 + 客户端本地错误）。
 *
 * 约定：客户端所有分支判断都基于机器可读的 `code`，`message` 只用于展示。
 */

/** 服务端错误码全表（§7.1）。 */
export const API_ERROR_CODES = [
  'INVALID_ARGUMENT',
  'INVALID_PATH',
  'PATH_NOT_NORMALIZED',
  'UNAUTHENTICATED',
  'PERMISSION_DENIED',
  'ACCOUNT_DISABLED',
  'NOT_FOUND',
  'OUT_OF_DATE',
  'LOCKED',
  'NEEDS_LOCK',
  'NAME_COLLISION',
  'COMMIT_TOKEN_EXPIRED',
  'PURGE_BLOCKED',
  'HASH_MISMATCH',
  'PAYLOAD_TOO_LARGE',
  'REPO_QUOTA',
  'RATE_LIMITED',
  'INTERNAL',
  'MAINTENANCE',
  // 客户端本地：连接失败 / 超时等，服务端没有对应码。
  'NETWORK',
  'UNKNOWN',
] as const;

export type ApiErrorCode = (typeof API_ERROR_CODES)[number];

/** 无需重试的错误码：重试只会得到同样的结果（§7.1「重试语义」）。 */
const NON_RETRYABLE: ReadonlySet<ApiErrorCode> = new Set<ApiErrorCode>([
  'INVALID_ARGUMENT',
  'INVALID_PATH',
  'PATH_NOT_NORMALIZED',
  'UNAUTHENTICATED',
  'PERMISSION_DENIED',
  'ACCOUNT_DISABLED',
  'NOT_FOUND',
  'NEEDS_LOCK',
  'NAME_COLLISION',
  'HASH_MISMATCH',
  'REPO_QUOTA',
]);

/** 服务端返回的错误。 */
export class ApiError extends Error {
  readonly status: number;
  readonly code: ApiErrorCode;
  readonly details: unknown;
  readonly retryAfterMs: number | null;

  constructor(args: {
    status: number;
    code: ApiErrorCode;
    message: string;
    details?: unknown;
    retryAfterMs?: number | null;
  }) {
    super(args.message);
    this.name = 'ApiError';
    this.status = args.status;
    this.code = args.code;
    this.details = args.details ?? null;
    this.retryAfterMs = args.retryAfterMs ?? null;
  }

  /** 是否值得重试（§7.1：可安全重试 = GET/HEAD、PUT blob、分块 PUT、POST /commit）。 */
  retryable(): boolean {
    if (NON_RETRYABLE.has(this.code)) return false;
    // 4xx 中除 429 外一律不重试；5xx 与 429 退避重试。
    return this.status === 429 || this.status >= 500;
  }
}

/** 工作副本本地错误码。 */
export type WcErrorCode =
  | 'NOT_A_WORKING_COPY'
  | 'NOT_FOUND'
  | 'WC_BUSY'
  | 'WC_CORRUPT'
  | 'PATH_INVALID'
  | 'NAME_COLLISION'
  | 'CONFLICT'
  | 'LOCAL_MODIFIED'
  | 'NO_CHANGES'
  | 'NOT_LOCKED'
  | 'IO';

/** 工作副本/本地操作错误。 */
export class WcError extends Error {
  readonly code: WcErrorCode;
  readonly details: unknown;

  constructor(code: WcErrorCode, message: string, details?: unknown) {
    super(message);
    this.name = 'WcError';
    this.code = code;
    this.details = details ?? null;
  }
}

/** 把任意抛出物归一成 ApiError，便于按 code 分支。 */
export function toApiError(e: unknown): ApiError {
  if (e instanceof ApiError) return e;
  if (e instanceof Error && e.name === 'AbortError') {
    return new ApiError({ status: 0, code: 'NETWORK', message: '请求超时' });
  }
  const msg = e instanceof Error ? e.message : String(e);
  return new ApiError({ status: 0, code: 'NETWORK', message: msg });
}
