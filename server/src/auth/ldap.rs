//! LDAP 认证（§9.1）：服务账号 bind + 用户 bind 两段式。
//!
//! 密码绝不落库；身份稳定标识 external_id（entryUUID）。
//! LDAP 不可用 → Err(LdapUnavailable)，不静默回退本地认证。

use crate::config::LdapConfig;
use crate::error::AppError;
use ldap3::{LdapConn, Scope, SearchEntry};

/// LDAP 认证成功返回的用户信息（用于 JIT 建号/刷新）。
#[derive(Debug, Clone)]
pub struct LdapUser {
    pub username: String,
    pub display_name: String,
    pub external_id: String,
    pub dn: String,
}

/// RFC 4515 过滤器转义。
fn ldap_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '*' => out.push_str("\\2a"),
            '(' => out.push_str("\\28"),
            ')' => out.push_str("\\29"),
            '\\' => out.push_str("\\5c"),
            '\0' => out.push_str("\\00"),
            _ => out.push(c),
        }
    }
    out
}

/// 两段式 bind 认证。返回 Ok(None) = 凭据错误/用户不存在；Err = LDAP 服务不可用等。
pub fn authenticate(cfg: &LdapConfig, username: &str, password: &str) -> Result<Option<LdapUser>, AppError> {
    if !cfg.enabled {
        return Ok(None);
    }
    if cfg.url.is_empty() || cfg.bind_dn.is_empty() || cfg.user_base.is_empty() {
        return Err(AppError::LdapUnavailable("LDAP 配置不完整（url/bind_dn/user_base）".into()));
    }
    // 明文 ldap:// 必须显式 allow_insecure（§9.1 传输约束）
    if cfg.url.starts_with("ldap://") && !cfg.allow_insecure {
        return Err(AppError::LdapUnavailable(
            "明文 ldap:// 需在配置中显式设置 allow_insecure = true".into(),
        ));
    }

    // 第一段：服务账号 bind + 搜索用户 DN
    let mut svc = LdapConn::new(&cfg.url)
        .map_err(|e| AppError::LdapUnavailable(format!("连接 LDAP 失败: {e}")))?;
    let bind = svc
        .simple_bind(&cfg.bind_dn, &cfg.bind_password)
        .map_err(|e| AppError::LdapUnavailable(format!("LDAP 服务账号 bind 失败: {e}")))?;
    if bind.rc != 0 {
        return Err(AppError::LdapUnavailable(format!(
            "LDAP 服务账号 bind 被拒绝: {} {}",
            bind.rc, bind.text
        )));
    }
    let filter = cfg.user_filter.replace("{username}", &ldap_escape(username));
    let sr = svc
        .search(&cfg.user_base, Scope::Subtree, &filter, &["cn", "mail", "entryUUID"])
        .map_err(|e| AppError::LdapUnavailable(format!("LDAP 搜索失败: {e}")))?;
    let (entries, _res) = sr
        .success()
        .map_err(|e| AppError::LdapUnavailable(format!("LDAP 搜索结果异常: {e}")))?;
    if entries.len() != 1 {
        // 0 个或多个匹配都按"认证失败"处理（不泄露账号存在性）
        return Ok(None);
    }
    let entry = SearchEntry::construct(entries.into_iter().next().unwrap());
    let dn = entry.dn;

    // 第二段：用户 DN + 用户密码 bind
    let mut usr = LdapConn::new(&cfg.url)
        .map_err(|e| AppError::LdapUnavailable(format!("连接 LDAP 失败: {e}")))?;
    let ubind = usr
        .simple_bind(&dn, password)
        .map_err(|e| AppError::LdapUnavailable(format!("LDAP 用户 bind 失败: {e}")))?;
    if ubind.rc != 0 {
        return Ok(None); // 密码错误
    }

    let ext = entry
        .attrs
        .get("entryUUID")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default();
    if ext.is_empty() {
        return Err(AppError::LdapUnavailable(
            "LDAP 条目缺少 entryUUID（external_id 不可用）".into(),
        ));
    }

    Ok(Some(LdapUser {
        username: username.to_string(),
        display_name: entry
            .attrs
            .get("cn")
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default(),
        external_id: ext,
        dn,
    }))
}
