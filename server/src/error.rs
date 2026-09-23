//! AppError → HTTP 响应映射（§7.1 错误码全表）。
//!
//! 所有非 2xx 响应统一结构：
//! ```json
//! { "error": { "code": "OUT_OF_DATE", "message": "...", "details": {} } }
//! ```

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    /// 400 INVALID_ARGUMENT
    InvalidArgument(String),
    /// 400 INVALID_PATH（§6.6 ③④：保留名/非法字符/超长/穿越）
    InvalidPath(String),
    /// 400 PATH_NOT_NORMALIZED（§6.6 ①）
    PathNotNormalized(String),
    /// 401 UNAUTHENTICATED
    Unauthenticated(String),
    /// 403 PERMISSION_DENIED
    PermissionDenied(String),
    /// 403 ACCOUNT_DISABLED
    AccountDisabled(String),
    /// 404 NOT_FOUND
    NotFound(String),
    /// 409 冲突类（OUT_OF_DATE / LOCKED / NEEDS_LOCK / NAME_COLLISION / COMMIT_TOKEN_EXPIRED / PURGE_BLOCKED）
    Conflict {
        code: &'static str,
        message: String,
        details: serde_json::Value,
    },
    /// 412 HASH_MISMATCH
    HashMismatch(String),
    /// 413 PAYLOAD_TOO_LARGE
    PayloadTooLarge(String),
    /// 429 RATE_LIMITED（Retry-After 头）
    RateLimited { retry_after_secs: u64, message: String },
    /// 500 INTERNAL
    Internal(String),
    /// 503 LDAP_UNAVAILABLE（不静默回退本地认证，§9.1）
    LdapUnavailable(String),
    /// 503 MAINTENANCE
    Maintenance(String),
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            AppError::InvalidArgument(_) => "INVALID_ARGUMENT",
            AppError::InvalidPath(_) => "INVALID_PATH",
            AppError::PathNotNormalized(_) => "PATH_NOT_NORMALIZED",
            AppError::Unauthenticated(_) => "UNAUTHENTICATED",
            AppError::PermissionDenied(_) => "PERMISSION_DENIED",
            AppError::AccountDisabled(_) => "ACCOUNT_DISABLED",
            AppError::NotFound(_) => "NOT_FOUND",
            AppError::Conflict { code, .. } => code,
            AppError::HashMismatch(_) => "HASH_MISMATCH",
            AppError::PayloadTooLarge(_) => "PAYLOAD_TOO_LARGE",
            AppError::RateLimited { .. } => "RATE_LIMITED",
            AppError::Internal(_) => "INTERNAL",
            AppError::LdapUnavailable(_) => "LDAP_UNAVAILABLE",
            AppError::Maintenance(_) => "MAINTENANCE",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            AppError::InvalidArgument(_)
            | AppError::InvalidPath(_)
            | AppError::PathNotNormalized(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            AppError::PermissionDenied(_) | AppError::AccountDisabled(_) => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict { .. } => StatusCode::CONFLICT,
            AppError::HashMismatch(_) => StatusCode::PRECONDITION_FAILED,
            AppError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::LdapUnavailable(_) | AppError::Maintenance(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        }
    }

    fn message(&self) -> String {
        match self {
            AppError::InvalidArgument(m)
            | AppError::InvalidPath(m)
            | AppError::PathNotNormalized(m)
            | AppError::Unauthenticated(m)
            | AppError::PermissionDenied(m)
            | AppError::AccountDisabled(m)
            | AppError::NotFound(m)
            | AppError::HashMismatch(m)
            | AppError::PayloadTooLarge(m)
            | AppError::Internal(m)
            | AppError::LdapUnavailable(m)
            | AppError::Maintenance(m) => m.clone(),
            AppError::Conflict { message, .. } => message.clone(),
            AppError::RateLimited { message, .. } => message.clone(),
        }
    }

    // ---- 冲突类构造器（§7.1） ----

    /// 409 OUT_OF_DATE：base_rev ≠ head_rev
    pub fn out_of_date(head_rev: i64, paths: Vec<String>) -> AppError {
        AppError::Conflict {
            code: "OUT_OF_DATE",
            message: "工作副本落后于服务端，请先 update".into(),
            details: json!({ "head_rev": head_rev, "paths": paths }),
        }
    }

    /// 409 COMMIT_TOKEN_EXPIRED：prepare 超 5 分钟
    pub fn commit_token_expired() -> AppError {
        AppError::Conflict {
            code: "COMMIT_TOKEN_EXPIRED",
            message: "commit_token 已过期，请重新 prepare（commit_id 不变）".into(),
            details: serde_json::Value::Null,
        }
    }

    /// 409 LOCKED：被他人锁阻塞
    pub fn locked(message: String, details: serde_json::Value) -> AppError {
        AppError::Conflict { code: "LOCKED", message, details }
    }

    /// 409 NEEDS_LOCK：提交的路径里存在本人未持锁的文件。
    ///
    /// v0.4.17：不再有 glob 过滤 —— **任何文件变更路径都要求本人持锁**（§5.2）。
    /// `locked_by` 是其中被他人持锁的那部分（客户端自动补锁时注定失败的路径）。
    pub fn needs_lock(paths: Vec<String>, locked_by: serde_json::Value) -> AppError {
        let has_others = locked_by.as_array().map(|a| !a.is_empty()).unwrap_or(false);
        AppError::Conflict {
            code: "NEEDS_LOCK",
            message: if has_others {
                "以下路径未由本人持锁（其中部分被他人持锁），无法提交".into()
            } else {
                "提交前必须对每个文件加锁（先锁后提交）".into()
            },
            details: json!({ "paths": paths, "locked_by": locked_by }),
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "code": self.code(),
                "message": self.message(),
                "details": match &self {
                    AppError::Conflict { details, .. } => details.clone(),
                    _ => serde_json::Value::Null,
                },
            }
        });
        let status = self.status();
        let mut resp = (status, Json(body)).into_response();
        if let AppError::RateLimited { retry_after_secs, .. } = &self {
            if let Ok(v) = i64::try_from(*retry_after_secs) {
                resp.headers_mut()
                    .insert("Retry-After", axum::http::HeaderValue::from(v));
            }
        }
        resp
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Internal(format!("database error: {e}"))
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Internal(format!("io error: {e}"))
    }
}
