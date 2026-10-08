pub mod clipboard;
pub mod config;
pub mod discovery;
pub mod engine_host;
pub mod error;
pub mod protocol;
pub mod roster;
pub mod security;
pub mod storage;
pub mod transport;

/// Android JNI 绑定层 (纯 FFI 薄壳, 实质逻辑在 `engine_host`)。
///
/// 只在 Android 构建: `jni` crate 在加载时需要 libjvm, 桌面上链接会失败。
/// 业务逻辑之所以留在 `engine_host` 这个跨平台模块里, 就是为了让它
/// 能在桌面上被单元测试覆盖 —— 见 `engine_host.rs` 的模块说明。
#[cfg(target_os = "android")]
pub mod mobile;

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::info;

pub use config::AppConfig;
pub use config::{
    resolve_app_dir, sanitize_device_name, CLOSE_ACTION_ASK, CLOSE_ACTION_EXIT, CLOSE_ACTION_TRAY,
    DEFAULT_DISCOVERY_PORT, DEFAULT_TRANSFER_PORT, DISCOVERY_MULTICAST_ADDR,
};
pub use discovery::{DiscoveredDevice, DiscoveryService};
pub use error::{FeisuoError, Result};
pub use protocol::{ApprovalAction, ApprovalRequest, RemoteFileEntry, VolumeInfo};
pub use roster::{build_roster, DeviceRosterEntry};
pub use clipboard::{ClipboardContent, MAX_PAYLOAD_BYTES};
pub use security::{
    endpoint_kind_label, endpoint_priority, format_bytes, AccessMode, AccessScope, BindOutcome,
    Decision, DeviceEndpoint, DeviceIdentity, Op, Presence, SecurityEvent,
    SecurityEventKind, SessionGrant, TransferMetrics, TransferRecord, TrustLevel, TrustStore,
    TrustedDevice,
};
pub use transport::{
    BrowseTarget, classify_ip, LinkProfile, OverlayKind, PhaseTimings, RemoteBrowseListing,
    ThroughputStats, TransferClient, TransferDiagnostics, TransferDirection, TransferProgress,
    TransferServer,
    TransferStatus,
};

/// 引擎及其事件通道。
///
/// 四个返回值原本是裸元组 `(engine, disc_rx, progress_rx, approval_rx)`,
/// 调用方极易把接收端顺序搞错 —— 而顺序错了不会编译失败, 只会让
/// 传输进度被当成设备发现事件消费掉, 表现为"进度条永远不动"。
pub struct EngineHandles {
    pub engine: FeisuoEngine,
    /// 局域网设备发现事件
    pub devices: broadcast::Receiver<DiscoveredDevice>,
    /// 传输进度事件
    pub progress: broadcast::Receiver<TransferProgress>,
    /// 入站传输审批请求
    pub approval: broadcast::Receiver<ApprovalRequest>,
}

impl EngineHandles {
    /// 拆成元组, 便于旧的解构写法继续工作
    #[allow(clippy::type_complexity)]
    pub fn into_tuple(
        self,
    ) -> (
        FeisuoEngine,
        broadcast::Receiver<DiscoveredDevice>,
        broadcast::Receiver<TransferProgress>,
        broadcast::Receiver<ApprovalRequest>,
    ) {
        (self.engine, self.devices, self.progress, self.approval)
    }
}

pub struct FeisuoEngine {
    pub identity: Arc<DeviceIdentity>,
    pub config: Arc<RwLock<AppConfig>>,
    pub trust_store: Arc<TrustStore>,
    pub discovery: Arc<DiscoveryService>,
    pub server: Arc<TransferServer>,
    pub client: Arc<TransferClient>,
    pub progress_tx: broadcast::Sender<TransferProgress>,
    pub approval_tx: broadcast::Sender<ApprovalRequest>,
    /// 本节点的数据根目录 (配置 / 信任库 / 设备私钥 / 日志)。
    /// Android 宿主必须显式注入 `context.filesDir`, 否则会退化成 CWD 相对路径。
    pub app_dir: PathBuf,
    /// 引擎是否已成功启动。
    ///
    /// 为什么需要这个显式状态: 启动失败时 `get_online_devices` 之类的命令
    /// **不会**报错 —— 发现服务的设备表本来就是空的, 返回 `Ok([])`。
    /// 于是前端若靠"调用没抛异常"来推断在线状态, 就会在引擎完全没起来的
    /// 情况下照样显示"在线", 甚至把真正的错误横幅清掉。
    /// 托盘常驻 + 开机自启下窗口根本不显示, 用户只看到一个托盘图标, 以为
    /// 一切正常 —— 直到某天需要传文件才发现永远传不了。
    started: std::sync::atomic::AtomicBool,
    /// 最近一次启动失败的原因; 成功启动后清空。
    start_error: std::sync::Mutex<Option<String>>,
    /// 最近一次目录展开的结果（§7.6），供 UI 展示"发了多少、跳过了多少"。
    ///
    /// 存在的原因：目录递归会**跳过**符号链接、隐藏文件、内部目录。
    /// 静默跳过的话，用户拖进去一个含 `.git` 的项目，收到后少了 3000 个
    /// 文件却完全不知道为什么。必须说出来。
    last_scan: std::sync::Mutex<Option<crate::storage::FolderScan>>,
    /// 本机**最近一次发起**的传输：`peer_id -> (transfer_id, 发起时刻)`。
    ///
    /// ## 为什么必须记在这里
    ///
    /// 撤销（§5.2）需要 `transfer_id`，而它由 [`compute_resume_key`]
    /// 在 `send_files_with_code` 内部算出，调用方（UI / 托盘）拿不到。
    /// 早先桌面端 command 直接传了个**占位值**（对方设备 id）——
    /// 而 staging 目录是按 `transfer_id` 命名的，那个占位值永远匹配不上：
    /// 撤销请求"成功"了，但什么都没撤掉，界面还显示"已撤销"。
    ///
    /// 比占位更糟的是它让**撤销看起来是能用的**：用户拖完立刻反悔、
    /// 点撤销、看到绿色提示、以为文件没发出去，几秒后文件却出现在
    /// 收件目录里。要求 ② 的主路径整个是假的。
    ///
    /// 存"最近一次"而不是全部：撤销窗口只有 5 秒, 且同一对设备同时只会有
    /// 一个在传。存全部会让 Map 随 (设备 × 传输次数) 无界增长。
    last_outgoing: Arc<std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>>,
    /// 用户已点撤销、但 `transfer_id` 还没算出来的对端。
    ///
    /// 发送在 `compute_resume_key`（要读完整文件算哈希）之前就会被 UI 取消。
    /// 那时 `last_outgoing` 还是空的，对端也还没收到 `MSG_CANCEL`。
    /// 记下对端 id，等 key 一算出来就立刻补发取消，并让发送循环自己停。
    abort_pending: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl FeisuoEngine {
    /// 取最近一次目录展开的结果（§7.6）。
    ///
    /// 在**发送完成之后**调用才有意义 —— 展开发生在建清单之前。
    /// 与其让 `send_files` 多返回一个复杂结构，不如让 UI 在拿到
    /// "发送成功"之后再问一次：这样"跳过详情"与"发送结果"在时间上
    /// 一定是对应的，不会出现"显示的是上一次的展开结果"。
    pub fn take_last_scan(&self) -> Option<crate::storage::FolderScan> {
        self.last_scan
            .lock()
            .ok()
            .and_then(|mut s| s.take())
    }
}

/// 一张生效中的短期授权（§2.3.1）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActiveGrant {
    /// 操作类别：`receive` / `browse` / `pull`。**按类别隔离**，不互通。
    pub scope: String,
    /// 展示用的类别名。
    ///
    /// 由 core 给而不是前端硬编码中文映射表：那张表散在几处就会漂移，
    /// 而漂移的表现是「界面上写着浏览、实际给的是传输」——
    /// **界面上说的范围必须等于实际生效的范围**。
    pub scope_label: String,
    /// 过期时间（epoch 秒）。到点自动失效，不需要任何后台任务。
    pub expires_at: i64,
}

/// 授权 scope 的展示名（§2.3.1）。
///
/// 刻意用**穷举 match** 而不是查表：新增一个 `Op` 时这里会立刻显出
/// 缺口，而带 `_ =>` 的查表会静默显示成原始英文。
pub fn grant_scope_label(scope: &str) -> &'static str {
    match scope {
        "receive" => "收文件",
        "push" => "写入文件",
        "browse" => "浏览目录",
        "pull" => "取走文件",
        "all" => "全部操作",
        _ => "未知操作",
    }
}


impl FeisuoEngine {
    /// 按平台约定初始化 (桌面端入口)。
    pub async fn init() -> Result<EngineHandles> {
        let app_dir = resolve_app_dir();
        Self::init_in(app_dir).await
    }

    /// 在指定数据目录下初始化。
    ///
    /// Android / iOS 这类沙箱平台**必须**走这个入口:
    /// `resolve_app_dir()` 在 Android 上只能猜一个路径, 而沙箱应用的 CWD
    /// 不是可持久化位置 —— 猜错会导致每次冷启动都换目录, 设备指纹与信任库
    /// 全部丢失, 用户每重启一次就得重新配对。
    pub async fn init_in(app_dir: PathBuf) -> Result<EngineHandles> {
        std::fs::create_dir_all(&app_dir).map_err(|e| {
            FeisuoError::io_context(
                format!("创建数据目录失败: {}", app_dir.display()),
                e,
            )
        })?;

        let identity = Arc::new(DeviceIdentity::load_or_generate_at(
            app_dir.join("device_identity.key"),
        )?);
        let config = Arc::new(RwLock::new(AppConfig::load_or_default_in(&app_dir)));
        let trust_store = Arc::new(TrustStore::open_at(app_dir.join("trust_store.db"))?);

        let mut handles = Self::assemble(identity, config, trust_store)?;
        // assemble() 只能从环境变量反推目录, 这里以调用方显式传入的为准。
        // Android 宿主正是靠这一点保证配置/私钥/信任库都落在 filesDir 下。
        handles.engine.app_dir = app_dir;
        Ok(handles)
    }

    /// 用外部提供的身份 / 配置 / 信任库组装引擎。
    ///
    /// 桌面端只需要 `init()`, 而"同一进程内跑两个节点"的集成测试
    /// 必须能各自持有独立的私钥、配置与 SQLite 库, 走 `init()` 会被全局
    /// `AppConfig::get_app_dir()` 绑死, 两个节点共用同一份状态。
    pub fn assemble(
        identity: Arc<DeviceIdentity>,
        config: Arc<RwLock<AppConfig>>,
        trust_store: Arc<TrustStore>,
    ) -> Result<EngineHandles> {
        let (discovery_service, devices) =
            DiscoveryService::new(identity.clone(), config.clone(), trust_store.clone());
        let discovery = Arc::new(discovery_service);

        let (progress_tx, progress) = broadcast::channel(256);
        let (approval_tx, approval) = broadcast::channel(64);

        let client = Arc::new(TransferClient::new(
            identity.clone(),
            config.clone(),
            progress_tx.clone(),
        ));

        let server = Arc::new(TransferServer::new(
            identity.clone(),
            config.clone(),
            trust_store.clone(),
            client.clone(),
            progress_tx.clone(),
            approval_tx.clone(),
        ));

        let engine = Self {
            // assemble() 由测试直接调用, 拿不到 init_in 的 app_dir 参数,
            // 此时如实反映"本进程实际会写到哪里", 不做任何美化。
            // 桌面端 / Android 宿主请走 init() / init_in(), 那里的值才是权威的。
            app_dir: std::env::var("FEISUO_APP_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| resolve_app_dir()),
            started: std::sync::atomic::AtomicBool::new(false),
            start_error: std::sync::Mutex::new(None),
            last_scan: std::sync::Mutex::new(None),
            last_outgoing: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            abort_pending: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            identity,
            config,
            trust_store,
            discovery,
            server,
            client,
            progress_tx,
            approval_tx,
        };

        Ok(EngineHandles {
            engine,
            devices,
            progress,
            approval,
        })
    }

    /// 启动引擎。
    ///
    /// 接收 `&Arc<Self>`（而不是 `&self`）是为了让周期任务能安全地
    /// 持有一份强引用 —— 用 `&self` 的话，后台任务要么活得比引擎久
    /// （use-after-free），要么得靠裸指针转 `&'static` 绕过生命周期检查。
    /// 那个"绕过"在桌面端碰巧安全、在重构后就不一定了。
    /// 两个宿主（桌面 / Android）本来就都持有 `Arc<FeisuoEngine>`，
    /// 所以这个签名对它们都是零成本的。
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        info!(
            "Starting Feisuo Engine for device: {} ({})",
            self.identity.device_id,
            self.config.read().await.device_name
        );
        // 失败原因必须落到状态里, 不能只往上报。
        // 上报走的是一次性事件, 而引擎是在 `setup()` 里 spawn 出去的,
        // 报错时间点(实测 8ms)远早于前端挂载监听器 —— 事件会被丢掉,
        // 错误就此彻底消失, 没有任何补救路径。
        let r = self.start_inner().await;
        match &r {
            Ok(()) => {
                self.started
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut slot) = self.start_error.lock() {
                    *slot = None;
                }
            }
            Err(e) => {
                self.started
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut slot) = self.start_error.lock() {
                    *slot = Some(e.to_string());
                }
            }
        }
        r
    }

    async fn start_inner(self: &Arc<Self>) -> Result<()> {
        self.discovery.start().await?;
        self.server.start().await?;
        self.spawn_maintenance();
        Ok(())
    }

    /// 周期性 housekeeping（会话授权 + 断点续传记录）。
    ///
    /// ## 为什么放在引擎里而不是宿主里
    ///
    /// 这两样都是**引擎的状态**（存在 trust_store 里），宿主不该负责清理。
    /// 而且桌面端与 Android 端各自建定时器的话，两边的保留期迟早不一致。
    ///
    /// ## 为什么启动时先立刻跑一次
    ///
    /// 用户很可能几天没开过机器，过期数据恰恰是在关机期间积累的。
    /// 只靠定时器意味着"今天第一次启动"的那一批不会立刻被清。
    ///
    /// 间隔取 6 小时：清理很便宜（两次 DELETE + 少量 `is_file()`），
    /// 但**每个断点续传文件都要做一次存在性检查**，
    /// 记录很多时跑太频繁会有可观开销。
    fn spawn_maintenance(self: &Arc<Self>) {
        // 启动即清一次
        self.purge_expired_session_grants();
        self.purge_completed_parts();

        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
            // 第一次 tick 立刻返回，但上面已手动清过一次，跳过它。
            tick.tick().await;
            loop {
                tick.tick().await;
                engine.purge_expired_session_grants();
                engine.purge_completed_parts();
            }
        });
    }

    /// 引擎是否已成功启动。
    pub fn is_started(&self) -> bool {
        self.started.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 最近一次启动失败的原因 (成功启动后为 None)。
    pub fn start_error(&self) -> Option<String> {
        self.start_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 停止发现与传输服务。
    ///
    /// 不阻塞等待后台循环退出 —— 只是置关闭信号。之所以不 `async`:
    /// 调用方 (Android JNI 关闭、桌面退出) 拿到的是一个**可能已经关闭**的
    /// runtime, 在上面 `.await` 一个永远不会完成的 future 就等于把进程挂死。
    pub fn stop(&self) {
        self.started
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.discovery.stop();
        self.server.stop();
    }

    /// 发送文件。返回**因断点续传而跳过的文件数**。
    ///
    /// 收到 [`FeisuoError::GrantCodeRequired`] 时**待发队列应保留**，
    /// 宿主弹码、让用户抄到对端屏幕、带上码重试本方法。
    /// 详见 [`FeisuoEngine::send_files_with_code`]。
    pub async fn send_files(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        files: Vec<PathBuf>,
    ) -> Result<u32> {
        self.send_files_with_code(
            target_ip,
            target_port,
            target_device_id,
            target_device_name,
            files,
            "",
        )
        .await
    }

    /// 发送到**对方收件目录下的指定子目录**（穿梭 §7.7）。
    ///
    /// `dest_sub_path` 是相对**对方收件目录**的路径；空串 = 收件根。
    /// 接收端会用与取回方向**完全相同**的校验（拒绝 `..`/绝对路径/盘符/
    /// 隐藏目录/超深 → 逐段创建 → 确认仍在收件目录之内），
    /// 所以它**不能**用来写到收件目录之外。
    pub async fn send_files_to_dest(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        files: Vec<PathBuf>,
        dest_sub_path: &str,
    ) -> Result<u32> {
        self.send_files_to_dest_with_code(
            target_ip,
            target_port,
            target_device_id,
            target_device_name,
            files,
            dest_sub_path,
            "",
        )
        .await
    }

    /// 同 [`FeisuoEngine::send_files_to_dest`]，但带本次匹配码（§2.3）。
    pub async fn send_files_to_dest_with_code(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        files: Vec<PathBuf>,
        dest_sub_path: &str,
        grant_code: &str,
    ) -> Result<u32> {
        self.send_files_to_dest_inner(
            target_ip,
            target_port,
            target_device_id,
            target_device_name,
            files,
            dest_sub_path,
            grant_code,
        )
        .await
    }

    /// 带**本次传输码**的发送（§2.3）。
    ///
    /// 第一次传空串；对端返回 `GrantCodeRequired` 后，宿主展示自己生成的
    /// 6 位码（用户抄到对端屏幕上念给对方），再带上码重试。
    ///
    /// ## 为什么码由宿主生成而不是 core 生成
    ///
    /// 码必须显示在**用户看得见的界面上**，这是宿主的职责。
    /// core 生成再抛事件给 UI 也能做，但那样"用户到底看到没有"
    /// 这个判断就落到了 core 头上 —— 而它无从知道。
    pub async fn send_files_with_code(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        files: Vec<PathBuf>,
        grant_code: &str,
    ) -> Result<u32> {
        self.send_files_to_dest_inner(
            target_ip,
            target_port,
            target_device_id,
            target_device_name,
            files,
            "",
            grant_code,
        )
        .await
    }

    async fn send_files_to_dest_inner(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        files: Vec<PathBuf>,
        dest_sub_path: &str,
        grant_code: &str,
    ) -> Result<u32> {
        // 必须在展开 / 哈希**之前**记下「这一次」。
        // 用户在读盘期间点的撤销才算这一次；更早的残留标记不算。
        self.client.begin_outgoing(target_device_id);
        // 目录在这一步**展开**（§7.6）。协议不需要改：清单的
        // `relative_path` 本来就允许多级，接收端也已有逐段校验。
        //
        // 展开放在这里而不是传输层的理由：传输层只认"一批文件 + 各自的
        // 目标相对路径"，它不需要知道目标路径是怎么来的。
        let (items, scan) = crate::storage::folder_scan::expand_all(&files)?;
        if self.client.outgoing_aborted(target_device_id) {
            return Err(FeisuoError::LocallyAborted {
                message: "已在本机撤销，文件尚未发出".into(),
                bytes_sent: 0,
            });
        }
        if items.is_empty() {
            let hint = scan
                .limit_hit
                .clone()
                .unwrap_or_else(|| "所选内容里没有可发送的文件".to_string());
            return Err(FeisuoError::Protocol(format!(
                "{}（跳过 {} 项，其中符号链接 {} 个）",
                hint,
                scan.skipped,
                scan.symlinks_skipped
            )));
        }
        if let Ok(mut slot) = self.last_scan.lock() {
            *slot = Some(scan);
        }
        // 断点续传的 transfer_id 必须**内容相关且重试稳定**（P1 ⑪）。
        // 见 [`compute_resume_key`] 的说明 —— 这是续传能否生效的关键。
        let resume_key = Self::compute_resume_key(target_device_id, dest_sub_path, &items);
        // 记下"这次发起的 transfer_id", 供 5 秒撤销窗口查询。
        // 必须在**发送前**记: 撤销可能在下一次 await 里就发生。
        if let Ok(mut m) = self.last_outgoing.lock() {
            m.insert(
                target_device_id.to_string(),
                (resume_key.clone(), std::time::Instant::now()),
            );
        }
        // 用户在哈希还没算完时就点了撤销：编号现在才有，立刻补发给对端，
        // 同时让即将开始的发送循环在写清单之前停住。
        let aborted_during_hash = self
            .abort_pending
            .lock()
            .map(|mut s| s.remove(target_device_id))
            .unwrap_or(false);
        if aborted_during_hash {
            self.client.abort_outgoing(target_device_id);
            // 对端此时通常还没建暂存目录（清单还没发出去）。
            // 仍然补发一次：哈希若与「上一次已开传」撞车，对端靠这个标记停。
            // 失败不阻塞 —— 本机这一侧已经不会再写了。
            if let Err(e) = self
                .client
                .cancel_transfer(target_ip, target_port, target_device_id, &resume_key)
                .await
            {
                tracing::info!("撤销补发未送达（本机已停止发送）: {}", e);
            }
        }
        self.send_items_with_code(
            target_ip,
            target_port,
            target_device_id,
            target_device_name,
            items,
            grant_code,
            Some(resume_key),
            dest_sub_path,
        )
        .await
    }

    /// 计算断点续传用的稳定 key。
    ///
    /// = `BLAKE3( device_id | dest_sub_path | 每个 (相对路径, 文件 BLAKE3) )`
    ///
    /// ## 为什么必须是内容哈希而不是"随便一个 id"
    ///
    /// key 的唯一职责是回答一个问题：**"这批文件和上次是同一批吗？"**
    /// - 用随机 id：每次都不同 ⇒ 续传永不触发（早先的实现就是这样，
    ///   代码完整但功能 100% 不生效）；
    /// - 用"文件路径 + 大小 + 时间戳"：改内容但大小不变时会误判成同一批
    ///   ⇒ **跳过一个用户刚更新的文件**，且用户永远查不出来；
    /// - 用内容哈希：改了就是改了，没改就是没改。
    ///
    /// 代价是要读一遍每个文件算哈希。传输本身也要读一遍，
    /// 所以这让"预扫描"阶段多一倍的磁盘读 —— 对 1 GiB 文件是秒级，
    /// 换来的是"绝不误跳过"这个不可退让的正确性。
    ///
    /// ## `dest_sub_path` 必须参与，否则换个落点就静默跳过（§7.7）
    ///
    /// key 不含落点时，同一批文件发给同一台设备、但指定不同落点，
    /// 会得到**同一个** transfer_id。接收端 `plan_resume` 拿它去查
    /// `transfer_parts`，查到上一次的记录，而复核判据是
    /// "该相对路径下的文件哈希一致" —— 它读的是**上次的落点**那个文件，
    /// 确实存在、确实一致，于是判定"已完整接收"并跳过。
    ///
    /// 用户看到的：穿梭里把 `a.csv` 拖到对方 `2026/`，
    /// 对方提示"已跳过 1 个此前已完整接收的文件"，
    /// 而 `2026/a.csv` **从来没有出现过**。
    /// 这比"不续传"糟得多：不续传最多多传一次，这个是**静默少一个文件**。
    ///
    /// 归一化后再算：客户端已经做过 `\` → `/` 与 trim，
    /// 但 `2026` 与 `2026/`、`./2026` 应当等价，不该让它们分成两次传输。
    pub fn compute_resume_key(
        target_device_id: &str,
        dest_sub_path: &str,
        items: &[(PathBuf, String)],
    ) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(target_device_id.as_bytes());
        hasher.update(b"|");
        hasher.update(Self::canonical_dest_key(dest_sub_path).as_bytes());
        hasher.update(b"|");
        // 排序后再算：同一批文件以不同顺序拖入应当得到同一个 key，
        // 否则用户"换个顺序重发"就会全量重传。
        let mut sorted: Vec<&(PathBuf, String)> = items.iter().collect();
        sorted.sort_by(|a, b| a.1.cmp(&b.1));
        for (path, rel) in sorted {
            hasher.update(rel.as_bytes());
            hasher.update(b"=");
            // 读不到就退回"大小 + 路径" —— 宁可不续传，也不误跳过
            match std::fs::read(path) {
                Ok(bytes) => {
                    hasher.update(blake3::hash(&bytes).as_bytes());
                }
                Err(_) => {
                    hasher.update(b"?");
                    hasher.update(
                        std::fs::metadata(path)
                            .map(|m| m.len().to_le_bytes())
                            .unwrap_or([0u8; 8])
                            .as_slice(),
                    );
                }
            }
            hasher.update(b";");
        }
        hasher.finalize().to_hex().to_string()
    }

    /// 落点子路径的 key 归一形式（仅用于 [`Self::compute_resume_key`]）。
    ///
    /// 只做"同义写法归一"，**不做安全校验** —— 安全那部分由接收端
    /// `normalize_sub_path` 负责。这里多管一点就多一处可能与接收端
    /// 判定不一致的地方，而不一致的表现是"该重传的不重传"。
    ///
    /// 归一：去首尾空白与 `/`、折叠重复分隔符、去掉 `.` 段。
    fn canonical_dest_key(dest_sub_path: &str) -> String {
        // 先把归一后的整体字符串绑到变量上，再借用它切分：
        // 直接在链上 `.split('/')` 会借用一个**临时 String**，
        // 编译期就报 E0716（临时值在借用点之前被释放）。
        let normalized = dest_sub_path.trim().replace('\\', "/");
        let segs: Vec<&str> = normalized
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        segs.join("/")
    }

    /// 内部：带码 + 稳定 transfer_id 发送已归一化的 `(路径, 目标相对名)` 列表。
    async fn send_items_with_code(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        target_device_name: &str,
        files: Vec<(PathBuf, String)>,
        grant_code: &str,
        resume_key: Option<String>,
        dest_sub_path: &str,
    ) -> Result<u32> {
        let (max_records, retention_days) = {
            let cfg = self.config.read().await;
            (cfg.max_history_records, cfg.record_retention_days)
        };

        let file_count = files.len();
        // items 的第二项就是目标相对名, 直接用它做展示名 ——
        // 从路径再取一次 file_name 会在穿梭通道上显示成裸文件名,
        // 丢掉用户真正看到的目录层级（§7.7）。
        let first_name = files
            .first()
            .map(|(_, rel)| rel.clone())
            .unwrap_or_else(|| "未知文件".to_string());
        let total_size: u64 = files
            .iter()
            .map(|(p, _)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .sum();
        let file_paths: Vec<String> = files
            .iter()
            .map(|(p, _)| p.to_string_lossy().to_string())
            .collect();

        // 走带诊断的通道: 旧实现把 elapsed 算出来后丢弃, 导致"速度"无从取数
        let res = self
            .client
            .send_files_as_diag_full(
                target_ip,
                target_port,
                target_device_id,
                target_device_name,
                files,
                grant_code,
                resume_key.clone(),
                dest_sub_path,
            )
            .await;

        // 「需要传输码」是**协商回合**不是失败 —— 不该在历史里留一条
        // "传输失败" 记录。用户接下来会带上码重试, 那时才是真正的一次传输。
        if matches!(res, Err(FeisuoError::GrantCodeRequired(_))) {
            return res.map(|_| 0);
        }
        // 本机撤销不是失败，但不能当没发生过。写「失败」会让人以为没撤成，
        // 什么都不写则关掉窗口后无从对账。记成「已取消」。
        let locally_cancelled =
            matches!(res, Err(FeisuoError::LocallyAborted { .. } | FeisuoError::Cancelled));

        let status = if locally_cancelled {
            "cancelled"
        } else if res.is_ok() {
            "completed"
        } else {
            "failed"
        };
        let label = if file_count > 1 {
            format!("{} 等 {} 个文件", first_name, file_count)
        } else {
            first_name
        };
        let report = res.as_ref().ok();
        // 完整诊断落库, 供事后导出分析（§9.7.6 的链路信息展示也依赖它）
        if let Some(r) = report {
            if let Err(e) = self.trust_store.insert_diagnostics(&r.diag) {
                tracing::warn!("写入传输诊断失败: {}", e);
            }
        }
        let metrics = match report {
            Some(r) => r.metrics,
            None => crate::security::TransferMetrics {
                declared_size: total_size,
                ..Default::default()
            },
        };
        let bytes_recorded = match &res {
            Ok(r) => r.bytes_sent,
            Err(FeisuoError::LocallyAborted { bytes_sent, .. }) => *bytes_sent,
            Err(_) => 0,
        };
        let _ = self.trust_store.add_transfer_record(
            &label,
            bytes_recorded,
            "send",
            target_device_name,
            target_ip,
            status,
            max_records,
            retention_days,
            metrics,
            &file_paths,
        );

        // 断点续传（P1 ⑪）：把跳过的文件追加到传输记录标签里。
        //
        // 不这么做的话历史里会显示"发送了 2 个文件 3.2MB"，
        // 而用户明明拖了 10 个 —— 他会以为其余 8 个丢了，
        // 于是反复重传。**记录必须反映实际发生了什么**。
        let mut skipped = 0u32;
        let res = res.map(|r| {
            skipped = r.skipped.len() as u32;
            if skipped > 0 {
                info!(
                    "{}（断点续传跳过 {} 个已存在文件：{}）",
                    label,
                    skipped,
                    r.skipped.join("、")
                );
            }
        });
        res.map(|_| skipped)
    }

    pub async fn pair_with_device(
        &self,
        target_ip: &str,
        target_port: u16,
        pin: &str,
    ) -> Result<TrustedDevice> {
        self.client
            .pair_with_device(target_ip, target_port, pin, self.trust_store.clone())
            .await
    }

    pub async fn probe_device(&self, ip: &str) -> Result<DiscoveredDevice> {
        self.discovery.probe_ip(ip).await
    }

    /// 浏览对端目录 (双栏穿梭右栏，§7.2)。
    ///
    /// `target` 携带卷 / 卷内相对路径 / 分页游标。`volume` 留空时
    /// 走 1.x 语义（只看对端收件目录镜像），用于与旧版本对端互通。
    pub async fn list_remote_files(
        &self,
        target_ip: &str,
        target_port: u16,
        target: &crate::transport::BrowseTarget,
    ) -> Result<RemoteBrowseListing> {
        self.client
            .list_remote_files(target_ip, target_port, target)
            .await
    }

    /// 带**本次传输码**的浏览（§2.3）。
    ///
    /// 对端处于「每次匹配码」等级时返回 [`FeisuoError::GrantCodeRequired`]，
    /// 宿主弹码后用本方法带码重试。
    ///
    /// ## 为什么必须有这个入口（而不是让宿主自己去改 `BrowseTarget`）
    ///
    /// 旧实现只有 [`Self::list_remote_files`] 一个入口，而
    /// `BrowseTarget.grant_code` 字段是有的 —— 于是宿主**能填却没地方填**：
    /// 浏览在「每次匹配码」等级下会拿到 `GrantCodeRequired`，但除了
    /// 原地拼一个 `BrowseTarget` 再调回去（每个调用点各写一遍）之外
    /// 没有正规路径。
    ///
    /// 取回与传输都有 `_with_code` 变体（见 [`Self::request_pull_with_code`] /
    /// [`Self::send_files_with_code`]），浏览独缺 —— 这正是
    /// 「一个功能被实现三遍、其中一遍没接通」的典型形态。
    pub async fn list_remote_files_with_code(
        &self,
        target_ip: &str,
        target_port: u16,
        target: &crate::transport::BrowseTarget,
        grant_code: &str,
    ) -> Result<RemoteBrowseListing> {
        let mut t = target.clone();
        t.grant_code = grant_code.trim().to_string();
        self.list_remote_files(target_ip, target_port, &t).await
    }

    /// 1.x 语义浏览（只看对端收件目录）。保留它是为了**显式**降级，
    /// 而不是让上层各处自己判断"要不要传 volume"——那种判断散在 UI 里
    /// 一定会漏，漏一处就是"点开对方 D 盘却只看到收件目录"。
    pub async fn list_remote_files_legacy(
        &self,
        target_ip: &str,
        target_port: u16,
        sub_path: &str,
    ) -> Result<RemoteBrowseListing> {
        self.client
            .list_remote_files_legacy(target_ip, target_port, sub_path)
            .await
    }

    /// 请求对端把指定文件推送回本机 (双栏穿梭"取回")。
    /// `sub_paths` 每一项是相对落盘根目录的路径, 可以含子目录层级。
    /// `dest_sub_path` 指定本机落点子目录（§7.7），空串 = 落收件根。
    pub async fn request_pull(
        &self,
        target_ip: &str,
        target_port: u16,
        sub_paths: Vec<String>,
        dest_sub_path: &str,
    ) -> Result<String> {
        self.request_pull_with_code(target_ip, target_port, sub_paths, dest_sub_path, "")
            .await
    }

    /// 带**本次传输码**的取回（§2.3）。
    ///
    /// 对端处于「每次匹配码」等级时返回 `GrantCodeRequired`，
    /// 宿主弹码后带码重试。
    pub async fn request_pull_with_code(
        &self,
        target_ip: &str,
        target_port: u16,
        sub_paths: Vec<String>,
        dest_sub_path: &str,
        grant_code: &str,
    ) -> Result<String> {
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
    ///
    /// `sub_paths` 里**可以放目录**：对端会递归展开并保留目录层级，
    /// 每个展开出来的文件**单独**过一次访问范围判定。
    pub async fn request_pull_in_volume(
        &self,
        target_ip: &str,
        target_port: u16,
        sub_paths: Vec<String>,
        dest_sub_path: &str,
        grant_code: &str,
        volume: &str,
    ) -> Result<String> {
        self.client
            .request_pull_in_volume(
                target_ip,
                target_port,
                sub_paths,
                dest_sub_path,
                grant_code,
                volume,
            )
            .await
    }

    /// 提交一次审批结果。
    ///
    /// **审批人只决定"允许一次 / 允许并永久信任 / 拒绝"，不输入任何码。**
    /// 「每次匹配码」等级下要出示的码是**本机生成、显示在审批窗口上**的，
    /// 由发起方从自己的界面敲回来 —— 见
    /// `server::ApprovalManager::issue_challenge` 里关于方向的论证。
    pub fn respond_approval(&self, approval_id: &str, action: ApprovalAction) -> bool {
        self.server.resolve_approval(approval_id, action)
    }

    pub fn list_transfer_records(&self, limit: u32) -> Result<Vec<TransferRecord>> {
        self.trust_store.list_transfer_records(limit)
    }

    pub fn clear_transfer_records(&self) -> Result<()> {
        self.trust_store.clear_transfer_records()
    }

    pub fn list_trusted_devices(&self) -> Result<Vec<TrustedDevice>> {
        self.trust_store.list_devices()
    }

    /// 解除配对（§14.5）：**双方**都变成未信任。
    ///
    /// ## 返回值语义（宿主必须照此告知用户）
    ///
    /// `(peer_notified, note)`：
    /// - `peer_notified = true` —— 对端也解除了，**双向**完成；
    /// - `peer_notified = false` —— 本机已降级，但**对方未收到通知**
    ///   （关机 / 离线 / 版本过旧无共同世代）。此时**必须**告知用户，
    ///   否则他以为"已经断干净了"，而对方仍可能继续给他静默投文件。
    ///
    /// ## 为什么**没有**"只删本地记录"的那个方法
    ///
    /// 早先这里还有一个 `remove_trusted_device`，理由是
    /// "保留它是为了让『只想删本地记录、暂不打扰对端』成为可能"。
    /// 查过之后删掉了，三个理由：
    ///
    /// 1. **没有任何调用方。** 界面走的是本方法（双向），Android 侧
    ///    还没接这个动作。于是它是一个**已注册但无人使用**的
    ///    Tauri 命令 —— 也就是说 WebView 里任何 JS 都能调它，
    ///    而它做的正是 §14.5 要消灭的那件事：**单方面断开**。
    /// 2. **它连"隐藏偏好"一起删。** `remove_device` 是 `DELETE`，
    ///    于是「隐藏 + 解除配对」之后那台设备会重新出现在主列表
    ///    （§14.3.1 的 I9）。
    /// 3. **同一个用户动作有两种存储效果。** `unpair_device` 命令里
    ///    "找不到对方地址"的兜底分支当时调的就是 `remove_trusted_device`
    ///    —— 于是**有没有 IP 决定了行是被 UPDATE 还是被 DELETE**：
    ///    有 IP → 行保留、`trust_level=pending`、设备留在名册里显示未信任；
    ///    没 IP → 行消失、设备从名册里**人间蒸发**、隐藏偏好丢失。
    ///    同一个按钮，两种结果，取决于一个用户看不见也控制不了的变量。
    ///
    /// 与 `blocked` 那一轮同一个教训：**删掉一条冗余通路，
    /// 比让它和另一条"保持一致"更彻底** —— 后者只是把不一致挪了个位置。
    ///
    /// 返回 [`crate::transport::UnpairReport`] 而不是 `(bool, String)`：
    /// 界面必须能区分「双向完成」「对方离线」「对方版本过旧」
    /// 「对方已重新绑定」—— 这些用户动作完全不同，只回一句话的话
    /// 界面就得靠 `includes()` 去猜，那正是本轮刚拆掉的反模式。
    pub async fn unpair_device(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
    ) -> Result<crate::transport::UnpairReport> {
        self.client
            .unpair_device(
                target_ip,
                target_port,
                target_device_id,
                self.trust_store.clone(),
            )
            .await
    }

    /// 「找不到对方地址」时的兜底：只能本地降级。
    ///
    /// ## 为什么它必须在 core 而不是宿主层
    ///
    /// 早先这个分支住在 Tauri 命令里，而它当时调的是 `DELETE` 那条
    /// 通路 —— 于是**有没有 IP 决定了行是被 UPDATE 还是被 DELETE**，
    /// 而这个差异**只能在源码里看出来、测不出来**：
    /// 命令函数要 `State<'_, AppState>`，集成测试够不到。
    ///
    /// 把它挪进 core 之后，"解除配对不会删掉这一行"就成了一条
    /// **可以断言的行为**，而不是一条只能靠 code review 保证的纪律。
    /// 守卫 `unpair_local_only_keeps_the_row_and_the_hidden_preference`。
    ///
    /// 语义与 [`FeisuoEngine::unpair_device`] 的本地降级部分**完全一致**
    /// （`UPDATE` → `pending`、保留行、保留 `visible`、清掉授权），
    /// 差别只在于**没有尝试通知对端**，且如实回报 `peer_notified = false`。
    pub fn unpair_device_local_only(
        &self,
        device_id: &str,
    ) -> Result<crate::transport::UnpairReport> {
        let local_applied = self.trust_store.downgrade_to_untrusted(device_id)?;
        let mut report = crate::transport::UnpairReport::local_only(
            "no_known_endpoint",
            String::new(),
        );
        report.local_applied = local_applied;
        report.build_user_message();
        Ok(report)
    }

    /// 撤销某台设备的全部短期授权（§2.3.1 的「可撤销」）。
    ///
    /// 返回被撤销的项数。**这是安全能力，不是便利功能** ——
    /// 没有撤销手段的授权只是"用户不知道它还在"。
    pub fn revoke_device_grants(&self, device_id: &str) -> Result<u32> {
        self.trust_store.revoke_session_grant(device_id, "")
    }

    /// 列出**全部设备**当前生效中的短期授权（§2.3.1）。
    ///
    /// 返回 `device_id → [授权…]`。界面必须能看到"这台设备现在免确认到
    /// 什么时候" —— 看不见的状态不该存在。
    pub fn list_active_grants(&self) -> Result<std::collections::HashMap<String, Vec<ActiveGrant>>> {
        let mut out = std::collections::HashMap::new();
        for d in self.trust_store.list_devices()? {
            let grants = self.trust_store.list_active_grants(&d.device_id)?;
            if grants.is_empty() {
                continue;
            }
            out.insert(
                d.device_id.clone(),
                grants
                    .into_iter()
                    .map(|(scope, expires_at)| ActiveGrant {
                        scope_label: grant_scope_label(&scope).to_string(),
                        scope,
                        expires_at,
                    })
                    .collect(),
            );
        }
        Ok(out)
    }

    /// 实际生效的授权窗口秒数（0 = 功能关闭）。
    ///
    /// 宿主层把它报给前端，**前端不许自己硬编码** ——
    /// core 那边有硬上界，前端写死会在用户调大配置时说谎。
    pub async fn session_grant_window(&self) -> u64 {
        let cfg = self.config.read().await;
        crate::config::effective_session_grant_ttl(cfg.session_grant_ttl_secs).unwrap_or(0)
    }

    /// 把一台设备降回**未信任**，保留它这一行（§14.3.1 的 I8 / I9）。
    ///
    /// 只在 [`FeisuoEngine::unpair_device_local_only`] 内部使用；
    /// 宿主层请走那个方法，好让"解除配对"的兜底路径**只有一条**。
    pub fn downgrade_to_untrusted(&self, device_id: &str) -> Result<bool> {
        self.trust_store.downgrade_to_untrusted(device_id)
    }

    // =======================================================================
    // 诊断 / 信任 / 范围（供宿主层命令转发）
    // =======================================================================

    /// 导出传输诊断报告（用户复现问题后交给开发者的标准通路，§9.7）。
    pub fn export_diagnostics_report(&self, limit: u32) -> Result<String> {
        self.trust_store
            .export_diagnostics_report(limit, &self.identity.device_id)
    }

    /// 读取最近的传输诊断记录。
    pub fn list_diagnostics(&self, limit: u32) -> Result<Vec<TransferDiagnostics>> {
        self.trust_store.list_diagnostics(limit)
    }

    /// 读取安全事件（§3.9）。
    pub fn list_security_events(&self, limit: u32) -> Result<Vec<SecurityEvent>> {
        self.trust_store.list_security_events(limit)
    }

    /// 设置对端信任等级（§2.3）。
    pub fn set_device_trust_level(&self, device_id: &str, level: TrustLevel) -> Result<()> {
        self.trust_store.set_trust_level(device_id, level)
    }

    /// 设置设备可见性（§3）。
    pub fn set_device_visible(&self, device_id: &str, visible: bool) -> Result<()> {
        self.trust_store.set_visible(device_id, visible)
    }

    /// 已隐藏设备列表（「已隐藏」抽屉，§3.1）。
    pub fn list_hidden_devices(&self) -> Result<Vec<TrustedDevice>> {
        self.trust_store.list_hidden_devices()
    }

    /// 构建设备名册：在线表 ∪ 信任库（§3.6）。
    ///
    /// 侧栏应当消费这个结果而不是 `get_online_devices()` ——
    /// 后者只有 20 秒 TTL，信任设备离线即消失。
    pub async fn get_roster(&self) -> Result<Vec<DeviceRosterEntry>> {
        let online = self.discovery.get_online_devices().await;
        let trusted = self.trust_store.list_devices()?;
        let now = chrono::Utc::now().timestamp();
        Ok(crate::roster::build_roster(
            &online,
            &trusted,
            &self.identity.device_id,
            now,
        ))
    }

    /// 读取对端可访问范围（§8）。
    pub fn get_access_scope(&self, peer_id: &str) -> Result<AccessScope> {
        self.trust_store.get_access_scope(peer_id)
    }

    /// 写入对端可访问范围（§8）。放宽时自动记安全事件。
    pub fn set_access_scope(&self, peer_id: &str, scope: &AccessScope) -> Result<()> {
        self.trust_store.set_access_scope(peer_id, scope)
    }

    /// 请求对端撤销本机发起的传输（直发撤销，§5.2）。
    ///
    /// 返回 `Ok(false)` = 对端已开始落盘，撤不掉。调用方必须如实告知用户。
    ///
    /// ## `transfer_id` 为空时怎么办
    ///
    /// 回落到 [`FeisuoEngine::last_outgoing_transfer_id`] 记录的"最近一次
    /// 发起"。**绝不**拿设备 id 之类的东西占位 —— 那会让撤销请求
    /// 匹配不到任何东西却回"成功"，用户以为撤销了而文件照样落地。
    /// 两个来源都没有时返回明确错误，让 UI 如实说"无法撤销"。
    pub async fn cancel_transfer(
        &self,
        target_ip: &str,
        target_port: u16,
        target_device_id: &str,
        transfer_id: &str,
    ) -> Result<bool> {
        // 先让**本机**发送循环停。这一步不依赖 transfer_id，
        // 所以用户在「进度条还没出现」时点撤销也有效 ——
        // 早先这个窗口里 UI 直接谎报成功，发送任务照常跑完。
        self.client.abort_outgoing(target_device_id);

        let tid = if transfer_id.trim().is_empty() {
            match self.last_outgoing_transfer_id(target_device_id) {
                Some(t) => t,
                None => {
                    // 编号还没算出来（整文件哈希进行中）。记下来，
                    // 等 `send_files_to_dest_inner` 算出 key 立刻补发取消。
                    // 本机这一侧已经不会再把字节写出去。
                    if let Ok(mut s) = self.abort_pending.lock() {
                        s.insert(target_device_id.to_string());
                    }
                    return Ok(true);
                }
            }
        } else {
            transfer_id.trim().to_string()
        };
        self.client
            .cancel_transfer(target_ip, target_port, target_device_id, &tid)
            .await
    }

    /// 本机最近一次向 `peer_id` 发起传输的 `transfer_id`。
    ///
    /// 超过 `max_age` 的一律视为"没有" —— 撤销窗口只有 5 秒,
    /// 拿一小时前的 id 去撤销毫无意义, 而它匹配上的概率也接近零。
    pub fn last_outgoing_transfer_id(&self, peer_id: &str) -> Option<String> {
        const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(120);
        let m = self.last_outgoing.lock().ok()?;
        let (tid, at) = m.get(peer_id)?;
        if at.elapsed() > MAX_AGE {
            return None;
        }
        Some(tid.clone())
    }

    /// 清理过期会话授权。由引擎启动后定时调用。
    pub fn purge_expired_session_grants(&self) {
        if let Err(e) = self.trust_store.purge_expired_grants() {
            tracing::warn!("清理过期会话授权失败: {}", e);
        }
    }

    /// 清理过期的断点续传记录（P1 ⑪）。
    ///
    /// 保留期与传输历史一致。**主要动机是隐私**：`committed_path`
    /// 记录了用户收件文件的完整路径，无限期留着等于把一份
    /// "用户收过什么文件、放在哪"的清单长期存盘。
    pub fn purge_completed_parts(&self) {
        let days = {
            let cfg = self.config.try_read();
            // 保留期是 u32，转换时给个下限 1：0 天意味着"每次启动都清空
            // 全部断点记录"，断点续传就等于从未生效过。
            (cfg.map(|c| c.record_retention_days).unwrap_or(30) as i64).max(1)
        };
        match self.trust_store.purge_completed_parts(days) {
            Ok(n) if n > 0 => {
                tracing::info!("已清理 {} 条过期断点续传记录（保留 {} 天）", n, days);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("清理断点续传记录失败: {}", e),
        }
    }
}
