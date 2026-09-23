//! 认证模块（§3.5 / §9.1）：登录分发、JIT、首登管理员。

pub mod ldap;
pub mod password;
pub mod provision;
pub mod session;

use crate::config::AuthConfig;
use crate::error::AppError;
use chrono::Utc;
use rusqlite::{params, Connection};

/// 认证结果：Ok(Some(user_id)) 登录成功；Ok(None) 用户名或密码错误。
/// Err = 服务异常（LDAP 不可用 / 账号禁用等）。
pub enum AuthOutcome {
    Authenticated {
        user_id: i64,
        source: &'static str,
        /// LDAP 认证成功且需要 JIT 建号/刷新（调用方负责事务与审计）
        ldap_user: Option<ldap::LdapUser>,
    },
    InvalidCredentials,
}

/// 统一登录入口（§9.1 顺序）：
/// 1. 本地同名用户 → argon2 校验
/// 2. 否则 LDAP 启用 → 两段式 bind
/// 3. 都不适用 → 统一"用户名或密码错误"
pub fn authenticate(
    conn: &Connection,
    cfg: &AuthConfig,
    username: &str,
    password: &str,
) -> Result<AuthOutcome, AppError> {
    if username.is_empty() || password.is_empty() {
        return Ok(AuthOutcome::InvalidCredentials);
    }

    // 本地用户优先（local_enabled=false 时完全跳过本地口令校验，直接走 LDAP）
    let local: Option<(i64, Option<String>, i64)> = if cfg.local_enabled {
        conn.query_row(
            "SELECT id, password_hash, disabled FROM users
              WHERE username = ?1 AND source = 'local'",
            params![username],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    } else {
        None
    };
    if let Some((user_id, hash, disabled)) = local {
        if disabled != 0 {
            return Err(AppError::AccountDisabled(format!("账号 `{username}` 已被禁用")));
        }
        let ok = match hash.as_deref().and_then(|h| password::verify_password(password, h)) {
            Some(v) => v,
            None => false, // 本地账号必须有合法 argon2 哈希
        };
        return Ok(if ok {
            let _ = conn.execute(
                "UPDATE users SET last_login_at = ?2 WHERE id = ?1",
                params![user_id, Utc::now().to_rfc3339()],
            );
            AuthOutcome::Authenticated { user_id, source: "local", ldap_user: None }
        } else {
            AuthOutcome::InvalidCredentials
        });
    }

    // LDAP
    if cfg.ldap.enabled {
        match ldap::authenticate(&cfg.ldap, username, password)? {
            Some(lu) => return Ok(AuthOutcome::Authenticated {
                user_id: 0, // 未建号，由调用方 provision
                source: "ldap",
                ldap_user: Some(lu),
            }),
            None => return Ok(AuthOutcome::InvalidCredentials),
        }
    }

    Ok(AuthOutcome::InvalidCredentials)
}

/// 登录失败限流（内存态，§9.2 登录失败退避）。
pub struct LoginLimiter {
    max_fails: u32,
    lockout: std::time::Duration,
    state: std::sync::Mutex<std::collections::HashMap<String, FailState>>,
}

#[derive(Default)]
struct FailState {
    fails: u32,
    locked_until: Option<std::time::Instant>,
}

impl LoginLimiter {
    pub fn new(max_fails: u32, lockout_secs: u64) -> LoginLimiter {
        LoginLimiter {
            max_fails,
            lockout: std::time::Duration::from_secs(lockout_secs),
            state: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 命中锁定 → Err(429)
    pub fn check(&self, username: &str) -> Result<(), AppError> {
        let map = self.state.lock().unwrap();
        if let Some(fs) = map.get(username) {
            if let Some(until) = fs.locked_until {
                if until > std::time::Instant::now() {
                    return Err(AppError::RateLimited {
                        retry_after_secs: (until - std::time::Instant::now()).as_secs().max(1),
                        message: format!("登录失败次数过多，账号 `{username}` 已临时锁定"),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn record_fail(&self, username: &str) {
        let mut map = self.state.lock().unwrap();
        let fs = map.entry(username.to_string()).or_default();
        fs.fails += 1;
        if fs.fails >= self.max_fails {
            fs.fails = 0;
            fs.locked_until = Some(std::time::Instant::now() + self.lockout);
        }
    }

    pub fn record_success(&self, username: &str) {
        self.state.lock().unwrap().remove(username);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db;
    use crate::storage::repo::create_user;

    fn cfg() -> AuthConfig {
        AuthConfig::default()
    }

    #[test]
    fn local_login_flow() {
        let (_dir, conn) = db::tests::test_db();
        let ph = password::hash_password("pw123456").unwrap();
        conn.execute(
            "INSERT INTO users (username, password_hash, source, created_at)
             VALUES ('carol', ?1, 'local', '2026-01-01T00:00:00Z')",
            params![ph],
        )
        .unwrap();

        match authenticate(&conn, &cfg(), "carol", "pw123456").unwrap() {
            AuthOutcome::Authenticated { user_id, source, .. } => {
                assert!(user_id > 0);
                assert_eq!(source, "local");
            }
            _ => panic!("应登录成功"),
        }
        assert!(matches!(
            authenticate(&conn, &cfg(), "carol", "wrong").unwrap(),
            AuthOutcome::InvalidCredentials
        ));
        assert!(matches!(
            authenticate(&conn, &cfg(), "ghost", "x").unwrap(),
            AuthOutcome::InvalidCredentials
        ));
    }

    #[test]
    fn disabled_local_rejected() {
        let (_dir, conn) = db::tests::test_db();
        let uid = create_user(&conn, "dave", false).unwrap();
        let ph = password::hash_password("pw").unwrap();
        conn.execute(
            "UPDATE users SET password_hash = ?2 WHERE id = ?1",
            params![uid, ph],
        )
        .unwrap();
        conn.execute("UPDATE users SET disabled = 1 WHERE id = ?1", params![uid]).unwrap();
        assert!(matches!(
            authenticate(&conn, &cfg(), "dave", "pw"),
            Err(AppError::AccountDisabled(_))
        ));
    }

    #[test]
    fn limiter_locks_after_fails() {
        let limiter = LoginLimiter::new(3, 60);
        for _ in 0..2 {
            limiter.check("eve").unwrap();
            limiter.record_fail("eve");
        }
        limiter.check("eve").unwrap();
        limiter.record_fail("eve"); // 第 3 次 → 锁定
        assert!(matches!(limiter.check("eve"), Err(AppError::RateLimited { .. })));
        limiter.record_success("eve");
        limiter.check("eve").unwrap();
    }
}
