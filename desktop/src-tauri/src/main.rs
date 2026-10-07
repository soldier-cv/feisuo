#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod commands;
mod logger;
mod os_shim;
// 下面几个守卫只在测试构建里需要(纯静态检查), 放进正式产物会留下
// 一整片死代码告警。
#[cfg(test)]
mod android_guard;
mod clipboard;
#[cfg(test)]
mod jni_guard;
#[cfg(test)]
mod theme_guard;
#[cfg(test)]
mod update_guard;
#[cfg(test)]
mod wiring_guard;
mod tray;
mod updater;

use std::sync::Arc;
use tauri::{Emitter, Manager, WindowEvent};
use tracing::{info, warn};
use feisuo_core::{AppConfig, FeisuoEngine};
use crate::commands::AppState;
use crate::updater::UpdateService;

/// 原生关闭事件 (Alt+F4 / 标题栏 ×) 转发给前端的事件名
const WINDOW_CLOSE_REQUESTED: &str = "feisuo://window-close-requested";

#[tokio::main]
async fn main() {
    // 更新换名接力的收尾必须发生在**任何**其他初始化之前:
    // 上一轮「立即更新」已经把新版本放到正式路径并拉起了本进程,
    // 这里把暂存的 .new 清掉。整个流程是同步的, 不依赖 Tauri 运行时。
    updater::apply_rollover_if_needed();

    let cfg = AppConfig::load_or_default();
    logger::init_logger(&cfg);

    let is_daemon = std::env::args().any(|arg| arg == "--daemon");
    info!("Starting Feisuo desktop host (daemon mode: {})", is_daemon);

    // Initialize core engine。
    // 引擎初始化失败必须给出可读原因并退出, 不能 panic 成一条
    // "thread 'main' panicked" 让用户完全无从下手。
    let handles = match FeisuoEngine::init().await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("Core engine init failed: {}", e);
            eprintln!("[feisuo] 核心引擎初始化失败: {}", e);
            std::process::exit(1);
        }
    };
    let (engine, mut disc_rx, mut progress_rx, mut approval_rx) = handles.into_tuple();
    let engine_arc = Arc::new(engine);
    info!("Core engine ready, data dir: {:?}", engine_arc.app_dir);

    let app_state = AppState {
        engine: engine_arc.clone(),
    };

    let builder = tauri::Builder::default()
        .manage(app_state)
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        // 全局热键 Ctrl+Alt+V（§6.4）。必须在这里 init,
        // 否则 `app.global_shortcut()` 拿不到扩展, 注册会静默失败。
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--daemon"]),
        ))
        .setup(move |app| {
            tray::setup_tray(app.handle())?;

            // 更新服务在这里创建 (而非 main 开头): 它需要 AppHandle 推事件。
            // 恢复上次已下载未安装的更新 + 按配置启动静默调度。
            let updater = Arc::new(UpdateService::new(app.handle().clone()));
            {
                let boot = updater.clone();
                tokio::spawn(async move { boot.restore_pending().await });
            }
            let auto_check = engine_arc.config.try_read().map(|c| c.auto_check_update).unwrap_or(true);
            updater.spawn_scheduler(auto_check);
            app.manage(updater);

            // 引擎在这里启动: 端口被占用等失败必须能反馈到界面上,
            // 否则用户只会看到"局域网设备 (0)"却不知道为什么连不上。
            let engine_for_start = engine_arc.clone();
            let handle_for_start = app.handle().clone();
            tokio::spawn(async move {
                if let Err(e) = engine_for_start.start().await {
                    tracing::error!("Engine start error: {}", e);
                    let _ = handle_for_start.emit("feisuo://engine-error", e.to_string());
                }
            });

            // ---- 暂存区 TTL：周期性清理（DESIGN_TRUST_SHUTTLE §6.5）----
            //
            // 为什么必须**周期性**而不是只在启动时清一次：
            // 飞梭是托盘常驻应用，可能连续运行数周不重启。此前清理只在
            // 前端 `onMounted` 触发一次，于是
            //
            //   第 0 天 09:00  启动（清理了当时的东西）
            //   第 0 天 10:00  复制一张含密码的截图 → 发送剪贴板 → 关掉预览没发
            //   第 0 ~ 21 天    应用一直运行 —— **文件一直不删**
            //   第 21 天        才等到重启时被清掉
            //
            // 也就是"24 小时 TTL"实际兑现成了"下次重启时才删"，最坏情况
            // 是明文密码在磁盘上躺了几周 —— 而这正是文档把暂存区迁到
            // app 私有目录时想解决的隐私问题。
            //
            // 为什么放在 Rust 侧而不是前端 setInterval：
            // 托盘常驻时窗口长期隐藏，WebView 的定时器会被节流甚至暂停，
            // 且 webview 一旦崩溃清理就跟着没了。宿主进程的定时任务不受
            // 窗口可见性影响。
            //
            // 间隔 30 分钟：远小于 24h 的 TTL，又不至于频繁扫盘。
            {
                let app_dir_for_purge = engine_arc.app_dir.clone();
                tokio::spawn(async move {
                    // 首轮刻意**不立即执行**：前端 onMounted 已经清过一次，
                    // 这里再清一遍只是重复扫盘。
                    let mut ticker =
                        tokio::time::interval(std::time::Duration::from_secs(30 * 60));
                    ticker.tick().await; // 消费一次立即触发的 tick
                    loop {
                        ticker.tick().await;
                        match feisuo_core::clipboard::purge_stale_staging(&app_dir_for_purge, 24)
                        {
                            Ok(n) if n > 0 => {
                                info!("暂存区 TTL 清理: 删除 {} 个超期文件", n);
                            }
                            Ok(_) => {}
                            Err(e) => warn!("暂存区 TTL 清理失败: {}", e),
                        }
                    }
                });
            }

            if let Some(window) = app.get_webview_window("main") {
                info!("Found main window, applying icon and visibility");
                if let Ok(icon) = tauri::image::Image::from_bytes(include_bytes!("../icons/128x128@2x.png")) {
                    let _ = window.set_icon(icon);
                }
                if is_daemon {
                    // tauri.conf.json 里窗口是 `visible: false`, 自启时**根本不会**
                    // 创建出可见窗口。旧配置是 `visible: true` + 这里再 hide(),
                    // 中间那一瞬窗口已经画出来了 —— 表现为每次登录都闪一下窗口。
                    // 这里仍显式 hide 一次是双保险(防止配置被改回 true)。
                    let _ = window.hide();
                } else {
                    // 手动启动: 此时才把窗口显示出来
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            } else {
                tracing::warn!("Main window 'main' not found in setup!");
            }

            // ---- 事件桥接: core -> webview ----
            // 旧写法是 `while let Ok(x) = rx.recv().await`,
            // 而 broadcast 的 `RecvError::Lagged` 也是 Err:
            // webview 只要卡顿一拍, 循环就永久退出, 此后再无任何进度事件与审批弹窗。
            let handle = app.handle().clone();
            tokio::spawn(async move {
                loop {
                    match disc_rx.recv().await {
                        Ok(dev) => {
                            let _ = handle.emit("feisuo://device-discovered", dev);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("设备发现事件积压 {} 条, 已丢弃最旧数据", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            let handle = app.handle().clone();
            tokio::spawn(async move {
                loop {
                    match progress_rx.recv().await {
                        Ok(progress) => {
                            let _ = handle.emit("feisuo://transfer-progress", progress);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("传输进度事件积压 {} 条, 已丢弃最旧数据", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            let handle_approval = app.handle().clone();
            tokio::spawn(async move {
                loop {
                    match approval_rx.recv().await {
                        Ok(req) => {
                            let _ = handle_approval.emit("feisuo://transfer-approval", req);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("审批事件积压 {} 条, 已丢弃最旧数据", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // 一律拦截原生关闭: 由前端弹出"最小化到托盘 / 退出程序"选择。
                // 旧实现无条件 hide(), 用户永远无法从窗口关掉程序, 只能去托盘找菜单。
                api.prevent_close();
                if let Err(e) = window.emit(WINDOW_CLOSE_REQUESTED, ()) {
                    warn!("无法向前端发送关闭请求: {}", e);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_local_info,
            commands::get_known_places,
            commands::get_online_devices,
            commands::get_device_roster,
            clipboard::read_clipboard_preview,
            clipboard::stage_clipboard_payload,
            clipboard::purge_clipboard_staging,
            clipboard::cleanup_clipboard_staging,
            commands::get_trusted_devices,
            // ❌ 原本这里还有 commands::remove_trusted_device ——
            // 一个「已注册但无调用方」的本地单向解除，见 commands.rs 的说明。
            // 解除配对走下面的 commands::unpair_device（双向）。
            commands::pair_with_device,
            commands::send_files,
            commands::generate_pair_pin,
            commands::open_receive_folder,
            commands::set_autostart,
            commands::list_directory_files,
            commands::list_remote_files,
            commands::request_pull,
            commands::probe_file_sizes,
            commands::preview_send_paths,
            commands::list_device_endpoints,
            commands::get_preferred_endpoint,
            commands::minimize_window,
            commands::toggle_maximize_window,
            commands::hide_window,
            commands::quit_app,
            commands::ensure_window_visible,
            commands::get_engine_status,
            commands::probe_device,
            commands::respond_approval,
            // §2.3.1 短期授权：授予走审批按钮，撤销与可见性走这三条。
            commands::revoke_device_grants,
            commands::list_active_grants,
            commands::get_session_grant_window,
            
            commands::unpair_device,
            commands::open_log_folder,
            commands::get_transfer_history,
            commands::clear_transfer_history,
            commands::export_transfer_diagnostics,
            commands::get_transfer_diagnostics,
            commands::get_security_events,
            commands::set_device_trust_level,
            commands::set_device_visible,
            commands::get_hidden_devices,
            commands::cancel_incoming_transfer,
            commands::get_device_access_scope,
            commands::set_device_access_scope,
            commands::stage_temp_payload,
            commands::cleanup_temp_payloads,
            commands::update_app_config,
            commands::get_update_status,
            commands::check_for_update,
            commands::apply_update,
            commands::open_update_page
        ]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
