use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager,
};
use tracing::{error, info, warn};

use crate::os_shim::open_in_explorer;
use crate::updater::UpdateService;

/// 全局热键：唤出主窗口并切到「发送剪贴板」标签。
///
/// ## 为什么必须是**全局**热键而不是前端 keydown
///
/// 飞梭是托盘常驻应用，窗口大部分时间处于隐藏状态。
/// 前端 `keydown` 只在窗口有焦点时触发 —— 也就是说用户想用剪贴板直发时
/// 必须先点一下任务栏图标把窗口叫出来，那这个热键就毫无意义。
/// `tauri-plugin-global-shortcut` 走 `RegisterHotKey`，窗口隐藏时照样触发。
pub const HOTKEY_SEND_CLIPBOARD: &str = "CommandOrControl+Alt+V";

pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let show_item = MenuItem::with_id(app, "show", "打开飞梭主面板", true, None::<&str>)?;
    let clipboard_item = MenuItem::with_id(
        app,
        "send_clipboard",
        format!("发送剪贴板内容\t{}", HOTKEY_SEND_CLIPBOARD),
        true,
        None::<&str>,
    )?;
    let recv_folder_item = MenuItem::with_id(app, "open_recv", "打开接收文件夹", true, None::<&str>)?;
    let log_folder_item = MenuItem::with_id(app, "open_log", "打开日志目录", true, None::<&str>)?;
    let update_item = MenuItem::with_id(app, "check_update", "检查更新…", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出飞梭", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &show_item,
            &clipboard_item,
            &recv_folder_item,
            &log_folder_item,
            &update_item,
            &MenuItem::with_id(app, "sep", "", false, None::<&str>)?,
            &quit_item,
        ],
    )?;

    let tray_icon = match tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png")) {
        Ok(img) => img,
        Err(_) => tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?,
    };

    let _tray = TrayIconBuilder::with_id("feisuo-tray")
        .icon(tray_icon)
        .tooltip("飞梭 Feisuo")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                show_main(app);
            }
            "send_clipboard" => {
                // 托盘常驻下窗口多半是隐藏的, 必须先唤出来并切到发送页,
                // 否则热键触发了却什么也没发生 —— 用户只会认为热键失灵。
                show_main(app);
                emit_open_clipboard(app);
            }
            "open_recv" => {
                // 旧实现硬编码 home/feisuo, 与用户在设置里改过的落盘目录不一致
                if let Some(state) = app.try_state::<crate::commands::AppState>() {
                    if let Ok(cfg) = state.engine.config.try_read() {
                        if let Err(e) = open_in_explorer(&cfg.receive_dir) {
                            tracing::error!("打开接收目录失败: {}", e);
                        }
                        return;
                    }
                }
                if let Some(home) = dirs::home_dir() {
                    let _ = open_in_explorer(&home.join("feisuo"));
                }
            }
            "open_log" => {
                if let Err(e) = open_in_explorer(&feisuo_core::AppConfig::get_log_dir()) {
                    tracing::error!("打开日志目录失败: {}", e);
                }
            }
            "check_update" => {
                // 托盘常驻下窗口多半是隐藏的, 必须先唤出来:
                // 否则检查结果(有新版 / 已是最新 / 检查失败)全都只发到
                // 一个看不见的窗口里, 用户点了菜单却毫无反馈。
                show_main(app);
                let handle = app.clone();
                let Some(updater) = app.try_state::<Arc<UpdateService>>() else {
                    warn!("更新服务尚未就绪, 忽略检查更新请求");
                    return;
                };
                let updater = updater.inner().clone();
                tauri::async_runtime::spawn(async move {
                    let status = updater.check_and_download(true).await;
                    info!(
                        "托盘触发检查更新: phase={} message={}",
                        status.phase.as_str(),
                        status.message
                    );
                    // 手动触发的结果统一唤窗, 托盘用户才看得到
                    if let Some(window) = handle.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                });
            }
            "quit" => {
                info!("Quitting Feisuo from tray menu");
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let is_visible = window.is_visible().unwrap_or(false);
                    let is_minimized = window.is_minimized().unwrap_or(false);
                    if is_visible && !is_minimized {
                        let _ = window.hide();
                    } else {
                        show_main(app);
                    }
                }
            }
        })
        .build(app)?;

    setup_hotkey(app)?;

    Ok(())
}

/// 唤出并聚焦主窗口。
///
/// 抽成函数是因为这个三行序列在托盘菜单里出现了四次 —— 而漏掉
/// `unminimize()` 的后果是"点了菜单窗口弹出来但还是最小化状态"，
/// 这类 bug 极难从日志里看出来。
fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 通知前端打开剪贴板预览（热键与托盘菜单共用）。
fn emit_open_clipboard(app: &AppHandle) {
    if let Err(e) = app.emit_to("main", "feisuo://open-clipboard", ()) {
        // 前端还没加载完是常态（冷启动时热键可能先到）
        warn!("发送剪贴板指令失败（前端可能尚未就绪）: {}", e);
    }
}

/// 注册全局热键。
///
/// 注册失败（热键被别的软件占用）**不阻止应用启动** ——
/// 那只是一个可选入口，失败就退回到托盘菜单，不该让用户打不开飞梭。
fn setup_hotkey(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

    let shortcut = match HOTKEY_SEND_CLIPBOARD.parse::<Shortcut>() {
        Ok(s) => s,
        Err(e) => {
            error!(
                "热键 {} 解析失败, 已跳过注册: {}",
                HOTKEY_SEND_CLIPBOARD, e
            );
            return Ok(());
        }
    };
    if let Err(e) = app.global_shortcut().on_shortcut(
        shortcut,
        move |app, _sc, event| {
            // Windows 上 on_shortcut 会在 Pressed 与 Released 各触发一次。
            // 不判状态就会**每按一次执行两遍**（读两遍剪贴板、发两次文件）。
            if event.state() != ShortcutState::Pressed {
                return;
            }
            show_main(app);
            emit_open_clipboard(app);
        },
    ) {
        error!(
            "注册全局热键 {} 失败（可能被其他软件占用）: {}；可改用托盘菜单",
            HOTKEY_SEND_CLIPBOARD, e
        );
        return Ok(());
    }
    info!("全局热键已注册: {}", HOTKEY_SEND_CLIPBOARD);
    Ok(())
}
