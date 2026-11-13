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

/// 把 CRLF 统一成 LF。
///
/// 守卫测试是**读源码做文本匹配**，而源码在工作区里的换行符取决于 checkout
/// 时的 `core.autocrlf` —— 同一份提交，本机可能是 LF、GitHub 的
/// windows runner 是 CRLF。`"\n}\n"` 这种"行尾必须是裸换行"的模式在 CRLF
/// 下永远匹配不到，于是守卫在本机绿、在 CI 红（实测：v0.1.24 的 CI 就是
/// 在 `showing_the_window_must_force_it_to_the_front` 上挂的，本机全绿）。
///
/// 结论：**守卫读进来的源码必须先归一化**，断言只描述代码本身，不描述
/// 谁在哪个平台上 clone 的。
fn normalize_newlines(s: &str) -> String {
    // 先配对消掉 `\r\n`，再把残留的孤立 `\r` 去掉 —— 切片刚好切在 `\n` 上时，
    // 末尾会留下一个没有后续 `\n` 的 `\r`（`&body[..i]` 就是这样切的），
    // 它同样是换行编码的产物，不该参与比较。
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

/// 取 `show_main` 的函数体（含签名行）。
///
/// 用"行首的 `}`"当结束标记，而不是"`\n}\n`"—— 后者把**行尾的换行符**也写进了
/// 匹配条件，CRLF 下必然失配。
fn show_main_body(src: &str) -> &str {
    let start = src
        .find("pub(crate) fn show_main(")
        .expect("找不到 show_main —— 唤出入口改名了?");
    let rest = &src[start..];
    // 跳过签名行本身：签名行里不会出现行首 `}`，但这样写更直白。
    let end = rest
        .match_indices("\n}")
        .map(|(i, _)| i)
        .find(|&i| {
            // 只认"整行只有一个 `}`"（允许行尾空格），函数体内部不会有这种行。
            rest[i + 2..]
                .split('\n')
                .next()
                .map(|tail| tail.trim().is_empty())
                .unwrap_or(true)
        })
        .expect("show_main 函数体没有正常结束");
    &rest[..end]
}

#[test]
fn showing_the_window_must_force_it_to_the_front() {
    let src = tray_src();
    let body = show_main_body(&src);
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

/// 回归测试：v0.1.24 的 CI 在 windows runner 上挂掉，本机全绿，差别只是
/// checkout 出来的换行符（CRLF vs LF）。断言必须对两者给出**同一个**结论。
#[test]
fn line_endings_must_not_change_the_verdict() {
    let lf = tray_src();
    // 模拟 GitHub windows runner 的 checkout：autocrlf 把 LF 变成 CRLF。
    let crlf = lf.replace('\n', "\r\n");
    assert_ne!(lf, crlf, "构造 CRLF 样本失败");

    // 归一化之后两者逐字节相同 —— 这才是守卫该看到的东西。
    assert_eq!(normalize_newlines(&lf), normalize_newlines(&crlf));

    // 没归一化时，CRLF 版函数体尾部会多一个 `\r`；归一化后必须逐字节相同，
    // 也就是说 CRLF 只影响"换行怎么写"，不影响"守卫读到了什么"。
    assert_eq!(
        normalize_newlines(show_main_body(&lf)),
        normalize_newlines(show_main_body(&crlf))
    );

    // 归一化之前，"`\n}\n`"在 CRLF 文本里根本不存在 —— 这就是当初失败的根因。
    assert!(
        !crlf.contains("\n}\n"),
        "CRLF 样本里居然出现了裸 LF 结尾的函数括号, 样本构造有问题"
    );
}
