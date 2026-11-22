//! 托盘形态守卫（主窗口始终显示）。
//!
//! 早先这里钉死"左键只认抬起 / 唤出必须带前台兜底"——那是"关窗进托盘、
//! 窗口经常隐藏"形态下的三次返工修复。现在主窗口始终显示、关窗即退出，
//! 托盘不再承担"唤出面板"的职责，那套逻辑整体删除。
//! 这里改钉新形态的不变量：托盘不得重新引入隐藏 / 切换 / 唤出逻辑。

use std::path::Path;

/// 把 CRLF 统一成 LF。
///
/// 守卫测试是**读源码做文本匹配**，而源码在工作区里的换行符取决于 checkout
/// 时的 `core.autocrlf` —— 同一份提交，本机可能是 LF、GitHub 的
/// windows runner 是 CRLF。断言只描述代码本身，不描述谁在哪个平台 clone 的。
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "")
}

fn tray_src() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("tray.rs");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e));
    normalize_newlines(&raw)
}

#[test]
fn tray_must_not_have_a_show_panel_menu_item() {
    let src = tray_src();
    assert!(
        !src.contains("\"show\""),
        "托盘不应再有「打开面板」菜单项：主窗口始终显示，无需唤出"
    );
    assert!(
        !src.contains("打开面板"),
        "托盘菜单里不该出现「打开面板」文案"
    );
}

#[test]
fn tray_must_never_hide_or_toggle_the_main_window() {
    let src = tray_src();
    for banned in [
        ".hide()",
        "fn toggle_main",
        "fn show_main",
        "fn ensure_foreground",
        "on_tray_icon_event",
    ] {
        assert!(
            !src.contains(banned),
            "托盘路径不得出现 `{banned}`：主窗口始终显示，关窗即退出，\
             任何隐藏/切换/唤出逻辑都会把这套形态改回去"
        );
    }
}

#[test]
fn tray_must_expose_quit_and_update_entries() {
    let src = tray_src();
    for needed in ["\"quit\"", "app.exit(0)", "\"check_update\""] {
        assert!(
            src.contains(needed),
            "托盘菜单缺少 {needed}：退出和检查更新必须仍可从托盘触达"
        );
    }
}

/// 回归测试：v0.1.24 的 CI 在 windows runner 上挂掉，本机全绿，差别只是
/// checkout 出来的换行符（CRLF vs LF）。断言必须对两者给出**同一个**结论。
#[test]
fn line_endings_must_not_change_the_verdict() {
    let lf = tray_src();
    let crlf = lf.replace('\n', "\r\n");
    assert_ne!(lf, crlf, "构造 CRLF 样本失败");

    assert_eq!(normalize_newlines(&lf), normalize_newlines(&crlf));

    // 关键：无论 LF 还是 CRLF，对"是否含被禁模式"必须给出同一结论。
    let banned_in_lf = lf.contains(".hide()") || lf.contains("fn show_main");
    let banned_in_crlf =
        normalize_newlines(&crlf).contains(".hide()") || normalize_newlines(&crlf).contains("fn show_main");
    assert_eq!(banned_in_lf, banned_in_crlf);
}
