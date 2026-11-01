use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
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
    let show_item = MenuItem::with_id(app, "show", "打开面板", true, None::<&str>)?;
    let clipboard_item = MenuItem::with_id(app, "send_clipboard", "发送剪贴板", true, None::<&str>)?;
    let recv_folder_item = MenuItem::with_id(app, "open_recv", "接收目录", true, None::<&str>)?;
    let log_folder_item = MenuItem::with_id(app, "open_log", "日志目录", true, None::<&str>)?;
    let update_item = MenuItem::with_id(app, "check_update", "检查更新", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出飞梭", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &show_item,
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
            "show" => {
                show_main(app);
            }
            "send_clipboard" => {
                // 托盘常驻下窗口多半是隐藏的, 必须先唤出来并切到发送页,
                // 否则事件发出去了却什么也没发生 —— 用户只会认为菜单失灵。
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
                // 托盘常驻下窗口多半是隐藏的, 必须先唤出来并切到设置页高亮更新区:
                // 否则检查结果(有新版 / 已是最新 / 检查失败)全都在看不见的页面, 用户体验无反馈。
                show_main(app);
                let _ = app.emit("feisuo://open-settings-update", ());
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
            // ❌ 这里曾经写的是 `if let Click { button: Left, .. } = event { 切换面板 }`,
            //    **完全忽略 button_state**, 结果左键单击只会"闪一下"、面板打不开。
            //
            //    原因在 tray-icon 的 Windows 实现里: WM_LBUTTONDOWN 与 WM_LBUTTONUP
            //    各自映射成一条 TrayIconEvent::Click, 只有 button_state 不同
            //    (Down / Up)。于是**一次物理点击会进这个回调两次**:
            //      第 1 次 Down → 窗口还隐藏着 → show_main() 把面板弹出来
            //      第 2 次 Up   → 刚弹出来的窗口 is_visible() == true → hide()
            //    肉眼看到的就是面板一闪即没。右键不受影响, 因为菜单是在
            //    WM_RBUTTONUP 这一条路径上弹的(tray-icon 内部只认那一条)。
            //
            //    修法有两层: ① 只认抬起(Up), 按下不动 —— 一次物理点击 = 一次切换,
            //    与 Windows 任务栏/托盘"抬键才响应"的惯例一致;
            //    ② 再加 400ms 节流 —— 双击在 Windows 上的消息序列是
            //    Down/Up/DBLCLK/Up, 两条 Up 照样会"弹出→收起"闪一下。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if !allow_tray_toggle() {
                    return;
                }
                toggle_main(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// 距上一次托盘切换不足这个间隔的重复事件一律丢弃。
/// 窗口的显示/隐藏是"有反作用"的操作, 连做两次等于什么也没做,
/// 用户只会看到一闪 —— 所以宁可吞掉双击的后半截。
const TRAY_TOGGLE_THROTTLE: std::time::Duration = std::time::Duration::from_millis(400);

static LAST_TRAY_TOGGLE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// 本次左键事件是否值得执行切换（节流）。
fn allow_tray_toggle() -> bool {
    let now = std::time::Instant::now();
    // 锁中毒说明上一次持锁的线程 panic 了, 这里没有不变量需要保护,
    // 取回内部值继续用即可, 绝不因为中毒就永久屏蔽托盘点击。
    let mut last = match LAST_TRAY_TOGGLE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(prev) = *last {
        if now.duration_since(prev) < TRAY_TOGGLE_THROTTLE {
            return false;
        }
    }
    *last = Some(now);
    true
}

/// 左键语义: 面板在前台就收回到托盘, 否则唤出。
fn toggle_main(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let is_visible = window.is_visible().unwrap_or(false);
    let is_minimized = window.is_minimized().unwrap_or(false);
    if is_visible && !is_minimized {
        let _ = window.hide();
    } else {
        show_main(app);
    }
}

/// 唤出并聚焦主窗口。
///
/// 抽成函数是因为这个三行序列在托盘菜单里出现了四次 —— 而漏掉
/// `unminimize()` 的后果是"点了菜单窗口弹出来但还是最小化状态"，
/// 这类 bug 极难从日志里看出来。
pub(crate) fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 通知前端打开剪贴板直发（托盘菜单触发）。
fn emit_open_clipboard(app: &AppHandle) {
    if let Err(e) = app.emit_to("main", "feisuo://open-clipboard", ()) {
        // 前端还没加载完是常态（冷启动时点击菜单可能先于窗口就绪）
        warn!("发送剪贴板指令失败（前端可能尚未就绪）: {}", e);
    }
}
