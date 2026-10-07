//! 桌面端「Rust 命令 ↔ 前端调用」双向对照守卫。
//!
//! 起因: 拖放到托盘常驻应用的隐藏窗口时, 文件进了发送清单却毫无可见反馈。
//! 修复方式是新增一个 `ensure_window_visible` 命令并在前端接上。
//! 这类"后端加了能力 / 前端忘了调"的失配, 编译期完全无感 —— Rust 侧
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
    // 唤窗必须放在**状态查询**这条路径上, 不能只放在事件回调里:
    // 事件实测比前端挂载监听器早 8ms, 必被丢弃。
    assert!(
        body.contains("ensureWindowVisible"),
        "syncEngineStatus 必须在引擎未启动时调用 ensureWindowVisible —— \
         事件路径会丢, 状态查询才是可靠兜底, 唤窗也必须在这里"
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

/// 用户必须知晓的外部事件, 必须把隐藏的窗口唤出来。
///
/// 飞梭是**托盘常驻**应用(关窗不退出), 窗口经常处于隐藏状态。
/// 而"应用内 toast"在窗口隐藏时用户根本看不到, 于是会出现两类静默故障:
/// 1. 传输完成/失败只有 toast —— 无人值守场景下用户永远不知道文件已到;
/// 2. 审批弹窗看不见 —— core 只等 60 秒, 于是静默等满再被拒,
///    对方以为传成功、本机什么都没发生, 双方对不上账。
#[test]
fn user_visible_events_must_surface_the_hidden_window() {
    let app = read(&ui_src().join("App.vue"));

    // 1) 审批请求
    let at = app
        .find("onApprovalRequest(")
        .expect("App.vue 必须订阅审批事件");
    let seg: String = app[at..].chars().take(700).collect();
    assert!(
        seg.contains("ensureWindowVisible"),
        "审批请求必须调用 ensureWindowVisible —— 窗口隐藏时弹窗不可见, \
         core 只等 60 秒, 用户会静默等满再被拒"
    );

    // 1b) 引擎启动失败: 同类问题里最严重的一处 —— 引擎挂了意味着**所有**传输
    // 都不工作, 而托盘图标照常显示, 是最难自查的一类故障。
    let at = app
        .find("onEngineError(")
        .expect("App.vue 必须订阅引擎错误事件");
    let seg: String = app[at..].chars().take(700).collect();
    assert!(
        seg.contains("ensureWindowVisible"),
        "引擎启动失败必须调用 ensureWindowVisible —— 托盘常驻下引擎挂掉是 \
         完全静默的: 图标在、功能无, 用户无从察觉"
    );

    // 2) 传输完成 / 失败
    let at = app
        .find("function handleTransferProgress(")
        .expect("找不到 handleTransferProgress");
    let body_start = app[at..].find('{').map(|i| at + i).unwrap();
    let chars: Vec<(usize, char)> = app[body_start..]
        .char_indices()
        .map(|(i, c)| (body_start + i, c))
        .collect();
    let mut depth = 0i32;
    let mut end = None;
    for (idx, &(_, c)) in chars.iter().enumerate() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(chars[idx].0);
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &app[body_start..end.expect("handleTransferProgress 函数体没有闭合")];

    assert!(
        body.contains("ensureWindowVisible"),
        "传输完成/失败必须调用 ensureWindowVisible —— 托盘常驻时应用内 toast \
         用户看不到, 等于白等"
    );
    let hits = body.matches("ensureWindowVisible").count();
    assert!(
        hits >= 2,
        "完成与失败两条分支都应唤出窗口, 实际只找到 {} 处",
        hits
    );
}

/// 拖放必须先把窗口唤出来。
///
/// 飞梭是托盘常驻应用, 窗口常处于隐藏状态, 而 Tauri 原生拖放在隐藏时照样触发。
/// 不唤出窗口的话, 用户把文件拖到看不见的地方, 文件进了清单却零反馈,
/// 表现为"拖了没反应"。
#[test]
fn native_drop_must_surface_the_hidden_window() {
    let bridge = read(&ui_src().join("api").join("feisuoBridge.ts"));
    let app = read(&ui_src().join("App.vue"));

    assert!(
        bridge.contains("onNativeDragDrop"),
        "必须有原生拖放监听 —— 浏览器 File 对象在 WebView2 下拿不到真实磁盘路径"
    );
    assert!(
        bridge.contains("\"ensure_window_visible\""),
        "桥接层必须暴露 ensure_window_visible"
    );

    // 找到 handleNativeDrop 的函数体, 确认里面真的调了它
    let at = app
        .find("function handleNativeDrop(")
        .expect("App.vue 里找不到 handleNativeDrop");
    let body_start = app[at..]
        .find('{')
        .map(|i| at + i)
        .expect("handleNativeDrop 缺少函数体");
    let chars: Vec<(usize, char)> = app[body_start..]
        .char_indices()
        .map(|(i, c)| (body_start + i, c))
        .collect();
    let mut depth = 0i32;
    let mut end = None;
    for (idx, &(_, c)) in chars.iter().enumerate() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(chars[idx].0);
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &app[body_start..end.expect("handleNativeDrop 函数体没有闭合")];

    assert!(
        body.contains("ensureWindowVisible"),
        "handleNativeDrop 必须调用 ensureWindowVisible —— 托盘常驻时窗口是隐藏的, \
         不唤出的话拖放完全静默"
    );

    // 后端命令必须真的存在
    let main_rs = read(&desktop_src().join("main.rs"));
    assert!(
        registered_commands(&main_rs).contains("ensure_window_visible"),
        "ensure_window_visible 没有在 generate_handler! 里注册"
    );
    let commands = read(&desktop_src().join("commands.rs"));
    assert!(
        commands.contains("pub fn ensure_window_visible"),
        "commands.rs 里缺少 ensure_window_visible 的实现"
    );
}
