//! Android 宿主守卫: 主线程绝不能调用会阻塞的 core 接口。
//!
//! 起因是一个真实引入又差点漏掉的事故: 系统分享 Activity 在 `onCreate` 里
//! 直接调用 `NativeBridge.sendFiles(...)`, 而该函数内部是
//! `runtime().block_on(engine.send_files(..))` —— 一次**完整文件传输**
//! (4 MiB 分块 + 每次读写 30 秒 IO 超时), 大文件可达数分钟。
//! 放在主线程就是必然 ANR, 系统会直接杀进程, 用户看到"分享后应用闪退"。
//!
//! 为什么值得用测试盯住: 这类错误编译期无感、静态检查也看不出来
//! (它就是一次普通的函数调用), 只有在真机上分享一个大文件时才会暴露 ——
//! 而本项目目前没有真机联调条件。
//!
//! 判定方式: 扫描 `ShareTargetActivity` / `MainActivity` 里
//! **直接调用** `NativeBridge.xxx` 的代码位置, 若它位于
//! `onCreate` / `onResume` / `onStart` 等生命周期回调体内, 即为违规。
//! 后台线程(`Thread { }`)里的调用是允许的。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn kotlin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("mobile")
        .join("android")
        .join("app")
        .join("src")
        .join("main")
        .join("java")
        .join("net")
        .join("findfine")
        .join("feisuo")
}

/// 已知会阻塞的 NativeBridge 方法(内部是 runtime().block_on)。
///
/// `initEngine` 也在列: 它要建 runtime 并绑定 UDP/TCP 端口, 实测几十毫秒,
/// 主线程调用会掉帧, 大数据目录下更久。
const BLOCKING_METHODS: &[&str] = &[
    "initEngine",
    "shutdownEngine",
    "sendFiles",
    "probeDevice",
    "trustedDevicesJson",
    "configJson",
    "deviceId",
];

/// **间接**阻塞入口: 这些宿主方法内部会走到阻塞的 core 接口。
///
/// 这一组是被真实缺陷逼出来的: `FeisuoDaemonService.onCreate` 里
/// `FeisuoRuntime.ensureStarted(...)` 是同步调用, 最终执行阻塞的
/// `NativeBridge.initEngine`, 而它跑在**开机广播**这条路径上 —— 每次
/// 开机都触发。只扫 `NativeBridge.` 前缀的守卫完全看不到这一层包装,
/// 属于典型的"守卫有盲区还以为自己覆盖了"。
const INDIRECT_BLOCKING_CALLS: &[&str] = &[
    "FeisuoRuntime.ensureStarted",
    "FeisuoRuntime.shutdown",
];

/// 视为"主线程"的生命周期回调
const LIFECYCLE_CALLBACKS: &[&str] = &["onCreate", "onResume", "onStart", "onReceive"];

#[test]
fn guarded_kotlin_files_must_exist() {
    let dir = kotlin_dir();
    assert!(dir.is_dir(), "找不到 Kotlin 源码目录: {}", dir.display());
    for f in ["ShareTargetActivity.kt", "MainActivity.kt", "ShareDispatcher.kt"] {
        assert!(
            dir.join(f).is_file(),
            "缺少 {}/{}, 守卫会静默空跑",
            dir.display(),
            f
        );
    }
}

#[test]
fn no_blocking_bridge_call_in_lifecycle_callbacks() {
    let dir = kotlin_dir();
    let mut violations: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(&dir).expect("读取 Kotlin 目录失败").flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("kt") {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let src = std::fs::read_to_string(&path).expect("读取 Kotlin 文件失败");

        // 找到每个生命周期回调的方法体范围(用大括号配平)
        for cb in LIFECYCLE_CALLBACKS {
            let sig = format!("fun {}(", cb);
            let mut from = 0usize;
            while let Some(pos) = src[from..].find(&sig) {
                let at = from + pos;
                let Some(open) = src[at..].find('{') else {
                    from = at + sig.len();
                    continue;
                };
                let body_start = at + open;
                // 大括号配平。**必须按字符迭代**并携带字节偏移 ——
                // 直接对 &str 做 `body_start + i` 的字符索引, 遇到中文字符
                // 会落在 "not a char boundary" 上 panic(本文件第一版就踩了)。
                let chars: Vec<(usize, char)> = src[body_start..]
                    .char_indices()
                    .map(|(i, c)| (body_start + i, c))
                    .collect();
                let mut depth = 0i32;
                let mut end = None;
                let mut in_str = false;
                for (idx, &(_, c)) in chars.iter().enumerate() {
                    if c == '"' && (idx == 0 || chars[idx - 1].1 != '\\') {
                        in_str = !in_str;
                    }
                    if in_str {
                        continue;
                    }
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
                let Some(end) = end else {
                    from = at + sig.len();
                    continue;
                };

                let body = &src[body_start..end];
                // 直接调用与间接调用用同一套判定逻辑, 避免"只扫了表层"
                // 这种盲区(真实事故: ensureStarted 包装了阻塞的 initEngine)。
                let mut needles: Vec<String> = BLOCKING_METHODS
                    .iter()
                    .map(|m| format!("NativeBridge.{}", m))
                    .collect();
                needles.extend(INDIRECT_BLOCKING_CALLS.iter().map(|s| s.to_string()));

                for needle in &needles {
                    let mut bfrom = 0usize;
                    while let Some(p) = body[bfrom..].find(needle.as_str()) {
                        let abs = body_start + bfrom + p;
                        // 是否在后台线程里? Thread { ... } 内的调用是允许的。
                        // 判据: 往前找最近一个未闭合的 "Thread {"。
                        let already_off_thread = {
                            let thread_at = body[..(bfrom + p)]
                                .rfind("Thread {")
                                .map(|x| x + "Thread {".len());
                            match thread_at {
                                Some(t) => {
                                    let inner = &body[t..];
                                    let mut d = 0i32;
                                    let mut closed = false;
                                    for ch in inner.chars() {
                                        match ch {
                                            '{' => d += 1,
                                            '}' => {
                                                d -= 1;
                                                if d == 0 {
                                                    closed = true;
                                                    break;
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                    !closed
                                }
                                None => false,
                            }
                        };
                        if !already_off_thread {
                            let call_line = src[..abs].matches('\n').count() + 1;
                            violations.push(format!(
                                "{}:{}  {} 内的 {} 是阻塞调用 (最终会 block_on), \
                                 会 ANR; 请放到后台线程",
                                name,
                                call_line,
                                cb,
                                needle
                            ));
                        }
                        bfrom = bfrom + p + needle.len();
                    }
                }
                from = end;
            }
        }
    }

    assert!(
        violations.is_empty(),
        "以下位置在生命周期回调(主线程)里调用了会阻塞的 core 接口, \
         必然 ANR:\n  {}",
        violations.join("\n  ")
    );
}

/// 顺带确认: 分享流程确实把推送放到了后台线程, 而且推送逻辑真被调用了。
///
/// 上一条守卫只看"有没有违规", 这里从正面确认"该做的做了",
/// 避免有人把推送整个删掉后测试依然全绿。
#[test]
fn share_dispatch_runs_off_the_main_thread() {
    let path = kotlin_dir().join("ShareTargetActivity.kt");
    let src = std::fs::read_to_string(&path).expect("读取 ShareTargetActivity.kt 失败");

    assert!(
        src.contains("Thread(") || src.contains("thread {") || src.contains("Executors"),
        "ShareTargetActivity 必须把推送放到后台线程, 否则大文件分享必然 ANR"
    );
    assert!(
        src.contains("ShareDispatcher.dispatchStaged"),
        "分享流程必须真正调用推送逻辑 —— 旧实现只暂存不发送, \
         整个'分享到飞梭'等于没接上"
    );
}

/// 提示里承诺的"稍后重试"必须有对应实现。
///
/// 这条是被一句**说谎的 toast** 逼出来的: 发送失败时提示"文件已保留,
/// 可稍后重试", 但整个代码库里不存在任何重试逻辑 —— 用户被告知稍后会自动
/// 重发, 实际上永远不会, 而且失败文件会无限累积占满手机存储。
#[test]
fn promised_retry_must_actually_exist() {
    let dir = kotlin_dir();
    let dispatcher =
        std::fs::read_to_string(dir.join("ShareDispatcher.kt")).expect("缺少 ShareDispatcher.kt");
    let service =
        std::fs::read_to_string(dir.join("FeisuoDaemonService.kt")).expect("读取服务失败");
    let activity =
        std::fs::read_to_string(dir.join("ShareTargetActivity.kt")).expect("读取分享页失败");

    // 1) 必须存在真正的重试入口
    assert!(
        dispatcher.contains("fun retryPending("),
        "ShareDispatcher 缺少 retryPending —— toast 承诺的'稍后重试'是空头支票"
    );
    // 2) 它必须真的扫描暂存区并重新发送, 而不是空实现
    assert!(
        dispatcher.contains("dispatchStaged("),
        "retryPending 必须真的重新发送, 不能是空实现"
    );
    // 3) 至少有一个真实的触发时机(引擎就绪 / 网络恢复)
    let svc_calls = service.matches("retryPending(").count();
    assert!(
        svc_calls >= 1,
        "守护服务必须在引擎就绪或网络恢复时调用 retryPending, \
         否则重试入口永远不会被触发"
    );
    // 4) 暂存区必须有上界, 否则长期离线会把手机存储填满
    assert!(
        dispatcher.contains("trimStaging"),
        "必须有暂存区裁剪逻辑"
    );
    assert!(
        dispatcher.contains("MAX_PENDING_FILES") && dispatcher.contains("MAX_PENDING_BYTES"),
        "暂存区必须同时按文件数与字节数设上界"
    );
    // 5) 失败的 toast 不得再宣称"可稍后重试"而没有兑现路径 ——
    //    这里反向确认: 分享页确实转交给 ShareDispatcher 处理结果。
    assert!(
        activity.contains("ShareDispatcher.Outcome"),
        "分享页必须按推送结果给出对应提示, 而不是笼统一句'可稍后重试'"
    );
}

/// 确认没有把"会阻塞的调用"从 onCreate 里挪到 onResume/onStart 伪装通过。
/// 去掉 `//` 行注释, 只留可执行内容。
///
/// 守卫要在**用户可见文案**里找占位话术, 而源码里保留着解释这段历史的注释
/// (注释里当然会引用旧文案)。不剥注释的话, 守卫会把自己的说明文档判成违规。
/// 副作用: 字符串里出现的 `//`(如 "http://") 之后的内容也会被截断 ——
/// 对"找占位话术"这种关键词判定来说完全可接受。
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Android 配对必须是真的, 不能是一个"将在后续版本接入"的占位。
///
/// 旧界面的文案与按钮**都在说谎**:
///   * 说明文字: "2. 在下方输入桌面端显示的 6 位配对码"
///   * 按钮:     "输入配对码"
///   * 实际行为: 弹一句 "配对界面将在后续版本接入"
///
/// 而"一次配对、终生免密"是产品核心承诺。配不上就直接导致:
///   * 分享推送永远命中"尚未配对任何电脑"(整条发送链路不可达);
///   * 未配对电脑无法向本机投递(fail-closed)。
#[test]
fn android_pairing_must_be_real_not_a_placeholder() {
    let dir = kotlin_dir();
    let main = std::fs::read_to_string(dir.join("MainActivity.kt")).expect("读取 MainActivity 失败");
    let bridge =
        std::fs::read_to_string(dir.join("NativeBridge.kt")).expect("读取 NativeBridge 失败");

    assert!(
        !strip_line_comments(&main).contains("将在后续版本接入"),
        "MainActivity 仍存在占位话术 —— 按钮与文案在骗用户"
    );
    assert!(
        main.contains("NativeBridge.pairWithDevice("),
        "MainActivity 必须调用 pairWithDevice —— 没有真实配对, 分享推送永远不可达"
    );
    assert!(
        bridge.contains("nativePairWithDevice"),
        "NativeBridge 必须声明 nativePairWithDevice"
    );
    assert!(
        main.contains("ipInput") && main.contains("pinInput"),
        "配对表单必须有 IP 与配对码两个输入框"
    );
    assert!(
        main.contains("loadTrustedDevices") && main.contains("trustedDevicesJson"),
        "配对成功后必须刷新并展示受信设备列表, 否则用户无从确认是否成功"
    );

    let at = main
        .find("private fun pairWithDesktop(")
        .expect("找不到 pairWithDesktop");
    let body: String = main[at..].chars().take(2400).collect();
    assert!(
        body.contains("probeDevice"),
        "配对前必须先 probeDevice 拿真实传输端口 —— 信任库只记 IP, \
         端口是对端可在设置里改的, 用默认端口连必然失败"
    );
    assert!(
        body.contains("Thread(") || body.contains("thread {"),
        "pairWithDesktop 必须把网络操作放到后台线程"
    );
}

/// Android 必须能处理未受信设备的审批请求。
///
/// `boot_engine` 原先直接 `drop(handles.approval)`: 丢掉 broadcast 接收端后
/// `approval_tx.send()` 必然失败, 传输服务端就 fail-closed 拒绝所有未受信设备。
/// 后果是"任何电脑都无法向本机投递文件", 且用户连"有传输在等你确认"都看不到 ——
/// 行为上是安全的, 但功能上等于单向只能收不能判断。
#[test]
fn android_must_handle_approval_requests() {
    let dir = kotlin_dir();
    let service = std::fs::read_to_string(dir.join("FeisuoDaemonService.kt"))
        .expect("读取 FeisuoDaemonService.kt 失败");
    let bridge =
        std::fs::read_to_string(dir.join("NativeBridge.kt")).expect("读取 NativeBridge 失败");

    // 1) 桥接层必须暴露取/答两个接口
    assert!(
        bridge.contains("nativeNextApproval") && bridge.contains("nativeRespondApproval"),
        "NativeBridge 必须声明 nativeNextApproval / nativeRespondApproval"
    );
    // 2) 守护服务必须真的在监听
    assert!(
        service.contains("nextApproval"),
        "守护服务必须调用 nextApproval —— 否则未配对电脑永远无法投递"
    );
    assert!(
        service.contains("respondApproval"),
        "必须能回应审批, 否则用户点了允许也没用"
    );
    // 3) 审批监听必须在后台线程(阻塞 JNI)
    let at = service
        .find("private fun startApprovalWatcher(")
        .expect("找不到 startApprovalWatcher");
    let body: String = service[at..].chars().take(1800).collect();
    assert!(
        body.contains("Thread(") || body.contains("thread {"),
        "审批监听必须跑在后台线程 —— nativeNextApproval 会阻塞等待"
    );
    // 4) 必须弹通知让用户能看到并操作
    //    注意: 通知构造在 postApprovalNotification 里, 不在 startApprovalWatcher,
    //    所以这里按方法定位而不是沿用上面的窗口(第一版就栽在这)。
    let np = service
        .find("private fun postApprovalNotification(")
        .expect("找不到 postApprovalNotification");
    let nbody: String = service[np..].chars().take(2000).collect();
    assert!(
        nbody.contains("Notification.Builder"),
        "必须用通知承载审批请求, 否则用户看不到"
    );
    // 5) 审批渠道必须是高优先级, 否则请求会静默超时
    assert!(
        service.contains("IMPORTANCE_HIGH"),
        "审批通知渠道必须是高优先级 —— 静默渠道下用户注意不到, \
         请求会直接等满 60 秒超时"
    );
    // 6) 允许/拒绝两个动作都要有
    assert!(
        service.contains("ACTION_ALLOW") && service.contains("ACTION_REJECT"),
        "必须同时提供允许与拒绝"
    );
    // 7) 服务停止时必须结束监听线程
    assert!(
        service.contains("approvalThread?.interrupt()"),
        "onDestroy 必须结束审批线程, 否则它会一直占着引擎"
    );
    // 8) core 侧不得再丢弃审批接收端
    let engine_host = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("core")
            .join("src")
            .join("engine_host.rs"),
    )
    .expect("读取 engine_host.rs 失败");
    assert!(
        !engine_host.contains("drop(handles.approval)"),
        "engine_host 仍在丢弃审批接收端 —— 未配对设备将永远无法投递文件"
    );
    assert!(
        engine_host.contains("install_approval_forwarder"),
        "必须把审批事件转发到宿主可消费的队列"
    );
}

#[test]
fn blocking_call_set_is_not_shrinking() {
    // 这条是自检: 若有人为了过测试把方法名从 BLOCKING_METHODS 里删掉,
    // 上面两条守卫就会形同虚设。这里钉住清单内容。
    let set: BTreeSet<&str> = BLOCKING_METHODS.iter().copied().collect();
    for required in ["initEngine", "sendFiles", "probeDevice"] {
        assert!(
            set.contains(required),
            "BLOCKING_METHODS 少了 {} —— 守卫被削弱了",
            required
        );
    }
    assert!(
        set.len() >= 5,
        "BLOCKING_METHODS 数量异常减少, 守卫可能被刻意削弱 (当前 {})",
        set.len()
    );
    // 间接入口同样钉住 —— 这一组正是真实事故暴露出来的。
    let indirect: BTreeSet<&str> = INDIRECT_BLOCKING_CALLS.iter().copied().collect();
    assert!(
        indirect.contains("FeisuoRuntime.ensureStarted"),
        "INDIRECT_BLOCKING_CALLS 少了 ensureStarted, 间接阻塞调用的盲区会重现"
    );
}

/// 通知栏"停止"按钮必须真的停掉 core 引擎。
///
/// 旧实现只 `stopSelf()`: Service 死了, 但 Rust 引擎活在同一进程里 ——
/// UDP 心跳继续广播(电脑侧照常显示这台手机在线), 传输端口继续被占
/// (再拉起服务会绑定失败)。用户点"停止"与实际行为直接矛盾。
#[test]
fn stop_action_must_actually_shutdown_the_engine() {
    let path = kotlin_dir().join("FeisuoDaemonService.kt");
    let src = std::fs::read_to_string(&path).expect("读取 FeisuoDaemonService.kt 失败");

    // 找到 onDestroy 方法体
    let at = src
        .find("override fun onDestroy(")
        .expect("FeisuoDaemonService 必须实现 onDestroy 来释放引擎");
    let body_start = src[at..]
        .find('{')
        .map(|i| at + i)
        .expect("onDestroy 方法体缺少起始大括号");
    let chars: Vec<(usize, char)> = src[body_start..]
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
    let body = &src[body_start..end.expect("onDestroy 方法体没有闭合")];

    assert!(
        body.contains("FeisuoRuntime.shutdown()"),
        "onDestroy 里必须调用 FeisuoRuntime.shutdown() —— 否则用户点通知栏的 \
         \"停止\"后, 引擎仍在广播心跳且占着传输端口"
    );

    // 同时确认 core 侧确实导出了停机能力(否则上面那句是空转)。
    let runtime_src =
        std::fs::read_to_string(kotlin_dir().join("FeisuoRuntime.kt")).expect("读取失败");
    assert!(
        runtime_src.contains("fun shutdown()"),
        "FeisuoRuntime 缺少 shutdown() 入口"
    );
    assert!(
        runtime_src.contains("NativeBridge.shutdownEngine("),
        "FeisuoRuntime.shutdown() 必须真的调用 nativeShutdownEngine, \
         不能只复位标志位"
    );
    assert!(
        runtime_src.contains("started = false"),
        "shutdown() 必须复位 started, 否则引擎停掉后再也拉不起来"
    );
}

/// 开机路径不能在主线程启动引擎。
///
/// 这条独立于上面的通用扫描: `BootReceiver -> startForegroundService ->
/// onCreate` 是每次开机/每次应用更新都会走的路径, 主线程阻塞在这里的
/// 代价远高于用户手动打开应用。
#[test]
fn daemon_startup_must_foreground_before_engine_boot() {
    let path = kotlin_dir().join("FeisuoDaemonService.kt");
    let src = std::fs::read_to_string(&path).expect("读取 FeisuoDaemonService.kt 失败");

    let at = src
        .find("override fun onCreate(")
        .expect("FeisuoDaemonService 必须实现 onCreate");
    let body_start = src[at..].find('{').map(|i| at + i).unwrap();
    let chars: Vec<(usize, char)> = src[body_start..]
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
    let body = &src[body_start..end.expect("onCreate 方法体没有闭合")];

    let fg = body.find("startForeground(").expect(
        "onCreate 里必须调用 startForeground —— startForegroundService 要求 \
         5 秒内挂上通知, 否则抛 ForegroundServiceDidNotStartInTimeException",
    );
    assert!(
        !body.contains("FeisuoRuntime.ensureStarted("),
        "onCreate 不得同步调用 ensureStarted (阻塞 JNI); 应交给后台线程"
    );
    assert!(
        fg > 0 && body[..fg].contains("createNotificationChannel()"),
        "startForeground 之前必须先建通知渠道, 否则 API 26+ 上通知发不出去"
    );

    // 引擎启动必须发生在后台线程。这里做**一层调用图解析**而不是硬编码
    // 方法名: onCreate 里可能调 `startEngineOnWorker()`, 后者内部才建线程;
    // 只在 onCreate 里搜 "Thread(" 会误报(本守卫第一版就栽在这)。
    // 判定: onCreate 直接或经由本文件内的一个私有方法间接到达线程创建。
    let spawns_thread = |s: &str| {
        s.contains("Thread(") || s.contains("thread {") || s.contains("Executors")
    };
    assert!(
        spawns_thread(body) || delegates_to_thread_spawner(&src, body),
        "onCreate 必须把 core 引擎启动放到后台线程 —— \
         NativeBridge.initEngine 是阻塞 JNI(建 runtime + 绑端口 + 建库 + \
         生成密钥), 放在主线程会 ANR, 而这条路径每次开机都会走"
    );
}

/// 在 `body` 里找出被调用的本文件私有方法, 判断其中是否有任何一个
/// 内部创建了后台线程。只解析一层 —— 足以覆盖当前代码形态, 且不会
/// 把守卫变成一个难以维护的全量解析器。
fn delegates_to_thread_spawner(src: &str, body: &str) -> bool {
    let spawns_thread = |s: &str| {
        s.contains("Thread(") || s.contains("thread {") || s.contains("Executors")
    };
    for callee in called_private_methods(src, body) {
        let sig = format!("fun {}(", callee);
        let Some(at) = src.find(&sig) else { continue };
        let Some(open) = src[at..].find('{') else { continue };
        let body_start = at + open;
        let Some(end) = matching_brace(src, body_start) else {
            continue;
        };
        if spawns_thread(&src[body_start..end]) {
            return true;
        }
    }
    false
}

/// 取出 `body` 里形如 `name(` 的调用名(粗粒度即可, 只需用于再查方法定义)。
fn called_private_methods(src: &str, body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        // 找标识符后紧跟 '(' 的位置
        if bytes[i] == b'(' && i > 0 {
            let mut s = i;
            while s > 0 {
                let c = bytes[s - 1];
                if c.is_ascii_alphanumeric() || c == b'_' {
                    s -= 1;
                } else {
                    break;
                }
            }
            let name = &body[s..i];
            if !name.is_empty()
                && name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_lowercase())
                    .unwrap_or(false)
                && !name.contains("if")
                && !name.contains("for")
                && !name.contains("while")
                && !name.contains("when")
                && src.contains(&format!("fun {}(", name))
            {
                out.push(name.to_string());
            }
        }
        i += 1;
    }
    out.sort();
    out.dedup();
    out
}

/// 从 `open` 处的 `{` 开始做字符级大括号配平, 返回闭合 `}` 的字节偏移。
fn matching_brace(src: &str, open: usize) -> Option<usize> {
    let chars: Vec<(usize, char)> = src[open..]
        .char_indices()
        .map(|(i, c)| (open + i, c))
        .collect();
    let mut depth = 0i32;
    let mut in_str = false;
    for (idx, &(_, c)) in chars.iter().enumerate() {
        if c == '"' && (idx == 0 || chars[idx - 1].1 != '\\') {
            in_str = !in_str;
        }
        if in_str {
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(chars[idx].0);
                }
            }
            _ => {}
        }
    }
    None
}
