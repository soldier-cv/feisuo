//! 桌面端「Rust 命令 ↔ 前端调用」双向对照守卫。
//!
//! 起因: 曾经为"托盘常驻隐藏窗口"新增 `ensure_window_visible` 唤窗命令。
//! 那套形态已废除（主窗口始终显示、关窗即退出），命令与全部调用点已移除。
//! 这里守的是命令注册与前端调用的双向一致 —— 这类"后端加了能力 /
//! 前端忘了调"的失配, 编译期完全无感 —— Rust 侧
//! `generate_handler!` 只是多一项, TS 侧类型也不会报错, 运行时才表现为
//! "功能没反应"。
//!
//! 与 `jni_guard` 同理, 这里把失配变成**编译失败**。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn desktop_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn ui_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("ui")
        .join("src")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e))
}

/// 解析 `tauri::generate_handler![ commands::foo, ... ]` 里注册的命令名。
fn registered_commands(main_rs: &str) -> BTreeSet<String> {
    let Some(start) = main_rs.find("generate_handler![") else {
        panic!("main.rs 里找不到 generate_handler! —— 守卫无法工作");
    };
    let body_start = start + "generate_handler![".len();
    // 逐字符做括号配平(源码含中文, 必须按字符而不是字节迭代)
    let chars: Vec<(usize, char)> = main_rs[body_start..]
        .char_indices()
        .map(|(i, c)| (body_start + i, c))
        .collect();
    let mut depth = 1i32;
    let mut end = None;
    for (idx, &(_, c)) in chars.iter().enumerate() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(chars[idx].0);
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &main_rs[body_start..end.expect("generate_handler! 的方括号没有闭合")];

    let mut out = BTreeSet::new();
    for line in body.lines() {
        let line = line.trim().trim_end_matches(',');
        // ⚠️ **必须跳过注释行**。
        //
        // 早先这里不跳，于是任何解释性的注释只要写了 `commands::xxx`
        // 就会被当成一条注册。实际踩到的是：删除 `remove_trusted_device`
        // 时我在注册表旁边留了句「原本这里还有 commands::remove_trusted_device」，
        // 守卫立刻报「已注册但前端从不调用」—— 而它**早就没注册了**。
        //
        // 这与本轮修掉的 `Deny(r) if r.contains("传输码")` 是同一类缺陷：
        // **判据挂在文本上，而文本里混着不承载语义的说明**。
        // 注释是给人看的，守卫必须当它不存在。
        if line.starts_with("//") || line.starts_with("/*") || line.starts_with('*') {
            continue;
        }
        // 接受**任意**模块限定路径，不只是 `commands::`。
        //
        // 早先只 `strip_prefix("commands::")`，于是 `clipboard::purge_clipboard_staging`
        // 这类注册**根本没被收集**：守卫报出 4 个"前端调用了未注册的命令"的假阳性，
        // 同时反向报"已注册但前端不调用"。两条都是守卫自己坏掉造成的假警报，
        // 而假警报比没有守卫更糟 —— 它让人开始习惯忽略红字。
        let Some((_module, name)) = line.rsplit_once("::") else {
            continue;
        };
        let name = name.trim();
        if !name.is_empty() {
            out.insert(name.to_string());
        }
    }
    assert!(
        !out.is_empty(),
        "从 generate_handler! 里一个命令都没解析出来 —— 解析逻辑坏了"
    );
    out
}

/// 解析前端 `invokeSafe("x")` / `invoke("x")` 里出现的命令名。
fn invoked_commands() -> BTreeSet<String> {
    let dir = ui_src();
    assert!(dir.is_dir(), "找不到前端源码目录: {}", dir.display());

    let mut out = BTreeSet::new();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)
            .unwrap_or_else(|e| panic!("读取 {} 失败: {}", d.display(), e))
            .flatten()
        {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "ts" && ext != "vue" {
                continue;
            }
            let src = read(&p);
            for marker in ["invokeSafe", "invoke"] {
                let mut from = 0usize;
                while let Some(pos) = src[from..].find(marker) {
                    // pos 是**相对 src[from..]** 的字节偏移, 切片时必须换算成
                    // 绝对偏移, 否则 from>0 时会切在多字节字符中间
                    // (源码里有中文, 这是第三次踩"not a char boundary")。
                    let abs = from + pos;
                    let at = abs + marker.len();
                    // 左边必须是标识符边界, 否则 myInvoke 之类会被误判
                    let left_ok = abs == 0
                        || !src[..abs]
                            .chars()
                            .next_back()
                            .map(|c| c.is_alphanumeric() || c == '_')
                            .unwrap_or(false);

                    // 标记之后必须**紧跟** `(` 或 `<...>(`。
                    // 只允许中间夹一个泛型参数组, 这样:
                    //   invokeSafe<T>("x") / invoke<boolean>("x")  -> 命中
                    //   const { invoke } = await import("...")      -> 落空
                    // (第二版就是漏了这个, 把模块路径 "@tauri-apps/api/core"
                    //  当成了命令名。)
                    let after = &src[at..];
                    let mut i = 0usize;
                    let chars: Vec<char> = after.chars().collect();
                    while i < chars.len() && chars[i].is_whitespace() {
                        i += 1;
                    }
                    if i < chars.len() && chars[i] == '<' {
                        let mut depth = 0i32;
                        while i < chars.len() {
                            match chars[i] {
                                '<' => depth += 1,
                                '>' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        i += 1;
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            i += 1;
                        }
                        while i < chars.len() && chars[i].is_whitespace() {
                            i += 1;
                        }
                    }
                    let looks_like_call = left_ok && i < chars.len() && chars[i] == '(';

                    if looks_like_call {
                        let byte_off: usize = chars[..i].iter().map(|c| c.len_utf8()).sum();
                        let arg = after[byte_off + 1..].trim_start();
                        if let Some(q) = arg.strip_prefix('"').or_else(|| arg.strip_prefix('\'')) {
                            if let Some(endq) = q.find('"').or_else(|| q.find('\'')) {
                                let name = &q[..endq];
                                // invoke 的也可能是事件名, 事件以 feisuo:// 开头
                                if !name.starts_with("feisuo://") && !name.is_empty() {
                                    out.insert(name.to_string());
                                }
                            }
                        }
                    }
                    from = at;
                }
            }
        }
    }
    out
}

#[test]
fn guard_files_must_exist() {
    assert!(
        desktop_src().join("main.rs").is_file(),
        "找不到 main.rs"
    );
    assert!(
        ui_src().join("api").join("feisuoBridge.ts").is_file(),
        "找不到 feisuoBridge.ts, 守卫会静默空跑"
    );
}

#[test]
fn frontend_must_not_call_unregistered_commands() {
    let registered = registered_commands(&read(&desktop_src().join("main.rs")));
    let invoked = invoked_commands();

    let missing: Vec<&String> = invoked.difference(&registered).collect();
    assert!(
        missing.is_empty(),
        "前端调用了未在 generate_handler! 注册的命令 —— 运行时会直接失败: {:?}\n\
         (已注册 {} 个)",
        missing,
        registered.len()
    );
}

#[test]
fn registered_commands_must_be_reachable_from_frontend() {
    let registered = registered_commands(&read(&desktop_src().join("main.rs")));
    let invoked = invoked_commands();

    // update_app_config 这类命令由桥接层统一封装后调用, 也应能扫到;
    // 若将来出现纯内部命令(仅托盘菜单调用), 在这里显式豁免并说明原因。
    const INTERNAL_ONLY: &[&str] = &[];

    let orphans: Vec<&String> = registered
        .difference(&invoked)
        .filter(|c| !INTERNAL_ONLY.contains(&c.as_str()))
        .collect();

    assert!(
        orphans.is_empty(),
        "以下命令已注册但前端从不调用 —— 要么是忘了接线(功能不会生效), \
         要么是死代码: {:?}",
        orphans
    );
}

/// 引擎健康状态必须来自**权威查询**, 不能靠"设备列表没抛异常"推断。
///
/// 引擎启动失败时(端口被占用最常见), `get_online_devices` 返回的是
/// `Ok([])` —— 发现服务的设备表本来就是空的, 调用**不会**报错。
/// 旧写法据此把在线状态置为 true 并顺手清空 `engineError`, 于是:
///   * 界面顶着绿灯显示"局域网在线";
///   * 真正的启动失败横幅在 5 秒内被自动擦掉;
///   * 托盘常驻下窗口不显示, 用户只看到托盘图标, 以为一切正常,
///     直到某天需要传文件才发现永远传不了。
#[test]
fn engine_health_must_be_queried_not_inferred() {
    let bridge = read(&ui_src().join("api").join("feisuoBridge.ts"));
    let app = read(&ui_src().join("App.vue"));
    let main_rs = read(&desktop_src().join("main.rs"));
    let commands = read(&desktop_src().join("commands.rs"));

    // 1) 桥接层与后端都必须有这个查询
    assert!(
        bridge.contains("get_engine_status"),
        "桥接层缺少 getEngineStatus"
    );
    assert!(
        registered_commands(&main_rs).contains("get_engine_status"),
        "get_engine_status 没有在 generate_handler! 里注册"
    );
    assert!(
        commands.contains("pub fn get_engine_status"),
        "commands.rs 缺少 get_engine_status 实现"
    );
    // 后端必须报**引擎自己的**状态, 而不是转发某次调用的成败
    assert!(
        commands.contains("is_started()"),
        "get_engine_status 必须读取引擎自身的 is_started(), \
         转发设备列表调用的成败等于回到旧的错误推断"
    );

    // 2) 前端必须有主动同步函数
    assert!(
        app.contains("function syncEngineStatus("),
        "App.vue 缺少 syncEngineStatus —— 引擎状态必须被主动查询"
    );

    // 3) syncEngineStatus 必须**从查询结果取值**, 而不是写死。
    //    只查 refreshDevices 里有没有 syncEngineStatus 调用是不够的 ——
    //    守卫第一版就栽在这: 注入把赋值搬到 syncEngineStatus 内部就绕过了。
    let at = app
        .find("function syncEngineStatus(")
        .expect("找不到 syncEngineStatus");
    let body_start = app[at..].find('{').map(|i| at + i).unwrap();
    let body: String = app[body_start..].chars().take(1400).collect();
    assert!(
        body.contains("st.running") || body.contains("status.running"),
        "engineOnline 必须取自查询结果, 不得写死 —— 写死就退回了 \
         '永远显示在线'的老路"
    );
    assert!(
        body.contains("st.error") || body.contains("status.error"),
        "engineError 必须取自查询结果"
    );
    assert!(
        !body.contains("engineOnline.value = true"),
        "syncEngineStatus 里不得把 engineOnline 写死为 true"
    );
    assert!(
        !body.contains("engineError.value = \"\";"),
        "syncEngineStatus 里不得无条件清空 engineError —— 引擎失败时那正是 \
         唯一让用户看见问题的线索"
    );
    // 4) refreshDevices 必须改用 syncEngineStatus
    let at = app
        .find("async function refreshDevices(")
        .expect("找不到 refreshDevices");
    let body_start = app[at..].find('{').map(|i| at + i).unwrap();
    let body: String = app[body_start..].chars().take(1400).collect();
    assert!(
        !body.contains("engineError.value = \"\""),
        "refreshDevices 里不得再清空 engineError"
    );
    assert!(
        !body.contains("engineOnline.value = true"),
        "refreshDevices 里不得再用'调用没抛异常即在线'的推断"
    );
    assert!(
        body.contains("syncEngineStatus"),
        "refreshDevices 必须改用 syncEngineStatus 同步真实引擎状态"
    );
}

/// 主窗口始终显示：关窗即退出，不再有"隐藏→唤出"形态。
///
/// 曾经的 `ensure_window_visible` 命令及前端全部调用点已移除；主窗口配置
/// 必须是 `visible: true`，且不再拦截原生关闭请求。任何一边回退都会让
/// "拖到任务栏按钮没反应"那类问题以另一种形式复现。
#[test]
fn main_window_must_always_be_visible_and_close_must_exit() {
    // desktop_src() 指向 src-tauri/src，配置文件在其上一级 src-tauri/
    let conf_path = desktop_src()
        .parent()
        .expect("src-tauri/src 的父目录不存在?")
        .join("tauri.conf.json");
    let conf = read(&conf_path);
    assert!(
        conf.contains("\"visible\": true"),
        "主窗口必须 visible: true —— 隐藏窗口在 Windows 上没有任务栏按钮，\
         拖文件到任务栏不会有任何反应"
    );

    let main_rs = read(&desktop_src().join("main.rs"));
    assert!(
        !main_rs.contains("prevent_close"),
        "main.rs 不得再拦截 CloseRequested —— 关窗应真正退出，而不是隐藏到托盘"
    );

    let app = read(&ui_src().join("App.vue"));
    let bridge = read(&ui_src().join("api").join("feisuoBridge.ts"));
    for token in ["ensureWindowVisible", "closeToTray", "hideWindow"] {
        assert!(
            !app.contains(token) && !bridge.contains(token),
            "前端不得再出现 `{token}` —— 那是'关窗隐藏→拖放唤出'旧形态的残留"
        );
    }

    let commands = read(&desktop_src().join("commands.rs"));
    for token in ["ensure_window_visible", "hide_window"] {
        assert!(
            !commands.contains(token),
            "commands.rs 不得再出现 `{token}` 命令 —— 旧形态已废除"
        );
    }
}

/// 拖放落点必须跟随光标的**就近聚焦**，且"放大的是谁就发给谁"。
///
/// Tauri 在原生层接管文件拖放，WebView2 收不到 HTML 的 `dragover`；加上窗口
/// 可能被遮挡，用户未必看得清光标压在哪张卡片上。因此按**纵向距离优先**
/// 吸附最近的可发送设备，聚焦卡片放大、其余淡化，松手发给聚焦的那台。
#[test]
fn native_drop_must_target_the_focused_device() {
    let bridge = read(&ui_src().join("api").join("feisuoBridge.ts"));
    let app = read(&ui_src().join("App.vue"));

    // 1. 原生事件必须把物理像素坐标转出来。
    assert!(
        bridge.contains("position: { x: p.position.x, y: p.position.y }"),
        "onNativeDragDrop 没有把 position 转给前端 —— 没有坐标就无法做就近聚焦"
    );

    // 2. 命中测试按 devicePixelRatio 换算（Tauri 物理像素 → CSS 像素）。
    assert!(app.contains("elementFromPoint("), "缺少 elementFromPoint 命中测试");
    assert!(
        app.contains("window.devicePixelRatio"),
        "命中测试没有按 devicePixelRatio 换算：125% 缩放下会偏到隔壁那台设备"
    );
    assert!(app.contains("data-device-id"), "设备卡片缺少可识别标记");

    // 3. 就近聚焦函数：按卡片实际位置 + 只考虑可发送设备。
    let f = app
        .find("function focusDeviceAtPoint(")
        .expect("缺少 focusDeviceAtPoint：拖放不再按光标就近聚焦");
    // 用 chars 截断，不能按字节切：中文一个字占 3 字节，按字节切会切在字符中间
    let fbody: String = app[f..].chars().take(900).collect();
    assert!(
        fbody.contains("getBoundingClientRect()"),
        "聚焦必须基于卡片实际位置，否则光标上下移动时高亮不会跟随"
    );
    assert!(
        fbody.contains("canDropTo("),
        "聚焦只能落在可发送设备上：聚焦一台离线设备会让人以为能发"
    );

    // 4. 松手发给聚焦的那台（高亮与落点一致），而不是当前选中设备。
    assert!(
        app.contains("const targetId = dragOverDeviceId.value || deviceCardAtPoint(ev.position)"),
        "drop 必须用当前聚焦的设备作为目标，否则会出现'放大的是 A、发出去的是 B'"
    );
    let drop_fn = app
        .find("async function handleNativeDrop(")
        .expect("App.vue 里找不到 handleNativeDrop");
    let fn_body: String = app[drop_fn..].chars().take(2000).collect();
    assert!(
        fn_body.contains("explicitDeviceId") && fn_body.contains("routeDroppedPathsToDevice"),
        "handleNativeDrop 必须使用松手处的设备 id 分派"
    );

    // 5. 聚焦的视觉反馈必须在模板和样式里都接上。
    for token in ["'drop-target'", "'drag-dimmed'"] {
        assert!(app.contains(token), "缺少聚焦视觉类 {token}：用户无法确认松手发给谁");
    }
}
