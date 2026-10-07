use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, RwLock};
use tracing::{error, info, warn};
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, DISCOVERY_MULTICAST_ADDR};
use crate::error::{FeisuoError, Result};
use crate::protocol::{BeaconPacket, PROTOCOL_VERSION};
use crate::security::{DeviceIdentity, TrustStore};
// 网卡枚举：要的是操作系统**算好的**广播地址与启用状态，
// 而不是自己按前缀长度猜。见 compute_broadcast_targets 里的长注释。
use if_addrs::IfAddr;

/// 一个发现窗口的统计快照（纯数据, 可直接断言）。
///
/// 见 `start` 里 [`DiscoveryStats`] 的注释: 它存在的唯一原因是
/// "每个计数器每轮只能被读一次" 这条前提写不出来就会静默失效。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryWindow {
    /// 收到的信标包总数
    pub received: u64,
    /// 采纳（进入在线列表）的包数。**含**被节流的那些 ——
    /// 节流只是跳过"验签 + 落库", 内存里的在线状态照常更新。
    pub accepted: u64,
    /// 因节流而跳过「验签 + 落库」的包数（`accepted` 的子集）
    pub throttled: u64,
    /// 验签不过 / 指纹对不上 / 受信设备查不到公钥
    pub sig_fail: u64,
    /// 自身回声, 或协议版本不兼容
    pub self_or_ver: u64,
    /// 不是合法的 BeaconPacket JSON
    pub parse_err: u64,
    /// 回包（Ping-Pong）成功发出的次数
    pub replied: u64,
}

impl DiscoveryWindow {
    /// 被拒绝的包数 —— 四类原因之和。
    pub fn rejected(&self) -> u64 {
        self.sig_fail + self.self_or_ver + self.parse_err
    }
}

/// 接收侧累加、广播侧每 30 秒取一次增量的计数器组。
#[derive(Debug, Default)]
pub struct DiscoveryStats {
    pub received: AtomicU64,
    pub parse_err: AtomicU64,
    pub self_or_ver: AtomicU64,
    pub sig_fail: AtomicU64,
    pub accepted: AtomicU64,
    pub replied: AtomicU64,
    /// 因节流而跳过了「验签 + 落库」的包数。见 `BEACON_THROTTLE`。
    pub throttled: AtomicU64,
}

impl DiscoveryStats {
    /// 取走**整个窗口**的增量并把计数器清零。
    ///
    /// 这是读取这些计数器的**唯一**入口。
    /// 每个计数器在这里 `swap` 恰好一次, 之后打印用的是返回的快照 ——
    /// 于是"同一计数器被读两次、第二次必然是 0"这个坑**写不出来**。
    pub fn take_window(&self) -> DiscoveryWindow {
        use std::sync::atomic::Ordering as O;
        DiscoveryWindow {
            received: self.received.swap(0, O::Relaxed),
            accepted: self.accepted.swap(0, O::Relaxed),
            throttled: self.throttled.swap(0, O::Relaxed),
            sig_fail: self.sig_fail.swap(0, O::Relaxed),
            self_or_ver: self.self_or_ver.swap(0, O::Relaxed),
            parse_err: self.parse_err.swap(0, O::Relaxed),
            replied: self.replied.swap(0, O::Relaxed),
        }
    }
}

/// 设备在"在线列表"中的存活时长
const DEVICE_TTL_SECS: i64 = 20;
/// 主动探测时等待对端应答的时长
const PROBE_WAIT_MILLIS: u64 = 1200;
/// 定向广播时额外扫描的 IPv4 子网前缀长度
const SUBNET_PREFIXES: [u8; 3] = [16, 20, 24];

/// 同一 `(device_id, 来源 IP)` 在这个窗口内重复到达时，只更新内存、
/// 不再重复验签与落库。
///
/// # 为什么需要节流
///
/// 每个信标在接收端原本要做：1 次公钥 SELECT + 1 次 **Ed25519 验签**
/// + **3 次数据库写**（`update_last_ip` / `mark_seen` / `remember_endpoint`）。
///
/// 而发送端每个周期要往 `2 + 1 + 1 + 网卡数×3` 个目标发包
/// （见 `compute_broadcast_targets`），并且 Windows 上 `255.255.255.255`
/// 会被**按网卡各发一份**。虚拟网卡多的机器（ZeroTier + Hyper-V + WSL + VPN）
/// 实测能到 **80+ 包/秒** —— 于是接收端变成 80 次/秒验签 + 240 次/秒写库。
///
/// 后果是实打实的：
/// · `trust_store.db-wal` 涨到 4 MB 而库本体只有 118 KB（实测）；
/// · 每个包都要过一次 Ed25519，纯 CPU 开销；
/// · 「[发现]」诊断行里 `已入库` 与 `收` 几乎相等，事后分析时
///   完全看不出"真正的设备只有 1 台"。
///
/// # 节流窗口为什么按 (device_id, IP) 而不是只按 device_id
///
/// 只按 device_id 节流会**削弱安全**：任何人都能伪造一个受信 device_id
/// 配一个任意 IP，把假地址塞进在线列表（而列表会拿它去连）。
/// 加上 IP 之后，只有"同一台设备从同一个地址重复发包"才走捷径 ——
/// 地址变了 = 可能是攻击者，必须走完整验签。
const BEACON_THROTTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// 目标清单**只在首次或变化时**该打日志。
///
/// 为什么要单独一个函数：早先在 async 块里直接
/// `let mut g = state.lock(); ... drop(g);` 然后紧跟一个 `.await`，
/// 编译器报 *"future cannot be sent between threads safely"* ——
/// `std::sync::MutexGuard` 不是 `Send`，而借用分析把它的存活范围
/// 一直算到了 `await` 之后（保守但正确）。
///
/// 把它收进一个**普通函数**就干净了：守卫是那个函数的局部变量，
/// 返回时必然已释放，不可能跨进调用方的 async 帧。
/// 这比"想办法让编译器接受 drop(g)"可靠 —— 后者依赖借用推断的细节，
/// 将来有人加一行代码就可能又触发。
fn note_plan_if_changed(state: &Mutex<Option<String>>, key: String) -> bool {
    let mut g = state.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_deref() == Some(key.as_str()) {
        return false;
    }
    *g = Some(key);
    true
}

/// 把操作系统的网卡列表过滤成"值得广播的"与"跳过的"。
///
/// 抽成独立函数是为了**能在测试里直接验证**——它原来内嵌在
/// `compute_broadcast_targets` 里，而后者是 `TransferServer` 的关联函数，
/// 要测就得先把整个引擎跑起来。而这段过滤恰恰是本轮改动的核心：
/// "每网卡恰好一个正确广播 + 跳过链路本地"，值不值得一条守卫测试，取决于
/// 它有多容易验证。
///
/// 返回 `(保留, 跳过)`。保留项是 `(网卡名, 地址/前缀, 广播地址)`。
fn plan_interfaces(
    ifaces: &[if_addrs::Interface],
) -> (
    Vec<(String, String, Ipv4Addr)>,
    Vec<(String, String)>,
    Vec<String>,
) {
    let mut keep = Vec::new();
    let mut skip = Vec::new();
    let mut derived = Vec::new();
    for iface in ifaces {
        // 用**借用**而不是移动：`iface.addr` 被移走后
        // 就不能再读 `iface.name` / `iface.is_oper_up()`。
        let IfAddr::V4(v4) = &iface.addr else {
            continue;
        };
        let label = format!("{} {}", iface.name, v4.ip);
        if v4.ip.is_loopback() {
            skip.push((label, "回环（已单独处理）".into()));
            continue;
        }
        // **链路本地（169.254.0.0/16）按定义是单机链路**，
        // 上面不存在别的设备，所以对它广播没有任何意义。
        // 而 Windows 会给"没插线的网卡"自动分配这个段的地址（APIPA），
        // 于是这些网卡会把广播算成 169.254.255.255 这种
        // "自己就是整个网段"的地址，纯属空转。
        //
        // 实测（装了 ZeroTier + Hyper-V/WSL + 蓝牙 + 未插线网卡的机器）：
        // 旧路径经 `local_ip_address` 能看到 7 张非回环网卡，其中 **4 张是
        // 链路本地**，各发 3 个 = 12 个白发的包。
        //
        // ⚠️ 真正滤掉这 12 个的其实是 `if-addrs` 本身：它的 `link-local`
        // feature **默认关闭**，链路本地地址压根不会出现在返回列表里。
        // 所以下面这个判断是**第二道**防线，不是主力。它的价值在于：
        // 不依赖第三方 crate 的 feature 默认值（那个默认值将来可能变），
        // 以及在确实拿到链路本地地址的平台上仍然正确。
        if v4.ip.is_link_local() {
            skip.push((
                label,
                "链路本地 169.254/16（单机链路，无其它设备）".into(),
            ));
            continue;
        }
        // 未启用的网卡发不出去，也不该占目标数。
        if !iface.is_oper_up() {
            skip.push((label, "网卡未启用".into()));
            continue;
        }
        // 广播地址：优先用操作系统给的。
        //
        // 操作系统**给不出**时（点对点、tun/tap、无广播能力的虚拟网卡 ——
        // Android 上的 ZeroTier tap、VPN 的 tun0 属于这类）**不要跳过这张网卡**。
        //
        // 理由是**覆盖范围会倒退**：早先的实现按 /16 /20 /24 猜，对这种网卡
        // 照样发得出去（发到包含它的那些网段）。新实现要是直接跳过，
        // 表现就是"升级之后某些网络上突然搜不到设备" ——
        // 而这种倒退极难归因，因为代码看起来明明是"少发几个包"。
        //
        // 手里已经有 `netmask` 了，自己算出来就是**正确**的那一个：
        // 广播地址 = ip | !netmask。这仍然是"问操作系统要"，
        // 只是最后一步由我们补上。
        let bcast = v4.broadcast.unwrap_or_else(|| {
            derived.push(iface.name.clone());
            let ip = u32::from(v4.ip);
            let mask = u32::from(v4.netmask);
            Ipv4Addr::from(ip | !mask)
        });
        keep.push((
            iface.name.clone(),
            format!("{}/{}", v4.ip, v4.prefixlen),
            bcast,
        ));
    }
    (keep, skip, derived)
}

/// 一次广播目标计算的明细。日志靠它说话 —— 见 `TransferServer::log_broadcast_plan`。
///
/// 为什么要有这个结构而不是直接返回 `Vec<SocketAddr>`：
/// "为什么这台机器搜不到设备"这个问题，第一步要排除的就是
/// **"我们到底在哪些网上发了包"**。只返回目标地址列表，日志里只能看到
/// 一堆 IP，看不出哪个来自哪张网卡、哪张被跳过了、为什么跳过。
#[derive(Debug, Default, Clone)]
struct BroadcastPlan {
    targets: Vec<SocketAddr>,
    /// (网卡名, 地址/前缀, 该网卡的广播地址) —— 只含真正会发的
    per_iface: Vec<(String, String, Ipv4Addr)>,
    /// 广播地址是**我们自己按 netmask 算**的网卡名（操作系统没提供）。
    /// Android 上 ZeroTier 的 tap / VPN 的 tun0 属于这类 ——
    /// 它们仍然会发包，只是地址不是系统给的，日志里要能区分开。
    derived_bcast: Vec<String>,
    /// (网卡名, 跳过原因) —— 事后能回答"为什么 ZeroTier 没在发包"
    skipped: Vec<(String, String)>,
    /// 平台查询失败, 已退回"猜掩码"的旧路径
    fell_back: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub device_id: String,
    pub device_name: String,
    pub os_type: String,
    pub ip: String,
    pub transfer_port: u16,
    pub is_trusted: bool,
    pub last_seen_secs: i64,
    /// 对端能力位（§7.3）。**不进签名载荷**。
    ///
    /// 有了它，界面在打开穿梭右栏之前就知道"这台设备能不能走真实卷浏览"，
    /// 不必先发一次收件目录浏览再等回执才知道。0 = 1.x 老客户端。
    #[serde(default)]
    pub caps: u32,
    /// 对端应用版本（仅展示与诊断，不参与互通判断）
    #[serde(default)]
    pub app_version: String,
}

impl DiscoveredDevice {
    /// 对端是否支持真实卷浏览。
    ///
    /// **caps == 0 判定为"不支持"**（而不是"是 1.x，兼容处理"）：
    /// 1.x 客户端的签名载荷只有 7 段，我们解析出的版本号与对端一致时
    /// 唯一可靠的判据就是 caps 本身。把它当支持会在第一次浏览时报错，
    /// 当不支持则退回收件目录 —— 后者对用户无感。
    pub fn supports_volume_browse(&self) -> bool {
        self.caps & crate::protocol::caps::BROWSE_VOLUMES != 0
    }
}

pub struct DiscoveryService {
    identity: Arc<DeviceIdentity>,
    config: Arc<RwLock<AppConfig>>,
    trust_store: Arc<TrustStore>,
    devices: Arc<RwLock<HashMap<String, DiscoveredDevice>>>,
    event_tx: broadcast::Sender<DiscoveredDevice>,
    socket: Arc<RwLock<Option<Arc<UdpSocket>>>>,
    /// 主动探测的应答等待表: 目标 IP -> 应答通道。
    ///
    /// 必须按**来源 IP** 匹配, 不能按 beacon 里的 nonce:
    /// 对端收到探测包后会立刻回一个**全新 nonce** 的信标
    /// (它只把 device_id 视作自己的身份, 不复用我们的 nonce),
    /// 所以按 nonce 匹配永远等不到任何应答 —— 表现为
    /// "直连探测"永远超时, 而对端明明在线。
    probe_waiters: Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<DiscoveredDevice>>>>,
    /// 关闭信号。广播循环与监听循环都 select 在它上面。
    ///
    /// 旧实现两个循环都是无出口的 `loop {}`, `start()` 之后没有任何办法
    /// 让它们停下来 —— 进程内第二次 `init()` 就会有两个发现服务同时广播,
    /// 同设备在对方列表里出现两次。Android 侧 Service 重建时同样会踩到。
    ///
    /// **必须配合 `stopped` 标志使用, 不能只靠 Notify**:
    /// `Notify::notify_waiters()` 只唤醒"调用那一刻已经注册"的等待者。
    /// 若循环恰好停在两轮之间(还没 poll 到 `notified()`), 这次唤醒就丢了,
    /// 循环会一直跑到进程结束 —— 实测正是如此, stop() 之后对端仍持续
    /// 看到本机在线。
    shutdown: Arc<tokio::sync::Notify>,
    /// 关闭标志。循环每轮开头检查一次, 不依赖唤醒时序。
    stopped: Arc<std::sync::atomic::AtomicBool>,
    /// 后台任务的 JoinHandle。
    ///
    /// 必须持有它们才能在 stop() 里 **abort**: 只置标志的话, 三个循环
    /// 要等到下一次 `select` 才会退出, 而承载 UDP/TCP 套接字的那个任务
    /// 在此之前一直占着端口 —— 于是"停止后立刻重启"必然因端口被占而失败。
    /// 实测就是这个现象: `boot -> shutdown -> boot` 第二次返回 INVALID_HANDLE。
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl DiscoveryService {
    pub fn new(
        identity: Arc<DeviceIdentity>,
        config: Arc<RwLock<AppConfig>>,
        trust_store: Arc<TrustStore>,
    ) -> (Self, broadcast::Receiver<DiscoveredDevice>) {
        let (event_tx, rx) = broadcast::channel(128);
        (
            Self {
                identity,
                config,
                trust_store,
                devices: Arc::new(RwLock::new(HashMap::new())),
                event_tx,
                socket: Arc::new(RwLock::new(None)),
                probe_waiters: Arc::new(std::sync::Mutex::new(HashMap::new())),
                shutdown: Arc::new(tokio::sync::Notify::new()),
                stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
            },
            rx,
        )
    }

    /// 请求停止发现服务 (广播循环 + 监听循环 + 过期清理循环)。
    ///
    /// 先置标志再唤醒, 然后 abort 掉后台任务并**等它们真正退出**,
    /// 最后清空套接字引用。三步都不能省, 少一步"停止后立刻重启"就会失败。
    ///
    /// ## 坑一：abort 是"请求取消"，不是"已经取消"
    ///
    /// `JoinHandle::abort()` 只是标记 task 该被丢弃。task 持有的
    /// `Arc<UdpSocket>` 要等它**被调度一次、future 被 drop** 之后才真的释放,
    /// 端口因此还要再晚**若干毫秒**才空出来。
    ///
    /// 而广播循环与监听循环各自 clone 了一份套接字, 它们随 task 一起消失。
    /// 所以**只**清空 `self.socket` 是不够的 —— 那只少了一条引用,
    /// task 手里的那几条还在。必须 join 到 task 真正结束。
    ///
    /// ## 坑二：`try_write` 一次就放弃
    ///
    /// 清空要拿写锁, 而 `probe_ip` / `reply_port` 正在用
    /// `socket.read().await` 持读锁。原来的写法只试一次, 撞上就跳过且不吭声
    /// —— 端口不释放, 紧接着的 `start()` 拿到 `AddrInUse`, 而报错文案写的是
    /// "是否已有另一个飞梭实例在运行"。占用者恰恰是刚刚退出的**他自己**。
    ///
    /// 症状是"退出再打开偶尔启动失败", 极难联想到是 `stop()` 里少了一行。
    /// 真实后果: 托盘退出/重开、Android 前台服务被系统杀掉后重启。
    pub fn stop(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        self.shutdown.notify_waiters();

        // 1) abort: 请求取消三个后台任务。
        let handles: Vec<tokio::task::JoinHandle<()>> = match self.tasks.lock() {
            Ok(mut tasks) => tasks.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        for h in &handles {
            h.abort();
        }
        // 2) 清空 self.socket: 它是**最后一条**不属于 task 的引用。
        //    这一步必须**等到真的清掉**为止（clear_socket 会重试）。
        self.clear_socket();
        // 3) join: 等 task 真正被丢弃 —— 那才是套接字引用真正 drop 的时刻。
        //    顺序上放在最后, 让它成为"端口已空"的确认。
        for h in handles {
            Self::join_aborted(h);
        }
    }

    /// 把 `self.socket` 置空, **重试到成功为止**（上限 2 秒）。
    ///
    /// ## 为什么必须重试
    ///
    /// 写锁会与 `probe_ip` / `reply_port` 的
    /// `socket.read().await` 竞争, 而那些都是"clone 一下就放"的短读锁。
    /// 原来的写法是 `try_write` **只试一次**, 撞上就跳过且不吭声 ——
    /// 于是清空变成一件**看运气**的事, 而失败的唯一表现是几十毫秒后
    /// 另一处报 `AddrInUse`, 与这里毫无关联。
    ///
    /// ## 为什么不用 `blocking_write
    ///
    /// 它会 panic —— 当调用发生在 tokio worker 线程**内部**时。
    /// `protocol_integration` 的
    /// `test_stop_releases_port_and_silences_discovery` 就是在 async 测试里
    /// 直接调 `stop()` 的, 这不是假想场景。
    ///
    /// 而 `stop()` 本身必须**同步**: 调用方（Android JNI 关闭、桌面退出钩子）
    /// 拿到的 runtime 可能已经关闭, 在上面 `.await` 等一个永不完成的
    /// future 等于把进程挂死。
    ///
    /// 于是只剩一条路: 重试 + 让出, 不阻塞。
    /// 让出用 `yield_now` 而不是 `sleep`: 持锁者马上就会放,
    /// 睡 1ms 是白等的延迟, 而 yield 能立刻让 holder 跑完并释放。
    fn clear_socket(&self) {
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
        let deadline = std::time::Instant::now() + DEADLINE;
        loop {
            match self.socket.try_write() {
                Ok(mut guard) => {
                    *guard = None;
                    return;
                }
                Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        tracing::error!(
                            "等待 2s 仍拿不到发现套接字写锁，端口可能仍被本进程占用"
                        );
                        return;
                    }
                    std::thread::yield_now();
                }
            }
        }
    }
    /// 等待一个**已被 abort** 的 task 结束（最多 2 秒）。
    ///
    /// 被 abort 的 `JoinHandle` 会在下一次被 poll 时立刻返回
    /// `JoinError::Cancelled`；而那次 poll 正是 runtime 丢弃 future、
    /// **释放它持有的套接字**的时刻。所以"等到 join 返回"就等于
    /// "等到端口空出来"。
    ///
    /// 用最朴素的 `is_finished()` 轮询 + 让出。`block_on` 在这里两头都
    /// 不行: `stop()` 可能运行在没有 runtime 的线程上（Android JNI /
    /// 桌面退出钩子），也可能在 worker 线程内（async 测试）。
    /// 2 秒是防挂死的上限，不是预期耗时 —— 正常是微秒级。
    ///
    /// `is_finished()` 变 true 时 tokio **已经**把 future drop 掉了
    /// （那正是"完成"的定义），所以这里不需要再取一次结果 ——
    /// 也就不需要 `futures::FutureExt::now_or_never`，不必为它引依赖。
    fn join_aborted(handle: tokio::task::JoinHandle<()>) {
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
        let deadline = std::time::Instant::now() + DEADLINE;
        while !handle.is_finished() {
            if std::time::Instant::now() >= deadline {
                tracing::error!(
                    "等待发现服务后台任务退出超时（2s），发现端口可能仍被本进程占用"
                );
                return;
            }
            std::thread::yield_now();
        }
    }

    /// 构造并签名本机信标
    async fn build_beacon(&self) -> BeaconPacket {
        let cfg = self.config.read().await;
        let mut packet = BeaconPacket {
            version: PROTOCOL_VERSION,
            device_id: self.identity.device_id.clone(),
            // 这里必须消毒, 不能直接用配置里的原始值。
            // 设备名会广播给局域网内**所有**对端并写进对端的传输记录与日志;
            // 一个带 `\n` 的名字能让对端 UI 排版错乱, 还能在对端日志里伪造记录
            // (log injection)。旧实现直接 clone 原始值, 而消毒只发生在
            // `AppConfig::device_display_name()` —— 那个函数根本没有调用点,
            // 等于消毒形同虚设。
            device_name: crate::config::sanitize_device_name(&cfg.device_name),
            os_type: std::env::consts::OS.to_string(),
            transfer_port: cfg.transfer_port,
            timestamp: chrono::Utc::now().timestamp(),
            nonce: uuid::Uuid::new_v4().to_string(),
            signature: String::new(),
            // 能力位图（§7.3）：对端据此决定能不能走真实卷浏览等新能力。
            // **不进签名载荷** —— 载荷随字段增减会让新旧两版互相验不过签。
            caps: crate::protocol::local_caps(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        packet.signature = self.identity.sign(packet.signing_payload().as_bytes());
        packet
    }

    pub async fn start(&self) -> Result<()> {
        let disc_port = self.config.read().await.discovery_port;

        // 重置关闭标志, 否则 stop() 之后无法重新启动。
        // 必须在绑定之前清: 绑定失败时不应把旧服务的关闭状态清掉。
        // 用 swap 而非 store, 保证只有真正走到成功路径才生效 ——
        // 这里的写法是"先清标志, 绑定失败再恢复", 避免失败后
        // 上一轮已经退出的循环被误判为"仍在运行"。
        let was_stopped = self
            .stopped
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        let bind_result = self.start_inner(disc_port).await;
        if bind_result.is_err() && was_stopped {
            // 绑定失败且本服务之前是停止态: 恢复标志, 保持"已停止"语义
            self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        bind_result
    }

    async fn start_inner(&self, disc_port: u16) -> Result<()> {

        // 绑定 UDP 套接字。端口被占用时不再静默回退到临时端口 ——
        // 旧实现回退后广播目标仍是配置端口, 该实例永远无法与其他设备握手,
        // 且只有一条 warn 日志, 现场极难排查。
        let bind_ip = self.config.read().await.discovery_bind.clone();
        let listen_addr: SocketAddr = format!("{}:{}", bind_ip, disc_port)
            .parse()
            .map_err(|e| FeisuoError::Network(format!("发现端口非法: {}", e)))?;
        let std_sock = std::net::UdpSocket::bind(listen_addr).map_err(|e| {
            FeisuoError::Network(format!(
                "无法绑定局域网发现端口 {}: {} (是否已有另一个飞梭实例在运行?)",
                disc_port, e
            ))
        })?;
        if let Err(e) = std_sock.set_broadcast(true) {
            warn!("启用 UDP 广播失败, 将仅依赖组播发现: {}", e);
        }
        std_sock.set_nonblocking(true)?;

        let socket = Arc::new(UdpSocket::from_std(std_sock)?);
        // 回复对端时必须使用真实绑定端口, 否则端口被占用/回退时互相找不到
        let reply_port = socket.local_addr()?.port();
        *self.socket.write().await = Some(socket.clone());

        let multi_ip: Ipv4Addr = DISCOVERY_MULTICAST_ADDR
            .parse()
            .map_err(|e| FeisuoError::Network(format!("组播地址非法: {}", e)))?;
        if let Err(e) = socket.join_multicast_v4(multi_ip, Ipv4Addr::UNSPECIFIED) {
            warn!("加入组播组失败 (仅依赖广播发现): {}", e);
        }
        if let Ok(ifaces) = local_ip_address::list_afinet_netifas() {
            for (_name, ip) in ifaces {
                if let IpAddr::V4(v4) = ip {
                    if !v4.is_loopback() && socket.join_multicast_v4(multi_ip, v4).is_err() {
                        warn!("在接口 {} 上加入组播组失败", v4);
                    }
                }
            }
        }

        info!(
            "Discovery service listening on {} (configured port {})",
            socket.local_addr()?,
            disc_port
        );

        // 发现侧计数器（§9.7 同一套思路：诊断要能回答"为什么搜不到"）
        //
        // 没有它们的话，"设备明明开着却搜不到"只能靠猜，而这个问题的
        // 三种成因处置完全不同：
        // - `rx == 0`：**一个包都没收到** ⇒ 网络层不通（组播被拦 /
        //   防火墙 / 不在同一网段），该查网络；
        // - `rx > 0` 但 `accepted == 0` ⇒ 包到了但全被拒（被拉黑 /
        //   签名不过 / 版本不兼容），该查信任与版本；
        // - `accepted > 0` 但 `replied == 0` ⇒ 认出来了却没回包，
        //   对端会认为"没回包"从而显示离线。
        //
        // 这三种在界面上**长得一模一样**（都是"搜不到"），
        // 所以必须在日志里就把它们区分开。
        //
        // 声明放在两个任务之前：广播循环要读（打摘要），
        // 接收循环要写（累加）。
        let stats = Arc::new(DiscoveryStats::default());

        // ---- 发现侧计数器的容器 ----
        //
        // ## 为什么是一整个结构而不是八个裸 `AtomicU64`
        //
        // 因为"**每个计数器每轮只能被读一次**"是这条路径的正确性前提,
        // 而裸原子量把它变成了口头约定: 旧代码把同样这四项既用在
        // "求和"上、又被单独读一次来显示明细, 而取增量用的是
        // `swap(0)` —— 第二次读必然是 0。
        //
        // 于是日志里那行
        // `拒绝 70（拉黑 0 / 签名或公钥 0 / 自身或版本 0 / 解析失败 0）`
        // 的**明细四项恒为 0**, 与真实原因毫无关系 ——
        // 而它恰恰是唯一能回答"包到了却被拒, 到底拒在哪一步"的仪表。
        // 真实值 12/34/5/7 打出来仍然是 0/0/0/0（已用 12 行复刻程序验证）。
        //
        // 收进结构体之后, "读两次"这件事**写不出来**了:
        // 取值只有 [`DiscoveryStats::take_window`] 一个入口,
        // 它一次性把所有计数器换出来, 之后打印用的是快照。
        // 顺带让"收 = 采纳 + 拒绝"这条恒等式可以直接断言。

        // **按来源 IP 统计收包数** —— 诊断盲区的补丁。
        //
        // ## 为什么需要它
        //
        // 实测（本机单实例、计划里只有 7 个广播目标 / 3 秒 = 2.3 包/秒）
        // 30 秒内收到 **111949** 个信标包，是自身发送量的约 1600 倍，
        // 而其中 99.97% 落在"重复已节流"、签名失败 0 个。
        //
        // 也就是说：**确实有别的飞梭实例（或某种重播机制）在高频发包**，
        // 但旧日志**答不出"是谁"** —— 它只报总数，不报来源。
        // 于是这个现象只能靠猜（我先后猜了"多实例"、"自己收自己"，
        // 两个都被数据否掉了）。
        //
        // "收包数远高于自己的发送量"是个**可判定的异常**，而没有来源
        // 就没有下一步动作。现在每个 30 秒摘要会带上 Top 3 来源。
        //
        // 容量有界：只保留计数最高的若干个来源，防止被伪造的源地址
        // （UDP 源 IP 随便填）撑爆内存。
        let src_counts: Arc<std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, u64>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

        // ---- 广播循环 ----
        // 广播侧只**读**计数器（每 30 秒打一次摘要），读用
        // `DiscoveryStats::take_window` 一次性取走整个窗口的增量。
        let b_stats = stats.clone();
        let b_socket = socket.clone();
        let b_trust_store = self.trust_store.clone();
        let b_identity = self.identity.clone();
        let b_config = self.config.clone();
        let b_shutdown = self.shutdown.clone();
        let b_stopped = self.stopped.clone();
        // 上一次打过日志的目标清单指纹（用来只打"首次 + 变化"）
        let b_last_plan = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        // 读取收包来源统计（接收侧写、这里读），见 src_counts 的注释
        let b_src_counts = src_counts.clone();
        let broadcast_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3));
            // 单播兜底的独立节拍（P1 ⑩）
            //
            // 为什么要独立于 3 秒的广播节拍：组播/广播在很多网络里根本
            // 不通（AP 的"客户端隔离"、交换机 IGMP snooping、企业防火墙、
            // 云主机安全组）。这时唯一的通路是**单播**。
            //
            // 为什么不复用 3 秒节拍：单播是**逐个地址**发的，
            // 10 台设备 × 4 个端点 = 40 个包/次。3 秒一次太频繁，
            // 而 30 秒是"用户能接受的最坏发现延迟"——也是 P1 的验收线
            // （"关掉所有组播/广播后，仅靠单播回呼仍能在 30s 内发现"）。
            let mut unicast_tick: u64 = 0;
            loop {
                // 标志检查放在 select 之前: notify_waiters 可能丢唤醒,
                // 标志是唯一可靠的判据。
                if b_stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    info!("发现广播循环已停止");
                    break;
                }
                tokio::select! {
                    // 关闭信号优先: 停止后不得再发任何信标
                    _ = b_shutdown.notified() => {
                        info!("发现广播循环已停止");
                        break;
                    }
                    _ = interval.tick() => {}
                }
                // select 醒来后再确认一次: tick 与 stop 同时发生时
                // 上面那个 break 可能没走到
                if b_stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    info!("发现广播循环已停止");
                    break;
                }
                let cfg = b_config.read().await;
                let mut packet = BeaconPacket {
                    version: PROTOCOL_VERSION,
                    device_id: b_identity.device_id.clone(),
                    // 同 build_beacon: 广播出去的设备名必须消毒
                    device_name: crate::config::sanitize_device_name(&cfg.device_name),
                    os_type: std::env::consts::OS.to_string(),
                    transfer_port: cfg.transfer_port,
                    timestamp: chrono::Utc::now().timestamp(),
                    nonce: uuid::Uuid::new_v4().to_string(),
                    signature: String::new(),
                    caps: crate::protocol::local_caps(),
                    app_version: env!("CARGO_PKG_VERSION").to_string(),
                };
                packet.signature = b_identity.sign(packet.signing_payload().as_bytes());
                drop(cfg);

                if let Ok(bytes) = serde_json::to_vec(&packet) {
                    // 已知端点优先，覆盖网排前面（`all_endpoints` 已排序）。
                    //
                    // 旧实现只取 `trusted_devices.last_ip` **一个**地址。
                    // 一台设备同时有 ZeroTier 与物理网卡时，存下的是哪个
                    // 取决于最后一次是谁先回包 —— DHCP 一换就彻底失联，
                    // 而用户既没重启也没改配置，表现为"设备明明开着却搜不到"。
                    let known_ips: Vec<String> = match b_trust_store.all_endpoints() {
                        Ok(eps) => eps
                            .into_iter()
                            // 端口为 0 = 老库只有裸 IP、端口未知。照样探：
                            // 信标是 UDP，端口错了收不到，但**不会**造成副作用；
                            // 而漏探则可能让设备彻底失联。
                            .map(|e| {
                                if e.port > 0 {
                                    format!("{}:{}", e.ip, e.port)
                                } else {
                                    e.ip
                                }
                            })
                            .collect(),
                        Err(e) => {
                            tracing::debug!("读取已知端点失败, 退回单地址: {}", e);
                            b_trust_store
                                .list_devices()
                                .map(|ds| {
                                    ds.into_iter()
                                        .filter(|d| !d.last_ip.trim().is_empty())
                                        .map(|d| d.last_ip)
                                        .collect()
                                })
                                .unwrap_or_default()
                        }
                    };
                    let plan = Self::compute_broadcast_targets(reply_port, known_ips.clone());
                    // 目标清单**只在首次或变化时**打日志：DHCP 续租会换 IP，
                    // 而变化正是"为什么突然搜不到设备"最需要的那条线索。
                    // 每 3 秒打一遍会把日志淹掉。
                    let plan_key = format!(
                        "{:?}|{:?}|{}",
                        plan.per_iface, plan.skipped, plan.fell_back
                    );
                    if note_plan_if_changed(&b_last_plan, plan_key) {
                        Self::log_broadcast_plan(&plan, reply_port);
                    }

                    for target in plan.targets {
                        if let Err(e) = b_socket.send_to(&bytes, target).await {
                            tracing::debug!("发现信标发送失败 {}: {}", target, e);
                        }
                    }

                    // ---- 单播兜底（每 30 秒）----
                    //
                    // 逐个**已知端点**单独发，并把结果记进诊断：
                    // 事后能回答"是组播不通，还是我们压根没往对端发过"。
                    unicast_tick += 1;
                    if unicast_tick % 10 == 1 {
                        let mut sent = 0usize;
                        let mut failed = 0usize;
                        for ep in &known_ips {
                            if ep.starts_with("127.") {
                                continue; // 回环已由广播目标覆盖
                            }
                            // 只取 IP 部分：发现端口由**本机实际绑定值**决定
                            // （`reply_port`），端点里记的可能是对方上次
                            // 报的端口，也可能因为对方改过端口而过期。
                            // 用错端口的单播是收不到的 —— 而这正是
                            // "设备明明开着却搜不到"的一类原因。
                            let ip_only: IpAddr = match ep
                                .rsplit_once(':')
                                .map(|(a, _)| a)
                                .unwrap_or(ep.as_str())
                                .trim()
                                .parse()
                            {
                                Ok(a) => a,
                                Err(_) => {
                                    failed += 1;
                                    continue;
                                }
                            };
                            let target = SocketAddr::new(ip_only, reply_port);
                            match b_socket.send_to(&bytes, target).await {
                                Ok(_) => sent += 1,
                                Err(e) => {
                                    failed += 1;
                                    tracing::debug!("单播信标失败 {}: {}", target, e);
                                }
                            }
                        }
                        if sent > 0 || failed > 0 {
                            tracing::info!(
                                "单播兜底: 已知端点 {} 个, 发出 {} 失败 {} (组播/广播不通时靠这条)",
                                known_ips.len(),
                                sent,
                                failed
                            );
                        }
                        // 顺手清理长期不响应的端点
                        if let Ok(n) = b_trust_store.prune_endpoints(4, 200) {
                            if n > 0 {
                                tracing::info!("已清理 {} 个失效设备端点", n);
                            }
                        }
                        // ---- 发现侧健康摘要（与上面的单播兜底同一节拍）----
                        //
                        // 「设备明明开着却搜不到」原来只能靠猜，而这个问题的
                        // 三种成因处置完全不同：
                        // - `rx == 0`：**一个包都没收到** ⇒ 网络层不通
                        //   （组播被拦 / 防火墙 / 不在同一网段），该查网络；
                        // - `rx > 0` 但 `accepted == 0` ⇒ 包到了但全被拒
                        //   （被拉黑 / 签名不过 / 版本不兼容），该查信任与版本；
                        // - `accepted > 0` 但 `replied == 0` ⇒ 认出来了却没回包，
                        //   对端会认为"没回包"从而显示离线。
                        //
                        // 这三种在界面上**长得一模一样**（都是"搜不到"），
                        // 所以必须在这里就把它们区分开。
                        //
                        // 整轮只取一次快照（`take_window` 内部每个计数器
                        // swap 一次）。之后所有打印都读这份快照 ——
                        // 详见上面 `DiscoveryStats` 的注释：旧代码对同样
                        // 这四项 `swap` 了两次，导致明细四项恒为 0，
                        // 而它恰恰是唯一能回答"到底拒在哪一步"的仪表。
                        let w = b_stats.take_window();
                        // 来源统计每轮**清空**再打印。
                        //
                        // 旧实现只在读的时候排序, 从不清空, 于是它其实是
                        // "进程启动至今的累计值" —— 而它紧挨着"收 N 包"
                        // 那个 30 秒增量打印。实测两者差了两个数量级
                        // (收 80 包 / 来源 100.64.0.120=1142), 任何人
                        // 都会把它读成"这一轮收到了 1142 包"。
                        // 注释里写的是"每个 30 秒摘要会带上 Top 3 来源",
                        // 意图本来就是每轮, 这里让它真的每轮。
                        let mut src_top: Vec<(std::net::IpAddr, u64)> =
                            if let Ok(mut m) = b_src_counts.lock() {
                                m.drain().collect()
                            } else {
                                Vec::new()
                            };
                        src_top.sort_by(|a, b| b.1.cmp(&a.1));
                        if w.received > 0 {
                            // 「重复包」这一项是**必须**打出来的：
                            // 不打的话 `采纳` 会和 `收` 几乎相等（实测
                            // 98921 / 98921），事后看日志会以为网络里真有
                            // 98921 台设备，而实际只有 1 台 —— 是对端每个
                            // 周期往几十个广播地址各发一份，接收端按
                            // (device_id, IP) 做了节流。
                            info!(
                                "[发现] 收 {} 包: 采纳 {}（其中重复 {} 已节流）· 拒绝 {}（签名或公钥 {} / 自身或版本 {} / 解析失败 {}）· 回包 {}",
                                w.received,
                                w.accepted,
                                w.throttled,
                                w.rejected(),
                                                w.sig_fail,
                                w.self_or_ver,
                                w.parse_err,
                                w.replied,
                            );
                            // 来源 Top 3 —— 见 src_counts 的注释：
                            // 没有它，"收包数是自身发送量的 1600 倍"这个异常
                            // 无法定位到人/设备，只能靠猜。
                            if !src_top.is_empty() {
                                let desc = src_top
                                    .iter()
                                    .take(3)
                                    .map(|(ip, c)| format!("{}={}", ip, c))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                info!("[发现] 本轮收包来源 Top{}: {}", src_top.len().min(3), desc);
                            }
                        } else if sent > 0 {
                            // 发得出去但一个回包都没有 —— 这正是
                            // "组播被拦、只靠单播"的典型现场，值得明确说出来
                            info!(
                                "[发现] 30 秒内**未收到任何信标**（已向 {} 个已知端点单播兜底）。\
                                 若目标设备确实在线，说明组播/广播在本网络被拦截，\
                                 依赖 30 秒一轮的单播发现",
                                sent
                            );
                        }
                    }
                }
            }
        });

        // ---- 监听循环 ----
        let l_socket = socket.clone();
        let l_devices = self.devices.clone();
        let l_trust_store = self.trust_store.clone();
        let l_identity = self.identity.clone();
        let l_config = self.config.clone();
        let l_event_tx = self.event_tx.clone();
        let l_waiters = self.probe_waiters.clone();
        let l_shutdown = self.shutdown.clone();
        let l_stopped = self.stopped.clone();
        // 接收侧累加、广播侧读取 —— 读取只有 `take_window` 一个入口，
        // 所以两边共用同一组计数器不会互相污染。
        let l_stats = stats.clone();
        // 接收侧累加、广播侧读取（见上面的 src_counts 注释）
        let l_src_counts = src_counts.clone();
        // (device_id, 来源 IP) -> 上次做完整验签+落库的时刻。
        // 见 BEACON_THROTTLE 的注释：按 IP 一起进键，是为了让
        // "换了个地址的同一 device_id" 仍然必须走完整验签。
        let l_verified = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::<String, std::time::Instant>::new(),
        ));
        let listen_task = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                // 标志优先: notify_waiters 可能丢唤醒
                if l_stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    info!("发现监听循环已停止");
                    break;
                }
                // recv_from 必须与关闭信号竞争: 否则 stop() 之后这个任务
                // 永远阻塞在收包上, 端口虽然被释放但任务和 socket 引用都还在,
                // 紧接着的 start() 会与它抢同一个套接字。
                let received = tokio::select! {
                    _ = l_shutdown.notified() => {
                        info!("发现监听循环已停止");
                        break;
                    }
                    r = l_socket.recv_from(&mut buf) => r,
                };
                let (len, peer_addr) = match received {
                    Ok(v) => v,
                    Err(e) => {
                        // Windows UDP 收包会因 ICMP 端口不可达返回 10054, 属正常现象
                        if e.raw_os_error() == Some(10054) {
                            tracing::trace!("UDP ICMP port unreachable (10054), ignoring");
                        } else {
                            error!("Discovery recv error: {}", e);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        continue;
                    }
                };

                // 旧实现为每个收到的信标 spawn 一个任务, 攻击者万级/秒的 UDP 就能
                // 制造万级任务 (廉价远程资源放大器), 这里改为内联处理
                l_stats.received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // 记来源（无论后面是"自己"还是"重复"还是"采纳"）——
                // 定位"是谁在发"只需要知道包从哪来。
                if let Ok(mut m) = l_src_counts.lock() {
                    *m.entry(peer_addr.ip()).or_insert(0) += 1;
                    // 有界：只在超出容量时裁剪一次，避免每包都排序。
                    if m.len() > 32 {
                        // 保留计数最大的 16 个（先按计数升序收集再截断）
                        let mut keep: Vec<(std::net::IpAddr, u64)> =
                            m.iter().map(|(k, v)| (*k, *v)).collect();
                        keep.sort_by_key(|(_, c)| *c);
                        let cut = keep.len() - 16;
                        for (ip, _) in keep.into_iter().take(cut) {
                            m.remove(&ip);
                        }
                    }
                }
                let Ok(beacon) = serde_json::from_slice::<BeaconPacket>(&buf[..len]) else {
                    l_stats.parse_err.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                };
                if beacon.device_id == l_identity.device_id || beacon.version != PROTOCOL_VERSION {
                    l_stats.self_or_ver.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }

                let ip = peer_addr.ip().to_string();
                let now = chrono::Utc::now().timestamp();

                // ---- 签名校验 ----
                // 旧实现完全信任信报自报的 device_id, 局域网任意主机都能
                // 冒充"已受信设备"刷信标, 把受害者 last_ip 污染到自己机器上。
                let is_trusted = l_trust_store
                    .is_device_trusted(&beacon.device_id)
                    .unwrap_or(false);
                // 节流：同一 (device_id, IP) 在窗口内重复到达时跳过
                // 「验签 + 3 次写库」，只让下面的内存更新照常发生。
                // **安全前提**：键里带 IP，所以伪造 device_id 换个地址来
                // 攻击时不会命中这个捷径，仍然必须过完整验签。
                //
                // 只给**受信**设备记账：否则攻击者用一堆假 device_id 就能
                // 让这张表无限增长（内存耗尽），而未受信设备本来就不走
                // 落库路径，记账对它们毫无用处。
                let throttle_key = format!("{}|{}", beacon.device_id, ip);
                let skip_persist = if !is_trusted {
                    false
                } else {
                    match l_verified.lock() {
                        Ok(mut map) => {
                            // 顺手清过期项。受信设备数很少，正常情况下
                            // 永远到不了这个阈值；它是防"某台设备换了很多
                            // 个 IP"这种长期运行后缓慢增长的兜底。
                            if map.len() > 64 {
                                map.retain(|_, at| at.elapsed() < BEACON_THROTTLE);
                            }
                            match map.get(&throttle_key) {
                                Some(&at) if at.elapsed() < BEACON_THROTTLE => true,
                                _ => {
                                    map.insert(throttle_key.clone(), std::time::Instant::now());
                                    false
                                }
                            }
                        }
                        // 锁被毒化（只有 OOM 那种情况）时**不要**放行，
                        // 宁可每包都验签。
                        Err(_) => false,
                    }
                };
                if skip_persist && is_trusted {
                    l_stats.throttled.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                if is_trusted && !skip_persist {
                    match l_trust_store
                        .get_device_pubkey(&beacon.device_id)
                        .unwrap_or(None)
                    {
                        Some(pk) => {
                            let id_matches =
                                DeviceIdentity::device_id_from_pubkey_hex(&pk)
                                    .map(|id| id == beacon.device_id)
                                    .unwrap_or(false);
                            let sig_ok = DeviceIdentity::verify(
                                &pk,
                                beacon.signing_payload().as_bytes(),
                                &beacon.signature,
                            )
                            .unwrap_or(false);
                            if !id_matches || !sig_ok {
                                warn!(
                                    "发现信标签名/指纹校验失败, 已忽略 {} 自称的设备 {}",
                                    ip, beacon.device_id
                                );
                                l_stats.sig_fail.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                // 验签失败**不能**留在节流表里：那正是攻击流量，
                                // 留着会让它挡住后续真包的正常节流记账。
                                if let Ok(mut map) = l_verified.lock() {
                                    map.remove(&throttle_key);
                                }
                                continue;
                            }
                            // 只有验签通过的受信设备才允许更新 last_ip / last_seen_at
                            let _ = l_trust_store.update_last_ip(&beacon.device_id, &ip);
                            // 记录"最后一次在线"（§3.6）。
                            // 这是离线灰显能说清"上次在线 X 前"的数据来源,
                            // **必须落库** —— 否则重启本机后全部变成未知。
                            let _ = l_trust_store.mark_seen(&beacon.device_id, &ip);
                            // 记住这个**具体端点**（P1 ⑩ / P4 ⑧）。
                            // 只记 verified=true 的 —— 验签刚通过，
                            // 这个地址确实可达。
                            //
                            // 端点性质用来源 IP 判定：ZeroTier 给
                            // 100.64/10，物理网卡给 10/8·172.16/12·192.168/16。
                            let kind = match crate::transport::classify_ip(&ip) {
                                crate::transport::OverlayKind::CarrierGradeNat100 => "overlay",
                                crate::transport::OverlayKind::PrivateLan => "lan",
                                // 回环单独一类：标成 "public" 会在「连接路径」
                                // 里显示成"公网"，而它其实是本机 —— 同一台
                                // 机器上跑两个实例时才出现。
                                crate::transport::OverlayKind::Loopback => "loopback",
                                _ => "public",
                            };
                            if let Err(e) = l_trust_store.remember_endpoint(
                                &beacon.device_id,
                                &ip,
                                beacon.transfer_port,
                                kind,
                                true,
                            ) {
                                tracing::debug!("记录设备端点失败: {}", e);
                            }
                        }
                        None => {
                            warn!("受信设备 {} 未找到公钥, 已忽略其信标", beacon.device_id);
                            l_stats.sig_fail.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            continue;
                        }
                    }
                }

                let dev = DiscoveredDevice {
                    device_id: beacon.device_id.clone(),
                    device_name: beacon.device_name,
                    os_type: beacon.os_type,
                    ip: ip.clone(),
                    transfer_port: beacon.transfer_port,
                    is_trusted,
                    last_seen_secs: now,
                    caps: beacon.caps,
                    app_version: beacon.app_version,
                };
                l_stats.accepted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                // 唤醒正在等待该来源的主动探测者。
                // 按来源 IP 匹配: 对端回信用的是它自己的 nonce, 无法与我们的探测
                // nonce 对上, 只有 IP 才是双方共有的标识。
                if let Ok(waiters) = l_waiters.lock() {
                    if let Some(tx) = waiters.get(&ip) {
                        let _ = tx.send(dev.clone());
                    }
                }

                {
                    let mut map = l_devices.write().await;
                    map.insert(beacon.device_id, dev.clone());
                }
                if l_event_tx.send(dev).is_err() {
                    tracing::debug!("发现事件通道无订阅者 (前端可能尚未加载)");
                }

                // ---- 即时回信 (Ping-Pong), 让双方尽快互相看见 ----
                let cfg = l_config.read().await;
                let mut reply = BeaconPacket {
                    version: PROTOCOL_VERSION,
                    device_id: l_identity.device_id.clone(),
                    // 同上: 回信同样是把本机名字交给对端, 必须消毒。
                    // 漏掉这一处的话, 主动探测(probe)路径会把脏名字发出去 ——
                    // 因为它走的是回信而不是广播, 只改广播是修不干净的。
                    device_name: crate::config::sanitize_device_name(&cfg.device_name),
                    os_type: std::env::consts::OS.to_string(),
                    transfer_port: cfg.transfer_port,
                    timestamp: chrono::Utc::now().timestamp(),
                    nonce: uuid::Uuid::new_v4().to_string(),
                    signature: String::new(),
                    caps: crate::protocol::local_caps(),
                    app_version: env!("CARGO_PKG_VERSION").to_string(),
                };
                drop(cfg);
                reply.signature = l_identity.sign(reply.signing_payload().as_bytes());
                if let Ok(reply_bytes) = serde_json::to_vec(&reply) {
                    // 回包必须发到对端的**源端口**，也就是 `peer_addr.port()`。
                    //
                    // 早先用 `reply_port`（**本机**的绑定端口）。UDP 的源端口
                    // 就是发送方套接字的绑定端口，所以对方收包时看到的
                    // `peer_addr.port()` 才是它自己的端口 —— 回包发到
                    // `reply_port` 只在"两边配置了同一个发现端口"时才碰巧正确。
                    //
                    // 一旦不碰巧就**静默丢包**：探测定时器会认为"没回包"，
                    // 于是走单播兜底；设备明明在线却显示离线，而且日志里
                    // 只有"没回包"没有一个字提到"回包发错端口"。
                    let target = SocketAddr::new(peer_addr.ip(), peer_addr.port());
                    if let Err(e) = l_socket.send_to(&reply_bytes, target).await {
                        // 回包失败是**可操作**的故障（对方会认为"没回包"从而
                        // 显示离线），所以用 warn 而不是 debug —— 默认日志级别
                        // 就能看到。成功那侧保持 debug，否则每 3 秒一条会
                        // 把日志轮转配额冲掉。
                        warn!("发现回包发送失败 {}: {}", target, e);
                    } else {
                        tracing::debug!(
                            "已回包给 {}（对端发现端口 {}，本机 {}）",
                            target,
                            peer_addr.port(),
                            reply_port
                        );
                    }
                    l_stats.replied.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });

        // ---- 过期清理 ----
        let p_devices = self.devices.clone();
        let p_shutdown = self.shutdown.clone();
        let p_stopped = self.stopped.clone();
        let cleanup_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                // 与广播/监听循环一样: 标志优先, 避免 notify_waiters 丢唤醒。
                // 漏掉这个循环的关闭更隐蔽 —— stop() 之后它每 5 秒仍写一次锁,
                // 第二次 start() 就会有两个清理任务同时操作同一张表。
                if p_stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    info!("发现过期清理循环已停止");
                    break;
                }
                tokio::select! {
                    _ = p_shutdown.notified() => {
                        info!("发现过期清理循环已停止");
                        break;
                    }
                    _ = interval.tick() => {}
                }
                let now = chrono::Utc::now().timestamp();
                let mut map = p_devices.write().await;
                map.retain(|_, d| (now - d.last_seen_secs) < DEVICE_TTL_SECS);
            }
        });

        // 登记三个后台任务, 使 stop() 能 abort 它们并立刻释放发现端口
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push(broadcast_task);
            tasks.push(listen_task);
            tasks.push(cleanup_task);
        }

        Ok(())
    }

    /// 每 3 秒算一次目标太浪费（要问操作系统），所以只在**首次**和**变化时**打日志。
    fn log_broadcast_plan(plan: &BroadcastPlan, disc_port: u16) {
        info!(
            "[发现] 发送计划: 共 {} 个目标 / 3 秒（端口 {}）{}",
            plan.targets.len(),
            disc_port,
            if plan.fell_back {
                " —— **平台网卡查询失败，已退回猜掩码的旧路径**（每个网卡发 3 个）"
            } else {
                ""
            }
        );
        for (name, addr, bcast) in &plan.per_iface {
            info!("    · 定向广播 {}  {}  ->  {}", name, addr, bcast);
        }
        // 单独点出哪些广播地址是**我们自己按 netmask 算的**（操作系统没给）。
        // Android 上 ZeroTier 的 tap、VPN 的 tun0 属于这类 ——
        // 事后判断"为什么某个网络上没发包"时，这个区分是必需的。
        if !plan.derived_bcast.is_empty() {
            info!(
                "    （其中 {} 张的广播地址由 netmask 推算，操作系统未提供：{}）",
                plan.derived_bcast.len(),
                plan.derived_bcast.join(", ")
            );
        }
        for (name, why) in &plan.skipped {
            tracing::debug!("    · 跳过 {}：{}", name, why);
        }
        if !plan.skipped.is_empty() {
            // 跳过项用 info 而不是 debug：用户报"搜不到设备"时，
            // 第一件要排除的就是"我们是不是根本没在某个网上发包"。
            info!(
                "    · 跳过 {} 张网卡（链路本地 / 回环 / 未启用）",
                plan.skipped.len()
            );
        }
    }

    fn compute_broadcast_targets(disc_port: u16, known_ips: Vec<String>) -> BroadcastPlan {
        let mut plan = BroadcastPlan::default();
        let mut targets = std::collections::HashSet::new();

        // 0. 回环单播。
        //    组播/广播在回环网卡上不工作, 不同进程跑两个节点时互相永远看不见,
        //    这会让"同机联调 / 自动化测试"完全无法进行。
        //    代价是每 3 秒多发一个 200 字节的本机包, 且自己发的包会被
        //    接收端按 device_id 自我过滤掉, 不会污染在线列表。
        targets.insert(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            disc_port,
        ));
        // Windows 上 127.255.255.255 是回环网段的受限广播地址, 实测能送达
        // 同机任意回环地址上的监听者。单机多实例(集成测试 / 手动跑两个节点)
        // 靠这一条互相发现 —— 只发 127.0.0.1 是发不到别的实例的。
        targets.insert(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 255, 255, 255)),
            disc_port,
        ));

        // 1. 组播地址
        if let Ok(addr) = format!("{}:{}", DISCOVERY_MULTICAST_ADDR, disc_port).parse() {
            targets.insert(addr);
        }

        // 2. 受限全局广播 (仅在组播被路由器拦截时作为兜底)
        //
        //    ⚠️ 它在**协议栈层面**就不可收窄：受限广播地址没有"指定网卡"
        //    的写法，操作系统会**按每张已启用网卡各发一份**。
        //    所以下面"每网卡一个定向广播"已经把可控的部分压到最小，
        //    而这一条的实际线上包数 = 已启用网卡数（不是 1）。
        //    留着它是因为它覆盖了"定向广播算错掩码"这种兜底场景。
        if let Ok(addr) = format!("255.255.255.255:{}", disc_port).parse() {
            targets.insert(addr);
        }

        // 3. 各网卡的**定向广播**。
        //
        //    ## 为什么是"问操作系统要", 而不是自己按 /16 /20 /24 猜三个
        //
        //    早先的实现对每张网卡算 3 个掩码的广播地址（[16,20,24]），
        //    因为 `local_ip_address` 只给 IP、不给掩码，而发 /24 在
        //    /16~/23 掩码的局域网里确实送不出去。
        //
        //    但"猜三个"的代价在真实机器上被严重低估了。实测一台
        //    装了 ZeroTier + Hyper-V/WSL + 蓝牙 + 未插线网卡的开发机：
        //      · 非回环网卡 7 张  =>  21 个定向广播 / 3 秒 = 7 个/秒
        //      · 其中 **12 个打在 169.254.x.x**（链路本地，见下）
        //      · 真正可能送到对端的只有 3 个
        //    而 `/16` 定向广播一次覆盖 **65534 个地址** —— 交换机要泛洪它、
        //    每台主机都要回应 65534 次 ARP、路由器还可能转发它，
        //    是行业里公认的**广播风暴**诱因。家用交换机不转发所以没出事，
        //    换到公司网段就难说了。
        //
        //    `if-addrs` 直接给出操作系统算好的 `broadcast`，还有 `oper_status`：
        //    于是从"猜 3 个"变成"每网卡**恰好 1 个正确的**"，
        //    **覆盖范围完全不变**（该到哪还是到哪），21 个降到 3 个。
        //
        //    这不是"收窄"，是"算对" —— 所以它不需要等任何实测验收。
        match if_addrs::get_if_addrs() {
            Ok(ifaces) => {
                let (keep, skip, derived) = plan_interfaces(&ifaces);
                for (_, _, bcast) in &keep {
                    targets.insert(SocketAddr::new(IpAddr::V4(*bcast), disc_port));
                }
                plan.per_iface = keep;
                plan.skipped = skip;
                plan.derived_bcast = derived;
            }
            Err(e) => {
                // 拿不到网卡信息时**退回猜掩码**：少发广播 = 在某些网络里
                // 彻底搜不到设备，那比多发几个包严重得多。
                // 这条路径会打 warn，因为它意味着"我们不知道自己在哪些网上"。
                plan.fell_back = true;
                warn!(
                    "查询网卡信息失败（{}），退回按 /16 /20 /24 各发一个广播（包数会明显变多）",
                    e
                );
                if let Ok(list) = local_ip_address::list_afinet_netifas() {
                    for (_name, ip) in list {
                        let IpAddr::V4(v4) = ip else { continue };
                        if v4.is_loopback() || v4.is_link_local() {
                            continue;
                        }
                        let o = v4.octets();
                        for prefix in SUBNET_PREFIXES {
                            let mask: u32 = u32::MAX << (32 - prefix as u32);
                            let net = u32::from_be_bytes(o) & mask;
                            let bcast = net | !mask;
                            targets.insert(SocketAddr::new(
                                IpAddr::V4(Ipv4Addr::from(bcast)),
                                disc_port,
                            ));
                        }
                    }
                }
            }
        }

        // 4. 历史受信设备 IP 单播 (DHCP 换 IP 后靠这条自动找回)
        for ip_str in known_ips {
            if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                targets.insert(SocketAddr::new(ip, disc_port));
            }
        }

        plan.targets = targets.into_iter().collect();
        plan
    }

    /// 本实例实际绑定的发现端口。
    ///
    /// 必须用真实端口而不是配置值: 万一绑定失败回退到临时端口(或对端
    /// 配的是别的端口), 用配置端口单播就永远发不到对方, 表现为
    /// "已连接"却始终搜不到设备。
    async fn reply_port(&self) -> u16 {
        // 取真实绑定端口, 拿不到再退回配置值
        if let Some(s) = self.socket.read().await.as_ref() {
            if let Ok(addr) = s.local_addr() {
                return addr.port();
            }
        }
        self.config.read().await.discovery_port
    }

    /// 主动探测某个 IP: 发一条单播信标并**真正等待对方应答**。
    /// 旧实现只是单向发包就 return Ok(())，而命令层却恒定返回 true，
    /// 于是 UI 上"已识别设备"永远是一句空话。
    pub async fn probe_ip(&self, ip: &str) -> Result<DiscoveredDevice> {
        let target_ip: IpAddr = ip
            .trim()
            .parse()
            .map_err(|_| FeisuoError::Network(format!("IP 格式非法: {}", ip)))?;
        // 回环地址是允许的: 接收端会按 device_id 过滤掉自己那份,
        // 因此探测 127.0.0.1 返回的一定是"另一个进程里的节点"。
        // 拒绝它会让同机联调与自动化测试无从下手。

        let disc_port = self.config.read().await.discovery_port;
        let (waiter_tx, mut waiter_rx) = broadcast::channel(4);

        let packet = self.build_beacon().await;
        {
            let mut waiters = self.probe_waiters.lock().unwrap_or_else(|e| e.into_inner());
            waiters.insert(target_ip.to_string(), waiter_tx);
        }

        let sock = {
            let guard = self.socket.read().await;
            match guard.as_ref() {
                Some(s) => Some(s.clone()),
                None => None,
            }
        };
        let sock = match sock {
            Some(s) => s,
            None => {
                let mut waiters = self.probe_waiters.lock().unwrap_or_else(|e| e.into_inner());
                waiters.remove(&target_ip.to_string());
                return Err(FeisuoError::Network(
                    "局域网发现服务尚未就绪, 请稍后再试".into(),
                ));
            }
        };

        let target = SocketAddr::new(target_ip, self.reply_port().await);
        if let Ok(bytes) = serde_json::to_vec(&packet) {
            let _ = sock.send_to(&bytes, target).await;
        }
        info!("Discovery unicast probe sent to {}", target);

        // 最多等 1.2 秒: 对端正常会在几十毫秒内回信
        let result = tokio::time::timeout(
            Duration::from_millis(PROBE_WAIT_MILLIS),
            waiter_rx.recv(),
        )
        .await;

        {
            let mut waiters = self.probe_waiters.lock().unwrap_or_else(|e| e.into_inner());
            waiters.remove(&target_ip.to_string());
        }

        match result {
            Ok(Ok(dev)) => Ok(dev),
            _ => Err(FeisuoError::Network(format!(
                "未能发现设备 {}: 无应答 (请确认对方已启动飞梭, 且防火墙未拦截 UDP {})",
                ip, disc_port
            ))),
        }
    }

    pub async fn get_online_devices(&self) -> Vec<DiscoveredDevice> {
        let map = self.devices.read().await;
        let mut list: Vec<DiscoveredDevice> = map.values().cloned().collect();
        list.sort_by(|a, b| {
            b.is_trusted
                .cmp(&a.is_trusted)
                .then_with(|| a.device_name.cmp(&b.device_name))
        });
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 守卫：`take_window` 取到的必须是**真实值**，且四项明细不得被清零。
    ///
    /// ## 它在防什么
    ///
    /// 30 秒摘要那一行曾经这样写（`core/src/discovery/mod.rs`）：
    /// ```text
    /// 拒绝 {g(blocked) + g(sig_fail) + g(self_or_ver) + g(parse_err)}
    ///      （拉黑 {g(blocked)} / 签名或公钥 {g(sig_fail)} / …）
    /// ```
    /// 同样这四个计数器被 `swap(0)` 了**两次** —— 求和那一次已经归零，
    /// 明细那一次读到的必然是 0。于是日志里恒是
    /// `拒绝 70（拉黑 0 / 签名或公钥 0 / 自身或版本 0 / 解析失败 0）`，
    /// **四项恒为零，与真实拒绝原因毫无关系**。
    ///
    /// 而它恰恰是唯一能回答"包到了却被拒，到底拒在哪一步"的仪表 ——
    /// 它的静默失效正是"两台机器永久互信却传不了"长期查不出原因的原因。
    ///
    /// 用一串**互不相同**的非零值是关键：全用同一个值（比如都填 1）的话，
    /// 求和与明细偶然一致，这个守卫就测不出问题。
    #[test]
    fn window_snapshot_keeps_real_values_and_resets() {
        use std::sync::atomic::Ordering as O;
        let s = DiscoveryStats::default();
        // 刻意互不相同, 且刻意让"三项之和"不等于其中任何一项。
        // 数值必须**满足恒等式** 收 = 采纳 + 三项之和, 否则下面那条断言
        // 报的是我编的数据不对, 而不是代码不对 —— 守卫必须只在代码
        // 出问题时才红。
        s.received.fetch_add(111, O::Relaxed);
        s.accepted.fetch_add(41, O::Relaxed);
        s.throttled.fetch_add(31, O::Relaxed);
        s.sig_fail.fetch_add(34, O::Relaxed);
        s.self_or_ver.fetch_add(5, O::Relaxed);
        s.parse_err.fetch_add(31, O::Relaxed); // 41+34+5+31 = 111
        s.replied.fetch_add(41, O::Relaxed);

        let w = s.take_window();

        // 明细必须原样保留 —— 这一条直接对应"各项恒为 0"那个缺陷
        assert_eq!(w.sig_fail, 34, "签名明细被读成 0: 计数器被读了两次");
        assert_eq!(w.self_or_ver, 5, "自身明细被读成 0: 计数器被读了两次");
        assert_eq!(w.parse_err, 31, "解析明细被读成 0: 计数器被读了两次");
        assert_eq!(w.rejected(), 34 + 5 + 31);
        assert_eq!(w.accepted, 41);
        assert_eq!(w.throttled, 31);
        assert_eq!(w.replied, 41);

        // 恒等式: 收 = 采纳 + 拒绝。明细一旦失真, 这条立刻不成立
        assert_eq!(w.received, w.accepted + w.rejected(), "收 ≠ 采纳 + 拒绝");

        // 节流是采纳的子集, 不能超过采纳
        assert!(w.throttled <= w.accepted);

        // 取增量必须真的清零, 否则下一轮会把上一轮的量重复计入
        let second = s.take_window();
        assert_eq!(second, DiscoveryWindow::default(), "take_window 必须清零");
    }

    /// 守卫：写入侧与读取侧的配平 —— 每个包必须落在**恰好一个**桶里。
    ///
    /// 上面那条守住"明细不为零"，这条守住"桶不重不漏"。
    /// 两者缺一不可：只有明细非零也可能算重（同一个包进了两个桶），
    /// 那会让 `收 = 采纳 + 拒绝` 这条恒等式失真。
    #[test]
    fn each_packet_lands_in_exactly_one_bucket() {
        use std::sync::atomic::Ordering as O;
        let s = DiscoveryStats::default();
        // 模拟 10 个包：2 个自身回声、3 个解析失败、1 个拉黑、
        // 4 个采纳（其中 3 个被节流）。
        for _ in 0..2 {
            s.received.fetch_add(1, O::Relaxed);
            s.self_or_ver.fetch_add(1, O::Relaxed);
        }
        for _ in 0..4 {
            s.received.fetch_add(1, O::Relaxed);
            s.parse_err.fetch_add(1, O::Relaxed);
        }
        for _ in 0..4 {
            s.received.fetch_add(1, O::Relaxed);
            s.accepted.fetch_add(1, O::Relaxed);
        }
        for _ in 0..3 {
            s.throttled.fetch_add(1, O::Relaxed);
        }

        let w = s.take_window();
        assert_eq!(w.received, 10);
        assert_eq!(w.accepted, 4);
        assert_eq!(w.rejected(), 2 + 4);
        assert_eq!(w.accepted + w.rejected(), w.received);
        // 采纳里的"新采纳"不能再被算成拒绝
        assert!(w.accepted >= w.throttled);
    }

    /// 守卫 + **实机现状输出**（`cargo test -- --nocapture` 就能看到）。
    ///
    /// 断言只有两条，但都是不可退化的：
    ///   1. 保留的每张网卡都有**恰好一个**广播地址（早先每网卡发 3 个）；
    ///   2. 链路本地（169.254/16）一张都不保留。
    /// 第 2 条是本轮减包量的主要来源，而它**不可能**导致"搜不到设备"——
    /// 169.254 按定义是单机链路（APIPA），上面不存在别的设备。
    #[test]
    fn interfaces_are_filtered_and_narrowed() {
        let ifaces = match if_addrs::get_if_addrs() {
            Ok(v) => v,
            Err(e) => {
                // 平台查询失败时生产代码会退回猜掩码的旧路径，不该在这里 panic
                eprintln!("跳过：本机网卡枚举失败（{}）", e);
                return;
            }
        };
        let (keep, skip, derived) = plan_interfaces(&ifaces);

        eprintln!("—— 本机实际发送计划 ——");
        eprintln!("网卡总数（含回环/IPv6）: {}", ifaces.len());
        for (name, addr, bcast) in &keep {
            eprintln!("  发送 {}  {}  ->  {}", name, addr, bcast);
        }
        for (name, why) in &skip {
            eprintln!("  跳过 {}：{}", name, why);
        }
        eprintln!(
            "结论：定向广播 {} 个",
            keep.len()
        );
        if !derived.is_empty() {
            eprintln!(
                "其中 {} 张的广播地址由 netmask 推算（系统未提供）: {}",
                derived.len(),
                derived.join(", ")
            );
        }
        // 用**旧路径真实枚举一次**来做对比，而不是拿 `keep.len() * 3` 估算 ——
        // 旧路径走 `local_ip_address`，它**能看到**链路本地网卡（`if-addrs`
        // 看不到），所以真实基数比"保留数 × 3"大。拿估算当对比会得出
        // 一个偏乐观的数字，而这类"改进了多少"的结论最不能糊。
        if let Ok(old) = local_ip_address::list_afinet_netifas() {
            let old_count = old
                .iter()
                .filter(|(_, ip)| match ip {
                    IpAddr::V4(v4) => !v4.is_loopback(),
                    IpAddr::V6(_) => false,
                })
                .count()
                * SUBNET_PREFIXES.len();
            eprintln!(
                "对比：旧路径（local_ip_address + 猜 /16 /20 /24）是 {} 个，\
                 降为 {} 个（-{:.0}%）",
                old_count,
                keep.len(),
                100.0 * (1.0 - keep.len() as f64 / old_count.max(1) as f64)
            );
        }

        for (name, addr, bcast) in &keep {
            assert!(
                !addr.split('/').next().unwrap().starts_with("169.254."),
                "链路本地地址必须被跳过，却出现在保留列表: {} {}",
                name,
                addr
            );
            assert!(
                !bcast.to_string().starts_with("169.254."),
                "链路本地不该产生广播目标: {}",
                bcast
            );
        }
        // 每张保留的网卡**恰好一个**广播地址 —— 由 plan_interfaces 的
        // 返回结构（不是 Vec<Vec>）本身保证，这里只是把意图钉成断言。
        assert_eq!(
            keep.len(),
            keep.iter().collect::<std::collections::HashSet<_>>().len(),
            "同一张网卡不应产生多个广播目标"
        );
    }
}