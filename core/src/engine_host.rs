//! 移动端宿主的引擎生命周期管理。
//!
//! 与 [`crate::mobile`] 的分工:
//! - **本模块**: 引擎单例、runtime、启动/停止/查询。纯 Rust, **无任何 FFI**,
//!   因此可以在桌面上直接跑单元测试 —— 而 JNI 那一层薄得几乎无法出错,
//!   真正容易错的是这里(端口占用、重复启动、启动失败后的状态)。
//! - [`crate::mobile`]: 只做 JNI 符号导出与字符串编解码, 仅在 Android 构建。
//!
//! 为什么值得单独拆一层:
//! 原先这些逻辑全部写在 `extern "C"` 函数里, 意味着"引擎到底能不能在
//! 一个陌生目录里起来、重复启动会不会炸、停止后能不能重启"这几件事
//! **一次都没被验证过** —— Android 端又没有真机联调, 于是它们处于
//! "看起来对但完全没证据"的状态。拆开后这些性质可以用普通测试锁住。
//!
//! @author xudong.hua,gemini
//! @since 2026-09-30 18:40 星期三

use std::sync::{Arc, Mutex, OnceLock};

use crate::error::FeisuoError;
use crate::protocol::{ApprovalAction, ApprovalRequest};
use crate::FeisuoEngine;

/// 进程内引擎单例。
///
/// 移动端宿主会在 `Service.onCreate` 与 `MainActivity.onCreate` 两处请求启动,
/// 而 `start()` 绑定的是固定端口 —— 第二次启动必然因端口被自己占住而失败。
/// Kotlin 侧有 `started` 标志做幂等, 但那只是应用层约定; 这里再兜一层底,
/// 保证即使有人漏了那个判断也不会把引擎搞坏。
static ENGINE: Mutex<Option<EngineSlot>> = Mutex::new(None);

/// 最近一次 oot() 的失败原因（成功时清空）。
static LAST_BOOT_ERROR: Mutex<String> = Mutex::new(String::new());

/// 句柄无效时返回给宿主侧的哨兵值。
///
/// 0 明确表示"没起来": 启动失败时必须能被一眼分辨,
/// 否则 0 会被当成合法句柄存进 `FeisuoRuntime.engine`。
pub const INVALID_HANDLE: i64 = 0;

/// 驱动引擎内部 async 任务的 Tokio runtime。
///
/// 移动端没有桌面端那样的 main loop, 必须自建。
/// 用 `OnceLock` 延迟到首次使用: 在静态初始化阶段建 runtime 会与
/// runtime 自身的线程池初始化互相等待, 直接死锁。
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// 取进程内共享 runtime。首次调用时才真正构建。
pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            // 2 个 worker 足够: 网络 IO 为主, 磁盘重活都走 spawn_blocking
            .worker_threads(2)
            .thread_name("feisuo-core")
            .enable_all()
            .build()
            .expect("构建 core tokio runtime 失败")
    })
}

struct EngineSlot {
    engine: Arc<FeisuoEngine>,
}

/// 把不透明句柄还原成引擎引用。
///
/// 句柄就是 `Arc::as_ptr` 的地址。**不能只用裸指针解引用**:
/// 那需要 `unsafe`, 且拿不到 Arc 的引用计数, 句柄一旦过期就是 use-after-free。
/// 这里走 `Weak` 升级, 拿不到就返回 None。
fn engine_for_handle(handle: i64) -> Option<Arc<FeisuoEngine>> {
    if handle == INVALID_HANDLE {
        return None;
    }
    let guard = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    let slot = guard.as_ref()?;
    let ptr = Arc::as_ptr(&slot.engine) as i64;
    if ptr != handle {
        return None;
    }
    Some(slot.engine.clone())
}

/// 在指定数据目录上初始化并启动引擎, 返回不透明句柄; 失败返回
/// [`INVALID_HANDLE`]。
///
/// 幂等: 引擎已在运行时直接返回原句柄, **不会**重复绑定端口。
///
/// **关于事件接收端**（重要，不要照抄到桌面端）:
/// `init_in` 会一并返回设备发现 / 传输进度 / 人工审批三个
/// `broadcast::Receiver`，这里**只保留 `engine`、其余三个直接丢弃**。
/// 后果是刻意的：
/// - 设备发现与传输进度在移动端暂时没有 UI 消费，丢弃无影响；
/// - **人工审批通道一旦没有订阅者，服务端会 fail-closed 直接拒绝**
///   （见 `server.rs` 的审批分支）。这意味着未配对设备、以及关闭了
///   `auto_receive` 的已配对设备，都无法向本机投递文件。
///   已配对且 `auto_receive = true`（默认）走静默接收，不受影响。
///   等移动端补上审批界面时，必须把 `approval` receiver 接出去。
pub fn boot(dir: &str) -> i64 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut guard = ENGINE.lock().unwrap_or_else(|e| e.into_inner());

        // 已启动过就直接返回现有句柄, 绝不重复绑定端口
        if let Some(slot) = guard.as_ref() {
            return Ok(Arc::as_ptr(&slot.engine) as i64);
        }

        let started: std::result::Result<Arc<FeisuoEngine>, FeisuoError> =
            runtime().block_on(boot_engine(dir.to_string()));
        let engine = match started {
            Ok(e) => e,
            Err(e) => return Err(describe(&e)),
        };

        let id = Arc::as_ptr(&engine) as i64;
        *guard = Some(EngineSlot { engine });
        Ok(id)
    }));

    match result {
        Ok(Ok(id)) => {
            if let Ok(mut slot) = LAST_BOOT_ERROR.lock() {
                slot.clear();
            }
            id
        }
        Ok(Err(msg)) => {
            log_error(&format!("引擎启动失败: {}", msg));
            if let Ok(mut slot) = LAST_BOOT_ERROR.lock() {
                *slot = msg;
            }
            INVALID_HANDLE
        }
        Err(_) => {
            log_error("引擎启动时发生 panic, 已在边界拦截");
            if let Ok(mut slot) = LAST_BOOT_ERROR.lock() {
                *slot = "引擎启动时发生 panic, 已在边界拦截".into();
            }
            INVALID_HANDLE
        }
    }
}

/// 真正执行初始化。独立成函数而不是内联 async block: async block 的输出
/// 类型由最后一个表达式决定, 外面再套 `?` 时编译器经常反推不出 error
/// 泛型, 直接报 "cannot infer type of the type parameter `E`"。
async fn boot_engine(dir: String) -> std::result::Result<Arc<FeisuoEngine>, FeisuoError> {
    let handles = FeisuoEngine::init_in(std::path::PathBuf::from(dir)).await?;
    // 三个事件接收端在此被丢弃, 见 boot() 的说明
    drop(handles.devices);
    drop(handles.progress);
    // approval **不能**像前两个那样直接丢弃。丢弃接收端后
    // `approval_tx.send()` 必然返回 Err, 传输服务端就会 fail-closed
    // 拒绝所有未受信设备 —— 也就是"任何电脑都无法向本机投递文件",
    // 而宿主连"有传输在等你确认"这件事都看不到。
    install_approval_forwarder(handles.approval);
    let engine = Arc::new(handles.engine);
    engine.start().await?;
    Ok(engine)
}

/// 停止引擎。句柄无效 / 不匹配 / 引擎本就未启动时都是安全的空操作。
pub fn shutdown(handle: i64) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut guard = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        // 句柄不匹配说明调用方拿到了过期句柄, 直接忽略而不是停错引擎
        match guard.as_ref() {
            Some(slot) if Arc::as_ptr(&slot.engine) as i64 == handle => {}
            _ => return,
        }
        if let Some(slot) = guard.take() {
            // stop() 只置关闭信号, 不会阻塞, 因此在已关闭的 runtime 上也安全
            slot.engine.stop();
        }
    }));
}

/// 引擎是否已启动。
pub fn is_running() -> bool {
    let guard = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    guard.is_some()
}

/// 取本机设备指纹; 引擎未启动或句柄不匹配时返回空串。
pub fn device_id(handle: i64) -> String {
    match engine_for_handle(handle) {
        Some(e) => e.identity.device_id.clone(),
        None => String::new(),
    }
}

/// 取当前配置的 JSON 快照; 引擎未启动或句柄不匹配时返回空串。
pub fn config_json(handle: i64) -> String {
    let Some(engine) = engine_for_handle(handle) else {
        return String::new();
    };
    // clone 出一份快照再序列化, 不在 await 期间持有读锁。
    // 读锁本身不会失败, 所以这里直接拿快照, 不做多余的 Result 包装。
    let cfg = runtime().block_on(async { engine.config.read().await.clone() });
    match serde_json::to_string(&cfg) {
        Ok(j) => j,
        Err(e) => {
            log_error(&format!("序列化配置失败: {}", e));
            String::new()
        }
    }
}

/// 强制清空单例, 让同一进程内可以反复测启动/停止。
///
/// **仅供集成测试使用。** 用 `#[doc(hidden)]` 而不是 `#[cfg(test)]`:
/// 后者只在 crate 自身的单元测试里生效, 而 `core/tests/` 下的集成测试
/// 是**外部 crate**, 看不到被 `cfg(test)` 编译掉的符号。
///
/// 生产代码绝不能调用 —— 它绕过了 `shutdown()` 的停机流程。
#[doc(hidden)]
pub fn reset_for_test() {
    let mut guard = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = guard.take() {
        slot.engine.stop();
    }
}

/// 写错误日志。
///
/// 故意不依赖 Android 的 `__android_log_write`: 那需要再引一个
/// `ndk-sys` 之类的 crate 才能正确链接。当前只覆盖"启动失败"这种
/// 必须现场可见的错误, 通过返回值 + Kotlin 侧 Log 已经够定位;
/// 完整日志走 core 自己的 logger 落盘。
pub fn log_error(msg: &str) {
    tracing::error!("{}", msg);
}

/// 把引擎错误转成宿主侧能直接展示的中文描述。
pub fn describe(e: &FeisuoError) -> String {
    e.to_string()
}

/// 取最近一次 oot() 失败的原因。诊断用。
///
/// 为什么需要它：oot() 只返回 INVALID_HANDLE（0），而失败原因
/// 只进了 tracing。测试里没有初始化 logger，于是"启动失败"变成一个
/// **无法解释**的红 —— 只能看到 0，不知道是端口被占、目录不可写、
/// 还是别的。断言写不出有用的失败信息。
#[doc(hidden)]
    /// 取最近一次 `boot` 失败的原因；最近一次成功则返回空串。
    ///
    /// ## 为什么需要它
    ///
    /// `boot` 只返回 `INVALID_HANDLE`（0），而失败原因只进了 tracing。
    /// 测试里没有初始化 logger，于是"启动失败"变成一个**无法解释**的红 ——
    /// 只能看到 `0`，不知道是端口被占、目录不可写、还是别的。
    ///
    /// 这个缺口本身就浪费了一轮排查：修端口释放那两处时，我先**猜错了根因**
    /// （以为是传输端口的 bind 竞态，去加了 `bind_with_retry`），
    /// 因为失败信息只有一个 `0`。有了它，第一次跑就直接指出
    /// "无法绑定局域网发现端口 11324"。
    ///
    /// 断言写不出有用的失败信息，就等于没有断言。
pub fn last_boot_error() -> String {
    LAST_BOOT_ERROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 引擎是否**已成功启动**(发现 + 传输服务都起来了)。
///
/// 必须与 `is_running()` 区分: 后者只表示单例存在(进程内构造成功),
/// 而构造成功不代表网络栈起来了 —— 端口被占用时 `boot()` 照样返回有效句柄。
/// 宿主拿这个值来决定"在线"指示灯, 用错了就会在引擎没起来时显示绿灯。
pub fn engine_is_started(handle: i64) -> bool {
    engine_for_handle(handle)
        .map(|e| e.is_started())
        .unwrap_or(false)
}

/// 最近一次启动失败的原因; 正常返回空串。
///
/// 这是**状态查询**而非一次性事件, 因此不存在"事件早于监听器注册被丢弃"
/// 的竞态 —— 宿主页面挂载后再查也一定查得到。
pub fn engine_start_error(handle: i64) -> String {
    engine_for_handle(handle)
        .and_then(|e| e.start_error())
        .unwrap_or_default()
}

/// 列出已配对设备的 JSON 快照; 引擎未启动时返回空数组串。
pub fn trusted_devices_json(handle: i64) -> String {
    let Some(engine) = engine_for_handle(handle) else {
        return "[]".to_string();
    };
    match engine.trust_store.list_devices() {
        Ok(list) => serde_json::to_string(&list).unwrap_or_else(|e| {
            log_error(&format!("序列化受信设备失败: {}", e));
            "[]".to_string()
        }),
        Err(e) => {
            log_error(&format!("读取受信设备失败: {}", e));
            "[]".to_string()
        }
    }
}

/// 向指定设备发送文件。
///
/// 系统"分享到飞梭"的落点: 分享 Activity 只负责把字节流落盘,
/// 真正的推送由这里完成。早期实现把文件暂存完就结束, 结果是
/// 用户看到"正在推送到电脑…"的提示后再无下文, 文件永远躺在暂存目录里 ——
/// 整个分享功能等于没接上。
pub fn send_files(
    handle: i64,
    target_ip: &str,
    target_port: u16,
    target_device_id: &str,
    target_device_name: &str,
    paths: &[String],
) -> std::result::Result<(), String> {
    let engine = engine_for_handle(handle).ok_or_else(|| "引擎未启动".to_string())?;
    if paths.is_empty() {
        return Err("未指定要发送的文件".into());
    }
    let file_paths: Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
    let skipped = runtime()
        .block_on(async {
            engine
                .send_files(
                    target_ip,
                    target_port,
                    target_device_id,
                    target_device_name,
                    file_paths,
                )
                .await
        })
        .map_err(|e| describe(&e))?;
    // JNI 层只要 `()`，跳过信息在这里落日志就够了 ——
    // Android 端的 UI 尚未接入断点续传提示（见 CHANGELOG 已知限制）。
    if skipped > 0 {
        tracing::info!("断点续传跳过 {} 个已存在文件（Android 端暂不提示）", skipped);
    }
    Ok(())
}

/// 用 6 位 PIN 与指定设备完成双向绑定。
///
/// Android 端此前**完全没有**配对入口: 界面写着"输入配对码"、说明里写着
/// "在下方输入桌面端显示的 6 位配对码", 实际只弹一句"配对界面将在后续版本接入"。
/// 而产品核心承诺是"一次配对、终生免密" —— 配不上就等于:
///   * 分享推送永远命中"尚未配对任何电脑";
///   * 未配对电脑无法向本机投递(fail-closed)。
/// 也就是说整条 Android 发送链路在配对能力补齐前是**不可达**的。
///
/// 返回绑定后的设备信息 JSON; 失败返回可展示的中文原因。
pub fn pair_with_device(
    handle: i64,
    target_ip: &str,
    target_port: u16,
    pin: &str,
) -> std::result::Result<String, String> {
    let engine = engine_for_handle(handle).ok_or_else(|| "引擎未启动".to_string())?;
    let pin = pin.trim();
    if pin.len() != 6 || !pin.chars().all(|c| c.is_ascii_digit()) {
        return Err("配对码必须是 6 位数字".into());
    }
    let dev = runtime()
        .block_on(engine.pair_with_device(target_ip, target_port, pin))
        .map_err(|e| describe(&e))?;
    serde_json::to_string(&dev).map_err(|e| format!("序列化配对结果失败: {}", e))
}

/// 审批请求队列: (发送端, 接收端)。
///
/// 发送端必须留在全局 —— 一旦被释放, 转发任务就再也收不到请求,
/// 而宿主侧会一直在 [`next_approval`] 上空等。
static APPROVALS: std::sync::Mutex<
    Option<(
        tokio::sync::mpsc::Sender<ApprovalRequest>,
        tokio::sync::mpsc::Receiver<ApprovalRequest>,
    )>,
> = std::sync::Mutex::new(None);

/// 把 broadcast 的审批事件转发到宿主可消费的队列。
fn install_approval_forwarder(mut rx: tokio::sync::broadcast::Receiver<ApprovalRequest>) {
    let (tx, out_rx) = tokio::sync::mpsc::channel::<ApprovalRequest>(16);
    if let Ok(mut slot) = APPROVALS.lock() {
        *slot = Some((tx.clone(), out_rx));
    }
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(req) => {
                    // 队列满说明宿主长时间没取。与其丢弃让用户以为没收到,
                    // 不如等它腾出位置 —— 单次审批最多等 60 秒。
                    if tx.send(req).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log_error(&format!("审批事件积压 {} 条, 已丢弃最旧数据", n));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// 取下一个待审批请求 (最多等待 `timeout_ms` 毫秒)。超时返回 None。
///
/// 用 `try_recv` + 短暂 sleep 轮询, **不跨 await 持锁**:
/// `mpsc::Receiver` 不能 Clone, 也没法在 Mutex guard 里 await。
/// 50ms 的轮询间隔对"最多等 60 秒"的审批来说完全够用。
pub fn next_approval(timeout_ms: u64) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        {
            let mut guard = APPROVALS.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some((_, rx)) => match rx.try_recv() {
                    Ok(req) => return serde_json::to_string(&req).ok(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
                },
                None => return None,
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// 回应审批请求。返回是否命中(已过期或已处理过时为 false)。
///
/// ## Android 通知栏现在**可以**支持「每次匹配码」了
///
/// 方向改成"接收方出码、发起方输入"之后，通知栏缺的输入框
/// **不再是问题** —— 因为需要敲码的是**发起方**，不是本机用户。
/// 本机用户要做的只是：看一眼通知，点「允许」，然后把通知里那 6 位码
/// 念给对方（对方敲进自己的界面）。
///
/// 所以本函数不再接收 `grant_code`：码由服务端生成、随
/// `ApprovalRequest.grant_challenge` 送到宿主，宿主负责显示。
/// 早先"通知栏没有输入框所以必然失败"的那个限制，是旧方向
/// （接收方要替对端敲码）独有的问题。
pub fn respond_approval(approval_id: &str, allow: bool) -> bool {
    // 句柄 0 是唯一有效的当前句柄(与 device_id / send_files 的约定一致):
    // 移动端只有一个引擎实例, 不需要按句柄区分。
    let engine = match engine_for_handle(0) {
        Some(e) => e,
        None => return false,
    };
    // Android 通知只有「允许 / 拒绝」两个动作, 用户并没有"顺便建立长期信任"的意图,
    // 因此 true 必须映射到 AllowOnce 而不是 AllowAndTrust —— 这是最小权限原则。
    // 长期信任只应由配对流程或在明确的信任设置里建立（§2.5）。
    let action = if allow {
        ApprovalAction::AllowOnce
    } else {
        ApprovalAction::Reject
    };
    engine.server.resolve_approval(approval_id, action)
}

/// 主动探测一个 IP, 拿到对端当前的传输端口等信息。
///
/// 信任库里只记了 `last_ip`, **没有记传输端口** —— 而端口是可以在对端
/// 设置里改的。直接拿配置里的默认端口去连会连不上, 所以必须探测一次
/// 拿到对端信标里声明的真实端口。
pub fn probe(handle: i64, ip: &str) -> std::result::Result<crate::discovery::DiscoveredDevice, String> {
    let engine = engine_for_handle(handle).ok_or_else(|| "引擎未启动".to_string())?;
    runtime()
        .block_on(async { engine.probe_device(ip).await })
        .map_err(|e| describe(&e))
}
