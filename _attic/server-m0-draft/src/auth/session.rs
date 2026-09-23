use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use super::User;
use crate::error::AppError;

pub struct Issued {
    pub token: String,
    pub expires_at: String,
}

/// 签发 opaque token：库里只存 sha256(token)，防库泄露后直接拿到可用凭据
pub fn create(
    conn: &Connection,
    user_id: i64,
    ttl_days: i64,
    user_agent: &str,
) -> Result<Issued, AppError> {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    let token = hex::encode(buf);
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));

    let now = Utc::now();
    let expires = now + Duration::days(ttl_days);
    conn.execute(
        "INSERT INTO sessions(token, user_id, created_at, expires_at, user_agent, last_seen)
         VALUES(?1, ?2, ?3, ?4, ?5, ?3)",
        params![token_hash, user_id, now.to_rfc3339(), expires.to_rfc3339(), user_agent],
    )?;
    Ok(Issued { token, expires_at: expires.to_rfc3339() })
}

pub fn resolve(conn: &Connection, token: &str) -> Result<Option<User>, AppError> {
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
    let now = Utc::now().to_rfc3339();

    let user = conn
        .query_row(
            "SELECT u.id, u.username, u.display_name, u.is_admin
               FROM sessions s JOIN users u ON u.id = s.user_id
              WHERE s.token = ?1 AND s.expires_at > ?2 AND u.disabled = 0",
            params![token_hash, now],
            User::from_row,
        )
        .optional()?;
    Ok(user)
}

pub fn revoke(conn: &Connection, token: &str) -> Result<bool, AppError> {
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
    let n = conn.execute("DELETE FROM sessions WHERE token = ?1", params![token_hash])?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password;
    use crate::storage::db::Db;

    fn user_id(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO users(username, password_hash, is_admin, created_at) VALUES('tester', ?1, 0, ?2)",
            params![password::hash_password("pw").unwrap(), Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn create_then_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let (token, uid) = db
            .with(|c| {
                let uid = user_id(c);
                let s = create(c, uid, 1, "test-agent")?;
                Ok((s.token, uid))
            })
            .unwrap();

        let user = db.with(|c| resolve(c, &token)).unwrap().unwrap();
        assert_eq!(user.id, uid);
        assert_eq!(user.username, "tester");
    }

    #[test]
    fn resolve_rejects_tampered_and_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let token = db
            .with(|c| {
                let uid = user_id(c);
                Ok(create(c, uid, 1, "").unwrap().token)
            })
            .unwrap();

        let mut tampered = token.clone();
        tampered.replace_range(0..1, "0");
        assert!(db.with(|c| resolve(c, &tampered)).unwrap().is_none());

        db.with(|c| revoke(c, &token)).unwrap();
        assert!(db.with(|c| resolve(c, &token)).unwrap().is_none());
    }
}
