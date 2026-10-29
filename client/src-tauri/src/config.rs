/**
 * 本地配置（§6.5 设置项 + "记住服务器列表"）。
 *
 * 落盘位置 `~/.b-artifact/config.json`（`B_ARTIFACT_HOME` 可重定向）。**JSON 形状与 TS 版
 * 逐字一致**（camelCase 字段名），否则升级后用户的最近列表/默认目录会静默丢失。
 *
 * 两条硬要求：
 *   1. **原子写**：先写 `<file>.tmp` 再 rename，断电/崩溃不会留下半截 JSON；
 *   2. **损坏可恢复**：解析失败时把坏文件改名留证，返回默认值而不是让应用起不来。
 */
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::engine::home::artifact_home;
use crate::engine::pristine::default_cache_dir;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RecentEntry {
    /// 工作副本根目录（绝对路径）。
    pub dir: String,
    pub repo: String,
    pub server: String,
    pub last_opened_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    /// 传输并发（§6.5 设置项）。
    pub concurrency: i64,
    /// 全局 blob 缓存路径。
    pub cache_dir: String,
    /// 最近连接过的服务器（去重、最近在前）。
    pub servers: Vec<String>,
    /// 最近打开的工作副本。
    pub recent: Vec<RecentEntry>,
    /// 检出目录的默认父目录。
    pub default_checkout_parent: String,
}

pub const CONCURRENCY_MIN: i64 = 1;
pub const CONCURRENCY_MAX: i64 = 32;
const SERVERS_MAX: usize = 10;
const RECENT_MAX: usize = 20;

pub fn default_config() -> AppConfig {
    AppConfig {
        concurrency: 4,
        cache_dir: default_cache_dir().to_string_lossy().to_string(),
        servers: vec![],
        recent: vec![],
        default_checkout_parent: String::new(),
    }
}

/// 配置文件路径。
pub fn config_path(home: Option<&Path>) -> PathBuf {
    home.unwrap_or(&artifact_home()).join("config.json")
}

fn clamp_concurrency(n: i64) -> i64 {
    n.clamp(CONCURRENCY_MIN, CONCURRENCY_MAX)
}

/// 把任意 JSON 规整成合法配置（脏字段逐个兜底，不做"全有或全无"）。
pub fn normalize_config(raw: Option<&serde_json::Value>) -> AppConfig {
    let d = default_config();
    let Some(serde_json::Value::Object(map)) = raw else {
        return d;
    };

    let servers: Vec<String> = map
        .get("servers")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .take(SERVERS_MAX)
                .collect()
        })
        .unwrap_or_default();

    let recent: Vec<RecentEntry> = map
        .get("recent")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|e| e.is_object())
                .map(|e| RecentEntry {
                    dir: e.get("dir").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    repo: e.get("repo").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    server: e.get("server").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    last_opened_at: e
                        .get("lastOpenedAt")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0),
                })
                .filter(|e| !e.dir.is_empty())
                .take(RECENT_MAX)
                .collect()
        })
        .unwrap_or_default();

    AppConfig {
        concurrency: map
            .get("concurrency")
            .and_then(|v| v.as_i64())
            .map(clamp_concurrency)
            .unwrap_or(d.concurrency),
        cache_dir: map
            .get("cacheDir")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or(d.cache_dir),
        servers,
        recent,
        default_checkout_parent: map
            .get("defaultCheckoutParent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

pub struct ConfigStore {
    file: PathBuf,
    cfg: Mutex<Option<AppConfig>>,
}

impl ConfigStore {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            file: file.unwrap_or_else(|| config_path(None)),
            cfg: Mutex::new(None),
        }
    }

    pub fn load(&self) -> AppConfig {
        let mut guard = self.cfg.lock().unwrap();
        if let Some(c) = guard.as_ref() {
            return c.clone();
        }
        let loaded = if !self.file.exists() {
            default_config()
        } else {
            match std::fs::read_to_string(&self.file)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            {
                Some(v) => normalize_config(Some(&v)),
                None => {
                    // 坏文件改名留证（下次写入会重建），应用照常启动
                    let broken = PathBuf::from(format!("{}.broken", self.file.to_string_lossy()));
                    let _ = std::fs::rename(&self.file, broken);
                    default_config()
                }
            }
        };
        *guard = Some(loaded.clone());
        loaded
    }

    pub fn get(&self) -> AppConfig {
        self.load()
    }

    pub fn save(&self, next: AppConfig) -> AppConfig {
        let value = serde_json::to_value(&next).unwrap_or(serde_json::Value::Null);
        let cfg = normalize_config(Some(&value));
        if let Some(parent) = self.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = PathBuf::from(format!("{}.tmp", self.file.to_string_lossy()));
        let body = format!("{}\n", serde_json::to_string_pretty(&cfg).unwrap_or_default());
        write_private(&tmp, &body);
        let _ = std::fs::rename(&tmp, &self.file);
        *self.cfg.lock().unwrap() = Some(cfg.clone());
        cfg
    }

    /// 只改传入的字段，其余保持原值（渲染层勾选式的 `app:setConfig`）。
    pub fn update(&self, concurrency: Option<i64>, cache_dir: Option<&str>, default_parent: Option<&str>) -> AppConfig {
        let mut c = self.load();
        if let Some(n) = concurrency {
            c.concurrency = clamp_concurrency(n);
        }
        if let Some(d) = cache_dir {
            c.cache_dir = d.to_string();
        }
        if let Some(p) = default_parent {
            c.default_checkout_parent = p.to_string();
        }
        self.save(c)
    }

    /// 记住一个服务器地址（最近在前、去重）。
    pub fn remember_server(&self, server: &str) -> AppConfig {
        let mut c = self.load();
        let mut servers = vec![server.to_string()];
        servers.extend(c.servers.iter().filter(|s| s.as_str() != server).cloned());
        servers.truncate(SERVERS_MAX);
        c.servers = servers;
        self.save(c)
    }

    pub fn forget_server(&self, server: &str) -> AppConfig {
        let mut c = self.load();
        c.servers.retain(|s| s != server);
        self.save(c)
    }

    pub fn remember_recent(&self, dir: &str, repo: &str, server: &str) -> AppConfig {
        let mut c = self.load();
        let entry = RecentEntry {
            dir: dir.to_string(),
            repo: repo.to_string(),
            server: server.to_string(),
            last_opened_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        };
        let mut recent = vec![entry];
        recent.extend(c.recent.iter().filter(|r| r.dir != dir).cloned());
        recent.truncate(RECENT_MAX);
        c.recent = recent;
        self.save(c)
    }

    pub fn drop_recent(&self, dir: &str) -> AppConfig {
        let mut c = self.load();
        c.recent.retain(|r| r.dir != dir);
        self.save(c)
    }

    /// 仅供测试：清掉内存缓存，强制下次 load 重新读盘。
    pub fn reset(&self) {
        *self.cfg.lock().unwrap() = None;
    }

    pub fn destroy(&self) {
        for p in [
            self.file.clone(),
            PathBuf::from(format!("{}.tmp", self.file.to_string_lossy())),
            PathBuf::from(format!("{}.broken", self.file.to_string_lossy())),
        ] {
            let _ = std::fs::remove_file(p);
        }
        self.reset();
    }
}

#[cfg(unix)]
fn write_private(path: &Path, body: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
    {
        use std::io::Write;
        let _ = f.write_all(body.as_bytes());
    }
}

#[cfg(not(unix))]
fn write_private(path: &Path, body: &str) {
    let _ = std::fs::write(path, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> ConfigStore {
        ConfigStore::new(Some(dir.join("config.json")))
    }

    #[test]
    fn defaults_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let c = store(dir.path()).load();
        assert_eq!(c.concurrency, 4);
        assert!(c.servers.is_empty());
        assert!(c.cache_dir.ends_with("cache"));
    }

    #[test]
    fn json_shape_is_camel_case() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.remember_recent("/wc-a", "r", "http://h");
        let raw = std::fs::read_to_string(dir.path().join("config.json")).unwrap();
        assert!(raw.contains("\"cacheDir\""), "cacheDir 必须 camelCase：{raw}");
        assert!(raw.contains("\"defaultCheckoutParent\""));
        assert!(raw.contains("\"lastOpenedAt\""));
    }

    #[test]
    fn clamps_concurrency_and_keeps_others() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let c = s.update(Some(99), Some("/tmp/c2"), Some("/tmp/p"));
        assert_eq!(c.concurrency, CONCURRENCY_MAX);
        assert_eq!(c.cache_dir, "/tmp/c2");
        assert_eq!(c.default_checkout_parent, "/tmp/p");
        // 再改一项不应抹掉别的
        let c = s.update(None, None, Some("/tmp/p2"));
        assert_eq!(c.cache_dir, "/tmp/c2");
        assert_eq!(c.default_checkout_parent, "/tmp/p2");
    }

    #[test]
    fn remember_server_dedupes_to_front() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.remember_server("http://a");
        s.remember_server("http://b");
        let c = s.remember_server("http://a");
        assert_eq!(c.servers, vec!["http://a", "http://b"]);
        let c = s.forget_server("http://a");
        assert_eq!(c.servers, vec!["http://b"]);
    }

    #[test]
    fn recent_is_deduped_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        for i in 0..25 {
            s.remember_recent(&format!("/wc-{i}"), "r", "http://h");
        }
        let c = s.get();
        assert_eq!(c.recent.len(), RECENT_MAX);
        assert_eq!(c.recent[0].dir, "/wc-24", "最近打开的在最前");

        s.remember_recent("/wc-24", "r", "http://h");
        let c = s.get();
        assert_eq!(
            c.recent.iter().filter(|r| r.dir == "/wc-24").count(),
            1,
            "同一目录不应重复出现"
        );

        s.drop_recent("/wc-24");
        assert!(s.get().recent.iter().all(|r| r.dir != "/wc-24"));
    }

    #[test]
    fn broken_file_is_renamed_and_defaults_returned() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("config.json");
        std::fs::write(&f, "{ not json").unwrap();
        let s = store(dir.path());
        let c = s.load();
        assert_eq!(c.concurrency, 4);
        assert!(
            dir.path().join("config.json.broken").exists(),
            "坏文件应改名留证"
        );
    }

    #[test]
    fn dirty_fields_fall_back_individually() {
        let raw = serde_json::json!({
            "concurrency": "many",
            "cacheDir": "",
            "servers": ["a", 1, "b"],
            "recent": [{ "dir": "" }, { "dir": "/ok", "lastOpenedAt": 5 }],
            "defaultCheckoutParent": 42
        });
        let c = normalize_config(Some(&raw));
        assert_eq!(c.concurrency, 4, "类型不对 → 默认值");
        assert!(c.cache_dir.ends_with("cache"), "空串 → 默认值");
        assert_eq!(c.servers, vec!["a", "b"], "非字符串项被剔除");
        assert_eq!(c.recent.len(), 1, "空 dir 的条目被剔除");
        assert_eq!(c.recent[0].last_opened_at, 5);
        assert_eq!(c.default_checkout_parent, "");
    }

    #[test]
    fn atomic_write_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.save(default_config());
        assert!(dir.path().join("config.json").exists());
        assert!(!dir.path().join("config.json.tmp").exists());
    }
}
