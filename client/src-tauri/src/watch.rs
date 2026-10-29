/**
 * 工作副本文件监听（§6.5 M4.8：状态自动同步）。
 *
 * 目标：用户在 Finder / 资源管理器里改了文件，界面**自己**就更新，不需要按"刷新状态"。
 *
 * 三层判断，缺一层就会出问题：
 * 1. **忽略自己的元数据**：`.b-artifact/` 下全是 wc.db、pristine、wc.lock——每次 status
 *    都会读它，若把它的变化也算成"用户改动"就会自激成死循环。
 * 2. **忽略规则命中的路径**：这些路径本来就不参与状态判定，给它们触发一次全量扫描纯属浪费。
 * 3. **抑制自己写入的文件**：update / revert / 检出落盘时我们会大量写工作文件。
 *
 * 与 TS 版的差别：`fs.watch({recursive})` 在 Linux 上不可用（必然降级），`notify` 三平台
 * 都支持递归；代价是 Linux 上受 inotify `max_user_watches` 限制，起不来时同样走降级。
 */
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify::{RecursiveMode, Watcher as _};

use crate::engine::db::WC_DIR;
use crate::engine::errors::{ClientError, ClientResult};
use crate::engine::paths::to_posix;

pub const DEBOUNCE_MS: u64 = 300;
/// 自写入抑制窗口：操作期间 + 操作后各一段时间（落盘往往是异步的，尾部事件会晚到）。
pub const SELF_WRITE_MS: u64 = 1500;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 该路径是否根本不值得触发一次状态重算。
///
/// `rel == ""` 表示"根有变化但不知道具体是谁"——**必须触发**（宁多扫一次，不错过一次）。
pub fn is_noise(
    rel: &str,
    is_ignored: Option<&(dyn Fn(&str, bool) -> bool + Send + Sync)>,
) -> bool {
    if rel.is_empty() {
        return false;
    }
    let segs: Vec<&str> = rel.split('/').collect();
    // 元数据目录（任何深度出现同名目录都要挡：部分检出/嵌套工作副本都可能出现）
    if segs.contains(&WC_DIR) {
        return true;
    }
    // 本地临时文件：`.mine`（冲突伴生）与编辑器交换文件，都不进版本控制
    let name = segs.last().copied().unwrap_or("");
    if name.ends_with(".mine") || name.ends_with('~') || name.starts_with(".#") {
        return true;
    }
    is_ignored.map(|f| f(rel, false)).unwrap_or(false)
}

pub type IgnoreFn = Arc<dyn Fn(&str, bool) -> bool + Send + Sync>;
pub type SettledFn = Arc<dyn Fn() + Send + Sync>;

/// 防抖 + 抑制 + 忽略规则三合一的工作副本监听器。
pub struct Watcher {
    stop: Arc<AtomicBool>,
    suppress_until: Arc<AtomicU64>,
    degraded: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    /// 告诉监听器"接下来的变化是我自己写的"，窗口结束后自动恢复。
    pub fn suppress(&self, ms: u64) {
        self.suppress_until.store(now_ms() + ms, Ordering::SeqCst);
    }

    pub fn is_suppressed(&self) -> bool {
        now_ms() < self.suppress_until.load(Ordering::SeqCst)
    }

    pub fn degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    /// 停止监听（幂等）。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 起监听。失败时返回 Err（上层据此置 `degraded`，界面露出"手动刷新"入口）。
pub fn spawn(
    root: &Path,
    is_ignored: Option<IgnoreFn>,
    on_settled: SettledFn,
) -> ClientResult<Watcher> {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .map_err(|e| ClientError::wc("IO", format!("文件监听不可用：{e}")))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|e| ClientError::wc("IO", format!("文件监听不可用：{e}")))?;

    let stop = Arc::new(AtomicBool::new(false));
    let suppress_until = Arc::new(AtomicU64::new(0));
    let degraded = Arc::new(AtomicBool::new(false));
    let root_buf = root.to_path_buf();

    let thread = {
        let stop = stop.clone();
        let suppress = suppress_until.clone();
        let degraded = degraded.clone();
        let debounce = DEBOUNCE_MS;
        std::thread::Builder::new()
            .name("b-artifact-watch".into())
            .spawn(move || {
                // watcher 必须活在这个线程里（drop 即停止监听）
                let _keep = watcher;
                let mut dirty = false;
                let mut last_event = 0u64;
                loop {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(Ok(event)) => {
                            for p in &event.paths {
                                let rel = p
                                    .strip_prefix(&root_buf)
                                    .map(|r| to_posix(&r.to_string_lossy()))
                                    .unwrap_or_default();
                                if !is_noise(&rel, is_ignored.as_deref()) {
                                    dirty = true;
                                    last_event = now_ms();
                                }
                            }
                        }
                        Ok(Err(_)) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    if dirty && now_ms().saturating_sub(last_event) >= debounce {
                        // 抑制窗口里进来的事件不丢弃，**推迟到窗口结束再算**：
                        // 用户完全可能在应用写盘后的 1.5s 内又改了别的文件，丢掉就等于
                        // 那次变化永远不反映到界面上。
                        if now_ms() < suppress.load(Ordering::SeqCst) {
                            continue;
                        }
                        dirty = false;
                        on_settled();
                    }
                }
                let _ = &degraded;
            })
            .map_err(|e| ClientError::wc("IO", format!("监听线程启动失败：{e}")))?
    };

    Ok(Watcher {
        stop,
        suppress_until,
        degraded,
        thread: Some(thread),
    })
}

/// 事件去重用（notify 常把一个保存动作拆成多条事件）。
#[allow(dead_code)]
fn dedupe(paths: &[std::path::PathBuf]) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for p in paths {
        *m.entry(p.to_string_lossy().to_string()).or_insert(0) += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rel_is_never_noise() {
        assert!(!is_noise("", None));
    }

    #[test]
    fn metadata_dir_is_noise_at_any_depth() {
        assert!(is_noise(".b-artifact", None));
        assert!(is_noise(".b-artifact/wc.db", None));
        assert!(is_noise("sub/.b-artifact/wc.db", None));
    }

    #[test]
    fn mine_and_editor_swaps_are_noise() {
        assert!(is_noise("a.psd.mine", None));
        assert!(is_noise("dir/a.psd.mine", None));
        assert!(is_noise("a.psd~", None));
        assert!(is_noise(".#a.psd", None));
        assert!(!is_noise("a.psd", None));
    }

    #[test]
    fn ignore_rules_are_consulted() {
        let f: IgnoreFn = Arc::new(|rel: &str, _is_dir: bool| rel.ends_with(".tmp"));
        assert!(is_noise("x.tmp", Some(f.as_ref())));
        assert!(!is_noise("x.psd", Some(f.as_ref())));
    }

    #[test]
    fn debounce_and_suppress_constants_are_stable() {
        // 这两个值直接决定"自动同步"的手感，改动要有意识
        assert_eq!(DEBOUNCE_MS, 300);
        assert_eq!(SELF_WRITE_MS, 1500);
    }
}
