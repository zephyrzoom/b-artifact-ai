//! 配置：默认值 ← TOML 文件 ← 命令行覆盖（§11.2）。

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub db: DbConfig,
}

/// 元数据库配置（M1.5 B6：连接池）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbConfig {
    /// 连接池上限。SQLite 在 WAL 下"多读单写"，池主要放大读并发；
    /// 写事务由 `BEGIN IMMEDIATE` + busy_timeout 串行，过大没有意义。
    pub pool_size: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub data_dir: String,
    pub base_url: String,
    /// 管理端前端构建产物目录（§8.1）。`/admin` 静态托管；
    /// 生产部署可直接把它指向与二进制同级的 `admin/dist`。
    pub admin_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// off | auto | always（默认 auto：zstd 收益 > 5% 才压缩）
    pub compression: String,
    pub max_file_size_mb: u64,
    /// ≥ 此大小走分块上传（§3.4）
    pub chunk_threshold_mb: u64,
}

/// 认证配置（§9.1）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct LdapConfig {
    pub enabled: bool,
    /// ldap:// 或 ldaps://（生产强制 ldaps 或置于 TLS 之后）
    pub url: String,
    pub bind_dn: String,
    pub bind_password: String,
    pub user_base: String,
    /// 搜索过滤器，{username} 占位
    pub user_filter: String,
    /// 显示名属性（v0.4.24：原为硬编码 cn）
    pub attr_display_name: String,
    /// 稳定唯一标识属性（v0.4.24：原为硬编码 entryUUID）。OpenLDAP = entryUUID，
    /// AD = objectGUID（**二进制值**，读回时按 hex 存，见 ldap.rs）。
    pub attr_unique_id: String,
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
            attr_display_name: "cn".to_string(),
            attr_unique_id: "entryUUID".to_string(),
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

    /// 配置文件名（探测用；改这个名字要同步改 build.yml 里的 example 文案）。
    pub const FILE_NAME: &'static str = "b-artifact.toml";

    /// 候选路径中挑第一个真实存在的文件（探测的纯逻辑，便于直接单测）。
    pub fn first_existing(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
        candidates.into_iter().find(|p| p.is_file())
    }

    /// 未显式指定 `--config` 时的默认探测（v0.4.24）。
    ///
    /// **为什么需要**：此前不带 `--config` 就是全默认值，一个文件都不找。
    /// 交付包里的 `b-artifact.toml` 放在 exe 同目录，用户按README 改完却"看起来没生效"，
    /// 而服务端既不报错也不提示 —— 配置静默失效是最难排查的一类问题。
    ///
    /// 顺序：**exe 同目录 → 当前工作目录**。exe 同目录优先，因为相对路径（`admin_dir = "admin/dist"`）
    /// 在服务化/计划任务场景下 CWD 常常是 `C:\Windows\System32`，此时只有 exe 同目录才认得出
    /// "管理员解压出来的就是这一份"。
    pub fn discover_default() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join(Self::FILE_NAME));
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(Self::FILE_NAME));
        }
        Self::first_existing(candidates)
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

    /// v0.4.24：**写错的配置项必须报错，不能静默忽略**。
    /// 这是本轮修复的核心——文档示例曾把 LDAP 写成顶层 `[ldap]`（实际是 `[auth.ldap]`），
    /// 而 `deny_unknown_fields` 缺席时该节被整段丢弃：LDAP 永远不启用、服务照常启动、无任何日志。
    /// 配置静默失效比配置报错难查得多，所以这条是硬门禁。
    #[test]
    fn unknown_top_level_section_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b-artifact.toml");
        // 复现真实事故：LDAP 写在顶层而不是 [auth] 下
        std::fs::write(
            &p,
            r#"
[ldap]
enabled = true
url = "ldaps://ldap.example.com"
bind_dn = "cn=svc,dc=example,dc=com"
bind_password = "x"
user_base = "ou=people,dc=example,dc=com"
"#,
        )
        .unwrap();
        let e = Config::load(Some(&p)).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("ldap"),
            "报错信息应指出未知项 ldap，实际: {msg}"
        );
    }

    /// 同上，覆盖文档 §11.2 里另外几项"看起来能配、其实不存在"的字段。
    #[test]
    fn unknown_fields_in_sub_sections_are_rejected() {
        for (name, toml_text, needle) in [
            ("auth", "[auth]\nmax_login_failures = 5\n", "max_login_failures"),
            (
                "ldap",
                "[auth.ldap]\nenabled = true\nattr_uid = \"objectGUID\"\n",
                "attr_uid",
            ),
            (
                "server",
                "[server]\ntls_cert = \"/etc/tls.crt\"\n",
                "tls_cert",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join(name);
            std::fs::write(&p, toml_text).unwrap();
            let e = Config::load(Some(&p)).unwrap_err();
            let msg = e.to_string();
            assert!(
                msg.contains(needle),
                "[{name}] 的未知项 {needle} 应被拒，实际: {msg}"
            );
        }
    }

    /// 加了 deny_unknown_fields 后，合法配置仍要能解析（防止把门禁做成"一律拒绝"）。
    #[test]
    fn known_fields_still_parse_after_deny_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ok.toml");
        std::fs::write(
            &p,
            r#"
[server]
listen = "0.0.0.0:9000"

[db]
pool_size = 8

[auth.ldap]
enabled = true
url = "ldaps://ad.example.com:636"
attr_display_name = "cn"
attr_unique_id = "objectGUID"
allow_insecure = false
"#,
        )
        .unwrap();
        let c = Config::load(Some(&p)).unwrap();
        assert_eq!(c.server.listen, "0.0.0.0:9000");
        assert_eq!(c.db.pool_size, 8);
        assert!(c.auth.ldap.enabled);
        assert_eq!(c.auth.ldap.attr_unique_id, "objectGUID", "AD 用 objectGUID");
    }

    /// 端到端：交付包里的 `b-artifact.toml.example`（CI 由 build.yml 生成）必须能被解析。
    /// 锁死两个反复踩过的错：**LDAP 节写错位置**（顶层 `[ldap]` → 实际是 `[auth.ldap]`）、
    /// **`compression = "zstd"`**（非法值，合法只有 off/auto/always）。
    /// 这两处过去都没人报错——静默忽略的代价是"配了等于没配"。
    #[test]
    fn delivery_example_toml_is_actually_loadable() {
        let wf = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.github/workflows/build.yml");
        let raw = std::fs::read_to_string(&wf).expect("build.yml 应存在");

        // 取heredoc 内容并去掉公共缩进，模拟 shell 展开后的样子
        let start = raw
            .find("<<'TOML'")
            .expect("build.yml 应有 TOML heredoc");
        let body_start = start + "<<'TOML'".len();
        let end = body_start + raw[body_start..].find("\n          TOML").expect("heredoc 应闭合");
        let body = &raw[body_start..end];
        let indent = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);
        let toml_text: String = body
            .lines()
            .map(|l| if l.len() > indent { &l[indent..] } else { "" })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !toml_text.trim().is_empty(),
            "从 build.yml 提取到的示例配置为空，提取逻辑该跟 CI 的 heredoc 同步更新"
        );

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(Config::FILE_NAME);
        std::fs::write(&p, &toml_text).unwrap();

        let c = Config::load(Some(&p))
            .unwrap_or_else(|e| panic!("交付包示例配置必须能加载，但报错了: {e}"));

        // 抽查几项，确认解析结果符合示例声明的意图
        assert_eq!(c.server.listen, "0.0.0.0:9000");
        assert_eq!(c.storage.compression, "auto", "示例里的值必须合法");
        assert!(
            matches!(c.storage.compression.as_str(), "off" | "auto" | "always"),
            "compression 只接受 off|auto|always，实际 {}",
            c.storage.compression
        );
        assert_eq!(c.auth.login_max_fails, 5);
        assert!(c.auth.first_user_admin);
        // LDAP 默认关闭但字段齐全
        assert!(!c.auth.ldap.enabled, "示例默认不应真开域账号登录");
        assert_eq!(c.auth.ldap.user_filter, "(uid={username})");
        assert_eq!(c.auth.ldap.attr_unique_id, "entryUUID");
        // 存储限制值合理
        assert!(c.storage.max_file_size_mb > 0);
    }

    /// v0.4.24：默认探测挑文件的逻辑。给一组候选，应取第一个真实存在的那个。
    /// **不碰 `std::env::set_current_dir`** —— CWD 是进程级全局状态，并行单测会互相污染
    /// （这类测试偶发失败极难查）。`discover_default()` 只负责组装候选，逻辑全在 `first_existing`。
    #[test]
    fn discover_picks_first_existing_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let hit = dir.path().join("hit.toml");
        std::fs::write(&hit, "[server]\nlisten = \"0.0.0.0:7777\"\n").unwrap();

        // exe 同目录不存在、CWD 存在 → 选中 CWD 那个
        let got = Config::first_existing([
            dir.path().join("absent.toml"),
            hit.clone(),
        ])
        .expect("存在候选应被选中");
        assert_eq!(got, hit);

        // 两份都在 → exe 同目录优先（顺序即优先级）
        let first = dir.path().join("first.toml");
        std::fs::write(&first, "[server]\nlisten = \"0.0.0.0:1111\"\n").unwrap();
        let got = Config::first_existing([first.clone(), hit.clone()]).unwrap();
        assert_eq!(got, first, "exe 同目录应优先于 CWD");

        // 真正读出来的是选中的那份
        let c = Config::load(Some(&got)).unwrap();
        assert_eq!(c.server.listen, "0.0.0.0:1111");
    }

    /// 一个都不存在 → None（=全默认，不报错）。CI / 冒烟都走这条路，不能被探测拖累。
    #[test]
    fn discover_returns_none_when_all_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::first_existing([
            dir.path().join("a.toml"),
            dir.path().join("b.toml"),
        ])
        .is_none());
    }

    /// 探测到的文件要被真正读出来（端到端串一下：探测 → load → 生效）
    #[test]
    fn discovered_file_is_actually_applied() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(Config::FILE_NAME);
        std::fs::write(
            &p,
            "[auth.ldap]\nenabled = true\nurl = \"ldaps://x\"\nattr_unique_id = \"objectGUID\"\n",
        )
        .unwrap();
        let found = Config::first_existing([p]).unwrap();
        let c = Config::load(Some(&found)).unwrap();
        assert!(c.auth.ldap.enabled, "探测到的配置必须真正生效，否则等于没做");
        assert_eq!(c.auth.ldap.attr_unique_id, "objectGUID");
    }
}
