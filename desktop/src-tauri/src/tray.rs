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
                    show_main(&handle);
                });
            }
            "quit" => {
                info!("Quitting Feisuo from tray menu");
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键单击托盘图标 = 把主面板叫到最前面，**不再"再点一次收起"**。
            //
            // 这个回调踩过两次同样的坑, 所以把结论留在代码里:
            //   ① tray-icon 0.25 的 Windows 实现把 WM_LBUTTONDOWN 与
            //      WM_LBUTTONUP 各映射成一条 TrayIconEvent::Click(只有
            //      button_state 不同), 一次物理点击会进这里两次。忽略
            //      button_state 时两次切换互相抵消, 面板一闪即没 —— 所以
            //      只认抬起(Up), 与 Windows 任务栏"抬键才响应"的惯例一致。
            //   ② 光靠"只认抬起 + 400ms 节流"仍然治不好本 bug: 只要多出来
            //      的那条 Up 落在节流窗口之外(双击慢于 400ms、鼠标微动连击、
            //      用户以为没点上再点一次), "切换"就会把刚弹出来的面板收回
            //      去, 表现依旧是"点一下闪一下, 只有右键菜单能打开"。
            //
            // 根子在于"切换"这个语义本身: 显示是有收益的操作, 隐藏不是 ——
            // 用户点托盘图标永远是想看到面板。Windows 上微信/QQ 的托盘也是
            // 点了就显示, 收起只走窗口上的关闭按钮。改成纯显示后, 多余的
            // 事件最多重复显示同一个窗口, 不可能再把面板弄丢。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// 唤出主窗口并把它真正抬到最前面。
///
/// 抽成函数是因为这个序列在托盘菜单里出现了四次 —— 而漏掉
/// `unminimize()` 的后果是"点了菜单窗口弹出来但还是最小化状态"，
/// 这类 bug 极难从日志里看出来。
pub(crate) fn show_main(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.show();
    if window.is_minimized().unwrap_or(false) {
        let _ = window.unminimize();
    }
    let _ = window.set_focus();
    ensure_foreground(app);
}

/// Windows 前台抢占的兜底。
///
/// 托盘图标的点击归属 explorer.exe, 我们调 `SetForegroundWindow` 时常被系统
/// 拒绝 —— 而且拒绝是静默的: 窗口照样画出来, 只是压在别的窗口后面、键盘焦点
/// 也不在它身上。用户看到的是"点了像没点", 于是再点一次; 只要那个版本的
/// 左键还是"切换"语义, 面板就会被收回去, 正是"闪一下就最小化"。
///
/// 这里等事件循环把 show/unminimize 处理完(tao 在 Windows 上把这几步投递到
/// 主线程执行), 再看一眼是否真的拿到焦点; 没拿到就短暂置顶再取消置顶 ——
/// 置顶不受前台权限限制, 能把窗口抬到本层最前, 取消后也不会永久置顶。
fn ensure_foreground(app: &AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let Some(window) = handle.get_webview_window("main") else {
            return;
        };
        // 这期间用户又把它收起了(点关闭 / 走托盘菜单), 就不要再拽回来
        if !window.is_visible().unwrap_or(false) || window.is_focused().unwrap_or(false) {
            return;
        }
        let _ = window.set_always_on_top(true);
        let _ = window.set_always_on_top(false);
        let _ = window.set_focus();
    });
}

/// 通知前端打开剪贴板直发（托盘菜单触发）。
fn emit_open_clipboard(app: &AppHandle) {
    if let Err(e) = app.emit_to("main", "feisuo://open-clipboard", ()) {
        // 前端还没加载完是常态（冷启动时点击菜单可能先于窗口就绪）
        warn!("发送剪贴板指令失败（前端可能尚未就绪）: {}", e);
    }
}
