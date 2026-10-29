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
            tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
            let Some(w) = handle.get_webview_window("main") else {
                eprintln!("[self-check] 找不到 main 窗口");
                std::process::exit(2);
            };
            let _ = w.show();
            // 前端跑完这两件事才算通过：① 首屏已挂载 ② 一次 invoke 往返成功。
            // 结论通过 `selfcheck(code)` 命令回传 —— **不能靠 document.title 回传**：
            // WKWebView 的 document.title 不会同步到 Tauri 的窗口标题（实测）。
            let js = r#"(async () => {
  const report = (code) => window.__TAURI_INTERNALS__.invoke('selfcheck', { code });
  try {
    const r = await window.__TAURI_INTERNALS__.invoke('ipc', { channel: 'app:info', payload: null });
    const root = document.querySelector('#app');
    const mounted = !!root && root.children.length > 0;
    await report((r && r.ok && mounted) ? 0 : 3);
  } catch (e) { await report(4); }
})();"#;
            if let Err(e) = w.eval(js) {
                eprintln!("[self-check] 注入失败：{e}");
                std::process::exit(2);
            }
            for _ in 0..60 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let code = commands::SELF_CHECK_CODE.load(std::sync::atomic::Ordering::SeqCst);
                if code != -1 {
                    eprintln!("[self-check] 结论码={code}（0=通过 3=未挂载或 IPC 失败 4=JS 异常）");
                    std::process::exit(if code == 0 { 0 } else { 1 });
                }
            }
            eprintln!("[self-check] 超时：前端没有回传结论（多半是白屏，JS 没跑起来）");
            std::process::exit(5);
        });
    }

    app.run(|_app, _event| {});
}
