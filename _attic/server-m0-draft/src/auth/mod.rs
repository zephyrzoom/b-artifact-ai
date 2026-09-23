pub mod password;
pub mod session;

use std::sync::Arc;

use rusqlite::{params, OptionalExtension};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Clone, serde::Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
}

impl User {
    pub(crate) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            username: row.get("username")?,
            display_name: row.get("display_name")?,
            is_admin: row.get::<_, i64>("is_admin")? != 0,
        })
    }
}

/// 校验用户名密码；成功返回用户，失败返回 None（不区分"无此用户"与"密码错误"）
pub fn authenticate(
    conn: &rusqlite::Connection,
    username: &str,
    password: &str,
) -> Result<Option<User>, AppError> {
    let row = conn
        .query_row(
            "SELECT id, username, display_name, is_admin, password_hash, disabled
               FROM users WHERE username = ?1",
            params![username],
            |r| {
                Ok((
                    r.get::<_, i64>("id")?,
                    r.get::<_, String>("username")?,
                    r.get::<_, String>("display_name")?,
                    r.get::<_, i64>("is_admin")?,
                    r.get::<_, String>("password_hash")?,
                    r.get::<_, i64>("disabled")?,
                ))
            },
        )
        .optional()?;

    let Some((id, name, display, is_admin, hash, disabled)) = row else {
        // 仍执行一次哈希，抹平"用户不存在"与"密码错误"的响应时延差
        let _ = password::hash_password(password);
        return Ok(None);
    };
    if disabled != 0 {
        return Err(AppError::Forbidden);
    }
    if !password::verify_password(password, &hash) {
        return Ok(None);
    }
    Ok(Some(User { id, username: name, display_name: display, is_admin: is_admin != 0 }))
}

/// users 表为空时创建管理员，密码未指定则随机生成并打印一次性到 stdout
pub fn bootstrap_admin(state: &Arc<AppState>, password: Option<&str>) -> anyhow::Result<()> {
    state.db.with(|conn| -> Result<(), AppError> {
        let count: i64 =
            conn.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
        if count > 0 {
            tracing::info!("已存在用户，跳过 bootstrap_admin");
            return Ok(());
        }

        let (pw, generated) = match password {
            Some(p) => (p.to_string(), false),
            None => (random_password(20), true),
        };
        let hash = password::hash_password(&pw)?;
        conn.execute(
            "INSERT INTO users(username, display_name, password_hash, is_admin, must_change_pw, created_at)
             VALUES('admin', '系统管理员', ?1, 1, 1, ?2)",
            params![hash, chrono::Utc::now().to_rfc3339()],
        )?;

        if generated {
            println!();
            println!("========================================================");
            println!("  已创建管理员账号 admin");
            println!("  初始密码（仅显示这一次）: {pw}");
            println!("  请登录后立即修改密码。");
            println!("========================================================");
            println!();
        } else {
            tracing::info!("已创建管理员账号 admin（密码来自命令行参数）");
        }
        Ok(())
    })?;
    Ok(())
}

fn random_password(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    use rand::RngCore;
    let mut buf = vec![0u8; len];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}
