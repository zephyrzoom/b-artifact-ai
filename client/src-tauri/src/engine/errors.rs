/**
 * 错误模型（§7.1 错误响应统一结构 + 客户端本地错误）。
 *
 * 与 TS 版 `core/errors.ts` 一一对应：客户端所有分支判断都基于机器可读的 `code`，
 * `message` 只用于展示。**码字符串一个字都不能改** —— 渲染层靠它分支。
 */
use std::fmt;

use serde_json::Value;

/// 服务端错误码全表（§7.1）+ 客户端本地的 NETWORK / UNKNOWN。
pub const API_ERROR_CODES: &[&str] = &[
    "INVALID_ARGUMENT",
    "INVALID_PATH",
    "PATH_NOT_NORMALIZED",
    "UNAUTHENTICATED",
    "PERMISSION_DENIED",
    "ACCOUNT_DISABLED",
    "NOT_FOUND",
    "OUT_OF_DATE",
    "LOCKED",
    "NEEDS_LOCK",
    "NAME_COLLISION",
    "COMMIT_TOKEN_EXPIRED",
    "PURGE_BLOCKED",
    "HASH_MISMATCH",
    "PAYLOAD_TOO_LARGE",
    "REPO_QUOTA",
    "RATE_LIMITED",
    "INTERNAL",
    "MAINTENANCE",
    "NETWORK",
    "UNKNOWN",
];

/// 无需重试的错误码：重试只会得到同样的结果（§7.1「重试语义」）。
const NON_RETRYABLE: &[&str] = &[
    "INVALID_ARGUMENT",
    "INVALID_PATH",
    "PATH_NOT_NORMALIZED",
    "UNAUTHENTICATED",
    "PERMISSION_DENIED",
    "ACCOUNT_DISABLED",
    "NOT_FOUND",
    "NEEDS_LOCK",
    "NAME_COLLISION",
    "HASH_MISMATCH",
    "REPO_QUOTA",
];

/// 客户端统一错误（合并 TS 版的 `ApiError` 与 `WcError`：IPC 边界只需要 code/message/details）。
#[derive(Debug, Clone)]
pub struct ClientError {
    /// 机器可读错误码（渲染层按它分支）。
    pub code: String,
    /// 展示用文案。
    pub message: String,
    /// 附加上下文（缺省不写进信封 —— 渲染层的 `if (error.details)` 才干净）。
    pub details: Option<Value>,
    /// HTTP 状态码（本地错误为 0）。
    pub status: u16,
    /// `Retry-After` 折算出的毫秒数。
    pub retry_after_ms: Option<u64>,
}

impl ClientError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
            status: 0,
            retry_after_ms: None,
        }
    }

    /// 服务端返回的错误。
    pub fn api(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            ..Self::new(code, message)
        }
    }

    /// 工作副本本地错误（`NOT_A_WORKING_COPY` / `WC_BUSY` / `CONFLICT` …）。
    pub fn wc(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message)
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_retry_after(mut self, ms: u64) -> Self {
        self.retry_after_ms = Some(ms);
        self
    }

    /// 是否值得重试（§7.1：可安全重试 = GET/HEAD、PUT blob、分块 PUT、POST /commit）。
    pub fn retryable(&self) -> bool {
        if NON_RETRYABLE.contains(&self.code.as_str()) {
            return false;
        }
        // 4xx 中除 429 外一律不重试；5xx 与 429 退避重试。
        self.status == 429 || self.status >= 500
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::wc("IO", e.to_string())
    }
}

impl From<rusqlite::Error> for ClientError {
    fn from(e: rusqlite::Error) -> Self {
        ClientError::wc("WC_CORRUPT", e.to_string())
    }
}

pub type ClientResult<T> = Result<T, ClientError>;

/// 便捷构造：`wcerr!("NO_CHANGES", "…")`
#[macro_export]
macro_rules! wcerr {
    ($code:expr, $($arg:tt)*) => {
        $crate::engine::errors::ClientError::wc($code, format!($($arg)*))
    };
}
