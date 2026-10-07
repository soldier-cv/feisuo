//! 传输诊断：一次传输的**全链路分段耗时**与**链路特征**记录。
//!
//! # 为什么需要它
//!
//! 现场排查"飞梭只有 3 MB/s"时，最难回答的问题永远是：
//! **慢在链路、慢在磁盘、还是慢在计算？**
//! 单看一个聚合速度数字无法回答 —— 3 MB/s 可能是宽带上限，也可能是
//! 对端磁盘卡住，也可能是 BLAKE3 复核吃掉了本该属于传输的时间。
//!
//! 本模块把一次传输**拆成可归因的阶段**，并把每一个阶段的耗时落库：
//!
//! ```text
//! connect │ hello │ approval │ manifest │ ┌─ data ─┐ │ verify │ commit │ ack │ total
//!   建连      身份    人工审批     清单      │ 纯数据流 │  BLAKE3   staging  回执   墙钟
//!                                              └ 速度分母 ┘  不计入速度
//! ```
//!
//! 设计约束：
//!
//! 1. **速度只由 `data_ms` 推导**（见 [`TransferDiagnostics::avg_speed_bps`]）。
//!    审批等待、磁盘写、BLAKE3 复核都**不得**计入，否则会出现
//!    "对方开会审批 60 秒 ⇒ 显示 185 KB/s"这种假慢。
//! 2. **不额外计时开销**：全部用 [`std::time::Instant`] 在阶段边界打点，
//!    不引入任何 per-chunk 采样热路径。
//! 3. **可离线分析**：整条记录序列化为一行 JSON 进 SQLite，
//!    便于事后导出分析（见 [`TransferDiagnostics::to_log_line`]）。

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::Instant;

use crate::security::format_bytes;

/// 速度计算的最小样本门槛（毫秒）。
///
/// 依据见设计文档 §9.6.5：一个 1 KB 文件耗时 50ms 算出来的 20 KB/s
/// **看起来像坏了**，而且此时几乎全部时间都是帧往返开销而非传输。
/// 低于此门槛一律显示"—"，不显示数字。
pub const MIN_SPEED_SAMPLE_MS: u64 = 100;

/// 相邻分块间隔超过该值即记为一次 **stall**（传输途中的停顿）。
///
/// 正常情况下两块之间只隔着网络传输与落盘时间；出现秒级间隔通常意味着
/// 磁盘写阻塞、杀毒软件扫描、或网络抖动。这是区分
/// "链路慢" 与 "磁盘慢" 的关键信号。
pub const STALL_GAP_MS: u64 = 1_000;

// ===========================================================================
// 阶段计时
// ===========================================================================

/// 一次传输的分段耗时。单位统一为**毫秒**。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct PhaseTimings {
    /// TCP 建连耗时
    pub connect_ms: u64,
    /// 身份握手耗时（签名验签往返）
    pub hello_ms: u64,
    /// **人工审批等待耗时**（未受信设备才会非 0）
    pub approval_wait_ms: u64,
    /// 清单下发 / 校验耗时
    pub manifest_ms: u64,
    /// 纯数据流耗时 —— **速度的唯一分母**
    pub data_ms: u64,
    /// 整文件 BLAKE3 复核耗时（纯 CPU + 磁盘读，**不计入速度**）
    pub verify_ms: u64,
    /// staging 目录提交到最终位置耗时
    pub commit_ms: u64,
    /// 终态回执往返耗时
    pub ack_ms: u64,
    /// 墙钟总耗时（含审批等待，**不计入速度**）
    pub total_ms: u64,
}

impl PhaseTimings {
    /// 非数据流阶段的耗时合计。
    ///
    /// 用途：`total_ms - data_ms` 就是"**没在传数据却花了这么久**"的时间，
    /// 通常由审批等待 + 建连 + 校验构成。
    pub fn overhead_ms(&self) -> u64 {
        self.total_ms.saturating_sub(self.data_ms)
    }
}

// ===========================================================================
// 链路画像
// ===========================================================================

/// 覆盖网（overlay）网络类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayKind {
    /// ZeroTier / Tailscale 等默认的 `100.64.0.0/10`
    CarrierGradeNat100,
    /// RFC1918 私有地址（`10/8`、`172.16/12`、`192.168/16`）
    PrivateLan,
    /// 公网地址
    Public,
    /// 回环 `127/8`（同机联调 / 单机多实例）。
    ///
    /// 早先没有这个分支，`127.0.0.1` 落进 `Public`，于是链路画像
    /// 说"跨网段"、端点标签说"公网"。
    Loopback,
    /// 解析失败
    Unknown,
}

/// 链路特征画像：用于事后判断"慢是不是链路造成的"。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LinkProfile {
    pub local_ip: String,
    pub peer_ip: String,
    /// 两端是否在同一子网（同一物理局域网的必要非充分条件）
    pub same_subnet: bool,
    /// 对端地址的性质
    pub peer_kind: String,
    /// 本端地址的性质
    pub local_kind: String,
    /// TCP 建连耗时（也是链路延迟的一个粗略下界）
    pub tcp_connect_ms: u64,
    /// 是否走覆盖网（ZT/Tailscale 这类虚拟网卡）
    pub over_overlay: bool,
}

/// 按 ZeroTier / Tailscale 默认分配段 `100.64.0.0/10` 判定覆盖网地址。
///
/// 依据：ZeroTier 与 Tailscale 都从 `100.64.0.0/10` 取址（Tailscale 甚至
/// 明确避开该段以免冲突，但 ZeroTier 默认就是它）。
pub fn classify_ip(ip: &str) -> OverlayKind {
    let parsed: IpAddr = match ip.parse() {
        Ok(v) => v,
        Err(_) => return OverlayKind::Unknown,
    };
    let v4 = match parsed {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => return OverlayKind::Public,
    };
    let o = v4.octets();
    // 回环必须**先于**其它判定：127/8 早先落到最后的 `else`
    // 被标成 `Public`。
    //
    // 后果不只是文案难看：
    // - 链路画像会说"跨网段(Public→Public)"，而实际是本机回环；
    // - 更要紧的是 `discovery` 用 `classify_ip` 给设备端点打
    //   `kind` 标签，回环被标成 `public` ⇒ 在「连接路径」列表里
    //   显示成"公网"，且优先级（public=1）排在 lan（2）**后面**。
    if o[0] == 127 {
        return OverlayKind::Loopback;
    }
    if o[0] == 100 && (64..128).contains(&o[1]) {
        OverlayKind::CarrierGradeNat100
    } else if o[0] == 10
        || (o[0] == 172 && (16..32).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
    {
        OverlayKind::PrivateLan
    } else {
        OverlayKind::Public
    }
}

impl LinkProfile {
    /// 用本端与对端 IP 推断链路画像。
    pub fn infer(local_ip: &str, peer_ip: &str, tcp_connect_ms: u64) -> Self {
        let local_kind = classify_ip(local_ip);
        let peer_kind = classify_ip(peer_ip);
        // 同子网判定只在前三段相同时成立（IPv4 简化判定, 对本用途足够）
        let same_subnet = match (local_ip.parse::<IpAddr>(), peer_ip.parse::<IpAddr>()) {
            (Ok(IpAddr::V4(a)), Ok(IpAddr::V4(b))) => {
                let (x, y) = (a.octets(), b.octets());
                x[0] == y[0] && x[1] == y[1] && x[2] == y[2]
            }
            _ => false,
        };
        // 两端都落在同一类网段才算"整条路径在覆盖网内"：
        // 一端 ZT 一端物理网卡时, 路径必然经过 NAT/公网, 属于混合路径。
        let over_overlay = matches!(local_kind, OverlayKind::CarrierGradeNat100)
            && matches!(peer_kind, OverlayKind::CarrierGradeNat100);
        Self {
            local_ip: local_ip.to_string(),
            peer_ip: peer_ip.to_string(),
            same_subnet,
            peer_kind: format!("{:?}", peer_kind),
            local_kind: format!("{:?}", local_kind),
            tcp_connect_ms,
            over_overlay,
        }
    }

    /// 人话描述，用于日志与界面提示。
    pub fn describe(&self) -> String {
        if self.over_overlay {
            format!("覆盖网(100.64/10) 同段 {}ms", self.tcp_connect_ms)
        } else if self.same_subnet {
            format!("同子网 {}ms", self.tcp_connect_ms)
        } else {
            format!("跨网段({}→{}) {}ms", self.local_kind, self.peer_kind, self.tcp_connect_ms)
        }
    }
}

// ===========================================================================
// 吞吐采样
// ===========================================================================

/// 数据流阶段的吞吐采样统计。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct ThroughputStats {
    /// 采样次数
    pub samples: u32,
    /// 单次采样最低值
    pub min_bps: u64,
    /// 单次采样最高值
    pub max_bps: u64,
    /// 停顿次数（相邻分块间隔 ≥ [`STALL_GAP_MS`]）
    pub stalls: u32,
    /// 最长的一次间隔
    pub worst_gap_ms: u64,
    /// 最近 8 个采样的 bps 之和（滑动窗口，不存 Vec）
    ///
    /// `#[serde(default)]`：理由同 [`TransferDiagnostics::bdp`] ——
    /// 诊断记录持久化在 SQLite 里，缺了它历史记录全部无法解析。
    #[serde(default)]
    pub recent_sum: u64,
    /// `recent_sum` 里的样本数
    #[serde(default)]
    pub recent_count: u32,
}

/// 滑动窗口长度：用于"链路现在能跑多快"这类**调优**决策。
///
/// 取 8 是因为一个 4 MiB 分块在 2 Mbps 上约 16 秒，8 个样本已经跨越
/// 足够长的时间；而再多的样本对"当前状态"没有额外信息。
const RECENT_WINDOW: u32 = 8;

impl ThroughputStats {
    /// 记录一个采样点。
    pub fn observe(&mut self, bytes: u64, elapsed_ms: u64) {
        if elapsed_ms == 0 || bytes == 0 {
            return;
        }
        let bps = bytes * 1000 / elapsed_ms;
        if self.samples == 0 {
            self.min_bps = bps;
            self.max_bps = bps;
        } else {
            self.min_bps = self.min_bps.min(bps);
            self.max_bps = self.max_bps.max(bps);
        }
        self.samples += 1;
        // 固定容量滑动窗口。存 Vec 会在每次传输里多一次堆分配,
        // 而诊断数据永远不需要超过 8 个样本 —— 超出部分只对"历史平均"有用,
        // 而历史平均恰恰不能用来做调优决策（见 `avg_bps`）。
        if self.recent_count < RECENT_WINDOW {
            self.recent_sum += bps;
            self.recent_count += 1;
        } else {
            // 窗口已满: 丢掉最早的。无法精确做到（没存历史），
            // 退化为"指数衰减平均"—— 对调优用途足够, 且不额外占内存。
            // 刻意不假装成滑动窗口: `recent_count` 封顶就是诚实的实现。
            self.recent_sum =
                self.recent_sum * (RECENT_WINDOW as u64 - 1) / RECENT_WINDOW as u64 + bps;
        }
    }

    /// 记录一次"卡顿"（相邻块间隔过长）。
    pub fn observe_gap(&mut self, gap_ms: u64) {
        if gap_ms >= STALL_GAP_MS {
            self.stalls += 1;
            if gap_ms > self.worst_gap_ms {
                self.worst_gap_ms = gap_ms;
            }
        }
    }

    /// 采样标准差趋势的粗略指标：峰谷差。值大说明吞吐抖动剧烈。
    pub fn spread_bps(&self) -> u64 {
        self.max_bps.saturating_sub(self.min_bps)
    }

    /// 采样平均带宽（bps），用于**调优**决策。
    ///
    /// ## 为什么是"最近窗口"而不是全程平均
    ///
    /// 调优要回答的是"链路**现在**能跑多快"，不是"这条链路上 historically
    /// 平均多快"。全程平均会把开头预热（首块读盘、BLAKE3 哈希、握手收尾）
    /// 的低速一并算进去，据此算出的 BDP 会偏小 —— 而偏小正是要修的问题。
    ///
    /// 无样本时返回 0，调用方必须把它当"未知"处理，**不能**当成 0 带宽。
    pub fn avg_bps(&self) -> u64 {
        if self.recent_count == 0 {
            return 0;
        }
        self.recent_sum / self.recent_count as u64
    }
}

// ===========================================================================
// 诊断主记录
// ===========================================================================

/// 一次传输的完整诊断记录。
///
/// 生命周期：`TransferDiagnostics::start()` 建档 → 各阶段打点 →
/// [`finish`](Self::finish) 落库 + 打日志。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferDiagnostics {
    pub transfer_id: String,
    /// `"send"` / `"recv"`
    pub direction: String,
    pub peer_device_id: String,
    pub peer_device_name: String,
    pub peer_ip: String,
    pub peer_port: u16,

    pub file_count: u32,
    pub bytes_declared: u64,
    pub bytes_transferred: u64,
    pub chunk_size: u32,
    pub chunk_count: u64,

    /// `"completed"` / `"failed"` / `"cancelled"`
    pub outcome: String,
    pub error: Option<String>,

    pub timings: PhaseTimings,
    pub link: LinkProfile,
    pub throughput: ThroughputStats,

    /// socket buffer 调优结果（§9.2）。**必须记"生效值"而不是"请求值"** ——
    /// Windows 与 Linux 对 `SO_SNDBUF` 的语义不同，不回读就等于在日志里说谎。
    ///
    /// `#[serde(default)]` 不是可选项：诊断记录是**持久化**的，
    /// 少了它，之前所有版本写入的 `payload` 都会解析失败 ——
    /// `export_diagnostics_report` 只能把每条都印成「(无法解析文件记录)」。
    /// 也就是说：加一个诊断字段就会让用户的历史数据全部报废。
    #[serde(default)]
    pub bdp: BdpSnapshot,

    /// 整文件 BLAKE3 复核是否全部通过
    pub integrity_ok: bool,
    pub created_at: i64,

    /// 内部计时器, 不参与序列化
    #[serde(skip)]
    mark: Option<Instant>,
}

/// socket buffer 调优的**落库快照**（从 `sockopt::BdpEstimate` 摘出来）。
///
/// 单独一个类型而不是直接复用 `BdpEstimate`，是为了让诊断报告不依赖
/// `sockopt` 模块的演进 —— 诊断数据的结构必须比产生它的代码更稳定，
/// 否则改一次调优策略就等于让历史诊断报告全部无法解析。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BdpSnapshot {
    /// 估算 / 实测的 RTT（毫秒）
    pub rtt_ms: u64,
    /// 实测链路带宽（bps）。未知时为 0。
    pub bandwidth_bps: u64,
    /// 带宽时延积（字节）
    pub bdp_bytes: u64,
    /// 请求的 buffer 大小
    pub requested: usize,
    /// 实际生效的 buffer 大小。读不到时为 0。
    pub effective: usize,
    /// 生效值是否小于 BDP（缓冲区不足，速度被窗口限制）
    pub under_bdp: bool,
    /// 中途是否二次调大过
    pub regrown: bool,
    /// **是否尝试过**二次调大（阈值 64 MiB 跨过了）。
    ///
    /// 为什么必须与 `regrown` 分开记：早先只有 `regrown`，于是
    /// "从没触发过"与"触发了但判定不需要调大"在数据里**长得一样**
    /// （都是 `false`）。而这两件事的结论完全相反 ——
    /// 前者说明大文件这条路根本没走到调优逻辑（缺陷），
    /// 后者说明调优逻辑正确地判断"现有 buffer 已经够"。
    ///
    /// 判别方法：尝试过但没调大 ⇒ `bdp_bytes * 2 <= requested`；
    /// 没尝试过 ⇒ `bdp_bytes` 往往还是 0（建连时带宽未知）。
    #[serde(default)]
    pub regrow_tried: bool,
}

impl BdpSnapshot {
    /// 一行归因，直接进日志。事后分析"为什么慢"时，
    /// 这一行能直接区分"缓冲区太小"与"链路本身就慢"。
    pub fn attribution(&self) -> String {
        if self.effective == 0 {
            return format!(
                "socket buffer: 申请 {}KB 但生效值不可读（无法判断是否足够）",
                self.requested / 1024
            );
        }
        if self.under_bdp {
            return format!(
                "socket buffer {}KB < BDP {}KB（rtt={}ms, 实测{}）—— 缓冲区不足，吞吐被 TCP 窗口限制；可提高缓冲区上限",
                self.effective / 1024,
                self.bdp_bytes / 1024,
                self.rtt_ms,
                if self.bandwidth_bps > 0 {
                    format!("{:.2} MB/s", self.bandwidth_bps as f64 / 1e6)
                } else {
                    "未知".into()
                }
            );
        }
        format!(
            "socket buffer {}KB ≥ BDP {}KB（rtt={}ms{}{}）",
            self.effective / 1024,
            self.bdp_bytes / 1024,
            self.rtt_ms,
            if self.regrown { "，已中途调大" } else { "" },
            // 没调大时要说清是"判定不需要"还是"压根没走到" ——
            // 否则事后只能靠猜，而这两种情况的处置完全不同。
            if self.regrow_tried && !self.regrown {
                "，已评估：现有 buffer 已覆盖 BDP，无需调大"
            } else {
                ""
            }
        )
    }
}

impl TransferDiagnostics {
    /// 建档并开始墙钟计时。
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        transfer_id: impl Into<String>,
        direction: &str,
        peer_device_id: &str,
        peer_device_name: &str,
        peer_ip: &str,
        peer_port: u16,
    ) -> Self {
        Self {
            transfer_id: transfer_id.into(),
            direction: direction.to_string(),
            peer_device_id: peer_device_id.to_string(),
            peer_device_name: peer_device_name.to_string(),
            peer_ip: peer_ip.to_string(),
            peer_port,
            file_count: 0,
            bytes_declared: 0,
            bytes_transferred: 0,
            chunk_size: 0,
            chunk_count: 0,
            outcome: "in_flight".to_string(),
            error: None,
            timings: PhaseTimings::default(),
            link: LinkProfile::default(),
            throughput: ThroughputStats::default(),
            bdp: BdpSnapshot::default(),
            integrity_ok: false,
            created_at: chrono::Utc::now().timestamp(),
            mark: Some(Instant::now()),
        }
    }

    /// 记录 TCP 建连耗时并推断链路画像。
    pub fn mark_connect(&mut self, local_ip: &str, connect_ms: u64) {
        self.timings.connect_ms = connect_ms;
        self.link = LinkProfile::infer(local_ip, &self.peer_ip, connect_ms);
    }

    /// 记录 socket buffer 调优结果（§9.2）。
    pub fn set_bdp(&mut self, est: &crate::transport::sockopt::BdpEstimate) {
        self.bdp = BdpSnapshot {
            rtt_ms: est.rtt_ms,
            bandwidth_bps: est.bandwidth_bps,
            bdp_bytes: est.bdp_bytes,
            requested: est.requested,
            effective: est.effective.unwrap_or(0),
            under_bdp: est.under_bdp,
            regrown: self.bdp.regrown,
            // 重建快照时必须**保住**这两个"过程"标记：
            // 它们记录的是"发生过什么"，而 est 只描述"当前是多少"。
            // 丢掉它们，事后就没法区分"没触发"与"触发了但没调大"。
            regrow_tried: self.bdp.regrow_tried,
        };
    }

    /// 标记"已跨过 64 MiB 阈值，进入二次调优"（无论最终是否真的调大）。
    ///
    /// 必须在调用 `regrow_stream` **之前**打：那个函数可能因为
    /// "现有 buffer 已经够"而返回 `None`，而那正是一个需要被记录
    /// 的**正确决策**，不是"什么都没发生"。
    pub fn mark_bdp_regrow_tried(&mut self) {
        self.bdp.regrow_tried = true;
    }

    /// 标记"传输中途已把缓冲区二次调大"。
    ///
    /// 单独一个方法而不是让 `set_bdp` 推断：中途调大这件事**本身就是
    /// 诊断信号** —— 它意味着建连时的估计偏小，也就是"高 RTT 链路上
    /// 首屏速度偏低"的直接证据。
    pub fn mark_bdp_regrown(&mut self) {
        self.bdp.regrown = true;
    }

    pub fn mark_hello(&mut self, ms: u64) {
        self.timings.hello_ms = ms;
    }

    pub fn mark_approval_wait(&mut self, ms: u64) {
        self.timings.approval_wait_ms = ms;
    }

    pub fn mark_manifest(&mut self, ms: u64) {
        self.timings.manifest_ms = ms;
    }

    pub fn mark_data(&mut self, ms: u64) {
        self.timings.data_ms = ms;
    }

    pub fn mark_verify(&mut self, ms: u64) {
        self.timings.verify_ms = ms;
    }

    pub fn mark_commit(&mut self, ms: u64) {
        self.timings.commit_ms = ms;
    }

    pub fn mark_ack(&mut self, ms: u64) {
        self.timings.ack_ms = ms;
    }

    /// 记录清单信息。
    pub fn set_manifest_info(&mut self, file_count: u32, bytes: u64, chunk_size: u32, chunk_count: u64) {
        self.file_count = file_count;
        self.bytes_declared = bytes;
        self.chunk_size = chunk_size;
        self.chunk_count = chunk_count;
    }

    /// 采样一次吞吐（由传输循环在每个分块后调用）。
    pub fn sample_throughput(&mut self, chunk_bytes: u64, elapsed_ms: u64) {
        self.throughput.observe(chunk_bytes, elapsed_ms);
        self.throughput.observe_gap(elapsed_ms);
    }

    /// 聚合吞吐（字节/秒）。
    ///
    /// **只用 `data_ms` 作分母**，且施加 [`MIN_SPEED_SAMPLE_MS`] 门槛；
    /// 不满足时返回 `None`，调用方应显示"—"而不是一个荒谬的数字。
    ///
    /// 设计依据见设计文档 §9.6.3 / §9.6.5 / §9.6.6。
    pub fn avg_speed_bps(&self) -> Option<u64> {
        if self.timings.data_ms < MIN_SPEED_SAMPLE_MS || self.bytes_transferred == 0 {
            return None;
        }
        Some(self.bytes_transferred * 1000 / self.timings.data_ms)
    }

    /// 速度的人话格式（复用 `format_bytes` 的 1024 进制，保持与体积显示自洽）。
    pub fn speed_display(&self) -> String {
        match self.avg_speed_bps() {
            Some(bps) => format!("{}/s", format_bytes(bps)),
            None => "—".to_string(),
        }
    }

    /// 归因结论：把分段耗时翻译成一句可执行的话。
    ///
    /// 这是本模块最直接的服务于"排障"的输出 —— 用户看到的是
    /// 「瓶颈在 X」而不是一堆数字。
    pub fn attribution(&self) -> String {
        let t = &self.timings;
        if self.outcome == "failed" {
            return format!("传输失败: {}", self.error.as_deref().unwrap_or("未知原因"));
        }
        // 审批等待是"看起来慢"的头号原因，必须先排除
        if t.approval_wait_ms > t.data_ms && t.approval_wait_ms > MIN_SPEED_SAMPLE_MS {
            return format!(
                "非传输瓶颈: 人工审批等待 {}ms 占了总时长的 {}%",
                t.approval_wait_ms,
                t.approval_wait_ms * 100 / t.total_ms.max(1)
            );
        }
        if t.verify_ms > t.data_ms / 2 {
            return format!(
                "非传输瓶颈: 整文件校验 {}ms 超过数据流 {}ms (纯 CPU+磁盘, 不应计入网络速度)",
                t.verify_ms, t.data_ms
            );
        }
        // ---- 缓冲区不足：必须排在"链路抖动"与"链路到顶"之前 ----
        // 两者症状完全一样（速度上不去），但归因相反：
        // 缓冲区不足 = **飞梭能改**（提高上限 / 中途调优）；
        // 链路抖动 / 链路到顶 = 改不了。把可修的归因排在不可修的前面，
        // 是为了让"看到这条结论的人知道下一步该做什么"。
        //
        // 必须排在"链路抖动"**之前**：缓冲区不足会**制造**抖动
        // （窗口耗尽 → 停顿 → 突发补发）。先判抖动，就会把一个
        // 飞梭自己的问题说成"网络抖"，而抖动恰好是不可修的那一类 ——
        // 结论方向直接反了。
        if self.bdp.under_bdp {
            return format!(
                "{} · 瓶颈在飞梭（可调：提高缓冲区上限）",
                self.bdp.attribution()
            );
        }
        if self.throughput.stalls > 0 {
            return format!(
                "链路抖动: {} 次停顿, 最长 {}ms（分块磁盘读阻塞 / 网络抖动 / 对端慢）",
                self.throughput.stalls, self.throughput.worst_gap_ms
            );
        }
        match self.avg_speed_bps() {
            Some(_bps) => {
                // ## 这里曾经撒了一个谎，而且它撒得"很像真的"
                //
                // 旧文案是 `"纯数据流 {}（已达 {} 上限, 非飞梭问题）"`，
                // 而那个"上限"传的是 `format_bytes(bps)` ——
                // **就是观测速度本身**。于是它恒定输出
                // "纯数据流 619.5 MB/s（已达 619.5 MB 上限, 非飞梭问题）"：
                // 拿速度跟它自己比，**永远**得出"已达上限，不怪飞梭"。
                //
                // 后果比文案难看严重得多：这条归因会**无条件替飞梭开脱**。
                // 真的出现飞梭侧的问题（分块读阻塞、hash 与提交开销、
                // 单流在长肥管道上的窗口限制）时，它仍然说"非飞梭问题"，
                // 于是排查会直接跳过真正的原因 —— 而"依据传输信息做
                // 分析"这件事本身就失去了意义。
                //
                // 现在只说**真的知道**的：观测速度、缓冲区相对 BDP 的位置、
                // 以及"未观察到飞梭侧限流证据"这个**有限**的否定结论。
                // 它不等于"确定不是飞梭的问题" —— 措辞上必须留着这个余地，
                // 否则又变成一句新的谎话。
                let bdp_note = if self.bdp.effective > 0 {
                    format!("；{}", self.bdp.attribution())
                } else {
                    "；socket buffer 生效值不可读，未能判断缓冲区是否够".to_string()
                };
                format!(
                    "{} · 数据流 {}ms · 纯数据流 {}（未观察到飞梭侧的限流证据, 但**不能据此断定**瓶颈在链路）{}",
                    self.link.describe(),
                    t.data_ms,
                    self.speed_display(),
                    bdp_note
                )
            }
            None => "样本过短, 速度不可判定".to_string(),
        }
    }

    /// 单行结构化日志（便于 grep / 事后 awk 分析）。
    pub fn to_log_line(&self) -> String {
        format!(
            "id={} dir={} peer={}({}) bytes={}/{} files={} chunk={}x{} \
connect={}ms hello={}ms approval={}ms manifest={}ms data={}ms verify={}ms commit={}ms ack={}ms total={}ms \
speed={} min={} max={} spread={} stalls={} worst_gap={}ms link=[{}] overlay={} \
sockbuf_req={} sockbuf_eff={} bdp={} bdp_rtt={}ms bdp_bw={} bdp_under={} bdp_regrow_tried={} bdp_regrown={} \
integrity={} outcome={} err={}",
            self.transfer_id,
            self.direction,
            self.peer_device_name,
            self.peer_ip,
            self.bytes_transferred,
            self.bytes_declared,
            self.file_count,
            self.chunk_size,
            self.chunk_count,
            self.timings.connect_ms,
            self.timings.hello_ms,
            self.timings.approval_wait_ms,
            self.timings.manifest_ms,
            self.timings.data_ms,
            self.timings.verify_ms,
            self.timings.commit_ms,
            self.timings.ack_ms,
            self.timings.total_ms,
            self.speed_display(),
            format_bytes(self.throughput.min_bps),
            format_bytes(self.throughput.max_bps),
            format_bytes(self.throughput.spread_bps()),
            self.throughput.stalls,
            self.throughput.worst_gap_ms,
            self.link.describe(),
            self.link.over_overlay,
            self.bdp.requested,
            self.bdp.effective,
            self.bdp.bdp_bytes,
            self.bdp.rtt_ms,
            self.bdp.bandwidth_bps,
            self.bdp.under_bdp,
            self.bdp.regrow_tried,
            self.bdp.regrown,
            self.integrity_ok,
            self.outcome,
            self.error.as_deref().unwrap_or("-"),
        )
    }

    /// 收尾：结算墙钟耗时。
    pub fn finish(&mut self, outcome: &str, error: Option<String>) {
        if let Some(mark) = self.mark.take() {
            self.timings.total_ms = mark.elapsed().as_millis() as u64;
        }
        self.outcome = outcome.to_string();
        self.error = error;
        self.integrity_ok = outcome == "completed";
    }
}

/// 简易阶段计时器：在 RAII 析构时把耗时写回 [`TransferDiagnostics`]。
///
/// 用法：
/// ```ignore
/// let mut d = TransferDiagnostics::start(..);
/// {
///     let _t = PhaseTimer::new(&mut d, Phase::Connect);
///     // ... 阶段工作 ...
/// }   // 析构时自动记录耗时
/// ```
pub struct PhaseTimer<'a> {
    diag: &'a mut TransferDiagnostics,
    phase: Phase,
    started: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connect,
    Hello,
    Approval,
    Manifest,
    Data,
    Verify,
    Commit,
    Ack,
}

impl<'a> PhaseTimer<'a> {
    pub fn new(diag: &'a mut TransferDiagnostics, phase: Phase) -> Self {
        Self { diag, phase, started: Instant::now() }
    }
}

impl Drop for PhaseTimer<'_> {
    fn drop(&mut self) {
        let ms = self.started.elapsed().as_millis() as u64;
        match self.phase {
            Phase::Connect => self.diag.timings.connect_ms = ms,
            Phase::Hello => self.diag.timings.hello_ms = ms,
            Phase::Approval => self.diag.timings.approval_wait_ms = ms,
            Phase::Manifest => self.diag.timings.manifest_ms = ms,
            Phase::Data => self.diag.timings.data_ms = ms,
            Phase::Verify => self.diag.timings.verify_ms = ms,
            Phase::Commit => self.diag.timings.commit_ms = ms,
            Phase::Ack => self.diag.timings.ack_ms = ms,
        }
    }
}
