/**
 * 文件 I/O 退避重试（§6.6 ④ 文件占用）。
 *
 * Windows 上 Photoshop / Office 会长期持有文件句柄，覆盖或删除必然失败。
 * 对这类**可恢复**的错误做退避重试；权限不足、路径不存在这类则立即失败——
 * 重试只会让用户多等几秒然后得到同样的结果。
 */
use std::io;
use std::time::Duration;

/// 可重试的 errno（与 TS 的 `RETRYABLE` 集合对齐）。
fn retryable_errno(code: i32) -> bool {
    // EPERM / EACCES / EBUSY / EMFILE / ENFILE / EAGAIN
    #[cfg(unix)]
    {
        if code == libc::EPERM
            || code == libc::EACCES
            || code == libc::EBUSY
            || code == libc::EMFILE
            || code == libc::ENFILE
            || code == libc::EAGAIN
        {
            return true;
        }
        false
    }
    #[cfg(windows)]
    {
        // ERROR_ACCESS_DENIED(5) / ERROR_SHARING_VIOLATION(32) / ERROR_LOCK_VIOLATION(33)
        matches!(code, 5 | 32 | 33)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = code;
        false
    }
}

/// 这个错误是否值得重试。
pub fn is_retryable(e: &io::Error) -> bool {
    match e.raw_os_error() {
        Some(code) => retryable_errno(code),
        // 拿不到 errno 时按 TS 版的 'UNKNOWN' 处理：宁可多试一次
        None => true,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RetryOptions {
    pub retries: u32,
    /// 基础退避毫秒，实际间隔为 base * 2^i。
    pub base_delay_ms: u64,
}

impl Default for RetryOptions {
    fn default() -> Self {
        Self {
            retries: 3,
            base_delay_ms: 1000,
        }
    }
}

/// 同步版本：失败按 2 的幂退避（base, 2base, 4base…），超出 retries 后把最后一个错误抛出。
pub fn with_retry<T>(
    opts: RetryOptions,
    mut f: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut last: Option<io::Error> = None;
    for i in 0..=opts.retries {
        if i > 0 {
            let delay = opts.base_delay_ms * 2u64.pow(i - 1);
            std::thread::sleep(Duration::from_millis(delay));
        }
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !is_retryable(&e) {
                    return Err(e);
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("with_retry: 未知失败")))
}

/// 异步版本（同上，用 tokio 定时器让出执行权）。
pub async fn with_retry_async<T, F, Fut>(opts: RetryOptions, mut f: F) -> io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = io::Result<T>>,
{
    let mut last: Option<io::Error> = None;
    for i in 0..=opts.retries {
        if i > 0 {
            let delay = opts.base_delay_ms * 2u64.pow(i - 1);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !is_retryable(&e) {
                    return Err(e);
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("with_retry: 未知失败")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_then_succeeds() {
        let mut n = 0;
        let r = with_retry(
            RetryOptions {
                retries: 3,
                base_delay_ms: 1,
            },
            || {
                n += 1;
                if n < 3 {
                    Err(io::Error::from_raw_os_error(libc::EBUSY))
                } else {
                    Ok(n)
                }
            },
        );
        assert_eq!(r.unwrap(), 3);
    }

    #[test]
    fn non_retryable_fails_immediately() {
        let mut n = 0;
        let r: io::Result<()> = with_retry(
            RetryOptions {
                retries: 3,
                base_delay_ms: 1,
            },
            || {
                n += 1;
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            },
        );
        assert!(r.is_err());
        assert_eq!(n, 1, "ENOENT 不应重试");
    }

    #[test]
    fn gives_up_after_retries() {
        let mut n = 0;
        let r: io::Result<()> = with_retry(
            RetryOptions {
                retries: 2,
                base_delay_ms: 1,
            },
            || {
                n += 1;
                Err(io::Error::from_raw_os_error(libc::EBUSY))
            },
        );
        assert!(r.is_err());
        assert_eq!(n, 3, "初始 1 次 + 重试 2 次");
    }
}
