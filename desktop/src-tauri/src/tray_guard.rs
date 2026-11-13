//! 托盘左键行为守卫。
//!
//! 同一个缺陷返工过三次, 每次都不是编译错误, 只能靠人工点托盘图标才发现:
//!
//! 1. 忽略 `button_state` —— 一次物理点击进回调两次(Down/Up), 显示与隐藏
//!    互相抵消, 面板一闪即没;
//! 2. 只认 Up 但仍然"切换" —— 多出来的那条 Up(双击慢于节流、鼠标连击、
//!    用户以为没点上再点一次)照样把刚弹出来的面板收回去;
//! 3. 唤出只用 `show()+set_focus()`, 没有前台兜底 —— 托盘点击的输入归属
//!    explorer.exe, `SetForegroundWindow` 被静默拒绝时窗口画在别的窗口后面,
//!    用户以为没打开, 于是再点一次, 又撞上第 2 条的"切换"。
//!
//! 所以这里把三件事钉死: 左键只认抬起、托盘路径绝不隐藏窗口、
//! 唤出序列必须带前台兜底。

use std::path::Path;

fn tray_src() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("tray.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e))
}

/// 取 `on_tray_icon_event(...)` 到 `.build(app)` 之间的回调体。
fn left_click_handler(src: &str) -> &str {
    let start = src
        .find(".on_tray_icon_event(")
        .expect("tray.rs 里找不到 on_tray_icon_event —— 托盘事件入口改名了?");
    let rest = &src[start..];
    let end = rest
        .find(".build(app)")
        .expect("on_tray_icon_event 之后找不到 .build(app) —— 托盘构建链被改写了?");
    &rest[..end]
}

#[test]
fn left_click_must_only_react_to_button_up() {
    let src = tray_src();
    let handler = left_click_handler(&src);
    assert!(
        handler.contains("MouseButtonState::Up"),
        "托盘左键必须只响应抬起事件: tray-icon 在 Windows 上对 WM_LBUTTONDOWN 与 \
         WM_LBUTTONUP 各发一条 Click, 只认抬起才能保证一次物理点击只处理一次"
    );
    assert!(
        !handler.contains("MouseButtonState::Down"),
        "托盘左键不应再处理按下事件 —— 那会让一次点击被处理两次"
    );
}

#[test]
fn tray_must_never_hide_the_main_window() {
    let src = tray_src();
    let handler = left_click_handler(&src);
    assert!(
        !handler.contains(".hide()"),
        "托盘事件路径不得隐藏主窗口: 点托盘图标只可能是想看面板, \"再点一次收起\"的\
         切换语义遇到任何多余事件都会把刚弹出的面板收回去"
    );
    assert!(
        !src.contains("fn toggle_main"),
        "toggle_main() 会被重新写成\"可见就收起\", 这正是本 bug 的来源; \
         显示入口只留 show_main()"
    );
}

#[test]
fn showing_the_window_must_force_it_to_the_front() {
    let src = tray_src();
    let start = src
        .find("pub(crate) fn show_main(")
        .expect("找不到 show_main —— 唤出入口改名了?");
    let body_end = src[start..]
        .find("\n}\n")
        .map(|i| start + i)
        .expect("show_main 函数体没有正常结束");
    let body = &src[start..body_end];
    for needed in [".show()", "unminimize()", "set_focus()", "ensure_foreground("] {
        assert!(
            body.contains(needed),
            "show_main() 缺少 {needed}: 托盘点击必须把面板真正抬到最前面, \
             否则窗口会被画在别的窗口后面, 用户以为没打开"
        );
    }
    assert!(
        src.contains("set_always_on_top(true)") && src.contains("set_always_on_top(false)"),
        "前台兜底缺少\"短暂置顶再取消置顶\": SetForegroundWindow 被系统拒绝时, \
         这是唯一不依赖前台权限的抬窗手段"
    );
}
