//! 会话：32 字节 CSPRNG token，DB 只存 sha256（§9.2）。

use crate::error::AppError;
use chrono::{Duration, Utc};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct SessionUser {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub source: String,
    pub is_admin: bool,
    pub disabled: bool,
    /// 所属组 id（§4.2 `pick` 的 group 主体需要）。会话签发时快照，改组后需重新登录生效。
    pub groups: Vec<i64>,
}

/// 签发会话。返回 (明文 token, expires_at)。
pub fn issue(
    conn: &Connection,
    user_id: i64,
    ttl_days: u32,
    user_agent: &str,
) -> Result<(String, String), AppError> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw)
        .map_err(|e| AppError::Internal(format!("CSPRNG: {e}")))?;
    let token = hex::encode(raw);
    let expires_at = (Utc::now() + Duration::days(ttl_days as i64)).to_rfc3339();
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sessions (token, user_id, created_at, expires_at, user_agent, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?3)",
        params![sha256_hex(&token), user_id, now, expires_at, user_agent],
    )?;
    Ok((token, expires_at))
}

/// 校验 token → 用户。过期/吊销/禁用 → None（禁用单独返回以便 403 ACCOUNT_DISABLED）。
///
/// v0.4.17：不再读 `users.email`（该列保留但不再对外暴露，见 §3.2）。
pub fn resolve(conn: &Connection, token: &str) -> Result<Option<SessionUser>, AppError> {
    let row = conn.query_row(
        "SELECT u.id, u.username, u.display_name, u.source, u.is_admin, u.disabled,
                s.expires_at
           FROM sessions s JOIN users u ON u.id = s.user_id
          WHERE s.token = ?1",
        params![sha256_hex(token)],
        |r| {
            Ok((
                (
                    r.get(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)? != 0,
                    r.get::<_, i64>(5)? != 0,
                ),
                r.get::<_, String>(6)?,
            ))
        },
    );
    match row {
        Ok((fields, expires_at)) => {
            let (id, username, display_name, source, is_admin, disabled) = fields;
            if disabled {
                return Ok(None); // 禁用账号：会话立即失效
            }
            if expires_at <= Utc::now().to_rfc3339() {
                return Ok(None);
            }
            let groups = load_groups(conn, id)?;
            let user = SessionUser {
                id,
                username,
                display_name,
                source,
                is_admin,
                disabled,
                groups,
            };
            // 惰性刷新 last_seen（失败可忽略）
            let _ = conn.execute(
                "UPDATE sessions SET last_seen = ?2 WHERE token = ?1",
                params![sha256_hex(token), Utc::now().to_rfc3339()],
            );
            Ok(Some(user))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 吊销 token。
pub fn revoke(conn: &Connection, token: &str) -> Result<(), AppError> {
    conn.execute(
        "DELETE FROM sessions WHERE token = ?1",
        params![sha256_hex(token)],
    )?;
    Ok(())
}

/// 吊销该用户除 keep_token 外的所有会话（改密后强制其他端下线）。
pub fn revoke_others(conn: &Connection, user_id: i64, keep_token: &str) -> Result<usize, AppError> {
    let n = conn.execute(
        "DELETE FROM sessions WHERE user_id = ?1 AND token <> ?2",
        params![user_id, sha256_hex(keep_token)],
    )?;
    Ok(n)
}

/// 加载用户所属组 id（§4.2：`pick` 里 group 主体的候选来源）。
pub fn load_groups(conn: &Connection, user_id: i64) -> Result<Vec<i64>, AppError> {
    let mut stmt = conn.prepare("SELECT group_id FROM group_members WHERE user_id = ?1")?;
    let rows = stmt.query_map(params![user_id], |r| r.get(0))?;
    let mut out = Vec::new();
    for g in rows {
        out.push(g?);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db;
    use crate::storage::repo::create_user;

    #[test]
    fn issue_resolve_revoke() {
        let (_dir, conn) = db::tests::test_db();
        let uid = create_user(&conn, "alice", false).unwrap();

        let (token, expires) = issue(&conn, uid, 30, "test-agent").unwrap();
        assert!(expires.as_str() > "2026");
        assert_ne!(token, sha256_hex(&token), "DB 存的必须是哈希，明文不落库");

        let user = resolve(&conn, &token).unwrap().unwrap();
        assert_eq!(user.username, "alice");

        revoke(&conn, &token).unwrap();
        assert!(resolve(&conn, &token).unwrap().is_none());
    }

    #[test]
    fn disabled_user_session_invalid() {
        let (_dir, conn) = db::tests::test_db();
        let uid = create_user(&conn, "bob", false).unwrap();
        let (token, _) = issue(&conn, uid, 30, "").unwrap();
        assert!(resolve(&conn, &token).unwrap().is_some());
        conn.execute("UPDATE users SET disabled = 1 WHERE id = ?1", params![uid]).unwrap();
        assert!(resolve(&conn, &token).unwrap().is_none(), "禁用后会话立即失效");
    }
}
