use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager,
};
use tracing::{info, warn};

use crate::os_shim::open_in_explorer;
use crate::updater::UpdateService;

pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    // 菜单项一律短名: "打开飞梭主面板"这类前缀冗余 —— 托盘图标已经表明这是飞梭。
    // ❌ 曾经这里还有全局热键 Ctrl+Alt+V 的注册（HOTKEY_SEND_CLIPBOARD + setup_hotkey）,
    //    且菜单项后面拼了原始字符串 "\tCommandOrControl+Alt+V" —— Windows 托盘菜单
    //    不会把它翻译成 "Ctrl+Alt+V", 用户看到的就是这一串英文。热键已整体移除,
    //    剪贴板直发只保留托盘菜单与发送页按钮两个入口。
    let clipboard_item = MenuItem::with_id(app, "send_clipboard", "发送剪贴板", true, None::<&str>)?;
    let recv_folder_item = MenuItem::with_id(app, "open_recv", "接收目录", true, None::<&str>)?;
    let log_folder_item = MenuItem::with_id(app, "open_log", "日志目录", true, None::<&str>)?;
    let update_item = MenuItem::with_id(app, "check_update", "检查更新", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出飞梭", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &clipboard_item,
            &recv_folder_item,
            &log_folder_item,
            &update_item,
            // ❌ 曾经是 `MenuItem::with_id(app, "sep", "", false, ...)` ——
            //    一个空标题的禁用菜单项, 在 Windows 上渲染成一条**灰色空带**而不是分隔线。
            //    必须用 PredefinedMenuItem::separator 才是真正的分隔线。
            &PredefinedMenuItem::separator(app)?,
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
            "send_clipboard" => {
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
                let _ = app.emit("feisuo://open-settings-update", ());
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
                });
            }
            "quit" => {
                info!("Quitting Feisuo from tray menu");
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}

/// 通知前端打开剪贴板直发（托盘菜单触发）。
fn emit_open_clipboard(app: &AppHandle) {
    if let Err(e) = app.emit_to("main", "feisuo://open-clipboard", ()) {
        // 前端还没加载完是常态（冷启动时点击菜单可能先于窗口就绪）
        warn!("发送剪贴板指令失败（前端可能尚未就绪）: {}", e);
    }
}
