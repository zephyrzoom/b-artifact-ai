/**
 * 登录会话（token 只在 Rust 侧，绝不进渲染层，§6.4 安全基线）。
 *
 * 落盘策略是**安全默认**：只有拿到系统钥匙串（macOS Keychain / Windows 凭据管理器 /
 * Linux Secret Service）才持久化；拿不到就只在内存里留着——宁可让用户下次启动
 * 重新登录，也不把明文 token 写到磁盘上。
 *
 * **与 Electron 版的唯一行为差异**：`safeStorage` 写的 `~/.b-artifact/session.enc`
 * 是 DPAPI/Keychain 的私有密文，Rust 解不开。首次启动若发现它，直接删掉并当作未登录
 * （用户需要重新登录一次）。这不是"顺手清垃圾"——留着它只会让每次启动都白读一遍。
 */
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::engine::home::artifact_home;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub server: String,
    pub username: String,
    pub token: String,
}

enum Backend {
    /// 系统钥匙串：service + account 定位一条 JSON 条目。
    Keyring { service: String, account: String },
    /// 只存内存（测试用；也是"钥匙串不可用"时的等价行为）。
    Memory,
}

pub struct SessionStore {
    backend: Backend,
    /// Electron 时代留下的加密文件（存在即删）。
    legacy_file: PathBuf,
    mem: Mutex<Option<SessionState>>,
}

impl SessionStore {
    pub fn new(home: Option<&Path>) -> Self {
        let home = home.unwrap_or(&artifact_home()).to_path_buf();
        Self {
            backend: Backend::Keyring {
                service: "b-artifact".to_string(),
                account: "session".to_string(),
            },
            legacy_file: home.join("session.enc"),
            mem: Mutex::new(None),
        }
    }

    /// 测试用：不碰真实钥匙串。
    pub fn memory_only(home: Option<&Path>) -> Self {
        let home = home.unwrap_or(&artifact_home()).to_path_buf();
        Self {
            backend: Backend::Memory,
            legacy_file: home.join("session.enc"),
            mem: Mutex::new(None),
        }
    }

    fn entry(&self) -> Option<keyring::Entry> {
        match &self.backend {
            Backend::Keyring { service, account } => {
                keyring::Entry::new(service, account).ok()
            }
            Backend::Memory => None,
        }
    }

    /// 清掉 Electron 版留下的密文（只有换过引擎才会存在）。
    fn drop_legacy_file(&self) {
        if self.legacy_file.exists() {
            let _ = std::fs::remove_file(&self.legacy_file);
        }
    }

    /// 当前会话（含 token）。启动时 lazy 读一次钥匙串。
    pub fn get(&self) -> Option<SessionState> {
        {
            let guard = self.mem.lock().unwrap();
            if guard.is_some() {
                return guard.clone();
            }
        }
        let Some(entry) = self.entry() else {
            // 内存后端：没有存量就当作未登录（同时清掉旧密文）
            self.drop_legacy_file();
            return None;
        };
        match entry.get_password() {
            Ok(raw) => match serde_json::from_str::<SessionState>(&raw) {
                Ok(s) if !s.token.is_empty() && !s.server.is_empty() => {
                    *self.mem.lock().unwrap() = Some(s.clone());
                    Some(s)
                }
                // 能取到但结构不对（旧版本写的 / 手工改过）：清掉，免得每次启动都白读
                _ => {
                    let _ = entry.delete_credential();
                    None
                }
            },
            Err(_) => {
                // 没有条目 / 钥匙串被重置 / 用户拒绝授权：都当作未登录
                self.drop_legacy_file();
                None
            }
        }
    }

    pub fn save(&self, state: SessionState) -> SessionState {
        *self.mem.lock().unwrap() = Some(state.clone());
        if let Some(entry) = self.entry() {
            if let Ok(raw) = serde_json::to_string(&state) {
                // 写失败（无后端 / 用户拒绝）就只在内存里留着：安全默认
                let _ = entry.set_password(&raw);
            }
        }
        state
    }

    pub fn clear(&self) {
        *self.mem.lock().unwrap() = None;
        if let Some(entry) = self.entry() {
            let _ = entry.delete_credential();
        }
        self.drop_legacy_file();
    }

    /// 只清内存（模拟进程重启），用于测试与"锁定界面"场景。
    pub fn reset(&self) {
        *self.mem.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SessionState {
        SessionState {
            server: "http://h".into(),
            username: "u".into(),
            token: "t".into(),
        }
    }

    #[test]
    fn memory_backend_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionStore::memory_only(Some(dir.path()));
        assert!(s.get().is_none());
        s.save(state());
        assert_eq!(s.get(), Some(state()));
        s.clear();
        assert!(s.get().is_none());
    }

    #[test]
    fn reset_keeps_disk_copy_but_drops_memory() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionStore::memory_only(Some(dir.path()));
        s.save(state());
        s.reset();
        // 内存后端没有磁盘副本，reset 之后就是未登录
        assert!(s.get().is_none());
    }

    #[test]
    fn legacy_electron_ciphertext_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("session.enc");
        std::fs::write(&legacy, b"\x00cipher").unwrap();
        let s = SessionStore::memory_only(Some(dir.path()));
        assert!(s.get().is_none());
        assert!(!legacy.exists(), "Electron 的密文必须清掉（Rust 解不开）");
    }
}
