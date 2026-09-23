/**
 * 文件 I/O 退避重试（§6.6 ④ 文件占用）。
 *
 * Windows 上 Photoshop / Office 会长期持有文件句柄，覆盖或删除必然失败。
 * 对这类**可恢复**的错误做退避重试；权限不足、路径不存在这类则立即失败——
 * 重试只会让用户多等几秒然后得到同样的结果。
 */

const RETRYABLE = new Set([
  'EBUSY', // 文件被占用
  'EPERM', // Windows 上删除只读/被占用文件常见
  'EACCES', // 瞬时占用（杀软扫描）
  'EMFILE', // 句柄耗尽
  'ENFILE',
  'EAGAIN',
  'UNKNOWN',
]);

export interface RetryOptions {
  retries?: number;
  /** 基础退避毫秒，实际间隔为 base * 2^i。 */
  baseDelayMs?: number;
  onRetry?: (err: NodeJS.ErrnoException, attempt: number) => void;
}

export async function withRetry<T>(fn: () => Promise<T>, opts: RetryOptions = {}): Promise<T> {
  const retries = opts.retries ?? 3;
  const base = opts.baseDelayMs ?? 1000;
  let last: NodeJS.ErrnoException | null = null;
  for (let i = 0; i <= retries; i++) {
    if (i > 0) {
      await sleep(base * 2 ** (i - 1));
      opts.onRetry?.(last!, i);
    }
    try {
      return await fn();
    } catch (e) {
      const err = e as NodeJS.ErrnoException;
      last = err;
      if (!RETRYABLE.has(err.code ?? 'UNKNOWN')) throw err;
    }
  }
  throw last ?? new Error('withRetry: 未知失败');
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
