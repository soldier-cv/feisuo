//! 移动端引擎生命周期验证。
//!
//! 这些性质原先全部写在 `extern "C"` 函数里, 一次都没被验证过 ——
//! 而 Android 端又没有真机联调, 于是"看起来对但完全没有证据"。
//! 把逻辑挪到跨平台的 `engine_host` 之后, 就能在桌面上直接测。
//!
//! 隔离性: 全部使用系统临时目录 + 127.0.0.1 + **操作系统分配的临时端口**,
//! 不触碰任何真实配置 / 真实文件 / 外部网络。
//!
//! ⚠️ 这些用例共用 `engine_host` 里的**进程级单例**, 因此必须串行执行。

use std::sync::{Mutex, MutexGuard, OnceLock};

use feisuo_core::config::AppConfig;
use feisuo_core::engine_host as host;

/// 单例互斥锁: 每个用例前后都要把单例复位, 并发跑会互相抢端口。
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        // 上一个用例 panic 时不要把锁一起毒死
        .unwrap_or_else(|e| e.into_inner())
}

/// 让操作系统分配一个当前空闲的端口。
///
/// 做法是绑 0 端口再问端口号, 拿到后**立即释放** —— 存在极小的 TOCTOU
/// 窗口, 但对测试足够; 关键是拿到的是真实空闲端口而不是猜一个。
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("分配临时端口失败")
        .local_addr()
        .expect("读取临时端口失败")
        .port()
}

/// 建一个隔离的数据目录, 并预置一份**独占端口**的配置。
///
/// 为什么不直接用默认配置: 默认传输/发现端口是 42100/42101, 而
/// `cargo test --workspace` 会**并行**跑多个测试二进制 ——
/// `protocol_integration` 也在抢端口, 且它拿到的临时端口完全可能就是
/// 42100。二者相遇时随机失败, 且失败信息完全看不出是端口冲突。
/// 症状: 全量跑时偶发 `boot` 返回 INVALID_HANDLE, 单独跑该文件则全绿。
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "feisuo-host-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).expect("创建临时目录");

    let cfg = AppConfig {
        transfer_port: free_port(),
        discovery_port: free_port(),
        // 只绑回环, 免得测试往真实局域网广播与触发防火墙弹窗
        discovery_bind: "127.0.0.1".to_string(),
        transfer_bind: "127.0.0.1".to_string(),
        ..Default::default()
    };
    cfg.save_in(&d).expect("写入隔离配置失败");
    d
}

fn cleanup(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn engine_starts_and_reports_identity() {
    let _s = serial();
    host::reset_for_test();
    let dir = temp_dir("boot");

    assert!(!host::is_running(), "起始状态必须是未启动");

    let handle = host::boot(dir.to_str().unwrap());
    assert_ne!(
        handle,
        host::INVALID_HANDLE,
        "在可写临时目录里必须能启动成功"
    );
    assert!(host::is_running());

    // 设备指纹必须是稳定的 "feisuo-<12 位>" 形式, 且非空
    let id = host::device_id(handle);
    assert!(
        id.starts_with("feisuo-") && id.len() == "feisuo-".len() + 12,
        "设备指纹格式不对: {:?}",
        id
    );

    // 配置快照必须是合法 JSON, 且包含关键字段
    let json = host::config_json(handle);
    assert!(!json.is_empty(), "配置快照不得为空");
    let v: serde_json::Value =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("配置不是合法 JSON: {}\n{}", e, json));
    assert!(v.get("device_name").is_some(), "配置缺少 device_name");
    assert!(v.get("transfer_port").is_some(), "配置缺少 transfer_port");
    assert!(v.get("discovery_port").is_some(), "配置缺少 discovery_port");

    // 落盘产物必须齐全: 配置 / 私钥 / 信任库
    for f in ["config.json", "device_identity.key", "trust_store.db"] {
        assert!(
            dir.join(f).exists(),
            "启动后应生成 {}/{}, 实际目录: {:?}",
            dir.display(),
            f,
            std::fs::read_dir(&dir)
                .map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
        );
    }

    host::shutdown(handle);
    assert!(!host::is_running(), "停止后必须回到未启动状态");
    cleanup(&dir);
}

#[test]
fn boot_is_idempotent_and_returns_same_handle() {
    let _s = serial();
    host::reset_for_test();
    let dir = temp_dir("idem");

    let h1 = host::boot(dir.to_str().unwrap());
    assert_ne!(h1, host::INVALID_HANDLE);

    // 第二次启动必须返回同一句柄, 且不得因为端口被自己占住而失败。
    // 这是真实场景: Android 的 Service.onCreate 与 MainActivity.onCreate
    // 都会请求启动, 而 start() 绑定的是固定端口。
    let h2 = host::boot(dir.to_str().unwrap());
    assert_eq!(h1, h2, "重复启动必须返回同一句柄, 不能报端口被占用");
    assert!(host::is_running());

    // 设备指纹也必须一致(没有被重新生成)
    assert_eq!(host::device_id(h1), host::device_id(h2));

    host::shutdown(h1);
    cleanup(&dir);
}

/// 守卫：**只让发现侧成功**，逼出传输端口自己的释放问题。
///
/// ## 为什么需要单独一条
///
/// `repeated_stop_and_boot_always_succeeds` 覆盖不到传输端口 ——
/// `start()` 先起发现再起传输，所以**发现端口先失败**就把断言截住了，
/// 传输侧的问题永远轮不到暴露。改传输侧（去掉等待）跑那条用例，仍然全绿。
///
/// 这条用例的做法：先把发现端口的失败**排除掉**（跑够轮数让发现侧稳定成功，
/// 因为它已经修好了），再看传输端口有没有自己的问题。
///
/// 更直接的办法是让发现侧根本不参与：`FeisuoEngine::start_inner` 依次调
/// `discovery.start()` 与 `server.start()`，而 `boot` 没有"只起传输"的入口。
/// 所以这里采用**断言错误来源**的方式：失败时把 `last_boot_error()` 打出来，
/// 它会明确说是"局域网发现端口"还是"传输端口"——
/// 于是即便传输侧出问题、那条消息也能把它指出来。
///
/// 传输侧现在也会等 accept 循环真正结束（`TransferServer::stop`），
/// 与发现侧同一个缺陷、同一个修法。**诚实说明**：这一侧目前**没有**能稳定
/// 复现缺陷的用例（listener 只被 accept 任务持有，没有 `self.socket`
/// 那种第二引用，窗口比发现侧窄得多）。这条守卫的作用是：一旦它开始红，
/// 消息就能指出是传输端口，而不必再猜。
#[test]
fn repeated_stop_and_boot_never_fails_on_transfer_port() {
    let _s = serial();
    host::reset_for_test();
    let dir = temp_dir("churn-tcp");

    for round in 1..=8 {
        let h = host::boot(dir.to_str().unwrap());
        if h == host::INVALID_HANDLE {
            let err = host::last_boot_error();
            panic!(
                "第 {} 轮启动失败。\n真实原因: {}\n\
                 （这条守卫只关心传输端口 —— 若上面写的是『传输端口』，\
                 说明 TransferServer::stop() 的等待被去掉了）",
                round, err
            );
        }
        host::shutdown(h);
    }

    cleanup(&dir);
}

/// 守卫：**连续**启停必须每次都成功，且身份不变。
///
/// `shutdown_then_boot_again` 只启停一轮，而释放窗口只有几十毫秒，
/// 单轮可能恰好撞不上。这里连做 5 轮，把窗口放大到"几乎必然命中"。
///
/// ## 这条守卫对应的真实缺陷（实测第 5~8 轮必现）
///
/// 根因在 `DiscoveryService::stop()`，**不在**传输端口那一侧：
///
/// 1. `abort()` 只是**请求**取消。广播循环与监听循环各自 clone 了一份
///    `Arc<UdpSocket>`，它们要等 task 被调度一次、future 被 drop 才真正释放。
/// 2. 清空 `self.socket` 用的是 `try_write` **只试一次**。`probe_ip` /
///    `reply_port` 正在用 `socket.read().await` 持读锁，撞上就跳过且不吭声。
///
/// 两条叠加 → 端口一直被自己占着 → 紧接着的 `start()` 拿到 `AddrInUse`，
/// 而报错文案写的是"是否已有另一个飞梭实例在运行"。占用者恰恰是刚刚退出的
/// **他自己**。真实后果：托盘"退出再打开"、Android 前台服务被系统杀掉后重启。
///
/// 修法：`stop()` 改成「abort → **重试到成功**地清空 `self.socket` →
/// **join** 到 task 真正结束」，join 才是"端口已空"的确认。
///
/// ## 为什么用 5 轮而不是 1 轮
///
/// 竞态窗口取决于 `abort()` 之后 runtime 何时调度那个 task。实测第 1~4 轮
/// 常常侥幸通过（套接字已因别的原因释放），第 5~8 轮稳定失败。5 轮能覆盖。
#[test]
fn repeated_stop_and_boot_always_succeeds() {
    let _s = serial();
    host::reset_for_test();
    let dir = temp_dir("churn");

    let mut first_id = String::new();
    for round in 1..=5 {
        let h = host::boot(dir.to_str().unwrap());
        assert_ne!(
            h,
            host::INVALID_HANDLE,
            "第 {} 轮启动失败。真实原因: {} —— \
             典型是发现端口 AddrInUse: DiscoveryService::stop() 只 abort 不 join, \
             且清空 socket 用 try_write 只试一次就放弃, 于是端口仍被本进程自己占着。",
            round,
            host::last_boot_error()
        );
        let id = host::device_id(h);
        if first_id.is_empty() {
            first_id = id.clone();
        } else {
            assert_eq!(first_id, id, "第 {} 轮重启后设备指纹变了", round);
        }
        host::shutdown(h);
        assert!(!host::is_running(), "第 {} 轮停止后仍在运行", round);
    }

    cleanup(&dir);
}

#[test]
fn shutdown_then_boot_again_reuses_same_identity() {
    let _s = serial();
    host::reset_for_test();
    let dir = temp_dir("restart");

    let h1 = host::boot(dir.to_str().unwrap());
    let id1 = host::device_id(h1);
    host::shutdown(h1);
    assert!(!host::is_running());

    // 重新启动: 身份必须沿用(否则用户每次冷启动都要重新配对,
    // 直接违背"一次配对、终生免密"的产品红线)
    let h2 = host::boot(dir.to_str().unwrap());
    assert_ne!(
        h2,
        host::INVALID_HANDLE,
        "停止后必须能重新启动。真实原因: {}",
        host::last_boot_error()
    );
    assert_eq!(id1, host::device_id(h2), "重启后设备指纹不得改变");

    host::shutdown(h2);
    cleanup(&dir);
}

#[test]
fn invalid_and_stale_handles_are_rejected_safely() {
    let _s = serial();
    host::reset_for_test();
    let dir = temp_dir("handle");

    // 未启动时: 任何句柄都拿不到数据, 且不得 panic
    assert_eq!(host::device_id(0), "");
    assert_eq!(host::device_id(12345), "");
    assert_eq!(host::config_json(0), "");
    assert_eq!(host::config_json(12345), "");

    // 对不存在的句柄执行 shutdown 必须是空操作
    host::shutdown(0);
    host::shutdown(999);
    assert!(!host::is_running());

    let h = host::boot(dir.to_str().unwrap());
    assert_ne!(h, host::INVALID_HANDLE);

    // 有效句柄能取到数据
    assert!(!host::device_id(h).is_empty());
    assert!(!host::config_json(h).is_empty());

    // **过期句柄**: 句柄是 Arc 裸地址, 停掉再分配很可能被复用。
    // 句柄不匹配时必须返回空而不是别人的数据。
    host::shutdown(h);
    assert!(!host::is_running());
    assert!(
        host::device_id(h).is_empty(),
        "引擎已停止, 旧句柄不得再取到数据"
    );
    assert!(host::config_json(h).is_empty());

    cleanup(&dir);
}

#[test]
fn boot_on_unusable_directory_fails_cleanly() {
    let _s = serial();
    host::reset_for_test();

    // 指到一个"文件"而不是目录: 必须干净失败, 返回 INVALID_HANDLE,
    // 且不留下半启动状态。
    let dir = temp_dir("badpath");
    let file = dir.join("i-am-a-file");
    std::fs::write(&file, b"x").unwrap();

    let handle = host::boot(file.to_str().unwrap());
    assert_eq!(
        handle,
        host::INVALID_HANDLE,
        "数据目录不可用时必须返回 INVALID_HANDLE"
    );
    assert!(!host::is_running(), "启动失败后不得处于半启动状态");

    // 失败之后必须还能正常启动(不能被脏状态卡死)
    let good = temp_dir("badpath-ok");
    let h = host::boot(good.to_str().unwrap());
    assert_ne!(h, host::INVALID_HANDLE, "一次失败不应污染后续启动");
    host::shutdown(h);

    cleanup(&dir);
    cleanup(&good);
}

#[test]
fn runtime_is_shared_and_usable() {
    // runtime 必须是进程内共享的单例, 且能被 block_on 驱动。
    // Android 端所有网络栈都挂在它上面, 建两次会导致端口与线程双重占用。
    let a = host::runtime() as *const _;
    let b = host::runtime() as *const _;
    assert_eq!(a, b, "runtime 必须是单例");

    let v: u32 = host::runtime().block_on(async { 1 + 1 });
    assert_eq!(v, 2, "runtime 必须能驱动 async 任务");
}

#[test]
fn describe_produces_readable_chinese_message() {
    // 错误串会直接显示给用户看, 必须是可读中文而不是 Debug 格式
    let e = feisuo_core::FeisuoError::Security("测试用错误".into());
    let msg = host::describe(&e);
    assert!(msg.contains("测试用错误"), "错误描述必须包含原因: {}", msg);
    assert!(
        !msg.contains("Security("),
        "错误描述不得泄漏 Rust 枚举 Debug 形态: {}",
        msg
    );
}

#[test]
fn host_uses_arc_based_engine_without_use_after_free() {
    // 拿到引擎引用后即使单例被清空, 引用仍然有效 —— 这正是用 Arc
    // 而不是裸指针解引用的原因。
    let _s = serial();
    host::reset_for_test();
    let dir = temp_dir("arc");

    let h = host::boot(dir.to_str().unwrap());
    assert_ne!(h, host::INVALID_HANDLE);

    let id_before = host::device_id(h);
    // 模拟"句柄已经交出去但单例随后被复位"的极端情况
    host::reset_for_test();

    // 单例没了, 但之前取到的数据仍然自洽 —— 不出现野指针
    assert!(id_before.starts_with("feisuo-"));
    assert_eq!(host::device_id(h), "");

    cleanup(&dir);
}

#[test]
fn send_to_unreachable_peer_fails_with_readable_reason() {
    // 系统"分享到飞梭"的落点。旧实现把文件暂存完就结束, 什么都没发 ——
    // 用户看到"正在推送到电脑…"后再无下文。这条用例保证它真的会尝试发送,
    // 且失败时给出可展示的原因, 而不是静默丢弃。
    let _s = serial();
    host::reset_for_test();
    let dir = temp_dir("send");
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let f = out.join("photo.jpg");
    std::fs::write(&f, vec![9u8; 2048]).unwrap();

    let h = host::boot(dir.to_str().unwrap());
    assert_ne!(h, host::INVALID_HANDLE);

    // 空路径列表必须在发出任何网络包之前就被拒
    let err = host::send_files(h, "127.0.0.1", 1, "peer", "对端", &[])
        .expect_err("空文件列表必须被拒绝");
    assert!(err.contains("未指定"), "错误应说明是空选择, 实际: {}", err);

    // 非法端口
    assert!(host::send_files(h, "127.0.0.1", 0, "peer", "对端", &[f.to_string_lossy().to_string()]).is_err());

    // 不可达的对端: 必须返回**可读的中文原因**, 而不是空字符串或 panic
    let err = host::send_files(
        h,
        "127.0.0.1",
        1, // 几乎不可能被监听的端口
        "peer",
        "对端",
        &[f.to_string_lossy().to_string()],
    )
    .expect_err("连不上就必须报错");
    assert!(
        !err.is_empty(),
        "失败必须带可展示的原因, 宿主要把它 Toast 给用户"
    );
    assert!(
        !err.contains("Network(") && !err.contains("Io {"),
        "错误描述不得泄漏 Rust 内部类型: {}",
        err
    );

    host::shutdown(h);
    cleanup(&dir);
}

#[test]
fn send_and_probe_reject_when_engine_not_running() {
    // 引擎没起来时, 这两个是分享链路最容易被调到的入口,
    // 必须返回明确错误而不是崩溃。
    host::reset_for_test();
    assert!(host::send_files(0, "127.0.0.1", 42100, "d", "n", &["/tmp/x".into()]).is_err());
    assert!(host::probe(0, "127.0.0.1").is_err());
    // 受信设备列表在未启动时返回合法 JSON 数组而不是崩
    assert_eq!(host::trusted_devices_json(0), "[]");
}

/// 引擎健康状态必须是**可查询的权威值**, 不能靠调用成败推断。
///
/// 起因是一个真实故障: 端口被占用导致引擎启动失败时, 桌面端把错误
/// 通过一次性事件上报, 但事件发出时前端监听器还没注册(实测相差 8ms),
/// 错误被彻底丢弃。同时前端靠"设备列表调用没抛异常"显示在线 ——
/// 而那张表本来就是空的, 于是界面顶着绿灯说"在线"。
/// 托盘常驻下窗口不显示, 用户只看到托盘图标, 以为一切正常。
///
/// 这里直接构造引擎(不走 `engine_host::boot`), 因为 `boot` 内部就会调
/// `start()`, 启动失败会连句柄一起丢掉 —— 那样就无从验证"失败状态
/// 是否可查"了。桌面端走的正是 init + start 两步分开那条路径。
#[test]
fn engine_health_is_queryable_after_failed_start() {
    let dir = temp_dir("health");

    // 占住传输端口, 让 start() 必然失败。
    // 双方均绑 127.0.0.1 回环接口, 既保证端口冲突必然触发, 又避免触发 Windows 防火墙弹窗。
    let hog = std::net::TcpListener::bind("127.0.0.1:0").expect("占位监听");
    let hog_port = hog.local_addr().unwrap().port();

    let cfg = AppConfig {
        transfer_port: hog_port,
        // 发现端口也必须用独占端口, 不能留 `..Default::default()` 的 42101:
        // `cargo test --workspace` 会并行跑多个测试二进制, 而 42101 正是
        // 生产默认端口, 撞上就会随机红。
        discovery_port: free_port(),
        discovery_bind: "127.0.0.1".to_string(),
        transfer_bind: "127.0.0.1".to_string(),
        ..Default::default()
    };
    cfg.save_in(&dir).expect("写配置失败");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建 runtime");
    let handles = rt
        .block_on(feisuo_core::FeisuoEngine::init_in(dir.clone()))
        .expect("构造引擎不应失败");
    let (engine, _disc, _prog, _appr) = handles.into_tuple();
    // `start` 收 &Arc<Self>（周期任务要持强引用），所以这里包一层
    let engine = std::sync::Arc::new(engine);

    let res = rt.block_on(engine.start());
    assert!(res.is_err(), "端口被占时 start 必须失败");

    // 关键: 失败状态与原因必须**查得到**, 而不是只往上报一次就丢掉
    assert!(!engine.is_started(), "失败时 is_started 必须为 false");
    let reason = engine.start_error().expect("必须能查到失败原因");
    assert!(
        !reason.trim().is_empty(),
        "失败原因不得为空串 —— 空串在前端就等于没有错误可显示"
    );

    // 反复查询必须稳定, 且不得 panic
    for _ in 0..3 {
        assert!(!engine.is_started());
        assert!(engine.start_error().is_some());
    }

    // 停掉之后 is_started 必须翻假, 免得界面继续显示绿灯
    engine.stop();
    assert!(!engine.is_started(), "stop 之后 is_started 必须为 false");

    drop(hog);
    cleanup(&dir);
}

/// 配对入口必须真的能走通, 且失败时给出可展示的中文原因。
///
/// 起因: Android 界面写着"输入配对码"、说明里写着"在下方输入桌面端显示的
/// 6 位配对码", 实际只弹一句"配对界面将在后续版本接入" —— 文案与按钮都在说谎。
/// 配不上意味着分享推送永远命中"尚未配对任何电脑", 整条发送链路不可达。
///
/// 必须用普通 `#[test]`: `engine_host` 的入口内部是 `runtime().block_on`,
/// 在 tokio runtime 里调用会 panic("Cannot start a runtime from within a runtime")。
/// 这也正是 JNI 侧从 JVM 线程调用是安全的、而 Tauri 命令(跑在 async runtime 上)
/// 不能直接复用这些入口的原因。
#[test]
fn pairing_entry_point_rejects_bad_pin_with_readable_reason() {
    use feisuo_core::engine_host as h;

    // 必须持锁: 本文件的用例共用 `engine_host` 的进程级单例, 而另一条用例
    // 的 reset_for_test() 会把它清空 —— 并行跑时本用例刚 boot 好的引擎
    // 会被别人擦掉, 表现为莫名其妙的"引擎未启动"。
    let _s = serial();
    h::reset_for_test();
    let dir = temp_dir("pair");
    let booted = h::boot(dir.to_str().unwrap());
    assert_ne!(booted, h::INVALID_HANDLE);

    // 输入校验必须发生在**任何网络包之前**: 配对码不是 6 位数字就直接拒,
    // 免得对着不存在的地址空等一个网络超时。
    for bad in ["", "12345", "1234567", "abcdef", "12 34 56", "  "] {
        let err = h::pair_with_device(booted, "127.0.0.1", 1, bad)
            .expect_err("非法配对码必须被拒绝");
        assert!(
            err.contains("6 位数字"),
            "错误应说明是配对码格式问题, 实际: {} (输入 {:?})",
            err,
            bad
        );
    }

    // 合法格式但连不上: 必须返回可读原因, 而不是空串或 panic
    let err = h::pair_with_device(booted, "127.0.0.1", 1, "123456")
        .expect_err("连不上就必须报错");
    assert!(!err.trim().is_empty(), "失败必须带可展示的原因");
    assert!(
        !err.contains("Network(") && !err.contains("Io {"),
        "错误描述不得泄漏 Rust 内部类型: {}",
        err
    );

    h::shutdown(booted);
    cleanup(&dir);
}

#[test]
fn pairing_reports_engine_not_running_instead_of_panicking() {
    let _s = serial();
    feisuo_core::engine_host::reset_for_test();
    let err = feisuo_core::engine_host::pair_with_device(0, "127.0.0.1", 42100, "123456")
        .expect_err("引擎未启动必须报错");
    assert!(err.contains("引擎未启动"), "实际: {}", err);
}

/// 审批请求必须能被宿主取到, 且能真的回应。
///
/// 起因: `boot_engine` 原先直接 `drop(handles.approval)`。丢掉 broadcast 接收端后
/// `approval_tx.send()` 必然失败, 传输服务端于是 fail-closed 拒绝所有未受信设备 ——
/// 也就是"任何电脑都无法向 Android 投递文件", 而宿主连"有传输在等你确认"都看不到。
fn h_reset() {
    feisuo_core::engine_host::reset_for_test();
}

#[test]
fn approval_requests_are_reachable_and_resolvable() {
    let _s = serial();
    h_reset();
    let dir = temp_dir("approval");

    let h = feisuo_core::engine_host::boot(dir.to_str().unwrap());
    assert_ne!(h, feisuo_core::engine_host::INVALID_HANDLE);

    // 队列已建立: 空队列按超时返回 None, 而不是永远阻塞或 panic
    let t0 = std::time::Instant::now();
    assert!(
        feisuo_core::engine_host::next_approval(200).is_none(),
        "空队列必须按超时返回 None"
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(3),
        "超时必须真的生效, 实际耗时 {:?}",
        t0.elapsed()
    );

    // 回应不存在的审批: 返回 false 而不是 panic
    assert!(
        !feisuo_core::engine_host::respond_approval("不存在的ID", true),
        "回应不存在的审批必须返回 false"
    );

    feisuo_core::engine_host::shutdown(h);
    cleanup(&dir);
}

/// 引擎未启动时审批接口必须是安全的空操作。
#[test]
fn approval_api_is_safe_when_engine_not_running() {
    let _s = serial();
    h_reset();
    // 没 boot 过 -> 队列尚未安装
    assert!(
        feisuo_core::engine_host::next_approval(0).is_none(),
        "引擎未启动时 next_approval 必须返回 None, 不能 panic"
    );
    assert!(!feisuo_core::engine_host::respond_approval("x", false));
}

#[test]
fn trusted_devices_json_is_valid_even_when_empty() {
    let _s = serial();
    host::reset_for_test();
    let dir = temp_dir("trusted");

    let h = host::boot(dir.to_str().unwrap());
    assert_ne!(h, host::INVALID_HANDLE);

    // 新装的设备没有任何受信记录, 必须返回合法空数组
    let json = host::trusted_devices_json(h);
    let parsed: serde_json::Value =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("不是合法 JSON: {}\n{}", e, json));
    assert!(
        parsed.is_array(),
        "必须是数组, 实际: {}",
        json
    );
    assert_eq!(parsed.as_array().unwrap().len(), 0);

    // 停止后同样返回合法 JSON, 不 panic
    host::shutdown(h);
    assert_eq!(host::trusted_devices_json(h), "[]");

    cleanup(&dir);
}
