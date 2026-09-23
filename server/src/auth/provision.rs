//! JIT 账号供给（§9.1）：LDAP 登录成功后建号/刷新信息；密码绝不落库。

use crate::auth::ldap::LdapUser;
use crate::error::AppError;
use chrono::Utc;
use rusqlite::{params, Connection};

/// upsert：不存在则建号（source=ldap），存在则刷新 display_name / ldap_dn。
/// v0.4.17：不再读写 `email`（该列保留但不再对外暴露，LDAP 侧也不再读取 `mail` 属性）。
/// 返回 user_id。同名本地用户存在 → Err（username_conflict = reject，需管理员处理）。
pub fn provision(conn: &Connection, lu: &LdapUser) -> Result<i64, AppError> {
    // 同名 local 用户 → 拒绝（不自动合并，§9.1 安全约束）
    let local_conflict: Option<i64> = conn
        .query_row(
            "SELECT id FROM users WHERE username = ?1 AND source = 'local'",
            params![lu.username],
            |r| r.get(0),
        )
        .ok();
    if local_conflict.is_some() {
        return Err(AppError::InvalidArgument(format!(
            "用户名 `{}` 与本地账号冲突，请联系管理员处理（LDAP 建号被拒绝）",
            lu.username
        )));
    }

    // 以 external_id 为稳定标识 upsert
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM users WHERE source = 'ldap' AND external_id = ?1",
            params![lu.external_id],
            |r| r.get(0),
        )
        .ok();
    if let Some(id) = existing {
        conn.execute(
            "UPDATE users SET display_name = ?2, ldap_dn = ?3, last_login_at = ?4
              WHERE id = ?1",
            params![id, lu.display_name, lu.dn, Utc::now().to_rfc3339()],
        )?;
        return Ok(id);
    }

    // external_id 无记录：username 可能已被另一个 ldap 账号占用（改名场景）
    let by_name: Option<i64> = conn
        .query_row(
            "SELECT id FROM users WHERE username = ?1 AND source = 'ldap'",
            params![lu.username],
            |r| r.get(0),
        )
        .ok();
    if let Some(id) = by_name {
        // 同名 ldap 账号但 external_id 不同：视为身份变更（DN/用户名复用），绑定到新 external_id
        conn.execute(
            "UPDATE users SET external_id = ?2, display_name = ?3, ldap_dn = ?4,
                    last_login_at = ?5
              WHERE id = ?1",
            params![id, lu.external_id, lu.display_name, lu.dn, Utc::now().to_rfc3339()],
        )?;
        return Ok(id);
    }

    conn.execute(
        "INSERT INTO users (username, display_name, source, ldap_dn, external_id,
                            is_admin, disabled, created_at, last_login_at)
         VALUES (?1, ?2, 'ldap', ?3, ?4, 0, 0, ?5, ?5)",
        params![
            lu.username,
            lu.display_name,
            lu.dn,
            lu.external_id,
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 首个登录用户 = 系统管理员（§9.1）。
/// 调用方须在事务内调用（users 计数与授予原子，SQLite 串行写规避竞争）。
/// 返回 true = 本次授予。
pub fn grant_first_admin_if_needed(conn: &Connection, user_id: i64) -> Result<bool, AppError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if count == 1 {
        conn.execute("UPDATE users SET is_admin = 1 WHERE id = ?1", params![user_id])?;
        crate::audit::log(
            conn,
            Some(user_id),
            None,
            "user.first_admin",
            &format!("user:{user_id}"),
            "users 表为空，首个登录用户自动成为系统管理员",
            "",
        )?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db;

    fn ldap_user(name: &str, ext: &str) -> LdapUser {
        LdapUser {
            username: name.into(),
            display_name: format!("DN of {name}"),
            external_id: ext.into(),
            dn: format!("uid={name},ou=people,dc=example,dc=com"),
        }
    }

    #[test]
    fn provision_creates_then_refreshes() {
        let (_dir, conn) = db::tests::test_db();
        let id1 = provision(&conn, &ldap_user("alice", "uuid-1")).unwrap();
        // 再登录：同 external_id → 刷新而非新建
        let id2 = provision(&conn, &ldap_user("alice", "uuid-1")).unwrap();
        assert_eq!(id1, id2);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        // 密码不落库
        let ph: Option<String> = conn
            .query_row("SELECT password_hash FROM users WHERE id=?1", params![id1], |r| r.get(0))
            .unwrap();
        assert!(ph.is_none());
    }

    #[test]
    fn provision_rejects_local_name_conflict() {
        let (_dir, conn) = db::tests::test_db();
        conn.execute(
            "INSERT INTO users (username, source, created_at) VALUES ('bob', 'local', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        assert!(provision(&conn, &ldap_user("bob", "uuid-9")).is_err());
    }

    #[test]
    fn first_admin_only_once() {
        let (_dir, conn) = db::tests::test_db();
        let u1 = provision(&conn, &ldap_user("first", "uuid-a")).unwrap();
        assert!(grant_first_admin_if_needed(&conn, u1).unwrap(), "首个用户应被授予");
        let admin: i64 = conn
            .query_row("SELECT is_admin FROM users WHERE id=?1", params![u1], |r| r.get(0))
            .unwrap();
        assert_eq!(admin, 1);
        let u2 = provision(&conn, &ldap_user("second", "uuid-b")).unwrap();
        assert!(!grant_first_admin_if_needed(&conn, u2).unwrap(), "第二个人不再授予");
        let admin2: i64 = conn
            .query_row("SELECT is_admin FROM users WHERE id=?1", params![u2], |r| r.get(0))
            .unwrap();
        assert_eq!(admin2, 0);
    }
}
