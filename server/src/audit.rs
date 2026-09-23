//! 审计日志（§9.2：登录/权限变更/强制解锁/提交/管理操作全量落库）。

use crate::error::AppError;
use chrono::Utc;
use rusqlite::{params, Connection};

pub fn log(
    conn: &Connection,
    user_id: Option<i64>,
    repo_id: Option<i64>,
    action: &str,
    target: &str,
    detail: &str,
    ip: &str,
) -> Result<(), AppError> {
    conn.execute(
        "INSERT INTO audit_log (ts, user_id, repo_id, action, target, detail, ip)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![Utc::now().to_rfc3339(), user_id, repo_id, action, target, detail, ip],
    )?;
    Ok(())
}
