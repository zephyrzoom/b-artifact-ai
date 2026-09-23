use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use crate::error::AppError;

/// argon2id（Argon2::default，RFC 9106 推荐参数）
pub fn hash_password(password: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::internal(format!("argon2: {e}")))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify() {
        let h = hash_password("s3cret-密码!").unwrap();
        assert!(h.starts_with("$argon2"));
        assert!(verify_password("s3cret-密码!", &h));
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn malformed_hash_is_rejected() {
        assert!(!verify_password("x", "not-a-hash"));
    }
}
