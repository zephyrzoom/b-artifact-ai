//! 配置：默认值 ← TOML 文件 ← 命令行覆盖（§11.2）。

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub db: DbConfig,
}

/// 元数据库配置（M1.5 B6：连接池）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DbConfig {
    /// 连接池上限。SQLite 在 WAL 下"多读单写"，池主要放大读并发；
    /// 写事务由 `BEGIN IMMEDIATE` + busy_timeout 串行，过大没有意义。
    pub pool_size: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: String,
    pub data_dir: String,
    pub base_url: String,
    /// 管理端前端构建产物目录（§8.1）。`/admin` 静态托管；
    /// 生产部署可直接把它指向与二进制同级的 `admin/dist`。
    pub admin_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// off | auto | always（默认 auto：zstd 收益 > 5% 才压缩）
    pub compression: String,
    pub max_file_size_mb: u64,
    /// ≥ 此大小走分块上传（§3.4）
    pub chunk_threshold_mb: u64,
}

/// 认证配置（§9.1）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub local_enabled: bool,
    /// users 表为空时，第一个成功登录的用户自动成为系统管理员
    pub first_user_admin: bool,
    pub session_ttl_days: u32,
    /// 登录失败退避：连续失败 N 次锁定 M 秒
    pub login_max_fails: u32,
    pub login_lockout_secs: u64,
    pub ldap: LdapConfig,
}

/// LDAP 配置（两段式 bind：服务账号搜索 + 用户 DN bind）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LdapConfig {
    pub enabled: bool,
    /// ldap:// 或 ldaps://（生产强制 ldaps 或置于 TLS 之后）
    pub url: String,
    pub bind_dn: String,
    pub bind_password: String,
    pub user_base: String,
    /// 搜索过滤器，{username} 占位
    pub user_filter: String,
    /// 明文 ldap:// 需显式开启（仅内网调试）
    pub allow_insecure: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: ServerConfig::default(),
            storage: StorageConfig::default(),
            auth: AuthConfig::default(),
            db: DbConfig::default(),
        }
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        // 运行期可覆盖（压测调参 / 运维临时收敛）：B_ARTIFACT_DB_POOL_SIZE
        let pool_size = std::env::var("B_ARTIFACT_DB_POOL_SIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n >= 1)
            // 默认 4：M1.6 实测（m 档完整跑，并发 8/32）4 与 2 互有胜负、
            // 但 8 在所有并发项上都明显更差（`download_blob_c8` 3529/s → 459/s），
            // 原因是查询修复后单条已足够快，连接再多只剩 WAL 读竞争。
            // 留 4 条是为了让长查询（历史树 6.5s 级）不至于堵死全部读。
            .unwrap_or(4);
        DbConfig { pool_size }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            listen: "127.0.0.1:8080".to_string(),
            data_dir: "./data".to_string(),
            base_url: String::new(),
            admin_dir: "admin/dist".to_string(),
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            compression: "auto".to_string(),
            max_file_size_mb: 8192,
            chunk_threshold_mb: 64,
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            local_enabled: true,
            first_user_admin: true,
            session_ttl_days: 30,
            login_max_fails: 5,
            login_lockout_secs: 60,
            ldap: LdapConfig::default(),
        }
    }
}

impl Default for LdapConfig {
    fn default() -> Self {
        LdapConfig {
            enabled: false,
            url: String::new(),
            bind_dn: String::new(),
            bind_password: String::new(),
            user_base: String::new(),
            user_filter: "(uid={username})".to_string(),
            allow_insecure: false,
        }
    }
}

impl Config {
    /// 读取 TOML 配置文件（可缺省，全部字段有默认值）。
    pub fn load(path: Option<&Path>) -> Result<Config, crate::error::AppError> {
        let mut cfg = Config::default();
        if let Some(p) = path {
            let raw = std::fs::read_to_string(p)?;
            let file_cfg: Config = toml::from_str(&raw).map_err(|e| {
                crate::error::AppError::InvalidArgument(format!("配置文件解析失败 {}: {e}", p.display()))
            })?;
            cfg = file_cfg;
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不传路径 = 全默认（CI / 冒烟 / 集成测试都走这条路）
    #[test]
    fn load_none_returns_defaults() {
        let c = Config::load(None).unwrap();
        assert_eq!(c.server.admin_dir, "admin/dist", "§8.1 默认托管目录");
        assert_eq!(c.server.listen, "127.0.0.1:8080");
        assert!(c.auth.local_enabled);
        assert!(c.auth.first_user_admin, "§9.1 首登管理员默认开");
        assert!(!c.auth.ldap.enabled);
        assert_eq!(c.auth.ldap.user_filter, "(uid={username})");
        assert_eq!(c.storage.compression, "auto");
        assert_eq!(c.storage.chunk_threshold_mb, 64);
        assert!(c.db.pool_size >= 1, "§M1.6 池大小至少 1");
    }

    /// TOML 覆盖：写了的生效、没写的回落到默认值
    #[test]
    fn load_toml_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b-artifact.toml");
        std::fs::write(
            &p,
            r#"
[server]
listen = "0.0.0.0:9000"
admin_dir = "/srv/admin"

[storage]
compression = "off"
max_file_size_mb = 1024

[auth]
session_ttl_days = 7
first_user_admin = false

[auth.ldap]
enabled = true
url = "ldaps://ldap.example.com"
user_base = "ou=people,dc=example,dc=com"
user_filter = "(sAMAccountName={username})"
"#,
        )
        .unwrap();

        let c = Config::load(Some(&p)).unwrap();
        assert_eq!(c.server.listen, "0.0.0.0:9000");
        assert_eq!(c.server.admin_dir, "/srv/admin");
        assert_eq!(c.storage.compression, "off");
        assert_eq!(c.storage.max_file_size_mb, 1024);
        assert_eq!(c.auth.session_ttl_days, 7);
        assert!(!c.auth.first_user_admin, "可关闭首登管理员");
        assert!(c.auth.ldap.enabled);
        assert_eq!(c.auth.ldap.url, "ldaps://ldap.example.com");
        assert_eq!(c.auth.ldap.user_base, "ou=people,dc=example,dc=com");
        assert_eq!(c.auth.ldap.user_filter, "(sAMAccountName={username})");
        // 文件里没写的仍走默认值
        assert!(c.auth.local_enabled);
        assert_eq!(c.storage.chunk_threshold_mb, 64);
        assert_eq!(c.auth.ldap.bind_dn, "");
    }

    /// 坏 TOML 应报 InvalidArgument 而不是 panic——启动时配置文件写错是常见运维事故
    #[test]
    fn load_bad_toml_is_invalid_argument() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.toml");
        std::fs::write(&p, "this is = not = valid toml").unwrap();
        let e = Config::load(Some(&p)).unwrap_err();
        assert!(
            matches!(e, crate::error::AppError::InvalidArgument(_)),
            "应报 InvalidArgument，实际: {e:?}"
        );
    }

    /// 配置文件不存在 → 透传 io 错误（不静默回退默认，否则路径写错会"看起来启动成功"）
    #[test]
    fn load_missing_file_errors() {
        let e = Config::load(Some(Path::new("/definitely/not/here.toml"))).unwrap_err();
        assert!(
            !matches!(e, crate::error::AppError::InvalidArgument(_)),
            "缺文件应报 io 错误而非解析错误: {e:?}"
        );
    }
}
