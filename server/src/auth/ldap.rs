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

/// 取唯一标识属性，**二进制值按 hex 存**（v0.4.24）。
///
/// AD 的 `objectGUID` 落在 `bin_attrs`（二进制）里，`attrs`（字符串 map）拿不到它。
/// 直接拿字符串会得到乱码，且乱码不稳定 —— 而这个值是 `external_id`（身份标识），
/// 不稳定意味着用户每次登录都被当成新账号重建。所以二进制一律 hex。
fn unique_id(entry: &SearchEntry, attr: &str) -> String {
    if let Some(bin) = entry.bin_attrs.get(attr) {
        if let Some(first) = bin.first() {
            if !first.is_empty() {
                return first.iter().map(|b| format!("{b:02x}")).collect::<String>();
            }
        }
    }
    entry
        .attrs
        .get(attr)
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default()
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
    // v0.4.24：属性名可配（AD 要用 objectGUID，不能写死 entryUUID）
    let attrs = [cfg.attr_display_name.as_str(), cfg.attr_unique_id.as_str()];
    let sr = svc
        .search(&cfg.user_base, Scope::Subtree, &filter, &attrs)
        .map_err(|e| AppError::LdapUnavailable(format!("LDAP 搜索失败: {e}")))?;
    let (entries, _res) = sr
        .success()
        .map_err(|e| AppError::LdapUnavailable(format!("LDAP 搜索结果异常: {e}")))?;
    if entries.len() != 1 {
        // 0 个或多个匹配都按"认证失败"处理（不泄露账号存在性）
        return Ok(None);
    }
    let entry = SearchEntry::construct(entries.into_iter().next().unwrap());
    let dn = entry.dn.clone();

    // 第二段：用户 DN + 用户密码 bind
    let mut usr = LdapConn::new(&cfg.url)
        .map_err(|e| AppError::LdapUnavailable(format!("连接 LDAP 失败: {e}")))?;
    let ubind = usr
        .simple_bind(&dn, password)
        .map_err(|e| AppError::LdapUnavailable(format!("LDAP 用户 bind 失败: {e}")))?;
    if ubind.rc != 0 {
        return Ok(None); // 密码错误
    }

    // v0.4.24：属性名可配 + 二进制值 hex 化
    let ext = unique_id(&entry, &cfg.attr_unique_id);
    if ext.is_empty() {
        return Err(AppError::LdapUnavailable(format!(
            "LDAP 条目缺少唯一标识属性 `{}`（external_id 不可用）",
            cfg.attr_unique_id
        )));
    }

    Ok(Some(LdapUser {
        username: username.to_string(),
        display_name: entry
            .attrs
            .get(&cfg.attr_display_name)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default(),
        external_id: ext,
        dn,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 直接实例化 `SearchEntry`——它的三个字段都是 pub，而 `construct()` 吃的是
    /// BER 原始字节（`ResultEntry`），测试里造不出也用不着。
    fn entry(
        attrs: &[(&str, &[&str])],
        bin_attrs: &[(&str, &[u8])],
    ) -> SearchEntry {
        SearchEntry {
            dn: "cn=x".to_string(),
            attrs: attrs
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        v.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                    )
                })
                .collect::<HashMap<_, _>>(),
            bin_attrs: bin_attrs
                .iter()
                .map(|(k, v)| (k.to_string(), vec![v.to_vec()]))
                .collect::<HashMap<_, _>>(),
        }
    }

    /// AD 的 objectGUID 是二进制：必须 hex 化，且**同一份字节每次得到同一串**——
    /// 它是 external_id，不稳定等于身份漂移（用户每次登录被当成新账号重建）。
    #[test]
    fn unique_id_hex_encodes_binary_guid() {
        let guid: &[u8] = &[0x00, 0x11, 0x22, 0xab, 0xff, 0xfe];
        let e = entry(&[("cn", &["Tester"])], &[("objectGUID", guid)]);
        let a = unique_id(&e, "objectGUID");
        assert_eq!(a, "001122abfffe");
        // 稳定性：重复取值一致（否则每次登录都换账号）
        assert_eq!(a, unique_id(&e, "objectGUID"));
        assert_eq!(a.len(), guid.len() * 2, "hex 长度应为字节数的两倍");
    }

    /// 属性不存在 / 空字节 → 空串，调用方据此报"缺少唯一标识属性"，而不是建出 id 为空的用户
    #[test]
    fn unique_id_empty_when_absent() {
        let e = entry(&[("cn", &["Tester"])], &[]);
        assert_eq!(unique_id(&e, "objectGUID"), "");
        assert_eq!(unique_id(&e, "entryUUID"), "");

        let empty = entry(&[], &[("objectGUID", &[])]);
        assert_eq!(unique_id(&empty, "objectGUID"), "");
    }

    /// 纯文本属性（OpenLDAP 的 entryUUID）走字符串形态也能取值
    #[test]
    fn unique_id_falls_back_to_string_attr() {
        let e = entry(&[("entryUUID", &["abc-123"])], &[("entryUUID", &[])]);
        assert_eq!(unique_id(&e, "entryUUID"), "abc-123");
    }

    /// 过滤器转义：用户输入里的元字符不能拼进过滤器改变查询语义
    #[test]
    fn filter_escapes_metacharacters() {
        assert_eq!(ldap_escape("a*b"), "a\\2ab");
        assert_eq!(ldap_escape("x)(cn=*"), "x\\29\\28cn=\\2a");
        assert_eq!(ldap_escape("normal"), "normal");
    }

    /// 前置校验必须发生在任何网络调用之前：配置不全/ 未允许明文时立刻返回，
    /// 不能真去连一个不存在的地址（单测里一发网络就会超时挂住）。
    #[test]
    fn preflight_validation_happens_before_network() {
        let mut cfg = LdapConfig::default();
        cfg.enabled = false;
        assert!(matches!(authenticate(&cfg, "u", "p"), Ok(None)));

        let mut cfg = LdapConfig::default();
        cfg.enabled = true;
        cfg.user_base = String::new(); // url/bind_dn/user_base 有缺失
        let e = authenticate(&cfg, "u", "p").unwrap_err();
        assert!(e.to_string().contains("配置不完整"), "实际: {e}");

        // 明文 ldap:// 未显式允许 → 拒绝
        let mut cfg = LdapConfig::default();
        cfg.enabled = true;
        cfg.url = "ldap://ldap.example.com".to_string();
        cfg.bind_dn = "cn=svc".to_string();
        cfg.user_base = "ou=p,dc=e".to_string();
        let e = authenticate(&cfg, "u", "p").unwrap_err();
        assert!(e.to_string().contains("allow_insecure"), "实际: {e}");
    }
}
