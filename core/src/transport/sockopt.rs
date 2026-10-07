//! 链路容量调优（§9.2）：显式 socket buffer + BDP 估算。
//!
//! # 为什么必须显式设置，而不是交给系统自动扩缩
//!
//! 用户的实际场景是**跨地域 ZeroTier 覆盖网**：本机实测宽带
//! 上行 3.5 MB/s、下行 2.74 MB/s，而 ZeroTier RTT 约 4 ms（局域网内）。
//! 跨地域时 RTT 会涨到 50~250 ms。
//!
//! 带宽时延积 `BDP = 带宽 × RTT`：
//! - 25 Mbps × 4 ms ≈ **12 KB** —— 内核默认缓冲区绰绰有余；
//! - 25 Mbps × 200 ms ≈ **625 KB** —— 默认值根本撑不住，发送窗口反复耗尽，
//!   表现为"带宽明明够但速度上不去"。
//!
//! Windows 的 `SO_SNDBUF`/`SO_RCVBUF` 自动扩缩逻辑（`Tcpip` 里的
//! `MaxSockBuf`）**不会**为单个连接按需扩张到 BDP 量级；Linux 的
//! `tcp_wmem`/`tcp_rmem` 也只是全局上限。所以两条路都走不通。
//!
//! 结论：两端都显式设一个足够大的 buffer，并**把实际生效值记进诊断日志**。
//!
//! # 为什么不做传输层加密（§9.2 D3 决策）
//!
//! 方案在设计文档里完整保留（分块 AEAD），本期不实现。理由是主场景走
//! ZeroTier，ZT 自身已加密，再套一层只在"ZT 中继节点"这一个威胁模型下
//! 有意义，而它带来的 CPU 成本（每 4 MiB 一次 AEAD）会直接压低吞吐 ——
//! 对一个目标是"链路利用率 ≥ 70%"的工具是净负收益。

use std::time::Duration;
use tokio::net::TcpStream;

use crate::transport::diagnostics::LinkProfile;

/// 默认发送/接收缓冲区下限。低于这个值不值得走系统调用。
pub const MIN_SOCK_BUF: usize = 256 * 1024;
/// 上限。超过这个值的收益趋近于零，而内存占用线性增长。
///
/// 取 4 MiB：25 Mbps × 200 ms 的 BDP 是 625 KB，4 MiB 已有 6 倍余量，
/// 足以覆盖 1 Gbps × 30 ms 级别的链路。
pub const MAX_SOCK_BUF: usize = 4 * 1024 * 1024;

/// BDP 估算结果。**全部字段都进诊断日志**——事后要能回答
/// "这次传输的缓冲区是不是给小了"。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BdpEstimate {
    /// 估算的 RTT（毫秒）
    pub rtt_ms: u64,
    /// 估算的链路带宽（bps）。拿不到时为 0。
    pub bandwidth_bps: u64,
    /// 估算的带宽时延积（字节）
    pub bdp_bytes: u64,
    /// 实际请求设置的 buffer 大小
    pub requested: usize,
    /// **实际生效**的 buffer 大小。
    ///
    /// 刻意与 `requested` 分开：Windows 的 `SO_SNDBUF` 语义是
    /// "**禁用**自动扩缩后固定为该值"，而 Linux 会 ×2 计入簿记开销。
    /// 两者都可能与请求值不同，不实测就会把"设了 4 MB"当成事实。
    pub effective: Option<usize>,
    /// 生效值是否小于 BDP（= 缓冲区不足，速度会受限）
    pub under_bdp: bool,
}

impl BdpEstimate {
    /// 人话描述，直接写进日志。
    pub fn describe(&self) -> String {
        format!(
            "BDP: rtt={}ms 带宽={} BDP={} 申请={}KB 生效={} 不足BDP={}",
            self.rtt_ms,
            if self.bandwidth_bps > 0 {
                format!("{:.1} Mbps", self.bandwidth_bps as f64 / 1e6)
            } else {
                "未知".into()
            },
            self.bdp_bytes / 1024,
            self.requested / 1024,
            self.effective
                .map(|v| format!("{}KB", v / 1024))
                .unwrap_or_else(|| "读取失败".into()),
            self.under_bdp
        )
    }
}

/// 由链路画像估算应当设置的缓冲区大小。
///
/// ## 带宽从哪来
///
/// 拿不到可靠带宽时**不猜**。理由：猜错的后果不对称 ——
/// - 猜**小**了：缓冲区不足，吞吐被压住（真实损失）；
/// - 猜**大**了：内存浪费（4 MiB/连接，局域网内可能同时有几十条连接）。
///
/// 所以只在"已知 RTT"时按 `MIN_SOCK_BUF` 起步，并且
/// **允许通过 `bandwidth_bps` 显式传入**（诊断模块在传输中途采样到真实
/// 吞吐后可以二次调大）。
pub fn estimate_buffer(rtt_ms: u64, bandwidth_bps: u64) -> usize {
    if rtt_ms == 0 {
        return MIN_SOCK_BUF;
    }
    let bdp = if bandwidth_bps > 0 {
        // bps * ms / 8 / 1000 = 字节
        (bandwidth_bps as u128 * rtt_ms as u128 / 8 / 1000) as u64
    } else {
        0
    };
    if bdp == 0 {
        return MIN_SOCK_BUF;
    }
    // 2 倍余量：覆盖丢包重传与速率波动。
    // 不留余量的话，一次 RTT 抖动就会让窗口重新收缩。
    let want = (bdp * 2).max(MIN_SOCK_BUF as u64);
    want.min(MAX_SOCK_BUF as u64) as usize
}

/// 对一条**已建立**的连接设置显式 buffer，并返回实测生效值。
///
/// ## 为什么取所有权再还回来
///
/// tokio 的 `TcpStream` **没有 `as_std()`**（只有消费式的 `into_std()`），
/// 而 `socket2::SockRef` 需要一个 `&std::net::TcpStream`。所以只能
/// `into_std()` → 设置 → `from_std()` 转回来。
///
/// `into_std` 会把 nonblocking 模式原样保留，`from_std` 立刻转回异步，
/// 中间不会丢数据（此刻还没有任何数据在途 —— 必须在 `connect` 之后、
/// 发送第一个字节之前调用）。
///
/// **读取失败不算错误**：某些平台（如 Android 的部分内核）不允许回读，
/// 此时返回 `None`，上层只记"已请求"，不谎称"已生效"。
///
/// ## 唯一的硬错误：`into_std` 失败
///
/// 它只在 socket 已被关闭时失败。此时**没有任何"继续用"的选项** ——
/// 编一个假的 std socket 顶上，只会把一个明确的错误变成后面某个
/// 莫名其妙的读写失败。所以直接返回 [`FeisuoError`]，让上层如实报错。
pub fn tune_stream(
    stream: TcpStream,
    requested: usize,
) -> crate::error::Result<(TcpStream, BdpEstimate)> {
    let requested = requested.clamp(MIN_SOCK_BUF, MAX_SOCK_BUF);
    let _ = stream.set_nodelay(true);

    let std_stream = stream
        .into_std()
        .map_err(|e| crate::error::FeisuoError::Io(e))?;
    let sock = socket2::SockRef::from(&std_stream);

    // 读侧在写侧之前设：读缓冲不足会先于写缓冲表现为吞吐塌陷，
    // 顺序反过来会让排查时先怀疑错的那一侧。
    let rcv_eff = set_buf_opt(&sock, requested, Side::Recv);
    let snd_eff = set_buf_opt(&sock, requested, Side::Send);
    // 顺带打开 keepalive：覆盖网（ZeroTier）在 NAT 上可能静默失效连接，
    // 没有 keepalive 时表现为"传输卡在某一跳不动"，且两端都不知道。
    // idle 设 30s：远大于任何正常的传输间隙，小于 NAT 典型 5 分钟超时。
    let _ = sock.set_tcp_keepalive(
        &socket2::TcpKeepalive::new().with_time(Duration::from_secs(30)),
    );
    drop(sock);

    // 取两侧较小值作为"生效值"：有效吞吐由**较小**的那一侧决定。
    let effective = match (rcv_eff, snd_eff) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let stream = TcpStream::from_std(std_stream)
        .map_err(|e| crate::error::FeisuoError::Io(e))?;
    Ok((
        stream,
        BdpEstimate {
            requested,
            effective,
            ..Default::default()
        },
    ))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Side {
    Send,
    Recv,
}

/// 设置一侧的 buffer 并**回读实际生效值**。
///
/// 回读不是可选的严谨性：Windows 的 `SO_SNDBUF` 语义是"设了就禁用自动
/// 扩缩"，而 Linux 会把簿记开销一并计入（设 1 MB 回读可能得 2 MB+）。
/// 不实测就等于在日志里说谎。
fn set_buf_opt(sock: &socket2::SockRef<'_>, size: usize, side: Side) -> Option<usize> {
    let set_res = match side {
        Side::Send => sock.set_send_buffer_size(size),
        Side::Recv => sock.set_recv_buffer_size(size),
    };
    if let Err(e) = set_res {
        // 设置失败不致命：内核默认值通常也能跑，只是可能慢。
        // 记 debug 而不是 warn —— warn 会在正常链路上刷屏。
        tracing::debug!("设置 socket buffer 失败 ({:?}, {}B): {}", side, size, e);
        return None;
    }
    match side {
        Side::Send => sock.send_buffer_size(),
        Side::Recv => sock.recv_buffer_size(),
    }
    .ok()
}

/// 传输中途**重新**调大 buffer。
///
/// ## 存在的理由
///
/// 建连时并不知道带宽 —— `estimate_buffer` 只能按 RTT 猜。这条路径让
/// 诊断模块在采样到真实吞吐后（`ThroughputStats`）二次调优：
///
/// ```text
/// 传输 3 秒后测得 2.1 MB/s, RTT 180ms
///   => BDP = 2.1e6 × 0.18 = 378 KB, 申请 756 KB
///   => 从 MIN_SOCK_BUF(256KB) 调大到 756KB
/// ```
///
/// ## 为什么返回值而不是 `&mut`
///
/// 早先的签名是 `regrow_stream(stream, &mut current, ...)`，函数体末尾
/// 用 `*current = BdpEstimate { .. }` 整体覆盖。这让调用方的
/// "上一次的值" 在数据流分析里变成**死赋值** —— 编译器直接警告
/// "value assigned is never read"。
///
/// 那条警告是对的：这个写法让"旧值"除了比较 `requested` 之外毫无用处，
/// 却又在类型上假装是状态延续。改成 `(stream, BdpEstimate)` 返回新值后，
/// "旧值 → 新值"的数据流是显式的。
///
/// 只增不减：传输中途缩小 buffer 会让已排队的数据被截断式重传，
/// 代价远大于收益。`None` = 不需要调大（带宽未知或已经够用）。
pub fn regrow_stream(
    stream: TcpStream,
    current: &BdpEstimate,
    rtt_ms: u64,
    measured_bps: u64,
) -> crate::error::Result<(TcpStream, Option<BdpEstimate>)> {
    if measured_bps == 0 || rtt_ms == 0 {
        return Ok((stream, None));
    }
    let bdp = (measured_bps as u128 * rtt_ms as u128 / 8 / 1000) as u64;
    let want = (bdp * 2).max(MIN_SOCK_BUF as u64).min(MAX_SOCK_BUF as u64) as usize;
    if want <= current.requested {
        return Ok((stream, None));
    }
    let (stream, next) = tune_stream(stream, want)?;
    tracing::info!(
        "socket buffer 二次调优: {}KB -> {}KB (生效 {}KB); bdp={}KB rtt={}ms 实测={:.2}MB/s",
        current.requested / 1024,
        want / 1024,
        next.effective.map(|v| v / 1024).unwrap_or(0),
        bdp / 1024,
        rtt_ms,
        measured_bps as f64 / 1e6,
    );
    Ok((
        stream,
        Some(BdpEstimate {
            rtt_ms,
            bandwidth_bps: measured_bps,
            bdp_bytes: bdp,
            requested: want,
            effective: next.effective,
            under_bdp: next.effective.map(|eff| (eff as u64) < bdp).unwrap_or(false),
        }),
    ))
}

/// 从 `LinkProfile` 估 RTT。
///
/// TCP 建连耗时是 RTT 的**下界**（1.5 个 RTT 左右），所以除以 2 后
/// 仍偏保守 —— 对"缓冲区宁可大一点"的决策方向是正确的。
pub fn rtt_from_profile(profile: &LinkProfile) -> u64 {
    if profile.tcp_connect_ms == 0 {
        return 0;
    }
    (profile.tcp_connect_ms / 2).max(1)
}

/// 建连后的统一调优入口：估算 → 设置 → 回填诊断 → 记日志。
pub fn tune_and_log(
    stream: TcpStream,
    local_ip: &str,
    peer_ip: &str,
    connect_ms: u64,
) -> crate::error::Result<(TcpStream, BdpEstimate)> {
    let profile = LinkProfile::infer(local_ip, peer_ip, connect_ms);
    let rtt_ms = rtt_from_profile(&profile);
    let size = estimate_buffer(rtt_ms, 0);
    let (stream, mut est) = tune_stream(stream, size)?;
    est.rtt_ms = rtt_ms;
    // 带宽未知时 BDP 留 0 —— 不拿"猜的带宽 × RTT"当事实写进日志。
    // 真实的 BDP 由 [`regrow_stream`] 在采样到吞吐后补上。
    est.bdp_bytes = 0;
    est.under_bdp = false;
    tracing::info!(
        "链路={} | {}",
        profile.describe(),
        est.describe()
    );
    Ok((stream, est))
}

/// 供 UI / 文档引用的建议值表（不做任何自动决策，只做展示）。
pub fn buffer_preset_table() -> Vec<(&'static str, usize)> {
    vec![
        ("RTT < 1ms（同局域网）", MIN_SOCK_BUF),
        ("RTT 20ms（同城）", estimate_buffer(20, 0)),
        ("RTT 100ms（跨省）", estimate_buffer(100, 0)),
        ("RTT 250ms（跨国）", estimate_buffer(250, 0)),
    ]
}

/// 把 BDP 估算挂到一次 IO 超时上（便于统一诊断口径）。
pub fn bdp_to_duration(est: &BdpEstimate) -> Duration {
    // 缓冲区越大越需要更长的 IO 超时：4 MiB 缓冲区在 2 Mbps 的链路上
    // 填满就要 16 秒，用默认超时会在正常传输中途被误杀。
    let base = est.requested as u64 / 1024; // KB
    Duration::from_secs((base / 512).clamp(10, 120))
}
