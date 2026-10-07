use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};

use crate::config::AppConfig;
use crate::error::{FeisuoError, Result};
use crate::protocol::*;
use crate::security::{BindOutcome, DeviceIdentity, TrustLevel, TrustStore, TrustedDevice};
use crate::storage::ChunkStore;
use crate::transport::diagnostics::TransferDiagnostics;
use crate::transport::server::{read_json_frame, write_json_frame};
use crate::transport::session::{TransferDirection, TransferProgress, TransferStatus};
use crate::transport::with_io_timeout;

/// 通知对端解除配对的上限（毫秒）。
///
/// 比 `TcpStream::connect` 那条 3.5 秒短：解除配对是**附带**动作，
/// 用户主要意图在本地降级，不该为了一个可能不在线的对端干等 3.5 秒。
const UNPAIR_NOTIFY_TIMEOUT_MS: u64 = 1200;

/// 解除配对的结果报告（§14.5）。
///
/// ## 为什么必须是结构化字段而不是一句话
///
/// 界面需要区分好几种**用户动作完全不同**的结果：
/// - 双向都解除了 → 干净收场；
/// - 本地降级但对方离线 → 对方仍可能继续给他投文件，需要提示；
/// - 对方版本过旧（无共同世代） → 需要升级才能真正断开；
/// - 对方已经重新配对 → 这条解除请求已过期，重试无意义。
///
/// 这些如果只回一句文案，界面就只能靠 `includes()` 去猜 —— 而那正是
/// 本轮在 `core` 里刚拆掉的反模式（`DenyCode` / `requires_grant_code`）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnpairReport {
    /// 本机是否已降级为未信任
    pub local_applied: bool,
    /// 对端是否**也**解除了（= 双向完成）
    pub peer_notified: bool,
    /// 机器可读原因：`applied` / `peer_unreachable` / `epoch_mismatch` /
    /// `no_shared_epoch` / `already_unpaired` / `not_in_trust_store`
    pub reason_code: String,
    /// 对端给出的说明（原样透出，便于排障）
    pub peer_message: String,
    /// 对端回显的**它当前的**配对世代（对账用；为空表示它没有）
    pub peer_epoch_seen: String,
    /// 直接可展示给用户的一句话
    pub user_message: String,
}

impl UnpairReport {
    /// 构造一份"只在本机做了处理"的报告。
    ///
    /// `local_applied` 初值是 `false`，由调用方在**真的降级之后**改写 ——
    /// 这样"构造"和"执行"不会脱节：不可能出现一份
    /// "声称已降级、其实没降"的报告。
    pub fn local_only(reason_code: &str, peer_message: String) -> Self {
        UnpairReport {
            local_applied: false,
            peer_notified: false,
            reason_code: reason_code.to_string(),
            peer_message,
            peer_epoch_seen: String::new(),
            user_message: String::new(),
        }
    }

    /// 由结构化字段拼出人话 —— **不靠匹配 `peer_message`**。
    pub fn build_user_message(&mut self) {
        // 未知原因码**不许**被静默吞掉：宁可给一句保守的话，也不能
        // 让用户以为已经双向断开。所以最后一条分支不区分 reason_code。
        self.user_message = match (self.local_applied, self.reason_code.as_str()) {
            (true, "applied") | (true, "already_unpaired") => {
                "已解除配对：双方都不再信任，需要重新配对才能再传文件".into()
            }
            (true, "epoch_mismatch") => {
                "本机已解除配对；但对方已与你重新绑定，这条解除请求对它无效（属正常）".into()
            }
            (true, "no_shared_epoch") => {
                "本机已解除配对；但对方版本过旧，无法同步解除 —— 请升级对方后再解除一次".into()
            }
            (true, _) => format!(
                "本机已解除配对，但**未能同步通知对方**（{}）—— \
                 对方仍可能继续给你推送文件，请稍后重试",
                if self.peer_message.trim().is_empty() {
                    "对方当前不在线"
                } else {
                    self.peer_message.trim()
                }
            ),
            // 本机没降级（信任库里根本没有这一行）。此时**不能说**"已解除"。
            (false, _) => {
                if self.reason_code == "peer_unreachable" {
                    "本机信任库里没有这台设备（可能已解除）".to_string()
                } else {
                    format!("本机未做任何改动（{}）", self.reason_code)
                }
            }
        };
    }
}

/// 发送成功后的度量回报（§9.6）。
#[derive(Debug, Clone)]
pub struct SendReport {
    /// 实际发出的字节数（失败时是已发出的部分）
    pub bytes_sent: u64,
    /// 落库用的原始量
    pub metrics: crate::security::TransferMetrics,
    /// 完整诊断记录
    pub diag: TransferDiagnostics,
    /// 断点续传跳过的文件名（P1 ⑪）。
    ///
    /// **必须报给用户** —— 静默跳过会让人以为"重传了 10 个文件"，
    /// 实际只传了 2 个；反过来漏报又让人以为"只传了 2 个"，
    /// 找不到另外 8 个去了哪。两个方向的误解都会导致重复操作。
    pub skipped: Vec<String>,
}

/// 取本机第一个非回环 IPv4 地址，用于链路画像推断。
///
/// 取不到就返回空串 —— 此时 `LinkProfile` 的同子网/覆盖网判定全部为 false，
/// 属于"信息缺失"，**不能因此让传输失败**。
fn local_ip_hint() -> String {
    local_ip_address::local_ip().map(|ip| ip.to_string()).unwrap_or_default()
}

/// 一次目录浏览的完整结果。
///
/// 不再只返回条目数组 —— 穿梭界面必须知道当前在哪一级、能不能返回上级、
/// 以及列表是不是被条数上限截断了(否则用户会以为文件凭空消失)。
#[derive(Debug, Clone, Default)]
pub struct RemoteBrowseListing {
    /// 当前目录下的条目
    pub files: Vec<RemoteFileEntry>,
    /// 当前相对路径, 空串 = 落盘根目录
    pub current_path: String,
    /// 上一级相对路径; 已在根目录时为 None
    pub parent_path: Option<String>,
    /// 条目数是否被 MAX_BROWSE_ENTRIES 截断
    pub truncated: bool,
    /// 服务端附带的说明 (如截断提示)
    pub message: String,
    // ---- v2：真实文件系统 ----
    /// 对端开放的可浏览卷（地址栏下拉的数据源）。1.x 对端返回空数组。
    pub volumes: Vec<crate::protocol::VolumeInfo>,
    /// 本目录总条目数（分页用）
    pub total: u32,
    /// 本次返回的偏移
    pub offset: u32,
    /// 对端是否走了真实卷模式。false = 对端是 1.x，只能看收件目录镜像。
    pub volume_mode: bool,
    /// **实际使用的**卷 id。请求发的是通配 `*` 时由服务端回显真实卷。
    pub volume: String,
    /// 对端能力集（诊断用）
    pub peer_caps: u32,
}

/// 卷通配符：让服务端挑一个可读卷（客户端不该猜盘符，见
/// [`crate::storage::volumes::VOLUME_ANY`]）。
pub const VOLUME_ANY: &str = crate::storage::volumes::VOLUME_ANY;

/// 浏览目标（§7.2）：**卷 + 卷内相对路径 + 分页**。
///
/// # 为什么不直接传绝对路径
///
/// 绝对路径（如 `D:\项目\2026`）在 Windows 和 Android 上根本不能互换，
/// 而且它把"哪个盘"和"盘里哪层"混在一起。对端拿到 `D:\Windows\...` 时
/// 既要查 `D:` 是否在 scope 内，又要逐段校验穿越 —— 两个维度耦合在一个
/// 字符串里，**几乎必然写出漏洞**。拆成两个字段后，
/// scope 判定只看 `volume`，穿越校验只看 `rel_path`，各管各的。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BrowseTarget {
    /// 卷 id。空串 = 1.x 语义（对端收件目录）。
    pub volume: String,
    /// 卷内相对路径，`/` 分隔。空串 = 卷根。
    pub rel_path: String,
    /// 分页偏移
    pub offset: u32,
    /// 分页长度，0 = 服务端默认
    pub limit: u32,
    /// **本次传输码**（对端处于「每次匹配码」等级时必填，§2.3）
    ///
    /// 浏览也是"由对端发起"的操作，所以与传输一样：**发起方出码**、
    /// 接收方在自己的审批界面里核对。反过来要求接收方在被请求前
    /// 就把码摆出来，那意味着它要常驻一个"待授权"界面。
    #[serde(default)]
    pub grant_code: String,
}

impl BrowseTarget {
    /// 卷根
    pub fn volume_root(volume: &str) -> Self {
        Self {
            volume: volume.to_string(),
            ..Default::default()
        }
    }

    /// 1.x 语义：只看收件目录
    pub fn legacy(sub_path: &str) -> Self {
        Self {
            volume: String::new(),
            rel_path: sub_path.to_string(),
            ..Default::default()
        }
    }

    /// 是否走真实卷模式
    pub fn is_volume_mode(&self) -> bool {
        !self.volume.is_empty()
    }

    /// 面包屑的各级名字（`项目/2026` → `["项目", "2026"]`）
    pub fn breadcrumb(&self) -> Vec<&str> {
        self.rel_path
            .split('/')
            .filter(|s| !s.is_empty())
            .collect()
    }
}

pub struct TransferClient {
    identity: Arc<DeviceIdentity>,
    config: Arc<RwLock<AppConfig>>,
    progress_tx: broadcast::Sender<TransferProgress>,
    /// 本机撤销标记：`peer_id -> 用户点撤销的时刻`。
    ///
    /// 只对 [`TransferClient::begin_outgoing`] 记下的**这一次**发送生效。
    /// 早于这次发送开始的标记一律忽略，否则「上次点过撤销」会把下一次发送误停。
    locally_aborted: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
    /// 这一次发往某对端是从什么时刻开始的（含哈希，还没建连）。
    outgoing_started: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
}

impl TransferClient {
    pub fn new(
        identity: Arc<DeviceIdentity>,
        config: Arc<RwLock<AppConfig>>,
        progress_tx: broadcast::Sender<TransferProgress>,
    ) -> Self {
        Self {
            identity,
            config,
            progress_tx,
            locally_aborted: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            outgoing_started: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// 记下「发往这台对端的这一次」从现在开始。
    ///
    /// 必须在读文件、算哈希**之前**调用。比这更早的撤销标记属于上一次，
    /// 在这里丢掉，不能让它误停这一次。
    pub fn begin_outgoing(&self, peer_id: &str) {
        if peer_id.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        if let Ok(mut s) = self.outgoing_started.lock() {
            s.insert(peer_id.to_string(), now);
        }
        if let Ok(mut s) = self.locally_aborted.lock() {
            if let Some(at) = s.get(peer_id) {
                if *at < now {
                    s.remove(peer_id);
                }
            }
        }
    }

    /// 标记「发往这台对端的当前传输应在本机中止」。
    pub fn abort_outgoing(&self, peer_id: &str) {
        if peer_id.is_empty() {
            return;
        }
        if let Ok(mut s) = self.locally_aborted.lock() {
            s.insert(peer_id.to_string(), std::time::Instant::now());
        }
    }

    /// 这一次发送是否已被本机撤销。不消费标记。
    pub fn outgoing_aborted(&self, peer_id: &str) -> bool {
        self.local_abort_applies(peer_id, false)
    }

    /// 这一次发送（[`begin_outgoing`] 之后）是否已被本机撤销。
    ///
    /// 命中后消费标记。下一次发送会再调 `begin_outgoing`，
    /// 更早的标记不会被算到它头上。
    fn take_local_abort(&self, peer_id: &str) -> bool {
        self.local_abort_applies(peer_id, true)
    }

    fn local_abort_applies(&self, peer_id: &str, consume: bool) -> bool {
        if peer_id.is_empty() {
            return false;
        }
        let started = self
            .outgoing_started
            .lock()
            .ok()
            .and_then(|s| s.get(peer_id).copied());
        let Some(started) = started else {
            return false;
        };
        let mut aborted = match self.locally_aborted.lock() {
            Ok(s) => s,
            Err(_) => return false,
        };
        match aborted.get(peer_id).copied() {
            Some(at) if at >= started => {
                if consume {
                    aborted.remove(peer_id);
                }
                true
            }
            _ => false,
        }
    }

    /// 发送文件, 落盘时**压平为裸文件名** (普通拖拽发送的既有行为)。
    pub async fn send_files(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        files: Vec<PathBuf>,
    ) -> Result<()> {
        // 非 UTF-8 文件名 / 根路径 在此处不再 unwrap 崩溃
        let mut items: Vec<(PathBuf, String)> = Vec::with_capacity(files.len());
        for path in files {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| {
                    FeisuoError::Protocol(format!("文件名无法编码为 UTF-8: {}", path.display()))
                })?
                .to_string();
            items.push((path, name));
        }
        self.send_files_as(
            target_ip,
            target_port,
            target_device_id,
            target_device_name,
            items,
        )
        .await
    }

    /// 带诊断的发送通道（普通拖拽发送用，落盘压平为裸文件名）。
    pub async fn send_files_diag(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        files: Vec<PathBuf>,
    ) -> Result<SendReport> {
        let mut items: Vec<(PathBuf, String)> = Vec::with_capacity(files.len());
        for path in files {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| {
                    FeisuoError::Protocol(format!("文件名无法编码为 UTF-8: {}", path.display()))
                })?
                .to_string();
            items.push((path, name));
        }
        self.send_files_as_diag(target_ip, target_port, target_device_id, target_device_name, items)
            .await
    }

    /// 发送文件并**保留相对目录层级**。
    ///
    /// `items` 每项是 `(本地绝对路径, 相对接收根目录的目标路径)`。
    /// 穿梭"取回"必须走这条通道: 用户在对端 `2026/报表/1月.csv` 里点了取回,
    /// 落到本机也应当是 `2026/报表/1月.csv`。旧的裸文件名通道会把所有层级
    /// 压平到接收根目录, 于是"在子文件夹里选文件"这件事完全没有意义。
    ///
    /// 目标路径由调用方给出, 接收端会逐段做穿越校验, 这里先自检一遍,
    /// 避免把明显非法的路径发到网络上浪费一次往返。
    pub async fn send_files_as(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        items: Vec<(PathBuf, String)>,
    ) -> Result<()> {
        self.send_files_as_diag(target_ip, target_port, target_device_id, target_device_name, items)
            .await
            .map(|_| ())
    }

    /// 带诊断的发送通道。
    ///
    /// 旧实现在发送循环里算出了 `elapsed` 紧接着 `let _ = elapsed;` **直接丢弃**，
    /// 于是"传输记录要显示速度"在发送侧根本拿不到数（§9.6.1）。本方法接上这条管道。
    pub async fn send_files_as_diag(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        items: Vec<(PathBuf, String)>,
    ) -> Result<SendReport> {
        self.send_files_as_diag_with_code(
            target_ip,
            target_port,
            target_device_id,
            target_device_name,
            items,
            "",
            "",
        )
        .await
    }

    /// 带诊断 + **本次传输码**的发送通道（§2.3）。
    ///
    /// ## 为什么码是"传进来"而不是"内部生成"
    ///
    /// 码必须显示在**用户看得见的界面上**，也就是宿主的职责。
    /// 让 core 生成再通过事件抛给 UI 也能做，但那样多绕一层，
    /// 而且 core 生成的码有个尴尬问题：它不知道用户有没有真的看到。
    /// 由 UI 生成并显式传进来，"用户看到了吗"这件事就自然落在 UI 手里。
    ///
    /// 第一次调用传空串；收到 [`FeisuoError::GrantCodeRequired`] 后，
    /// 宿主弹码让用户抄到对端屏幕、再带码重试本方法。
    pub async fn send_files_as_diag_with_code(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        items: Vec<(PathBuf, String)>,
        grant_code: &str,
        dest_sub_path: &str,
    ) -> Result<SendReport> {
        self.send_files_as_diag_full(
            target_ip,
            target_port,
            target_device_id,
            target_device_name,
            items,
            grant_code,
            None,
            dest_sub_path,
        )
        .await
    }

    /// 带**稳定 transfer_id**（断点续传的关键，P1 ⑪）与传输码的发送通道。
    ///
    /// `resume_key` 必须是**内容相关**且**重试稳定**的：
    /// 推荐"文件列表 + 各自 BLAKE3"的哈希。理由：
    /// - 用户改了某个文件 ⇒ 哈希变 ⇒ key 变 ⇒ 不可能被误判成"已收到"；
    /// - 同一批文件重试 ⇒ 哈希不变 ⇒ key 不变 ⇒ 续传生效。
    pub async fn send_files_as_diag_full(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        items: Vec<(PathBuf, String)>,
        grant_code: &str,
        resume_key: Option<String>,
        dest_sub_path: &str,
    ) -> Result<SendReport> {
        // 建档: 之后每个阶段都往里打点, 失败路径也要能产出可用记录
        let mut diag = TransferDiagnostics::start(
            uuid::Uuid::new_v4().to_string(),
            "send",
            target_device_id,
            target_device_name,
            target_ip,
            target_port,
        );
        if items.is_empty() {
            return Err(FeisuoError::Protocol("未选择任何文件".into()));
        }
        if items.len() > MAX_FILES_PER_BATCH {
            return Err(FeisuoError::Protocol(format!(
                "单次传输文件数超过 {} 上限",
                MAX_FILES_PER_BATCH
            )));
        }

        // 1. 预扫描文件, 任何一个不可读都直接失败, 不做"发一半才报错"
        let mut plan: Vec<(PathBuf, u64, String, u64)> = Vec::with_capacity(items.len());
        let mut total_size = 0u64;
        for (path, name) in &items {
            let meta = std::fs::metadata(path)
                .map_err(|e| FeisuoError::Io(e))?;
            if meta.is_dir() {
                return Err(FeisuoError::Protocol(format!(
                    "暂不支持发送文件夹: {}",
                    path.display()
                )));
            }
            let size = meta.len();
            // 自检相对路径, 非法路径不要发出去
            crate::storage::PathManager::validate_relative_subpath(name)?;
            total_size = total_size
                .checked_add(size)
                .ok_or_else(|| FeisuoError::Protocol("文件总体积溢出".into()))?;
            plan.push((path.clone(), size, name.clone(), 0));
        }
        diag.set_manifest_info(
            plan.len() as u32,
            total_size,
            CHUNK_SIZE as u32,
            plan.iter().map(|(_, sz, _, _)| ((*sz + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64).max(1)).sum(),
        );

        let addr = format!("{}:{}", target_ip, target_port);
        let connect_started = std::time::Instant::now();
        let connect_fut = TcpStream::connect(&addr);
        let stream = tokio::time::timeout(Duration::from_millis(3500), connect_fut)
            .await
            .map_err(|_| {
                FeisuoError::Network(format!(
                    "连接 {} 超时, 请确认对方已开启飞梭且端口 {} 可达",
                    addr, target_port
                ))
            })?
            .map_err(|e| FeisuoError::Network(format!("无法连接对端 {}: {}", addr, e)))?;

        // 建连耗时 = 链路延迟的粗略下界, 是链路画像的关键输入
        let connect_ms = connect_started.elapsed().as_millis() as u64;
        // 显式 socket buffer（§9.2）。必须在 connect 之后、第一个字节之前 ——
        // Windows 对未连接的 socket 设 SO_SNDBUF 会失败 (WSAENOTCONN)。
        let local_ip = local_ip_hint();
        let (mut stream, mut bdp) = crate::transport::sockopt::tune_and_log(
            stream,
            &local_ip,
            target_ip,
            connect_ms,
        )?;
        diag.mark_connect(&local_ip, connect_ms);
        diag.set_bdp(&bdp);
        // 持有原始建连耗时, 供中途二次调大时复用
        let base_connect_ms = connect_ms;

        // 预扫描（哈希整文件）可能要好几秒。用户在这期间点撤销时，
        // 还没有任何字节出站 —— 必须在开口之前就停下来，而不是建完连再传。
        if self.take_local_abort(target_device_id) {
            return Err(FeisuoError::LocallyAborted(
                "已在本机撤销，文件尚未发出".into(),
            ));
        }

        // 2. 消息类型 + 握手
        // tokio 的 TcpStream 没有 set_read/set_write_timeout, 统一用
        // tokio::time::timeout 逐段包裹, 保证"点发送后 UI 永远转圈"不可能发生。
        let hello_started = std::time::Instant::now();
        with_io_timeout(
            "发送消息类型",
            stream.write_all(&[MSG_TRANSFER]),
        )
        .await?;

        let now = chrono::Utc::now().timestamp();
        let nonce = uuid::Uuid::new_v4().to_string();
        let challenge = format!("{}:{}", nonce, now);
        let signature = self.identity.sign(challenge.as_bytes());
        let first_file_name = plan.first().map(|p| p.2.clone()).unwrap_or_default();

        let handshake = HandshakeRequest {
            version: PROTOCOL_VERSION,
            sender_id: self.identity.device_id.clone(),
            sender_name: self.config.read().await.device_name.clone(),
            timestamp: now,
            nonce,
            signature,
            sender_public_key_hex: self.identity.public_key_hex(),
            file_count: plan.len().min(u32::MAX as usize) as u32,
            total_size,
            first_file_name,
            grant_code: grant_code.trim().to_string(),
            dest_sub_path: dest_sub_path.trim().replace('\\', "/"),
        };
        write_json_frame(&mut stream, &handshake).await?;

        // 3. 读握手应答
        let hs_resp: HandshakeResponse = read_json_frame(&mut stream, "握手应答").await?;
        if !hs_resp.success {
            // 「每次匹配码」等级且本次没带码 —— 这**不是错误**，
            // 是一次正常的协商回合。宿主据此弹码、让用户抄到对端屏幕、
            // 然后带码重试。把它当错误报出去的话，用户只看到
            // "对方拒绝传输"，完全不知道该做什么。
            if hs_resp.requires_grant_code {
                info!(
                    "对端处于「每次匹配码」等级, 需要出示本次传输码（receiver={}）",
                    hs_resp.receiver_name
                );
                return Err(FeisuoError::GrantCodeRequired(hs_resp.message));
            }
            return Err(FeisuoError::Security(format!(
                "对方拒绝传输: {}",
                hs_resp.message
            )));
        }

        // 4. 校验应答方确实是本机信任的那台设备 (防止 ARP 欺骗窃取文件)
        if !target_device_id.is_empty() {
            if hs_resp.receiver_id != target_device_id {
                return Err(FeisuoError::Security(format!(
                    "对端身份不符: 期望 {}, 实际 {}",
                    target_device_id, hs_resp.receiver_id
                )));
            }
        }
        if !hs_resp.receiver_public_key_hex.is_empty() {
            let derived = DeviceIdentity::device_id_from_pubkey_hex(&hs_resp.receiver_public_key_hex)?;
            if derived != hs_resp.receiver_id {
                return Err(FeisuoError::Security(
                    "对端返回的设备指纹与公钥不匹配, 已中止传输".into(),
                ));
            }
            if !hs_resp.receiver_signature.is_empty()
                && !DeviceIdentity::verify(
                    &hs_resp.receiver_public_key_hex,
                    challenge.as_bytes(),
                    &hs_resp.receiver_signature,
                )?
            {
                return Err(FeisuoError::Security("对端握手签名校验失败".into()));
            }
        } else if !target_device_id.is_empty() {
            warn!("对端未返回公钥, 跳过接收方身份校验 (旧版本客户端?)");
        }

        // 握手阶段耗时（含对端人工审批等待）。
        // **这一段必须与"数据流时间"分开**：对方开会审批 60 秒会让总耗时暴涨，
        // 但那 60 秒一秒数据都没传，算进速度就是假慢（§9.6.3）。
        let hello_elapsed = hello_started.elapsed();
        diag.mark_hello(hello_elapsed.as_millis() as u64);
        let expected_wait = std::time::Duration::from_millis(300);
        if hello_elapsed > expected_wait {
            diag.mark_approval_wait(hello_elapsed.as_millis() as u64);
            tracing::info!(
                "握手耗时 {}ms 明显高于基线, 推测包含对端人工审批等待; 已从速度计算中排除",
                hello_elapsed.as_millis()
            );
        }

        // 5. 计算整文件哈希并组装清单 (阻塞 IO 放到阻塞线程池, 不占用 runtime worker)
        let mut file_metas: Vec<FileMeta> = Vec::with_capacity(plan.len());
        for (idx, (path, size, name, _)) in plan.iter().enumerate() {
            let hash = tokio::task::spawn_blocking({
                let path = path.clone();
                move || ChunkStore::hash_file(&path)
            })
            .await
            .map_err(|e| FeisuoError::Internal(format!("哈希任务异常: {}", e)))??;

            let chunk_count = if *size == 0 {
                1
            } else {
                (*size + (CHUNK_SIZE as u64) - 1) / (CHUNK_SIZE as u64)
            };
            file_metas.push(FileMeta {
                file_index: idx as u32,
                relative_path: name.clone(),
                file_size: *size,
                blake3_hash: hash,
                chunk_count,
            });
        }

        // ## 断点续传要求 transfer_id 在重试之间**保持不变**（P1 ⑪）
        //
        // 早先这里是 `Uuid::new_v4()`，每次发送一个全新 id。而接收端的
        // `transfer_parts` 是按 `transfer_id` 索引的 —— 于是**续传永远
        // 不可能触发**：新 id 在库里查不到任何记录，所有文件都会重传。
        // 代码看起来完整，功能 100% 不生效。
        //
        // 正确做法：调用方给一个**稳定**的 resume_key（UI 的队列条目 id，
        // 或"文件列表 + 各自 BLAKE3"的哈希），重试时复用它。
        // 这样"改了内容的文件"哈希变了 ⇒ 换 key ⇒ 不会被误跳过；
        // "同一批文件重试"哈希不变 ⇒ 同 key ⇒ 续传生效。
        let transfer_id = match resume_key {
            Some(k) if !k.trim().is_empty() => k.trim().to_string(),
            _ => uuid::Uuid::new_v4().to_string(),
        };
        let mut manifest = TransferManifest {
            transfer_id: transfer_id.clone(),
            sender_id: self.identity.device_id.clone(),
            total_size,
            chunk_size: CHUNK_SIZE as u32,
            files: file_metas,
            timestamp: now,
            signature: String::new(),
        };
        // 清单必须签名: 接收方会用它验证文件名/大小/分块数未被篡改
        manifest.signature = self.identity.sign(manifest.signing_payload().as_bytes());
        // 诊断记录与清单用同一个 transfer_id, 便于事后按 id 串起两侧记录
        diag.transfer_id = transfer_id.clone();

        // 握手期间用户可能已经点了撤销（进度事件还没发，UI 拿不到 id）。
        // 清单一旦写出，对端就会按它建暂存目录 —— 所以在写之前再查一次。
        // 命中后把标记放回去：下面的分块循环和收尾都靠它停，不能在这里消费掉。
        if self
            .locally_aborted
            .lock()
            .map(|s| s.contains_key(target_device_id))
            .unwrap_or(false)
        {
            let _ = self.take_local_abort(target_device_id);
            return Err(FeisuoError::LocallyAborted(
                "已在本机撤销，文件尚未发出".into(),
            ));
        }

        let manifest_started = std::time::Instant::now();
        write_json_frame(&mut stream, &manifest).await?;
        diag.mark_manifest(manifest_started.elapsed().as_millis() as u64);

        // ---- 5.5 读清单应答：接收方告知哪些文件它已经有了（断点续传）----
        //
        // 1.x 对端不返回这一帧, 读它会解析失败。因此**只在能确认对端是
        // 2.x 时才读** —— 而"能确认"需要握手应答带回对端 caps。
        let mut needed_set: std::collections::HashSet<u32> =
            (0..plan.len() as u32).collect();
        let mut skipped_names: Vec<String> = Vec::new();
        if hs_resp.caps & crate::protocol::caps::RESUME != 0 {
            let ack: ManifestAck = read_json_frame(&mut stream, "清单应答").await?;
            if !ack.success {
                return Err(FeisuoError::Protocol(format!(
                    "对方拒绝本次清单: {}",
                    ack.message
                )));
            }
            // 越界下标必须拒: 对端点名一个不存在的文件, 会让接收端把数据
            // 写到一个从未分配过的目标位置上, 而分块校验仍会通过
            // （因为 hash 是对的）—— 静默写坏文件。
            if ack.needed.iter().any(|i| *i as usize >= plan.len()) {
                return Err(FeisuoError::Security(
                    "对端返回的待传文件下标越界, 已中断（可能被篡改）".into(),
                ));
            }
            needed_set = ack.needed.iter().copied().collect();
            for c in &ack.completed {
                skipped_names.push(c.relative_path.clone());
            }
            if !skipped_names.is_empty() {
                info!(
                    "断点续传: 对端已有 {} 个文件, 本次只发 {} 个（{}）",
                    skipped_names.len(),
                    needed_set.len(),
                    ack.message
                );
            }
        }

        // 6. 流式发送分块
        let started_at = Instant::now();
        let mut bytes_sent = 0u64;
        let mut last_chunk_at = started_at;
        // 本次传输是否已经尝试过二次调大（只试一次, 避免反复换手柄）
        let mut bdp_regrow_tried = false;

        for (file_idx, (path, _, name, _)) in plan.iter().enumerate() {
            // 断点续传（P1 ⑪）：只发接收方点名要的文件。
            //
            // ⚠️ `file_idx` 仍用**清单里的原始下标**（不是"第几个要发的文件"）——
            // 接收端按 `file_index` 定位落盘路径与 `FileMeta`，改成本地连续下标
            // 会让所有续传文件落到错误路径上，而且校验会通过（因为分块头里的
            // hash 是对的）—— 这类 bug 极难发现。
            if !needed_set.contains(&(file_idx as u32)) {
                // 名字已经在上面按 `ack.completed` 记过一次。
                // 这里再 push 会让「对端已存在」变成真实数量的两倍，
                // 界面上「跳过 2 个」其实只跳过了 1 个。
                continue;
            }
            let meta = manifest.files[file_idx].clone();
            // 整个文件只开一次句柄（§9.3 第 1 行）。
            // 旧实现每块 `File::open`, 4 GiB 文件就是 1024 次 open + 1024 次 seek。
            // 开句柄是同步 IO, 必须离开 runtime worker。
            let reader = match tokio::task::spawn_blocking({
                let path = path.clone();
                move || crate::storage::ChunkReader::open(&path, CHUNK_SIZE)
            })
            .await
            .map_err(|e| FeisuoError::Internal(format!("打开文件任务异常: {}", e)))? {
                Ok(r) => Arc::new(r),
                Err(e) => {
                    return Err(FeisuoError::Internal(format!(
                        "打开待发送文件失败: {} ({})",
                        path.display(),
                        e
                    )))
                }
            };
            for chunk_idx in 0..meta.chunk_count {
                // 本机撤销：停在分块边界，不再把下一块写出去。
                //
                // 进度事件要等这一块发完才有，所以用户在「第一块还在读盘」
                // 时点的撤销，靠的就是这里，而不是对端的 cancelled 集合。
                // 对端那一侧由引擎在拿到 transfer_id 之后补发 MSG_CANCEL。
                if self.take_local_abort(target_device_id) {
                    self.emit_terminal(
                        &transfer_id,
                        target_device_id,
                        target_device_name,
                        name,
                        bytes_sent,
                        total_size,
                        TransferStatus::Cancelled,
                    );
                    diag.finish("cancelled", Some("本机在传输过程中撤销".into()));
                    return Err(FeisuoError::LocallyAborted(
                        "已在本机撤销本次传输".into(),
                    ));
                }
                // 4 MiB 的同步磁盘读绝不能占用 runtime worker:
                // 一个 9 MB 的文件就要卡住 worker 三次, 并发传输时
                // 会把发现广播、心跳、审批响应一起饿死。
                let chunk_data = tokio::task::spawn_blocking({
                    let reader = reader.clone();
                    move || reader.read_chunk(chunk_idx)
                })
                .await
                .map_err(|e| FeisuoError::Internal(format!("分块读取任务异常: {}", e)))??;
                let hash = ChunkStore::hash_chunk(&chunk_data);

                let header = ChunkHeader {
                    transfer_id: transfer_id.clone(),
                    file_index: file_idx as u32,
                    chunk_index: chunk_idx,
                    data_length: chunk_data.len() as u32,
                    chunk_blake3: hash,
                };
                write_json_frame(&mut stream, &header).await?;
                if !chunk_data.is_empty() {
                    super::write_with_timeout(&mut stream, &chunk_data, "发送分块数据").await?;
                }

                bytes_sent += chunk_data.len() as u64;
                // 逐块采样吞吐 + 记录停顿。
                // 停顿(相邻块间隔过大)通常意味着磁盘读阻塞或网络抖动,
                // 是区分"链路慢"与"磁盘慢"的关键信号（§9.7）。
                let now_instant = Instant::now();
                let gap_ms = now_instant.duration_since(last_chunk_at).as_millis() as u64;
                last_chunk_at = now_instant;
                diag.sample_throughput(chunk_data.len() as u64, gap_ms);
                let elapsed = started_at.elapsed().as_secs_f64().max(0.001);
                let _ = self.progress_tx.send(TransferProgress {
                    transfer_id: transfer_id.clone(),
                    direction: TransferDirection::Send,
                    peer_device_id: target_device_id.to_string(),
                    peer_device_name: target_device_name.to_string(),
                    current_file: name.clone(),
                    file_index: file_idx as u32,
                    total_files: plan.len() as u32,
                    bytes_transferred: bytes_sent,
                    total_bytes: total_size,
                    // total_size 为 0 时旧实现算出 NaN -> JSON null -> 前端反序列化持续报错
                    progress_percent: if total_size == 0 {
                        100.0
                    } else {
                        (bytes_sent as f64 / total_size as f64 * 100.0).min(100.0) as f32
                    },
                    speed_bytes_per_sec: (bytes_sent as f64 / elapsed) as u64,
                    status: TransferStatus::Transferring,
                });

                // ---- 传输中途二次调大 socket buffer（§9.2）----
                // 建连时只能按 RTT 猜带宽。跑够 8 秒、且已有稳定吞吐样本后
                // 再按**实测**带宽重算 BDP, 把缓冲区从 256KB 提到实际需要的量级。
                //
                // 触发阈值取 8 秒: 更早的话样本里混着握手与首块预热, 带宽被低估,
                // 调大的值还是不够, 白白换一次手柄。
                if !bdp_regrow_tried && bytes_sent >= 64 * 1024 * 1024 {
                    // **先打标记再调用**：regrow_stream 可能因为
                    // "现有 buffer 已经覆盖 BDP" 而返回 None ——
                    // 那是**正确决策**，不是"什么都没发生"。
                    diag.mark_bdp_regrow_tried();
                    let rtt_ms =
                        crate::transport::sockopt::rtt_from_profile(&diag.link);
                    let measured = diag.throughput.avg_bps();
                    let (new_stream, new_est) =
                        crate::transport::sockopt::regrow_stream(
                            stream,
                            &bdp,
                            rtt_ms.max(base_connect_ms / 2).max(1),
                            measured,
                        )?;
                    stream = new_stream;
                    if let Some(est) = new_est {
                        bdp = est;
                        diag.set_bdp(&bdp);
                        diag.mark_bdp_regrown();
                    }
                    // 无论是否调大都只试一次: 反复换手柄的风险大于收益
                    bdp_regrow_tried = true;
                }
            }
        }
        // 数据流阶段必须在 flush/读回执之前打点, 否则会低估吞吐。
        let data_ms = started_at.elapsed().as_millis() as u64;
        diag.mark_data(data_ms);
        diag.bytes_transferred = bytes_sent;
        with_io_timeout("刷新发送缓冲区", stream.flush()).await?;

        // 7. 读最终回执
        let ack_started = std::time::Instant::now();
        let ack: TransferAck = read_json_frame(&mut stream, "传输回执").await?;
        diag.mark_ack(ack_started.elapsed().as_millis() as u64);
        if !ack.success {
            let msg = ack
                .error_msg
                .clone()
                .unwrap_or_else(|| "对端未说明失败原因".into());
            self.emit_terminal(
                &transfer_id,
                target_device_id,
                target_device_name,
                &name_or_unknown(plan.first()),
                bytes_sent,
                total_size,
                TransferStatus::Failed(msg.clone()),
            );
            diag.finish("failed", Some(format!("对端报告失败: {}", msg)));
            return Err(FeisuoError::Protocol(format!("传输失败: {}", msg)));
        }

        // 8. 终态进度
        self.emit_terminal(
            &transfer_id,
            target_device_id,
            target_device_name,
            &format!("{} 个文件", plan.len()),
            bytes_sent,
            total_size,
            TransferStatus::Completed,
        );
        let _ = bytes_sent;
        diag.finish("completed", None);

        info!("{}", diag.to_log_line());
        info!("[诊断] 发送侧归因: {}", diag.attribution());
        Ok(SendReport {
            bytes_sent,
            metrics: crate::security::TransferMetrics::from_diagnostics(&diag),
            diag,
            skipped: skipped_names,
        })
    }

    /// 请求对端撤销本机发起的传输（直发的 5 秒撤销窗口，§5.2）。
    ///
    /// **尽力而为**：返回 `Ok(false)` 表示对端已经开始落盘，撤不掉。
    /// 调用方必须把这个结果如实告诉用户 —— 给一个假的安全感
    /// 比没有撤销按钮更危险。
    pub async fn cancel_transfer(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        transfer_id: &str,
    ) -> Result<bool> {
        let addr = format!("{}:{}", target_ip, target_port);
        let mut stream =
            tokio::time::timeout(Duration::from_millis(1500), TcpStream::connect(&addr))
                .await
                .map_err(|_| {
                    FeisuoError::Network(format!("撤销请求无法送达 {}: 连接超时", addr))
                })?
                .map_err(|e| FeisuoError::Network(format!("撤销请求无法送达 {}: {}", addr, e)))?;
        let _ = stream.set_nodelay(true);

        with_io_timeout("发送撤销消息类型", stream.write_all(&[MSG_CANCEL])).await?;

        let now = chrono::Utc::now().timestamp();
        let nonce = uuid::Uuid::new_v4().to_string();
        let challenge = format!("{}:{}", nonce, now);
        let req = CancelRequest {
            version: PROTOCOL_VERSION,
            sender_id: self.identity.device_id.clone(),
            transfer_id: transfer_id.to_string(),
            timestamp: now,
            nonce,
            signature: self.identity.sign(challenge.as_bytes()),
        };
        write_json_frame(&mut stream, &req).await?;

        let resp: CancelResponse =
            read_json_frame(&mut stream, "撤销应答").await?;
        if !target_device_id.is_empty() && resp.receiver_id != target_device_id {
            return Err(FeisuoError::Security(format!(
                "撤销应答来源不符: 期望 {}, 实际 {}",
                target_device_id, resp.receiver_id
            )));
        }
        if !resp.success {
            tracing::info!("撤销被拒: {}", resp.message);
        }
        Ok(resp.success)
    }

    fn emit_terminal(
        &self,
        transfer_id: &str,
        target_device_id: &str,
        target_device_name: &str,
        current_file: &str,
        bytes: u64,
        total: u64,
        status: TransferStatus,
    ) {
        let _ = self.progress_tx.send(TransferProgress {
            transfer_id: transfer_id.to_string(),
            direction: TransferDirection::Send,
            peer_device_id: target_device_id.to_string(),
            peer_device_name: target_device_name.to_string(),
            current_file: current_file.to_string(),
            file_index: 0,
            total_files: 1,
            bytes_transferred: bytes,
            total_bytes: total,
            progress_percent: if matches!(status, TransferStatus::Completed) {
                100.0
            } else {
                0.0
            },
            speed_bytes_per_sec: 0,
            status,
        });
    }

    /// 浏览对端目录 (仅已配对设备可浏览)。
    ///
    /// 旧签名只接受"相对收件根目录的子路径"——那不是文件系统浏览器。
    /// 现在走 [`BrowseTarget`]：真实卷 + 绝对路径 + 分页（§7.2/§7.3）。
    /// 保留 [`Self::list_remote_files_legacy`] 走 1.x 语义。
    pub async fn list_remote_files(
        &self,
        target_ip: &str,
        target_port: u16,
        target: &BrowseTarget,
    ) -> Result<RemoteBrowseListing> {
        let addr = format!("{}:{}", target_ip, target_port);
        let mut stream = tokio::time::timeout(
            Duration::from_millis(3500),
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| FeisuoError::Network(format!("连接 {} 超时", addr)))?
        .map_err(|e| FeisuoError::Network(format!("无法连接对端 {}: {}", addr, e)))?;

        let now = chrono::Utc::now().timestamp();
        let nonce = uuid::Uuid::new_v4().to_string();
        let req = BrowseRequest {
            version: PROTOCOL_VERSION,
            requester_id: self.identity.device_id.clone(),
            requester_name: self.config.read().await.device_name.clone(),
            sub_path: target.rel_path.clone(),
            timestamp: now,
            signature: self
                .identity
                .sign(format!("{}:{}", nonce, now).as_bytes()),
            nonce,
            volume: target.volume.clone(),
            rel_path: target.rel_path.clone(),
            offset: target.offset,
            limit: target.limit,
            caps: crate::protocol::local_caps(),
            grant_code: target.grant_code.clone(),
        };

        with_io_timeout("发送浏览请求类型", stream.write_all(&[MSG_BROWSE])).await?;
        write_json_frame(&mut stream, &req).await?;

        let resp: BrowseResponse = read_json_frame(&mut stream, "浏览应答").await?;
        if !resp.success {
            // 只认**结构化**字段。
            //
            // 旧实现是 `resp.message.contains("传输码") || contains("匹配码")`,
            // 那是把"这是协商回合"这个判断挂在中文措辞上 —— 对端一改文案
            // 就静默失效, 而失效的方向是"把要码说成拒绝", 用户能做的
            // 只有重试, 于是永远卡在这一步。
            //
            // 代价: **旧对端不发这个字段**时, 「每次匹配码」下的浏览会退化成
            // 一句普通的失败。但那句失败里仍然写着"对方尚未出示本次匹配码…",
            // **用户看得到原因**, 而静默的错误分类才是最坏的。
            // 混跑窗口只存在于"一端刚升级"的期间。
            if resp.requires_grant_code {
                return Err(FeisuoError::GrantCodeRequired(resp.message));
            }
            return Err(FeisuoError::Security(format!(
                "无法浏览对端目录: {}",
                resp.message
            )));
        }
        let listing = RemoteBrowseListing {
            files: resp.files,
            current_path: resp.current_path,
            parent_path: resp.parent_path,
            truncated: resp.truncated,
            message: resp.message,
            volumes: resp.volumes,
            total: resp.total,
            offset: resp.offset,
            volume_mode: resp.volume_mode,
            volume: resp.volume,
            peer_caps: resp.caps,
        };
        // 协商结果写进诊断日志：事后分析"为什么界面没出现盘符下拉"时，
        // 一行日志就能定位是对端 1.x 还是本机 scope 配错。
        tracing::info!(
            "Browse {}:{} vol={} rel={} -> {} entries (total={}, mode={}, peer_caps={})",
            target_ip, target_port, target.volume, target.rel_path,
            listing.files.len(), listing.total,
            if listing.volume_mode { "volume" } else { "legacy" },
            crate::protocol::describe_caps(listing.peer_caps),
        );
        Ok(listing)
    }

    /// 1.x 语义浏览：只看对端收件目录镜像。
    ///
    /// 存在的意义是**与旧版本对端互通** —— 此时 `volume` 留空，
    /// 旧服务端会忽略新字段并按 `sub_path` 返回。
    pub async fn list_remote_files_legacy(
        &self,
        target_ip: &str,
        target_port: u16,
        sub_path: &str,
    ) -> Result<RemoteBrowseListing> {
        self.list_remote_files(
            target_ip,
            target_port,
            &BrowseTarget::legacy(sub_path),
        )
        .await
    }

    /// 请求对端把落盘目录中的指定文件推送回本机 (双栏穿梭"取回")。
    ///
    /// `sub_paths` 里的每一项都是相对落盘根目录的路径, **可以含子目录层级**
    /// (例如 `2024/报表/1月.xlsx`)。服务端会逐段做穿越校验。
    pub async fn request_pull(
        &self,
        target_ip: &str,
        target_port: u16,
        sub_paths: Vec<String>,
        dest_sub_path: &str,
    ) -> Result<()> {
        self.request_pull_with_code(target_ip, target_port, sub_paths, dest_sub_path, "")
            .await
    }

    /// 带**本次传输码**的取回请求（§2.3）。
    ///
    /// 对端处于「每次匹配码」等级时，错误会是
    /// [`FeisuoError::GrantCodeRequired`] —— 宿主弹码、让用户抄给对方，
    /// 再带上码重试。
    pub async fn request_pull_with_code(
        &self,
        target_ip: &str,
        target_port: u16,
        sub_paths: Vec<String>,
        dest_sub_path: &str,
        grant_code: &str,
    ) -> Result<()> {
        self.request_pull_in_volume(
            target_ip,
            target_port,
            sub_paths,
            dest_sub_path,
            grant_code,
            "",
        )
        .await
    }

    /// 在**指定卷**里取回（v2 卷模式，§7.2 / §7.7）。
    ///
    /// `volume` 空串 = 1.x 语义，只在对端收件目录里找（向后兼容）。
    /// 非空时 `sub_paths` 是**卷内相对路径**。
    pub async fn request_pull_in_volume(
        &self,
        target_ip: &str,
        target_port: u16,
        sub_paths: Vec<String>,
        dest_sub_path: &str,
        grant_code: &str,
        volume: &str,
    ) -> Result<()> {
        if sub_paths.is_empty() {
            return Err(FeisuoError::Protocol("未选择要取回的文件".into()));
        }
        if sub_paths.len() > MAX_FILES_PER_BATCH {
            return Err(FeisuoError::Protocol(format!(
                "单次取回文件数超过 {} 上限",
                MAX_FILES_PER_BATCH
            )));
        }

        let addr = format!("{}:{}", target_ip, target_port);
        let mut stream = tokio::time::timeout(Duration::from_millis(3500), TcpStream::connect(&addr))
            .await
            .map_err(|_| FeisuoError::Network(format!("连接 {} 超时", addr)))?
            .map_err(|e| FeisuoError::Network(format!("无法连接对端 {}: {}", addr, e)))?;
        let _ = stream.set_nodelay(true);

        let now = chrono::Utc::now().timestamp();
        let nonce = uuid::Uuid::new_v4().to_string();
        let local_port = self.config.read().await.transfer_port;
        let req = PullRequest {
            version: PROTOCOL_VERSION,
            requester_id: self.identity.device_id.clone(),
            requester_name: self.config.read().await.device_name.clone(),
            requester_port: local_port,
            sub_paths,
            timestamp: now,
            signature: self
                .identity
                .sign(format!("{}:{}", nonce, now).as_bytes()),
            nonce,
            dest_sub_path: dest_sub_path.to_string(),
            volume: volume.trim().to_string(),
            caps: crate::protocol::local_caps(),
            grant_code: grant_code.trim().to_string(),
        };

        with_io_timeout("发送取回请求类型", stream.write_all(&[MSG_PULL])).await?;
        write_json_frame(&mut stream, &req).await?;

        let resp: PullResponse = read_json_frame(&mut stream, "取回应答").await?;
        if !resp.success {
            // 「每次匹配码」等级：这是一次**正常的协商回合**，不是失败。
            // 宿主据此弹码重试，而不是报"对方拒绝了取回请求"。
            //
            // 只认结构化字段（理由同 browse 路径：靠中文措辞分类会静默失效）。
            if resp.requires_grant_code {
                return Err(FeisuoError::GrantCodeRequired(resp.message));
            }
            return Err(FeisuoError::Security(format!(
                "对方拒绝了取回请求: {}",
                resp.message
            )));
        }
        info!("Pull request accepted by {}", resp.receiver_name);
        Ok(())
    }

    /// 双向解除配对（§14.5）：**先**通知对端，**再**改本地状态。
    ///
    /// ## 为什么顺序不能反
    ///
    /// 需求是「一方解除配对，**双方**都有变成未信任」。旧实现只做本地
    /// `remove_device`，于是对端仍是永久信任并继续静默收文件，
    /// 而两边界面都显示「永久信任」—— 用户完全无从察觉。
    ///
    /// ## 为什么**通知失败也照样改本地**
    ///
    /// 用户按下"解除配对"的意图是**他自己的机器不再信任这台设备**，
    /// 这个不能因为对端离线而失败 —— 否则"对方关机时点解除配对"
    /// 什么都不会发生，用户会以为按钮坏了。返回的 `peer_notified=false`
    /// 让宿主能**如实告知**"对方不在，未能同步"，由用户决定稍后再试。
    /// 静默只改一边才是不可接受的（那正是旧行为）。
    ///
    /// ## 防重放靠世代，不靠 nonce 缓存
    ///
    /// `peer_epoch` 是双方在配对时协商出的共同串（见
    /// [`PairRequest::pairing_epoch`]）。对端只在它与自己当前绑定世代
    /// 一致时才生效，于是**录下旧帧重放拆不掉新绑定**。
    pub async fn unpair_device(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        trust_store: Arc<TrustStore>,
    ) -> Result<UnpairReport> {
        let epoch = trust_store.pairing_epoch(target_device_id)?.unwrap_or_default();

        // 先通知对端。**通知失败不阻止本地降级**（理由见文档注释）。
        let mut report = UnpairReport::local_only("peer_unreachable", String::new());
        match tokio::time::timeout(
            Duration::from_millis(UNPAIR_NOTIFY_TIMEOUT_MS),
            self.send_unpair_frame(&format!("{}:{}", target_ip, target_port), target_device_id, &epoch),
        )
        .await
        {
            Ok(Ok(resp)) => {
                report.peer_notified = resp.success;
                report.reason_code = resp.reason_code.clone();
                report.peer_message = resp.message.clone();
                report.peer_epoch_seen = resp.pairing_epoch.clone();
            }
            Ok(Err(e)) => {
                tracing::debug!("解除配对: 通知对端失败: {}", e);
                report.reason_code = "peer_unreachable".into();
                report.peer_message = e.to_string();
            }
            Err(_) => {
                tracing::debug!("解除配对: 通知对端超时");
                report.reason_code = "peer_unreachable".into();
            }
        }

        // 无论通知成不成，本地都要降级。用 downgrade 而不是 remove_device：
        // 后者是 DELETE，会连 `visible` 一起删，于是"隐藏 + 解除配对"之后
        // 那台设备会重新出现在主列表（§14.3.1 的 I9）。
        report.local_applied = trust_store.downgrade_to_untrusted(target_device_id)?;
        report.build_user_message();
        Ok(report)
    }

    async fn send_unpair_frame(
        &self,
        addr: &str,
        target_device_id: &str,
        epoch: &str,
    ) -> Result<UnpairResponse> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| FeisuoError::Network(format!("无法连接对端 {}: {}", addr, e)))?;
        let mut stream = stream;
        let _ = stream.set_nodelay(true);

        with_io_timeout("发送解除配对消息类型", stream.write_all(&[MSG_UNPAIR])).await?;

        let now = chrono::Utc::now().timestamp();
        let nonce = uuid::Uuid::new_v4().to_string();
        let mut req = UnpairRequest {
            version: PROTOCOL_VERSION,
            initiator_id: self.identity.device_id.clone(),
            target_id: target_device_id.to_string(),
            peer_epoch: epoch.to_string(),
            timestamp: now,
            nonce,
            signature: String::new(),
        };
        req.signature = self.identity.sign(req.signing_payload().as_bytes());
        write_json_frame(&mut stream, &req).await?;

        let resp: UnpairResponse = read_json_frame(&mut stream, "解除配对应答").await?;
        Ok(resp)
    }

    pub async fn pair_with_device(
        &self,
        target_ip: &str,
        target_port: u16,
        pin: &str,
        trust_store: Arc<TrustStore>,
    ) -> Result<TrustedDevice> {
        let addr = format!("{}:{}", target_ip, target_port);
        let connect_fut = TcpStream::connect(&addr);
        let mut stream = tokio::time::timeout(Duration::from_millis(3500), connect_fut)
            .await
            .map_err(|_| {
                FeisuoError::Network(
                    "连接对端超时（3.5秒未响应，请检查对端是否已开启飞梭）".into(),
                )
            })?
            .map_err(|e| FeisuoError::Network(format!("无法连接对端 {}: {}", addr, e)))?;
        let _ = stream.set_nodelay(true);

        with_io_timeout("发送配对请求类型", stream.write_all(&[MSG_PAIR])).await?;

        // 配对世代（§14.5.1）：发起方生成并带给接收方，双方各存一份。
        // 解除配对时用它防重放 —— 没有它，旧帧就能拆掉**新**绑定。
        let epoch = uuid::Uuid::new_v4().to_string();
        let pair_req = PairRequest {
            pin_code: pin.to_string(),
            device_id: self.identity.device_id.clone(),
            device_name: self.config.read().await.device_name.clone(),
            public_key_hex: self.identity.public_key_hex(),
            pairing_epoch: epoch.to_string(),
        };
        write_json_frame(&mut stream, &pair_req).await?;

        let resp: PairResponse = read_json_frame(&mut stream, "配对应答").await?;

        if !resp.success {
            return Err(FeisuoError::Security(
                resp.error_msg.unwrap_or_else(|| "配对码错误或已失效".into()),
            ));
        }

        // 对端返回的 device_id 必须与它的公钥一致
        let derived = DeviceIdentity::device_id_from_pubkey_hex(&resp.public_key_hex)?;
        if derived != resp.device_id {
            return Err(FeisuoError::Security(
                "对端返回的设备指纹与公钥不匹配, 配对已中止".into(),
            ));
        }

        let dev = TrustedDevice {
            device_id: resp.device_id,
            device_name: if resp.device_name.trim().is_empty() {
                target_ip.to_string()
            } else {
                resp.device_name
            },
            public_key_hex: resp.public_key_hex,
            last_ip: target_ip.to_string(),
            bound_at: chrono::Utc::now().to_rfc3339(),
            is_trusted: true,
            // 走完 6 位 PIN 配对即"终生免密"的核心承诺, 因此是永久信任
            // **以对端回填的为准**（而不是我们生成的那个）:
            // 旧接收方会忽略我们带的值并自己生成一个, 只有用它
            // 才能保证两边存的是同一个串。
            pairing_epoch: resp.pairing_epoch.clone(),
                        trust_level: TrustLevel::Permanent,
            // 刚配对成功的设备必须出现在设备列表里, 否则用户配完了却看不到它
            visible: true,
            // 此刻就是它最后一次在线的时刻
            last_seen_at: chrono::Utc::now().timestamp(),
        };

        // 绝不覆盖既有身份的公钥 (防身份顶替)
        match trust_store.bind_device(&dev)? {
            BindOutcome::KeyConflict => {
                return Err(FeisuoError::Security(
                    "该设备指纹已绑定其他密钥, 请先在受信列表中解除后重新配对".into(),
                ))
            }
            BindOutcome::Added | BindOutcome::Refreshed => {}
        }

        Ok(dev)
    }
}

fn name_or_unknown(plan: Option<&(PathBuf, u64, String, u64)>) -> String {
    plan.map(|p| p.2.clone()).unwrap_or_else(|| "未知文件".into())
}
