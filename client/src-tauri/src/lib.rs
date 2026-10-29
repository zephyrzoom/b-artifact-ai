/**
 * 客户端库（Rust 侧全部能力）。
 *
 * 两个 bin 共用它：
 *   - `b-artifact-app`（main.rs）：Tauri 桌面壳
 *   - `b-artifact`（cli.rs）：命令行（**不依赖 tauri**，CI / 服务器可跑）
 *
 * 与 TS 版的分层对应关系见 `docs/客户端Tauri迁移方案.md` §2.2 / §4。
 */
pub mod channels;
pub mod commands;
pub mod config;
pub mod engine;
pub mod session;
pub mod state;
pub mod watch;

use std::sync::Arc;

use tauri::Manager;

/// 桌面壳入口（main.rs 只是它的薄包装）。
pub fn run() {
    let state = Arc::new(state::AppState::new());
    let state_for_setup = state.clone();
    // 打包门禁用：`--self-check` 走"注入 JS → 断言渲染层已挂载且一次 IPC 往返成功 → 退出码"
    // （macOS 没有 WebDriver，原 Electron 那套 CDP 冒烟在 Tauri 下不可用）
    let self_check = std::env::args().any(|a| a == "--self-check");

    let app = tauri::Builder::default()
        // 单实例：第二次启动把已有窗口带到前台（§6.4）
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![commands::ipc, commands::selfcheck])
        .setup(move |app| {
            // AppHandle 只用于两件事：推送事件、弹原生对话框
            state_for_setup.attach(app.handle().clone());

            // 兜底显示：窗口是 `visible: false` 起的（避免白底闪一下），正常由
            // `on_page_load` 显示。但**页面加载失败时那个回调可能永远不来**，用户就会看到
            // "应用启动了却没有窗口"（macOS 上只看得到菜单栏）。所以再加一道定时兜底。
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                    if let Some(w) = handle.get_webview_window("main") {
                        if !w.is_visible().unwrap_or(true) {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                });
            }
            // 缓存目录必须存在，否则引擎第一次写就会失败
            let cfg = state_for_setup.config.load();
            if !cfg.cache_dir.is_empty() {
                let _ = std::fs::create_dir_all(&cfg.cache_dir);
            }
            Ok(())
        })
        .on_page_load(|webview, payload| {
            // 窗口先隐藏、前端首屏就绪后再显示 —— 等价于 Electron 的 ready-to-show，
            // 避免"白底闪一下"。
            if payload.event() == tauri::webview::PageLoadEvent::Finished {
                let _ = webview.window().show();
            }
        })
        .build(tauri::generate_context!())
        .expect("b-artifact 启动失败");

    if self_check {
        let handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            // 自检必须是**轮询**而不是"睡固定时间再打一枪"：
            // 三平台 WebView 的就绪时机差很多（WKWebView 会把 eval 排队，WebView2 /
            // WebKitGTK 在文档没好之前 eval 会直接报错，CI 冷启动更慢）。
            // 第一版在 macOS 上一次过、在 linux/windows 上双双 exit 2 —— 就是这个问题。
            let mut win = None;
            for _ in 0..75 {
                // 最多等 15s
                if let Some(w) = handle.get_webview_window("main") {
                    win = Some(w);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            let Some(w) = win else {
                eprintln!("[self-check] 15s 内没有拿到 main 窗口，无法自检");
                std::process::exit(2);
            };
            let _ = w.show();

            // 前端跑完这两件事才算通过：① 首屏已挂载 ② 一次 invoke 往返成功。
            // 结论通过 `selfcheck(code)` 命令回传 —— **不能靠 document.title 回传**：
            // WKWebView 的 document.title 不会同步到 Tauri 的窗口标题（实测）。
            // 每次都重新注入：可能落在导航完成之前的旧文档里，那一枪就白打了。
            let js = r#"(async () => {
  const report = (code) => window.__TAURI_INTERNALS__.invoke('selfcheck', { code });
  try {
    const root = document.querySelector('#app');
    if (document.readyState === 'loading' || !root || root.children.length === 0) {
      await report(3);   // 诊断：页面还没挂载，等下一次注入
      return;
    }
    const r = await window.__TAURI_INTERNALS__.invoke('ipc', { channel: 'app:info', payload: null });
    if (r && r.ok) await report(0);   // 只有这一步才算通过
  } catch (e) { await report(4); }    // 诊断：JS/IPC 抛异常，同样继续重试
})();"#;

            let mut eval_errors = 0usize;
            let mut last_error = String::new();
            for attempt in 0..60 {
                // 最多 30s：CI 冷启动 + 首次加载 1 MB 前端
                let code = commands::SELF_CHECK_CODE.load(std::sync::atomic::Ordering::SeqCst);
                if code == 0 {
                    let diag = commands::SELF_CHECK_DIAG.load(std::sync::atomic::Ordering::SeqCst);
                    eprintln!(
                        "[self-check] 通过（第 {attempt} 次注入后；期间诊断码={diag}，-1=无 3=曾未挂载 4=曾抛异常）"
                    );
                    std::process::exit(0);
                }
                match w.eval(js) {
                    Ok(()) => {}
                    Err(e) => {
                        eval_errors += 1;
                        last_error = e.to_string();
                        if attempt % 10 == 0 {
                            eprintln!("[self-check] 第 {attempt} 次注入失败（{eval_errors} 次累计）：{e}");
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let diag = commands::SELF_CHECK_DIAG.load(std::sync::atomic::Ordering::SeqCst);
            eprintln!(
                "[self-check] 30s 内没通过：注入失败 {eval_errors} 次（最后一次：{last_error}），诊断码={diag}（-1=JS 从未执行 3=页面在但未挂载 4=JS/IPC 抛异常）"
            );
            std::process::exit(5);
        });
    }

    app.run(|_app, _event| {});
}
