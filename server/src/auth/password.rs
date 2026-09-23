//! 本地口令：argon2id（§9.2：m=64MiB, t=3, p=4）+ 口令强度策略。

use crate::error::AppError;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

/// 口令强度下限（字符数）与上限。
pub const MIN_LEN: usize = 8;
pub const MAX_LEN: usize = 256;

/// 口令强度策略（§9.2，v0.4.17）。**唯一的强度校验入口** ——
/// 建用户 / 重置密码 / 改密 / CLI 建管理员四条路径都调它，避免各处口径漂移。
///
/// 规则：长度 **≥8 个字符**（按字符算，不是字节）且 ≤256，并且必须同时含
/// **数字 / 大写字母 / 小写字母 / 符号** 四类。
///
/// 两处刻意的取舍：
///
/// 1. **按字符计数**。原来四处校验都写 `String::len() < 8`，数的是 UTF-8 字节 ——
///    `密码密码密码` 只有 3 个字符却能通过。改用 `chars().count()`。
/// 2. **"符号"= 非字母数字的可打印字符**（含中文）。中文密码里的汉字算符号，
///    但"全中文口令"会因为缺数字与大写字母被拒 —— 这是有意的：它对暴力破解太弱。
///
/// **登录路径不调用本函数**：LDAP 用户的口令策略在目录服务侧，在这里拦会把
/// 存量老账号直接锁在门外。
pub fn validate_policy(password: &str) -> Result<(), AppError> {
    let n = password.chars().count();
    if n < MIN_LEN {
        return Err(AppError::InvalidArgument(format!(
            "密码至少 {MIN_LEN} 个字符（当前 {n} 个）"
        )));
    }
    if n > MAX_LEN {
        return Err(AppError::InvalidArgument(format!(
            "密码过长（{n} 字符，上限 {MAX_LEN}）"
        )));
    }
    let mut has_digit = false;
    let mut has_upper = false;
    let mut has_lower = false;
    let mut has_symbol = false;
    for c in password.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c.is_ascii_uppercase() {
            has_upper = true;
        } else if c.is_ascii_lowercase() {
            has_lower = true;
        } else {
            // 剩下的都算符号：ASCII 标点 + 非 ASCII（中文等）
            has_symbol = true;
        }
    }
    let mut missing: Vec<&str> = vec![];
    if !has_digit {
        missing.push("数字");
    }
    if !has_upper {
        missing.push("大写字母");
    }
    if !has_lower {
        missing.push("小写字母");
    }
    if !has_symbol {
        missing.push("符号");
    }
    if !missing.is_empty() {
        return Err(AppError::InvalidArgument(format!(
            "密码必须同时包含数字、大写字母、小写字母、符号（缺少：{}）",
            missing.join("、")
        )));
    }
    Ok(())
}

/// 推荐参数：m=64MiB(65536 KiB), t=3, p=4（§9.2）。
fn argon2() -> Argon2<'static> {
    use argon2::Params;
    let params = Params::new(65536, 3, 4, None)
        .expect("argon2 params");
    Argon2::from(params)
}

/// 明文 → PHC 字符串（含随机盐）。
/// 盐直接由 getrandom 生成（不依赖 rand_core 的 OsRng feature 组合）。
pub fn hash_password(password: &str) -> Result<String, AppError> {
    let mut salt_bytes = [0u8; 16];
    getrandom::getrandom(&mut salt_bytes)
        .map_err(|e| AppError::Internal(format!("CSPRNG: {e}")))?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| AppError::Internal(format!("盐编码失败: {e}")))?;
    let hash = argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AppError::Internal(format!("argon2 hash: {e}")))?;
    Ok(hash.to_string())
}

/// 校验明文与 PHC 串；`None` = hash 非法（按验证失败处理）。
pub fn verify_password(password: &str, phc: &str) -> Option<bool> {
    let parsed = PasswordHash::new(phc).ok()?;
    Some(argon2().verify_password(password.as_bytes(), &parsed).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_roundtrip() {
        let phc = hash_password("s3cret-PW!").unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert_eq!(verify_password("s3cret-PW!", &phc), Some(true));
        assert_eq!(verify_password("wrong", &phc), Some(false));
        assert_eq!(verify_password("x", "not-a-phc"), None);
    }

    /// 强度策略的完整边界表（§12.2 `auth` 行要求的用例集）。
    #[test]
    fn policy_accepts_only_strong_passwords() {
        // 合格
        for ok in ["Abcdefg1!", "P@ssw0rd", "aB3!aB3!", "Aa1!aaaa", "ZZzz0!xx"] {
            assert!(validate_policy(ok).is_ok(), "应当通过：{ok}");
        }
    }

    #[test]
    fn policy_rejects_short_and_overlong() {
        // 7 字符（四类齐备）→ 拒
        assert!(validate_policy("Ab1!abc").is_err(), "7 字符应被拒");
        // 8 字符（四类齐备）→ 过，作为下限的对照
        assert!(validate_policy("Ab1!abcd").is_ok(), "8 字符应通过");
        // 257 字符 → 拒
        let long = format!("aB1!{}", "a".repeat(MAX_LEN));
        assert!(long.chars().count() > MAX_LEN);
        assert!(validate_policy(&long).is_err(), "超长应被拒");
    }

    #[test]
    fn policy_requires_all_four_classes() {
        assert!(validate_policy("12345678").is_err(), "缺大写/小写/符号");
        assert!(validate_policy("abcdefgh").is_err(), "缺数字/大写/符号");
        assert!(validate_policy("ABCDEFGH").is_err(), "缺数字/小写/符号");
        assert!(validate_policy("Abcdefgh").is_err(), "缺数字/符号");
        assert!(validate_policy("Abcdefg1").is_err(), "缺符号");
        assert!(validate_policy("abcdefg1").is_err(), "缺大写");
        assert!(validate_policy("ABCDEFG1").is_err(), "缺小写");
        assert!(validate_policy("Abcdefg!").is_err(), "缺数字");
        // 错误信息要点名缺了什么，否则用户只能靠猜
        let e = validate_policy("Abcdefgh").unwrap_err();
        assert!(format!("{e}").contains("符号"), "错误信息应指出缺失类别：{e}");
    }

    /// 汉字的两种口径：按字符计数不按字节；汉字算"符号"但不能凑齐四类。
    #[test]
    fn policy_counts_chars_not_bytes() {
        // 5 个汉字 = 10 字节、但只有 5 个字符 → 必须被拒（旧实现 `len() < 8` 数的是字节，会放行）
        assert!(validate_policy("密码密码密").is_err(), "汉字要按字符计数");
        // 8 个汉字：字符数够，但缺数字/大写/小写 → 拒
        assert!(validate_policy("一二三四五六七八").is_err());
        // 汉字 + ASCII 三类 → 通过（汉字充当"符号"）
        assert!(validate_policy("密码密码密码Ab1").is_ok(), "汉字可作符号，但数字与大小写仍需齐备");
    }
}
