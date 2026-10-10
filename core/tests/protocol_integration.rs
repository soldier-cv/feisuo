//! 飞梭核心引擎端到端集成验证。
//!
//! 目的: 协议层在安全加固中被大幅改动 (清单签名、帧长度上限、路径穿越防护、
//! 新增目录浏览 / 取回消息、拒绝必须出站等), 编译通过并不等于能跑通。
//! 本文件在**同一进程内起两个完全独立的节点**, 覆盖:
//!   配对 -> 发送 -> 接收 -> 目录浏览 -> 取回 -> 各类攻击 / 异常路径。
//!
//! 隔离性: 全部使用系统临时目录 + 127.0.0.1 + 操作系统临时端口,
//! 不触碰任何真实配置 / 真实文件 / 外部网络, 结束后清理临时目录。

use std::path::PathBuf;
use std::sync::{Arc, Once};
use std::time::Duration;

use feisuo_core::config::DEFAULT_TRANSFER_PORT;
use feisuo_core::sanitize_device_name;
use feisuo_core::security::DeviceIdentity;
use feisuo_core::{
    AppConfig, ApprovalAction, ApprovalRequest, FeisuoEngine, TransferProgress, TrustStore,
    TrustedDevice, DISCOVERY_MULTICAST_ADDR,
};
use tokio::sync::{broadcast, RwLock};

const PAIR_TIMEOUT: Duration = Duration::from_secs(8);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(12);

/// 初始化 tracing 订阅。
///
/// 没有它, 传输失败只会写进日志然后被静默吞掉 —— 测试里只能看到
/// "等待超时", 完全看不出是哪一环断的, 排查成本极高。
/// 设为 `FEISUO_TEST_LOG=1` 才真正输出, 默认保持安静。
fn init_tracing() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var("FEISUO_TEST_LOG").is_ok() {
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .with_test_writer()
                .try_init();
        }
    });
}

/// 一个测试节点: 引擎 + 目录 + 事件接收端
struct Node {
    engine: Arc<FeisuoEngine>,
    dir: PathBuf,
    /// 构造时固化, 避免在 async 上下文里调用 blocking_read
    port: u16,
    inbox_dir: PathBuf,
    progress_rx: broadcast::Receiver<TransferProgress>,
    approval_rx: broadcast::Receiver<ApprovalRequest>,
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "feisuo-it-{}-{}-{}",
        tag,
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).expect("创建临时目录");
    d
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("分配临时端口")
        .local_addr()
        .unwrap()
        .port()
}

/// 发现测试的互斥锁。
/// 需要发现服务的用例会在**独立回环地址**上各起一个节点, 但它们共用同一个
/// 发现端口 —— 而 `127.255.255.255` 广播是能送达所有回环地址的, 于是
/// 上一组用例的节点会收到下一组的信标并抢走应答。
/// 串行化是这里唯一正确的做法: 发现测试之间共享全局网络状态。
fn discovery_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        // 上一个用例 panic 时不要把锁一起毒死
        .unwrap_or_else(|e| e.into_inner())
}

/// 整个测试进程共用一个发现端口。
/// UDP 广播是"发到本节点配置的那个端口", 端口不一致则互相不可见;
/// 真实部署里所有设备都是同一个 42101, 这里也必须一致。
fn shared_discovery_port() -> u16 {
    use std::sync::OnceLock;
    static PORT: OnceLock<u16> = OnceLock::new();
    *PORT.get_or_init(|| free_port())
}

impl Node {
    /// 构造节点; `listen` 为 true 时同时拉起 TCP 传输服务
    async fn spawn(tag: &str, listen: bool) -> Node {
        Self::spawn_with_bind(tag, listen, "127.0.0.1").await
    }

    /// 指定发现服务绑定地址的构造方式。
    /// 单机多实例必须绑到不同回环地址, 否则第二个实例绑同一端口必然失败。
    async fn spawn_with_bind(tag: &str, listen: bool, discovery_bind: &str) -> Node {
        init_tracing();
        let dir = temp_dir(tag);
        let identity = Arc::new(
            DeviceIdentity::load_or_generate_at(dir.join("device_identity.key"))
                .expect("生成测试身份"),
        );
        let trust_store = Arc::new(
            TrustStore::open_at(dir.join("trust_store.db")).expect("打开测试信任库"),
        );

        // 传输端口交给操作系统分配, 避免与真实运行中的飞梭冲突。
        // 发现端口必须**各节点相同**: UDP 广播是"发到本节点配置的那个端口",
        // 两端端口不一致就永远互相看不见 —— 生产环境里所有节点都用 42101,
        // 测试也必须照这个前提来。
        let receive_dir = dir.join("inbox");
        std::fs::create_dir_all(&receive_dir).expect("创建落盘目录");
        let cfg = AppConfig {
            device_name: format!("node-{}", tag),
            transfer_port: free_port(),
            discovery_port: shared_discovery_port(),
            auto_receive: true,
            autostart: false,
            receive_dir,
            log_level: "ERROR".into(),
            max_log_size_mb: 1,
            max_history_records: 200,
            record_retention_days: 30,
            max_concurrent_transfers: 3,
            transfer_bind: "127.0.0.1".into(),
            discovery_bind: discovery_bind.into(),
            close_action: "ask".into(),
            theme: "dark".into(),
            auto_check_update: false,
            allowed_peer_subnets: Vec::new(),
            approval_timeout_secs: 60,
            session_grant_ttl_secs: 300,
        };

        let handles =
            FeisuoEngine::assemble(identity, Arc::new(RwLock::new(cfg)), trust_store)
                .expect("组装引擎");
        let (engine, _disc_rx, progress_rx, approval_rx) = handles.into_tuple();

        let engine = Arc::new(engine);
        // 构造期间固化端口与落盘目录, 避免在 async 上下文里调用 blocking_read
        let (port, inbox_dir) = {
            let cfg = engine.config.read().await;
            (cfg.transfer_port, cfg.receive_dir.clone())
        };
        let node = Node {
            port,
            inbox_dir,
            engine,
            dir,
            progress_rx,
            approval_rx,
        };
        if listen {
            node.engine.server.start().await.expect("启动传输服务");
        }
        node
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn inbox(&self) -> PathBuf {
        self.inbox_dir.clone()
    }

    /// `epoch` 是配对握手协商出的共同世代。
    ///
    /// 传空串等于"绕过协商"，那种情况下双向解除配对会明确拒绝
    /// （`no_shared_epoch`）—— 与真实运行一致。
    fn public_with_epoch(&self, epoch: &str) -> TrustedDevice {
        let mut d = self.public();
        d.pairing_epoch = epoch.to_string();
        d
    }

    fn public(&self) -> TrustedDevice {
        TrustedDevice {
            device_id: self.engine.identity.device_id.clone(),
            device_name: format!("node-{}", self.dir.file_name().unwrap().to_string_lossy()),
            public_key_hex: self.engine.identity.public_key_hex(),
            last_ip: "127.0.0.1".into(),
            bound_at: chrono::Utc::now().to_rfc3339(),
            is_trusted: true,
            trust_level: feisuo_core::security::TrustLevel::Permanent,
            visible: true,
            last_seen_at: chrono::Utc::now().timestamp(),
            pairing_epoch: String::new(),
        }
    }

    fn write_out(&self, name: &str, size: usize) -> PathBuf {
        write_test_file(&self.dir.join("out"), name, size)
    }

    /// 在独立子目录里写同名文件, 用于验证"两个同名文件同时在途"的场景
    fn write_out_isolated(&self, tag: &str, name: &str, size: usize) -> PathBuf {
        write_test_file(&self.dir.join("out").join(tag), name, size)
    }
}

fn write_test_file(dir: &std::path::Path, name: &str, size: usize) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    // 确定性内容, 便于逐字节校验往返一致
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    std::fs::write(&p, &data).expect("写入测试文件");
    p
}

fn cleanup(nodes: &[Node]) {
    for n in nodes {
        let _ = std::fs::remove_dir_all(&n.dir);
    }
}

/// 建立双向信任: a 走完整配对握手, b 侧登记 a 的公钥
async fn setup_pair(a: &Node, b: &Node) {
    let (_b_pin, _) = b.engine.trust_store.generate_pair_pin();
    // b 生成配对码 -> a 用它发起配对 -> b 侧绑定 a
    let paired = a
        .engine
        .pair_with_device("127.0.0.1", b.port(), &_b_pin)
        .await
        .expect("配对应当成功");
    assert_eq!(paired.device_id, b.engine.identity.device_id);

    // 反向: b 侧把 a 登记为受信设备, 这样 a -> b 的传输可以静默接收。
    //
    // **必须带上 A 侧收到的那个世代** —— 否则两边存的不是一个串,
    // 双向解除配对会以 `no_shared_epoch` 拒绝, 而测试会误以为是功能坏了。
    // 这个测试辅助函数此前直接 `bind_device(&a.public())`（世代为空）,
    // 是一处"测试捷径与真实握手不一致"的坑。
    let negotiated_epoch = a
        .engine
        .trust_store
        .pairing_epoch(&b.engine.identity.device_id)
        .unwrap()
        .expect("配对握手后 A 侧应已记录共同世代");
    b.engine
        .trust_store
        .bind_device(&a.public_with_epoch(&negotiated_epoch))
        .expect("b 侧登记 a");
    b.engine
        .trust_store
        .update_last_ip(&a.engine.identity.device_id, "127.0.0.1")
        .expect("更新 a 的最近 IP");
    // a 侧记录 b 的公钥(配对握手已写入), 这里补一次 IP 刷新
    a.engine
        .trust_store
        .update_last_ip(&b.engine.identity.device_id, "127.0.0.1")
        .expect("更新 b 的最近 IP");
}

/// **重新配对必须恢复信任**（§14.11 决策 5）。
///
/// 这条守的是一个真实缺陷：`bind_device` 的 Refreshed 分支
/// （"device_id 与公钥都没变"）刻意**不**写回 `trust_level`，
/// 理由写在代码注释里 —— "用户可能已把它降级为 session"。
/// 那个理由对**没被解除过**的设备是对的，但对**被解除过**的设备是错的：
///
/// 1. A ↔ B 配对（两侧 `permanent`）；
/// 2. A 解除配对 → **双方**降级为 `pending`（§14.5 的双向语义）；
/// 3. B 重新走完整配对仪式 → 服务端按 §2.5 给出默认档 `permanent`；
/// 4. 但 `bind_device` 走 Refreshed，把 `permanent` **丢掉** ⇒ 仍是 `pending`。
///
/// 用户看到的是"配对成功"，设备却还是「未信任」，
/// 之后每一次传输都要重新弹审批 —— 而**没有任何界面能改这个状态**
/// （`set_device_trust_level` 只接受 `permanent` / `session`，
/// 而 `pending` 设备在 UI 上根本不给"提升信任"的入口）。
///
/// 判据不是"无脑覆盖"，而是**区分 `pending` 的两种来源**：
///
/// - `pending` **不是**一个用户可选的档位 —— `set_device_trust_level`
///   只接受 `permanent` / `session`。它只由两个地方产生：
///   建行时的默认值、以及解除配对。两种都不是"用户的选择"。
/// - `session` **是**用户的选择，降级后重新配对必须保留。
///
/// 所以规则是：**仅当现存档位是 `pending` 时才写回配对时选定的档位。**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repair_after_unpair_restores_trust_but_keeps_user_downgrade() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    // 注意方向: `b_view_of_a` 是 **a 的 device_id**, 查的是 **b 的库**。
    // 写成 b 自己的 id 会去 b 的库里查 b —— 永远查不到。
    let a_in_b = a.engine.identity.device_id.clone();
    let b_in_a = b.engine.identity.device_id.clone();

    let level_of = |n: &Node, id: &str| {
        n.engine
            .trust_store
            .trust_level(id)
            .unwrap()
            .map(|l| l.as_db_str().to_string())
    };
    assert_eq!(level_of(&b, &a_in_b).as_deref(), Some("permanent"));

    // ---- 第一半：解除配对把两边都降为 pending ----
    b.engine
        .trust_store
        .downgrade_to_untrusted(&a_in_b)
        .expect("b 侧降级 a");
    assert_eq!(
        level_of(&b, &a_in_b).as_deref(),
        Some("pending"),
        "解除配对后必须变成未信任"
    );

    // ---- 第二半：b 重新走完整配对仪式 ----
    // 这是一次**真实的网络握手**，不是直接调 bind_device ——
    // 因为"配对仪式"才是用户表达"我重新建立关系"的动作。
    let (_pin, _) = b.engine.trust_store.generate_pair_pin();
    let paired = a
        .engine
        .pair_with_device("127.0.0.1", b.port(), &_pin)
        .await
        .expect("重新配对应当成功");
    assert_eq!(paired.device_id, b_in_a);

    // a 侧：a 主动发起重新配对，走的是 handle_pair 的对端登记分支
    let epoch = a
        .engine
        .trust_store
        .pairing_epoch(&b_in_a)
        .unwrap()
        .expect("重新配对后应记录共同世代");
    b.engine
        .trust_store
        .bind_device(&a.public_with_epoch(&epoch))
        .expect("b 侧重新登记 a");

    assert_eq!(
        level_of(&a, &b_in_a).as_deref(),
        Some("permanent"),
        "重新配对后 a 侧必须恢复信任 —— 用户看到的是「配对成功」"
    );
    assert_eq!(
        level_of(&b, &a_in_b).as_deref(),
        Some("permanent"),
        "重新配对后 b 侧必须恢复信任，不能停在未信任"
    );

    // ---- 第三半：用户主动降级过的 session，重新配对**必须保留** ----
    // 这是另一半判据。没有它，上面的修复会退化成"重新配对就无脑提升信任"，
    // 那比原缺陷更糟 —— 它会静默推翻用户的明确选择。
    b.engine
        .trust_store
        .set_trust_level(&a_in_b, feisuo_core::security::TrustLevel::Session)
        .expect("b 侧把 a 降为每次认证");
    b.engine
        .trust_store
        .bind_device(&a.public_with_epoch(&epoch))
        .expect("a 再次配对（模拟设备重连）");
    assert_eq!(
        level_of(&b, &a_in_b).as_deref(),
        Some("session"),
        "用户自己选的「每次认证」不能被重新配对悄悄改回永久信任"
    );

    // ---- 第四半：`is_trusted` 兼容列必须与 `trust_level` 一致 ----
    // `is_trusted` 是旧模型的遗留列，但 `list_devices` **仍然把它读进 DTO**
    // （`trust_store.rs` 的 `is_trusted: row.get::<_, i32>(5)? == 1`）。
    // 两列一旦漂移，DTO 就会同时说两件互相矛盾的话，而调用方
    // 分不出哪句是权威的。
    //
    // 语义基准是 `is_paired()` —— **`session` 也算已绑定**
    // （它有 Ed25519 身份绑定，只是每次操作要码）。旧列的语义是
    // "能/不能"，在新模型下"能"= permanent | session。
    //
    // 写 `trust_level` 的每条路径都必须同步 `is_trusted` ——
    // 同一个事实两处真相，本轮修的全是这类。
    let dto = b
        .engine
        .trust_store
        .list_devices()
        .expect("读取设备列表")
        .into_iter()
        .find(|d| d.device_id == a_in_b)
        .expect("b 的库里应当有 a");
    assert_eq!(
        dto.is_trusted, dto.trust_level.is_paired(),
        "`is_trusted` 兼容列与 `trust_level` 漂移了：DTO 会同时说两件互相矛盾的话"
    );
    assert_eq!(
        dto.trust_level,
        feisuo_core::security::TrustLevel::Session,
        "本半段的场景是「每次认证」"
    );

    // 解除配对后两列必须**一起**掉到"未绑定"
    b.engine
        .trust_store
        .downgrade_to_untrusted(&a_in_b)
        .expect("再次解除配对");
    let dto2 = b
        .engine
        .trust_store
        .list_devices()
        .expect("读取设备列表")
        .into_iter()
        .find(|d| d.device_id == a_in_b)
        .expect("b 的库里应当仍有 a（行保留，只是不再信任）");
    assert_eq!(
        dto2.is_trusted, dto2.trust_level.is_paired(),
        "解除配对后 `is_trusted` 与 `trust_level` 又漂移了"
    );
    assert!(
        !dto2.is_trusted,
        "解除配对后 `is_trusted` 必须是 false（当前 {}）",
        dto2.is_trusted
    );

    cleanup(&[a, b]);
}

/// **「找不到对方地址」的兜底不得删掉这一行**（§14.11 决策 2 的兜底路径）。
///
/// 这条守的是一个**曾经真实存在、且只能靠 code review 才能发现**的缺陷：
/// `unpair_device` 命令里「找不到 IP」的分支走的是 `DELETE` 那条通路，
/// 于是
///
/// > **有没有 IP 决定了行是被 `UPDATE` 还是被 `DELETE`。**
///
/// 同一个「解除配对」按钮，两种结果，取决于一个用户看不见、
/// 也控制不了的变量：
///
/// | | 行 | `visible` | 名册里表现为 |
/// |:--|:---|:--:|:---|
/// | 有 IP | 保留，`trust_level=pending` | 保留 | 「未信任」灰态，用户可重新配对 |
/// | 没 IP | **被删** | **丢失** | **从主列表里人间蒸发** |
///
/// 第二行那个"用户刚隐藏的设备因为恰好没记住 IP 而重新出现"，
/// 是 §14.3.1 的 I9 已经修过一次的那个缺陷 —— 只不过从
/// 「解除配对」那条路又溜了进来。
///
/// **为什么这条能测**：兜底逻辑已挪进 core 的 `unpair_device_local_only`。
/// 留在 Tauri 命令里就测不到（命令函数要 `State<'_, AppState>`），
/// 于是那条差异只能靠人读代码发现。**能测的分支就不该留在测不到的地方。**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpair_local_only_keeps_the_row_and_the_hidden_preference() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    let a_in_b = a.engine.identity.device_id.clone();
    b.engine
        .trust_store
        .set_visible(&a_in_b, false)
        .expect("b 隐藏 a");

    // 没有 IP 的兜底（endpoint 传空 —— 与命令里 `ip.trim().is_empty()` 同一条路）
    let report = b
        .engine
        .unpair_device_local_only(&a_in_b)
        .expect("本地降级应当成功");

    assert!(report.local_applied, "本机确实降级了");
    assert!(!report.peer_notified, "对端没被通知 —— 必须如实回报");
    assert_eq!(
        report.reason_code, "no_known_endpoint",
        "原因码必须机器可读，界面靠它分支"
    );
    assert!(
        !report.user_message.trim().is_empty(),
        "必须给出人话，且不能声称已双向断开"
    );
    assert!(
        !report.user_message.contains("双方都不再信任"),
        "没通知到对端就不能说「双方都不再信任」—— 实际: {}",
        report.user_message
    );

    // ---- 核心断言：这一行必须在，且 `visible` 必须在 ----
    let devices = b.engine.trust_store.list_devices().expect("读取设备列表");
    let row = devices
        .iter()
        .find(|d| d.device_id == a_in_b)
        .expect("❌ 行被删掉了：解除配对不许删行（用户会以为设备凭空消失）");
    assert_eq!(
        row.trust_level,
        feisuo_core::security::TrustLevel::Pending,
        "必须降级为未信任"
    );
    assert!(
        !row.visible,
        "❌ 隐藏偏好被一并删掉了：那是**显示**偏好，与信任无关（I9）"
    );
    assert!(!row.trust_level.is_paired());

    // 隐藏抽屉里仍然找得到它 —— 用户随时可以取消隐藏
    let hidden = b
        .engine
        .trust_store
        .list_hidden_devices()
        .expect("读取已隐藏列表");
    assert!(
        hidden.iter().any(|d| d.device_id == a_in_b),
        "它应当在「已隐藏」抽屉里，而不是消失"
    );

    // 重新配对后仍能恢复（决策 5），前提是这一行还在
    let (_pin, _) = b.engine.trust_store.generate_pair_pin();
    a.engine
        .pair_with_device("127.0.0.1", b.port(), &_pin)
        .await
        .expect("重新配对应当成功");
    let epoch = a
        .engine
        .trust_store
        .pairing_epoch(&b.engine.identity.device_id)
        .unwrap()
        .expect("应记录共同世代");
    b.engine
        .trust_store
        .bind_device(&a.public_with_epoch(&epoch))
        .expect("b 侧重新登记 a");
    let after = b
        .engine
        .trust_store
        .list_devices()
        .expect("读取设备列表")
        .into_iter()
        .find(|d| d.device_id == a_in_b)
        .expect("行必须还在");
    assert_eq!(after.trust_level, feisuo_core::security::TrustLevel::Permanent);
    assert!(!after.visible, "重新配对**不**该把用户藏起来的设备放回主列表");

    // ---- 第五半：对一台**库里根本没有**的设备解除配对 ----
    // `local_applied` 必须是 false，且文案**不许**说成已解除。
    //
    // 这一半守的是"报告必须诚实"：宿主层按 `local_applied` 决定
    // 弹红色还是绿色 toast（`App.vue` 的 `removeTrusted`）。谎报成
    // true ⇒ 用户以为成功了，而实际上什么都没发生。
    let ghost = "no-such-device-0000";
    let ghost_report = b
        .engine
        .unpair_device_local_only(ghost)
        .expect("对不存在的设备也不该报错");
    assert!(
        !ghost_report.local_applied,
        "库里没有这一行，就不能说本机已降级 —— 界面会据此弹『已解除』"
    );
    assert!(
        !ghost_report.user_message.contains("已解除"),
        "没改动任何东西时，文案不许说『已解除』（实际: {}）",
        ghost_report.user_message
    );

    cleanup(&[a, b]);
}

/// **「同类操作短期授权」必须真的生效，且真的只对同类操作生效**（§2.3.1）。
///
/// 这条守的是**整个功能**，四段各自对应一条安全属性：
///
/// 1. **有效期内，同类操作不再要码。** 用户点「允许本次，并在 5 分钟内
///    免重复确认」之后，第二次发同一个 `session` 设备必须**直接成功**，
///    服务端**不弹审批**（`approval_rx` 收不到任何东西就是证据）。
/// 2. **跨类别不生效。** `receive` 的授权**不能**用来 `browse`。
///    这是本设计最关键的一条：授权是**按操作类别**的，不是"信任了"。
/// 3. **`AllowOnce` 不写授权。** 它的语义就是"只这一次" ——
///    写了就是偷偷加副作用。
/// 4. **配 0 = 功能关闭**，连授权都不写。
///
/// ## 为什么第 1 段要断言"服务端没弹审批"
///
/// 只断言"第二次成功了"是不够的 —— 成功可能有别的路径。
/// "服务端没弹窗"才是**用户真正感知到的那件事**：省掉了一次打断。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_grant_saves_the_second_prompt_but_only_for_the_same_op() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;
    let a_in_b = a.engine.identity.device_id.clone();
    // 提前取出标量：`async move` 会把 `b` 整个搬进协程，
    // 而后面还要用 `b.engine` 反复查授权。
    let b_port = b.port();

    // B 把 A 降为「每次认证」
    b.engine
        .trust_store
        .set_trust_level(&a_in_b, feisuo_core::security::TrustLevel::Session)
        .expect("降级为每次认证");

    // ---- 第一次：要码，且用户选了「免重复确认」 ----
    let mut rx = b.approval_rx.resubscribe();
    let send1 = tokio::spawn({
        let e = a.engine.clone();
        let id = b.engine.identity.device_id.clone();
        let s = a.write_out("batch-1.bin", 512);
        async move { e.send_files("127.0.0.1", b_port, &id, "node-B", vec![s]).await }
    });
    let req = tokio::time::timeout(Duration::from_secs(8), rx.recv())
        .await
        .expect("第一次应当弹审批")
        .expect("审批通道");
    assert!(
        req.grant_challenge.len() >= 6,
        "「每次认证」等级下必须出示码，实际 challenge={:?}",
        req.grant_challenge
    );
    b.engine
        .respond_approval(&req.approval_id, ApprovalAction::AllowWithGrant);
    let code = req.grant_challenge.trim().to_string();
    let err = send1
        .await
        .expect("发送任务结束")
        .expect_err("第一次没带码，应当是 GrantCodeRequired");
    assert!(
        matches!(err, feisuo_core::FeisuoError::GrantCodeRequired(_)),
        "第一次必须是协商回合，实际: {}",
        err
    );
    // ⚠️ 此刻**还没写授权** —— 用户已经点了按钮，但发起方还没出示码。
    // 授权只在**比码通过**后写（§2.3.1 的第 1 条安全性质）：
    // 否则"用户点了允许"就会变成一台没出示过码的设备拿到了免确认资格。
    assert!(
        !b.engine
            .trust_store
            .has_valid_grant(&a_in_b, "receive")
            .expect("查授权"),
        "❌ 用户点了按钮但对方还没出示码，授权就写下了 —— \
         免确认资格不该在核验之前发放"
    );

    // 带码重试 —— 这一轮才真正比码、才写授权。
    //
    // ⚠️ 重试是**一条全新连接**，所以服务端会**再弹一次**审批
    // （码因为指纹复用而相同，但 `approval_id` 是新的）。
    // 没人应答的话它会等到 30s 超时 —— 那正是「读取 握手应答 长度超时」。
    let mut rx_retry = b.approval_rx.resubscribe();
    let retry = tokio::spawn({
        let e = a.engine.clone();
        let id = b.engine.identity.device_id.clone();
        // **同名同大小、内容不同**，两个目的：
        //
        // 1. 请求指纹是 `op|file_count|total_size|first_file_name` ——
        //    同名同大小 ⇒ 指纹不变 ⇒ **码被复用**（真实重试就是这个前提；
        //    换名就换指纹、换指纹就换码，用户在两个窗口上永远对不上）。
        // 2. 断点续传的 transfer_id 是**内容哈希** ——
        //    内容不同 ⇒ key 不同 ⇒ 不会被当成"上次的同一批"而跳过。
        let s = a.dir.join("out").join("batch-1.bin");
        std::fs::write(
            &s,
            (0..512u32).map(|i| ((i as u8).wrapping_add(7)) as u8).collect::<Vec<u8>>(),
        )
        .expect("写入重试用的文件");
        let c = code.clone();
        async move { e.send_files_with_code("127.0.0.1", b_port, &id, "node-B", vec![s], &c).await }
    });
    let req_r = tokio::time::timeout(Duration::from_secs(8), rx_retry.recv())
        .await
        .expect("带码重试应当再次弹审批")
        .expect("审批通道");
    assert_eq!(
        req_r.grant_challenge.trim(),
        code,
        "同一个请求指纹必须复用同一个码 —— 否则用户在两个窗口上永远对不上"
    );
    b.engine
        .respond_approval(&req_r.approval_id, ApprovalAction::AllowWithGrant);
    // 返回值不作为断言依据 —— 本条守的是**授权的授予与隔离**，
    // 不是传输计数。`Ok(())` 本身就是"这次握手成功了"的信号。
    retry
        .await
        .expect("重试任务结束")
        .expect("带码重试应当成功");
    // 授权已经写下（此刻只有 B 的库里能看到）
    assert!(
        b.engine
            .trust_store
            .has_valid_grant(&a_in_b, "receive")
            .expect("查授权"),
        "比码通过 + 用户选了「免重复确认」= 必须真的写进 session_grants"
    );

    // ---- 第二次：同类操作，**不应该再弹审批**，也不该要码 ----
    let mut rx2 = b.approval_rx.resubscribe();
    let send2 = tokio::spawn({
        let e = a.engine.clone();
        let id = b.engine.identity.device_id.clone();
        let s = a.write_out("batch-2.bin", 512);
        async move { e.send_files("127.0.0.1", b_port, &id, "node-B", vec![s]).await }
    });
    send2
        .await
        .expect("第二次发送任务结束")
        .expect("有效授权期内，同类操作应当直接成功，不再要码");

    // 服务端**没有**弹审批 —— 这才是用户省掉的那次打断
    let popped = tokio::time::timeout(Duration::from_millis(600), rx2.recv()).await;
    assert!(
        popped.is_err(),
        "有效授权期内不该再弹审批（却收到了一次）—— 免重复确认没生效"
    );
    // 文件真的落盘了
    assert!(
        std::path::Path::new(&b.inbox()).join("batch-2.bin").exists(),
        "第二次应当真的落盘"
    );

    // ---- 第三段：`receive` 的授权**不能**授权 `browse` ----
    //
    // 断言的是「**它仍然来问了**」，而不是「它返回了什么错误」。
    // 没人应答的话服务端会等到 30s 超时（`读取 浏览应答 长度超时`）——
    // 那个超时本身就是"要码"的证据，但让它跑满 30s 会让本条守卫
    // 慢到无法接受。所以：起一个后台浏览，断言审批**弹出来了**，
    // 然后立刻拒绝让任务收尾。
    let mut rx_browse = b.approval_rx.resubscribe();
    let browse = tokio::spawn({
        let e = a.engine.clone();
        async move {
            e.list_remote_files("127.0.0.1", b_port, &feisuo_core::BrowseTarget::legacy(""))
                .await
        }
    });
    let req_b = tokio::time::timeout(Duration::from_secs(8), rx_browse.recv())
        .await
        .expect("❌ receive 的授权放行了 browse —— 授权必须按操作类别隔离")
        .expect("审批通道");
    assert!(
        req_b.grant_challenge.len() >= 6,
        "browse 那一格必须是空的，所以它要自己的码（challenge={:?}）",
        req_b.grant_challenge
    );
    b.engine
        .respond_approval(&req_b.approval_id, ApprovalAction::Reject);
    let _ = browse.await;
    assert!(
        !b.engine
            .trust_store
            .has_valid_grant(&a_in_b, "browse")
            .expect("查授权"),
        "browse 那一格必须是空的"
    );

    // ---- 第四段：`AllowOnce` **在比码通过之后**也不写授权 ----
    //
    // ⚠️ 早先这一段只让用户点了「允许一次」就断言，**没让对方把码带回来** ——
    // 而授权只在比码通过后才写，于是那段断言无论 `AllowOnce` 写不写
    // 都会绿。**守卫必须真的走到"有资格写"的那一步再断言它没写。**
    // 这正是本轮第三次遇到"断言早于行为"。
    b.engine
        .trust_store
        .revoke_session_grant(&a_in_b, "receive")
        .expect("清掉 receive 授权");
    assert!(
        !b.engine
            .trust_store
            .has_valid_grant(&a_in_b, "receive")
            .expect("查授权")
    );
    let mut rx3 = b.approval_rx.resubscribe();
    let send3 = tokio::spawn({
        let e = a.engine.clone();
        let id = b.engine.identity.device_id.clone();
        let s = a.write_out("batch-3.bin", 512);
        async move { e.send_files("127.0.0.1", b_port, &id, "node-B", vec![s]).await }
    });
    let req3 = tokio::time::timeout(Duration::from_secs(8), rx3.recv())
        .await
        .expect("授权清掉后应当重新弹审批")
        .expect("审批通道");
    // 只点「允许一次」—— 它的语义就是"只这一次"
    b.engine
        .respond_approval(&req3.approval_id, ApprovalAction::AllowOnce);
    let code3 = req3.grant_challenge.trim().to_string();
    let _ = send3.await;
    // **带码重试** —— 走到比码通过那一步，它本该有机会写授权
    let mut rx3b = b.approval_rx.resubscribe();
    let retry3 = tokio::spawn({
        let e = a.engine.clone();
        let id = b.engine.identity.device_id.clone();
        let s = a.dir.join("out").join("batch-3.bin");
        std::fs::write(
            &s,
            (0..512u32).map(|i| ((i as u8).wrapping_add(31)) as u8).collect::<Vec<u8>>(),
        )
        .expect("写重试文件");
        let c = code3.clone();
        async move { e.send_files_with_code("127.0.0.1", b_port, &id, "node-B", vec![s], &c).await }
    });
    let req3b = tokio::time::timeout(Duration::from_secs(8), rx3b.recv())
        .await
        .expect("带码重试应当再次弹审批")
        .expect("审批通道");
    b.engine
        .respond_approval(&req3b.approval_id, ApprovalAction::AllowOnce);
    retry3
        .await
        .expect("重试任务结束")
        .expect("带码 + 只允许本次，应当成功");
    assert!(
        !b.engine
            .trust_store
            .has_valid_grant(&a_in_b, "receive")
            .expect("查授权"),
        "❌ `AllowOnce` 写了授权 —— 它的语义是「只这一次」，不该有副作用"
    );

    // ---- 第五段：配 0 = 功能关闭，连授权都不写 ----
    // 用户**主动**把 TTL 配成 0 时，即使点了第三个按钮也不该写授权 ——
    // 否则「关掉这个功能」只是不弹按钮，而已发出的旧授权还在生效。
    let _ = (code, code3);
    b.engine
        .trust_store
        .revoke_session_grant(&a_in_b, "")
        .expect("清空全部授权");
    {
        let mut cfg = b.engine.config.write().await;
        cfg.session_grant_ttl_secs = 0;
    }
    let mut rx4 = b.approval_rx.resubscribe();
    let send4 = tokio::spawn({
        let e = a.engine.clone();
        let id = b.engine.identity.device_id.clone();
        let s = a.write_out("batch-4.bin", 512);
        async move { e.send_files("127.0.0.1", b_port, &id, "node-B", vec![s]).await }
    });
    let req4 = tokio::time::timeout(Duration::from_secs(8), rx4.recv())
        .await
        .expect("授权清空后应当重新弹审批")
        .expect("审批通道");
    b.engine
        .respond_approval(&req4.approval_id, ApprovalAction::AllowWithGrant);
    let _ = send4.await;
    assert!(
        !b.engine
            .trust_store
            .has_valid_grant(&a_in_b, "receive")
            .expect("查授权"),
        "❌ TTL 配成 0 仍写了授权 —— 「关闭功能」必须真的关掉"
    );

    // ---- 第六段：**过期即失效**（「短期」这两个字是整件事的安全前提）----
    //
    // 前面五段只验了「写入 / 隔离 / 不给 AllowOnce / 可关闭」，
    // **一条都没验「到期」**。而如果 `has_valid_grant` 不看 `expires_at`，
    // 那么"5 分钟免确认"在事实上就是"永久免确认" ——
    // **而那正是 `session` 这一档存在的理由被悄悄取消**。
    // 变异测试实测过：那条变异能畅通通过前五段。
    b.engine
        .trust_store
        .revoke_session_grant(&a_in_b, "")
        .expect("清空全部授权");
    b.engine
        .trust_store
        .grant_session(&a_in_b, "receive", -1)
        .expect("写一张**已经过期**的授权（TTL -1）");
    assert!(
        !b.engine
            .trust_store
            .has_valid_grant(&a_in_b, "receive")
            .expect("查授权"),
        "❌ 过期的授权仍然有效 —— 「短期」失效，这张表就等于永久信任"
    );
    assert!(
        b.engine
            .trust_store
            .list_active_grants(&a_in_b)
            .expect("列授权")
            .is_empty(),
        "❌ 过期授权仍出现在「有效授权」列表里 —— 界面上会让用户以为它还活着"
    );

    // 可撤销性的另一半：**用户看得见**。
    b.engine
        .trust_store
        .grant_session(&a_in_b, "receive", 300)
        .expect("写一张有效的授权");
    let active = b
        .engine
        .trust_store
        .list_active_grants(&a_in_b)
        .expect("列授权");
    assert_eq!(
        active.len(),
        1,
        "有效授权必须能被列出来 —— 用户看得见才谈得上「可撤销」"
    );
    // 撤销必须真的撤销（可撤销 ≠ 有个函数而已）
    assert_eq!(
        b.engine
            .trust_store
            .revoke_session_grant(&a_in_b, "receive")
            .expect("撤销授权"),
        1,
        "撤销必须删掉 1 行 —— 界面上那个「取消免确认」才有意义"
    );
    assert!(
        !b.engine
            .trust_store
            .has_valid_grant(&a_in_b, "receive")
            .expect("查授权"),
        "撤销之后必须立刻失效"
    );

    cleanup(&[a, b]);
}

/// 递归列出目录内容 (相对路径), 失败诊断用。
fn list_tree(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(base: &std::path::Path, cur: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(cur) else {
            return;
        };
        for e in entries.flatten() {
            let rel = e
                .path()
                .strip_prefix(base)
                .unwrap_or(&e.path())
                .to_string_lossy()
                .to_string();
            if e.path().is_dir() {
                out.push(format!("{}/", rel));
                walk(base, &e.path(), out);
            } else {
                out.push(rel);
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    out
}

async fn wait_until<F: Fn() -> bool>(cond: F, what: &str) {
    let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "等待超时: {}",
            what
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

// ==============================================================
// 1. 完整链路: 配对 -> 发送 -> 接收 (跨多个分块)
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pair_send_receive_roundtrip() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;

    setup_pair(&a, &b).await;

    // 9MB+ 强制走完整分块 + 整文件 BLAKE3 校验路径
    let payload_size = 9 * 1024 * 1024 + 12345;
    let src = a.write_out("payload.bin", payload_size);

    a.engine
        .send_files(
            "127.0.0.1",
            b.port(),
            &b.engine.identity.device_id,
            "node-B",
            vec![src.clone()],
        )
        .await
        .expect("发送应当成功");

    let landed = b.inbox().join("payload.bin");
    assert!(landed.exists(), "接收文件未落盘: {}", landed.display());
    assert_eq!(
        std::fs::metadata(&landed).unwrap().len(),
        payload_size as u64,
        "落盘长度必须与源文件一致(不得残留旧尾部)"
    );
    assert_eq!(
        std::fs::read(&src).unwrap(),
        std::fs::read(&landed).unwrap(),
        "落盘内容必须与源文件逐字节一致"
    );

    // 传输历史两侧都要有记录
    let sent = a.engine.list_transfer_records(10).unwrap();
    let recv = b.engine.list_transfer_records(10).unwrap();
    assert_eq!(sent.len(), 1, "发送侧应有 1 条历史");
    assert_eq!(recv.len(), 1, "接收侧应有 1 条历史");
    assert_eq!(sent[0].status, "completed");
    assert_eq!(recv[0].direction, "recv");
    assert_eq!(recv[0].file_size, payload_size as u64);

    // 多文件批次: 历史应合并成一条而不是丢记录
    let f2 = a.write_out("second.txt", 1234);
    a.engine
        .send_files(
            "127.0.0.1",
            b.port(),
            &b.engine.identity.device_id,
            "node-B",
            vec![src.clone(), f2],
        )
        .await
        .expect("批量发送应当成功");
    let recv2 = b.engine.list_transfer_records(10).unwrap();
    assert_eq!(recv2.len(), 2, "批量传输也应留下历史");
    assert!(
        recv2[0].file_name.contains("payload.bin") && recv2[0].file_name.contains("2 个文件"),
        "批量历史应体现文件数量, 实际: {}",
        recv2[0].file_name
    );

    cleanup(&[a, b]);
}

// ==============================================================
// 2. 同名文件绝不覆盖 (AGENTS.md "安全落盘" 红线)
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_never_overwrites_existing_file() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    // 两个同名但内容不同的文件必须来自不同目录, 否则第二次写就把第一次覆盖了
    let src1 = a.write_out_isolated("batch1", "report.txt", 1000);
    let src2 = a.write_out_isolated("batch2", "report.txt", 2000);

    for src in [&src1, &src2] {
        a.engine
            .send_files(
                "127.0.0.1",
                b.port(),
                &b.engine.identity.device_id,
                "node-B",
                vec![src.clone()],
            )
            .await
            .expect("发送应当成功");
    }

    let first = b.inbox().join("report.txt");
    let second = b.inbox().join("report (1).txt");
    assert!(first.exists(), "第一个文件应保留原名");
    assert!(second.exists(), "第二个同名文件必须自动加序号");
    assert_eq!(
        std::fs::metadata(&first).unwrap().len(),
        1000,
        "旧文件不得被改写"
    );
    assert_eq!(std::fs::metadata(&second).unwrap().len(), 2000);

    cleanup(&[a, b]);
}

// ==============================================================
// 3. 目录浏览 + 取回 (双栏穿梭右栏)
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_browse_and_pull() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    let b_src1 = {
        let p = b.inbox().join("photo.jpg");
        std::fs::write(&p, vec![7u8; 3000]).unwrap();
        p
    };
    let b_src2 = {
        let p = b.inbox().join("clip.mp4");
        std::fs::write(&p, vec![9u8; 5000]).unwrap();
        p
    };

    let listing = a
        .engine
        .list_remote_files_legacy("127.0.0.1", b.port(), "")
        .await
        .expect("浏览对端目录应当成功");
    assert_eq!(listing.files.len(), 2, "应列出 2 个文件, 实际 {:?}", listing);
    assert!(listing.files.iter().any(|f| f.name == "photo.jpg"));
    assert!(listing.files.iter().any(|f| f.name == "clip.mp4"));
    assert_eq!(listing.current_path, "", "根目录的 current_path 必须是空串");
    assert!(listing.parent_path.is_none(), "根目录不该有上一级");
    assert!(!listing.truncated);

    // 取回: b 反向推送到 a
    a.engine
        .request_pull("127.0.0.1", b.port(), vec!["photo.jpg".into(), "clip.mp4".into()], "")
        .await
        .expect("取回请求应当被受理");

    let a_photo = a.inbox().join("photo.jpg");
    let a_clip = a.inbox().join("clip.mp4");
    wait_until(
        || a_photo.exists() && a_clip.exists(),
        "取回的文件未在等待窗口内落盘",
    )
    .await;

    assert_eq!(
        std::fs::read(&a_photo).unwrap(),
        std::fs::read(&b_src1).unwrap()
    );
    assert_eq!(
        std::fs::read(&a_clip).unwrap(),
        std::fs::read(&b_src2).unwrap()
    );

    cleanup(&[a, b]);
}

// ==============================================================
// 3b. 穿梭支持子文件夹导航 + 条目截断 (旧实现只能看落盘目录第一层)
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_browse_navigates_subdirectories() {
    let a = Node::spawn("NAV-A", true).await;
    let b = Node::spawn("NAV-B", true).await;
    setup_pair(&a, &b).await;

    // 构造两层子目录: 2026/报表
    let nested = b.inbox().join("2026").join("报表");
    std::fs::create_dir_all(&nested).expect("创建子目录");
    std::fs::write(nested.join("1月.csv"), vec![1u8; 1200]).unwrap();
    std::fs::write(nested.join("2月.csv"), vec![2u8; 2400]).unwrap();
    std::fs::write(b.inbox().join("根目录文件.txt"), b"root").unwrap();

    // 1) 根目录: 看到 2026 文件夹 + 根目录文件
    let root = a
        .engine
        .list_remote_files_legacy("127.0.0.1", b.port(), "")
        .await
        .expect("浏览根目录");
    assert!(
        root.files.iter().any(|f| f.name == "2026" && f.is_dir),
        "根目录必须列出 2026 文件夹, 实际 {:?}",
        root.files
    );
    assert!(root.files.iter().any(|f| f.name == "根目录文件.txt" && !f.is_dir));
    assert!(root.parent_path.is_none());

    // 2) 进入 2026: 只有一个 子文件夹"报表"
    let lvl1 = a
        .engine
        .list_remote_files_legacy("127.0.0.1", b.port(), "2026")
        .await
        .expect("浏览 2026");
    assert_eq!(lvl1.current_path, "2026");
    assert_eq!(lvl1.parent_path.as_deref(), Some(""));
    assert_eq!(lvl1.files.len(), 1, "2026 下应只有 报表 一个文件夹");
    assert!(lvl1.files[0].is_dir && lvl1.files[0].name == "报表");

    // 3) 进入 2026/报表: 看到两个 csv, 且有上级可回
    let lvl2 = a
        .engine
        .list_remote_files_legacy("127.0.0.1", b.port(), "2026/报表")
        .await
        .expect("浏览 2026/报表");
    assert_eq!(lvl2.current_path, "2026/报表");
    assert_eq!(lvl2.parent_path.as_deref(), Some("2026"));
    assert_eq!(lvl2.files.len(), 2, "报表下应有 2 个 csv");
    assert!(lvl2.files.iter().all(|f| !f.is_dir));
    assert!(lvl2.files.iter().any(|f| f.name == "1月.csv" && f.size == 1200));
    assert!(lvl2.files.iter().any(|f| f.name == "2月.csv" && f.size == 2400));

    // 4) 取回子目录里的文件 (旧实现只能取根目录文件, 且会压平目录结构)
    a.engine
        .request_pull(
            "127.0.0.1",
            b.port(),
            vec!["2026/报表/1月.csv".into()], ""
        )
        .await
        .expect("取回子目录文件应当被受理");

    // 落盘会重建子目录结构
    let got = a.inbox().join("2026").join("报表").join("1月.csv");
    let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
    while !got.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    assert!(
        got.exists(),
        "子目录文件未落盘; a 收件箱当前内容: {:?}",
        list_tree(&a.inbox())
    );
    assert_eq!(std::fs::read(&got).unwrap(), vec![1u8; 1200]);

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sender_preserves_relative_structure_when_asked() {
    // `send_files_as` 通道: 清单里的 relative_path 必须原样落到接收端,
    // 而不是被压平成裸文件名。这是穿梭"取回"能保留目录结构的前提。
    let a = Node::spawn("RS-A", true).await;
    let b = Node::spawn("RS-B", true).await;
    setup_pair(&a, &b).await;

    let nested = a.dir.join("out").join("docs");
    std::fs::create_dir_all(&nested).unwrap();
    let f1 = nested.join("one.txt");
    let f2 = nested.join("two.txt");
    std::fs::write(&f1, vec![1u8; 300]).unwrap();
    std::fs::write(&f2, vec![2u8; 400]).unwrap();

    a.engine
        .client
        .send_files_as(
            "127.0.0.1",
            b.port(),
            b.engine.identity.device_id.as_str(),
            "node-RS-B",
            vec![
                (f1.clone(), "docs/one.txt".to_string()),
                (f2.clone(), "docs/deep/two.txt".to_string()),
            ],
        )
        .await
        .expect("带相对路径的发送应当成功");

    // 两级结构都要被重建
    let got1 = b.inbox().join("docs").join("one.txt");
    let got2 = b.inbox().join("docs").join("deep").join("two.txt");
    let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
    while !(got1.exists() && got2.exists()) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    assert!(
        got1.exists(),
        "一级相对路径未落盘; b 收件箱: {:?}",
        list_tree(&b.inbox())
    );
    assert!(
        got2.exists(),
        "两级相对路径未落盘; b 收件箱: {:?}",
        list_tree(&b.inbox())
    );
    assert_eq!(std::fs::read(&got1).unwrap(), vec![1u8; 300]);
    assert_eq!(std::fs::read(&got2).unwrap(), vec![2u8; 400]);

    // 落盘后的目录必须可被对端逐级浏览到 (a 浏览 b)
    let seen = a
        .engine
        .list_remote_files_legacy("127.0.0.1", b.port(), "docs/deep")
        .await
        .expect("应当能浏览到 docs/deep");
    assert_eq!(seen.current_path, "docs/deep");
    assert!(seen.files.iter().any(|f| f.name == "two.txt"));

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_send_files_as_rejects_bad_relative_path_before_sending() {
    let a = Node::spawn("RVBAD-A", true).await;
    let b = Node::spawn("RVBAD-B", true).await;
    setup_pair(&a, &b).await;

    let f = write_test_file(&a.dir.join("out"), "payload.txt", 200);

    // 非法相对路径必须在**发出任何网络包之前**就被本地拒掉,
    // 否则会把一条注定被对端拒绝的清单发出去, 白跑一次往返还留下
    // 对端"签名校验失败"的误导性日志。
    for evil in ["../escape.txt", "/abs.txt", "a/../../b.txt", "C:/x.txt", ""] {
        let res = a
            .engine
            .client
            .send_files_as(
                "127.0.0.1",
                b.port(),
                b.engine.identity.device_id.as_str(),
                "node-RVBAD-B",
                vec![(f.clone(), evil.to_string())],
            )
            .await;
        assert!(
            res.is_err(),
            "非法相对路径必须被本地拒绝: {:?}, 实际 {:?}",
            evil,
            res.as_ref().map(|_| "ok")
        );
    }

    // b 的收件箱必须一个文件都没收到
    assert!(
        list_tree(&b.inbox()).is_empty(),
        "被拒的请求不得留下任何文件, 实际: {:?}",
        list_tree(&b.inbox())
    );

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pull_preserves_subdir_and_never_overwrites() {
    let a = Node::spawn("SUB-A", true).await;
    let b = Node::spawn("SUB-B", true).await;
    setup_pair(&a, &b).await;

    // a 的收件箱里已经有一个同名同路径的文件, 取回时绝不能被覆盖
    let existing = a.inbox().join("docs").join("report.csv");
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, b"ORIGINAL").unwrap();

    let src_dir = b.inbox().join("docs");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("report.csv"), vec![5u8; 800]).unwrap();

    a.engine
        .request_pull("127.0.0.1", b.port(), vec!["docs/report.csv".into()], "")
        .await
        .expect("取回应被受理");

    // 目录结构必须保留
    let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
    while !a
        .inbox()
        .join("docs")
        .read_dir()
        .map(|mut d| d.any(|e| e.map(|x| x.file_name() == "report (1).csv").unwrap_or(false)))
        .unwrap_or(false)
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    // 原有文件内容必须一字不变 (安全红线: 绝不覆盖)
    assert_eq!(
        std::fs::read(&existing).unwrap(),
        b"ORIGINAL",
        "同名文件绝不能被覆盖"
    );
    // 新文件必须带序号落在同一目录
    let dedup = a.inbox().join("docs").join("report (1).csv");
    assert!(
        dedup.exists(),
        "应当生成去重后的新文件; a 收件箱: {:?}",
        list_tree(&a.inbox())
    );
    assert_eq!(std::fs::read(&dedup).unwrap(), vec![5u8; 800]);

    cleanup(&[a, b]);
}

/// 守卫：**带落点**的取回，同名递增必须发生在落点目录里。
///
/// ## 为什么这条必须单独测
///
/// `test_pull_preserves_subdir_and_never_overwrites` 用的是 `dest_sub_path = ""`
/// （落收件根），`test_pull_preserves_subdir_and_never_overwrites` 里的
/// "落点"其实是**对方文件的相对路径**。而"落到本机某个子目录"这条路径
/// 是穿梭框的核心功能（用户把对方地址栏停在 `工作/2026`，取回就落那里），
/// 之前**没有任何集成测试**跑过它。
///
/// ## 它要证明的是两件事同时成立
///
/// 1. 落点生效：文件真的落进 `工作/2026/`，而不是收件根；
/// 2. 递增生效：那里已有同名文件时，新文件变成 `report (1).csv`，
///    **且原文件内容一字不变**。
///
/// 两条合起来才是用户看到的语义：「落到地址栏当前目录 + 绝不覆盖」。
/// 只测其中一条，另一条坏掉时不会有任何症状。
#[tokio::test]
async fn test_pull_to_dest_subpath_dedups_inside_that_dir() {
    let a = Node::spawn("DEST-A", true).await;
    let b = Node::spawn("DEST-B", true).await;
    setup_pair(&a, &b).await;

    // 本机收件目录里 `工作/2026/` 已经有同名同内容的文件
    let dir = a.inbox().join("工作").join("2026");
    std::fs::create_dir_all(&dir).unwrap();
    let existing = dir.join("report.csv");
    std::fs::write(&existing, b"ORIGINAL").unwrap();

    // 对方有同名文件、内容不同
    let src = b.inbox().join("report.csv");
    std::fs::write(&src, vec![7u8; 512]).unwrap();

    a.engine
        .request_pull(
            "127.0.0.1",
            b.port(),
            vec!["report.csv".into()],
            "工作/2026",
        )
        .await
        .expect("带落点的取回应被受理");

    let dedup = dir.join("report (1).csv");
    let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
    while !dedup.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    assert!(
        dedup.exists(),
        "带落点的取回应落在 `工作/2026/` 且去重为 report (1).csv。\
         实际收件树: {:?}",
        list_tree(&a.inbox())
    );
    assert_eq!(
        std::fs::read(&existing).unwrap(),
        b"ORIGINAL",
        "落点目录里的原文件绝不能被覆盖（安全红线）"
    );
    assert_eq!(std::fs::read(&dedup).unwrap(), vec![7u8; 512]);

    // 落点**不能**顺手在收件根也留一份 —— 那会让用户以为落点没生效，
    // 于是满盘找文件，或者手动再发一遍。
    assert!(
        !a.inbox().join("report (1).csv").exists(),
        "收件根不该出现同一份文件：落点必须真的生效。收件树: {:?}",
        list_tree(&a.inbox())
    );

    cleanup(&[a, b]);
}

#[test]
fn test_relative_subpath_validation() {
    use feisuo_core::storage::PathManager;

    // 合法: 多级相对路径
    for ok in [
        "a.txt",
        "2026/报表/1月.csv",
        "a/b/c/d/e.txt",
        "带 空格 的 文件.txt",
        "emoji😀.png",
    ] {
        assert!(
            PathManager::validate_relative_subpath(ok).is_ok(),
            "合法相对路径被误拒: {:?}",
            ok
        );
    }

    // 非法: 任何形式的逃逸
    for evil in [
        "",
        "   ",
        "..",
        ".",
        "../secret.txt",
        "../../etc/passwd",
        "a/../../b.txt",
        "a/../b.txt",
        "/etc/passwd",
        "/",
        "C:/Windows/system32",
        "C:\\Windows",
        "\\\\attacker\\share\\p.dll",
        "a\\..\\..\\b.txt",
        "a/\0/b.txt",
        "CON",
        "sub/CON.txt",
        "trailing ",
        "trailing.",
    ] {
        assert!(
            PathManager::validate_relative_subpath(evil).is_err(),
            "非法相对路径必须被拒绝: {:?}",
            evil
        );
    }

    // 超长路径
    let long = "a".repeat(600);
    assert!(PathManager::validate_relative_subpath(&long).is_err());
}

/// `resolve_unique_subpath` 必须**默认安全**, 而不是靠调用方记得先校验。
///
/// 它是 `pub` 的, 内部直接 `parent.push(seg)` + `create_dir_all` ——
/// 任何调用方忘了先调 validate, `../../` 就会在接收目录之外建出目录树。
/// 这条用例直接调用它(不预先校验), 断言逃逸被拒且磁盘上没留下任何痕迹。
#[test]
fn test_resolve_unique_subpath_is_safe_without_caller_validation() {
    use feisuo_core::storage::PathManager;

    let base = temp_dir("SAFE-RESOLVE");
    std::fs::create_dir_all(&base).unwrap();

    // 诱饵名带上本进程 id, 保证这是"本次运行专属"的检查。
    // 不能用固定名字: 否则历史运行(比如守卫自检时的违规注入)留下的残留
    // 会让这条用例永久失败, 而那与当前代码的对错无关。
    let decoy = format!("feisuo-decoy-{}", std::process::id());
    let sibling = base.parent().unwrap().join(&decoy);
    let escape_leaf = format!("escaped-{}.txt", std::process::id());

    // 先清掉本进程可能残留的痕迹, 保证下面断言的是"这次调用没新建"
    let _ = std::fs::remove_dir_all(&sibling);
    let _ = std::fs::remove_file(base.parent().unwrap().join(&escape_leaf));

    for evil in [
        format!("../{}", escape_leaf),
        format!("../../{}", escape_leaf),
        format!("a/../../{}", escape_leaf),
        "/etc/passwd".to_string(),
        "C:/Windows/system32/evil.dll".to_string(),
        "..".to_string(),
    ] {
        let res = PathManager::resolve_unique_subpath(&base, &evil);
        assert!(
            res.is_err(),
            "未预校验就调用 resolve_unique_subpath 时, 逃逸路径必须被拒: {:?}",
            evil
        );
    }

    // 关键断言: 不只是返回 Err, 磁盘上**确实没有**被建出任何越界目录/文件
    assert!(
        !sibling.exists(),
        "逃逸尝试竟在落盘目录之外建出了目录: {}",
        sibling.display()
    );
    let escaped = base.parent().unwrap().join(&escape_leaf);
    assert!(
        !escaped.exists(),
        "逃逸尝试在落盘目录之外创建了文件: {}",
        escaped.display()
    );

    // 合法路径仍然要能正常工作, 且确实建在 base 之内
    let ok = PathManager::resolve_unique_subpath(&base, "2026/报表/1月.csv").unwrap();
    assert!(
        ok.starts_with(&base),
        "合法路径却落到了 base 之外: {}",
        ok.display()
    );

    // 收尾: 把合法用例造出来的目录树一并清掉
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_browse_rejects_escaping_sub_path() {
    let a = Node::spawn("ESC-A", true).await;
    let b = Node::spawn("ESC-B", true).await;
    setup_pair(&a, &b).await;

    // 落盘目录外放一个诱饵文件, 任何逃逸都必须读不到它
    let outside = temp_dir("ESC-B").join("secret.txt");
    std::fs::write(&outside, b"TOP SECRET").unwrap();

    for evil in [
        "../secret.txt",
        "../../secret.txt",
        "2026/../../secret.txt",
        "/etc",
        "C:/Windows",
        ".hidden",
        "../.hidden/x",
        "",
    ] {
        let res = a
            .engine
            .list_remote_files_legacy("127.0.0.1", b.port(), evil)
            .await;
        // 空串是合法的根目录, 其余必须失败
        if evil.is_empty() {
            assert!(res.is_ok(), "空串应当就是根目录");
        } else {
            assert!(
                res.is_err(),
                "逃逸路径必须被拒绝: {:?}, 实际返回 {:?}",
                evil,
                res.as_ref().map(|l| l.files.len())
            );
        }
    }

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_browse_truncates_huge_directory() {
    let a = Node::spawn("TRUNC-A", true).await;
    let b = Node::spawn("TRUNC-B", true).await;
    setup_pair(&a, &b).await;

    // 造一个远超 MAX_BROWSE_ENTRIES 的目录。
    // 不截断的话应答会超过 MAX_JSON_FRAME (1 MiB), 对端读帧直接被拒 ——
    // 表现为"设备在线但穿梭右栏一直报错", 而且完全看不出是文件太多。
    let total = feisuo_core::protocol::MAX_BROWSE_ENTRIES + 250;
    for i in 0..total {
        std::fs::write(
            b.inbox().join(format!("f{:06}.dat", i)),
            format!("payload-{}", i),
        )
        .unwrap();
    }

    let listing = a
        .engine
        .list_remote_files_legacy("127.0.0.1", b.port(), "")
        .await
        .expect("超大目录也必须返回而不是报错");

    // ⚠️ 这里断言的是**分页**契约，不是"一次截断到 MAX_BROWSE_ENTRIES"。
    //
    // 早先的期望是 `files.len() == MAX_BROWSE_ENTRIES`（1000），那是没有
    // 分页时的行为。分页浏览（§7.3 第 1 点）落地后，一次请求只返回
    // 一页（`DEFAULT_PAGE_SIZE` = 500），剩下的靠"加载更多"继续取。
    // 断言仍按 1000 写，等于把一条**已经改成另一种形状**的契约钉死 ——
    // 真实行为没错，测试却红了，而红的原因与产品无关。
    //
    // 而且"必须截断到上限"这件事本身现在由三个量共同表达：
    // 本页条数、`truncated`、`total`。只查一个就会漏。
    let page = feisuo_core::storage::volumes::DEFAULT_PAGE_SIZE as usize;
    assert_eq!(
        listing.total as usize, total,
        "total 必须是目录真实条目数（用户据此知道还有多少没显示）"
    );
    assert_eq!(
        listing.files.len(),
        page,
        "单次必须只返回一页（实际 {}，期望 {}）",
        listing.files.len(),
        page
    );
    assert!(
        listing.truncated,
        "还有更多条目时 truncated 必须为真, 否则界面会把'只显示第 1 页'当成目录就这么多"
    );
    assert!(
        listing.message.contains("加载更多"),
        "必须告诉用户还有更多、且怎么继续看, 实际: {}",
        listing.message
    );

    // 「加载更多」必须真的能翻到下一页 —— 那是 UI 上那个按钮依赖的调用。
    //
    // 早先这条用例只验了第 1 页，于是"第 2 页取不到 / 翻页重复 / 翻页漏项"
    // 全都不会让任何测试变红。而用户点一次「加载更多」看到列表**没变化**，
    // 是典型的"界面坏了"体验。
    let page2 = a
        .engine
        .list_remote_files(
            "127.0.0.1",
            b.port(),
            &feisuo_core::transport::BrowseTarget {
                volume: String::new(),
                rel_path: String::new(),
                offset: listing.total.min(page as u32),
                limit: 0,
                grant_code: String::new(),
            },
        )
        .await
        .expect("第 2 页也必须返回而不是报错");
    assert_eq!(
        page2.offset as usize, page,
        "第 2 页的 offset 必须等于第 1 页的条数（否则界面无法正确续接）"
    );
    let first_names: Vec<&str> = listing.files.iter().map(|f| f.name.as_str()).collect();
    let second_names: Vec<&str> = page2.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        second_names.len(),
        (total - page).min(page),
        "第 2 页应返回剩余条目（不足一页时取剩余全部）"
    );
    for n in &second_names {
        assert!(
            !first_names.contains(n),
            "两页不得重复: {n} 同时出现在第 1 页与第 2 页（用户会以为文件在跳）"
        );
    }

    // 排序后再截断 => 两次列举结果必须完全一致 (否则用户以为文件随机丢失)
    let again = a
        .engine
        .list_remote_files_legacy("127.0.0.1", b.port(), "")
        .await
        .expect("二次列举");
    let first_names: Vec<&str> = listing.files.iter().map(|f| f.name.as_str()).collect();
    let second_names: Vec<&str> = again.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        first_names, second_names,
        "两次列举必须返回同一批文件 (先排序再截断)"
    );

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pull_rejects_escaping_sub_path() {
    let a = Node::spawn("PESC-A", true).await;
    let b = Node::spawn("PESC-B", true).await;
    setup_pair(&a, &b).await;

    let outside = temp_dir("PESC-B").join("secret.txt");
    std::fs::write(&outside, b"TOP SECRET").unwrap();

    for evil in [
        "../secret.txt",
        "sub/../../secret.txt",
        "/absolute.txt",
    ] {
        let res = a
            .engine
            .request_pull("127.0.0.1", b.port(), vec![evil.into()], "")
            .await;
        assert!(
            res.is_err(),
            "逃逸取回路径必须被拒绝: {:?}, 实际 {:?}",
            evil,
            res
        );
    }

    // 确认诱饵文件确实没被送到 a
    let leaked = a.inbox().join("secret.txt");
    assert!(!leaked.exists(), "绝不允许把落盘目录外的文件传过来");

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pull_rejects_empty_and_oversized_selection() {
    let a = Node::spawn("PLIM-A", true).await;
    let b = Node::spawn("PLIM-B", true).await;
    setup_pair(&a, &b).await;

    // 空选择必须在发出任何网络包之前就被拒
    assert!(
        a.engine
            .request_pull("127.0.0.1", b.port(), vec![], "")
            .await
            .is_err(),
        "空选择必须被拒绝"
    );

    // 超上限也必须被本地拒掉
    let too_many: Vec<String> = (0..=feisuo_core::protocol::MAX_FILES_PER_BATCH)
        .map(|i| format!("f{}.dat", i))
        .collect();
    let err = a
        .engine
        .request_pull("127.0.0.1", b.port(), too_many, "")
        .await
        .expect_err("超上限的取回必须被拒绝");
    assert!(
        err.to_string().contains("上限"),
        "错误信息应说明是超上限, 实际: {}",
        err
    );

    cleanup(&[a, b]);
}

// ==============================================================
// 4. 路径穿越 / 保留设备名 一律拒绝
// ==============================================================
#[test]
fn test_path_traversal_is_rejected() {
    use feisuo_core::storage::PathManager;

    let evil = [
        r"..\..\Windows\System32\drivers\etc\hosts",
        "../../../../etc/passwd",
        r"\Windows\System32\cmd.exe",
        r"C:\Users\Public\evil.exe",
        r"\\attacker\share\payload.dll",
        r"sub\dir\file.txt",
        "..",
        ".",
        "",
        "   ",
        "trailing.",
        "trailing ",
        "CON",
        "con.txt",
        "LPT1.log",
    ];
    for name in evil {
        assert!(
            PathManager::validate_relative_path(name).is_err(),
            "非法路径必须被拒绝: {:?}",
            name
        );
    }

    // 明确含 NUL 的文件名(单独构造, 避免源码里的转义被工具处理掉)
    assert!(PathManager::validate_relative_path("bad\u{0}name.txt").is_err());

    for name in [
        "report.pdf",
        "IMG_20260929_01.jpg",
        "中文名称 空格.txt",
        "a.b.c.tar.gz",
    ] {
        assert!(
            PathManager::validate_relative_path(name).is_ok(),
            "合法文件名被误拒: {:?}",
            name
        );
    }
}

// ==============================================================
// 5. 帧长度上限: 对端声明 4GiB 必须被立刻拒绝, 而不是 OOM
// ==============================================================
#[tokio::test]
async fn test_oversized_frame_is_rejected_without_allocating() {
    use feisuo_core::protocol;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let b = Node::spawn("B", true).await;

    let mut s = TcpStream::connect(("127.0.0.1", b.port())).await.unwrap();
    s.write_all(&[protocol::MSG_TRANSFER]).await.unwrap();
    // 声明 4 GiB 的 JSON 帧: 没有上限时这里会尝试一次性分配 4 GiB
    s.write_all(&0xFFFF_FFFFu32.to_be_bytes()).await.unwrap();
    s.flush().await.unwrap();

    let mut buf = [0u8; 16];
    match tokio::time::timeout(Duration::from_secs(3), s.read(&mut buf)).await {
        Ok(Ok(0)) => {}
        Ok(Ok(n)) => panic!("不应返回数据, 却收到 {} 字节", n),
        Ok(Err(_)) => {}
        Err(_) => panic!("服务端未及时拒绝超大帧(说明它在傻等或正在分配巨量内存)"),
    }

    // 服务端必须仍然健康
    let listing = std::fs::read_dir(b.inbox()).unwrap().count();
    assert_eq!(listing, 0, "非法连接不得落盘任何文件");

    cleanup(&[b]);
}

// ==============================================================
// 6. 未受信设备必须走审批; 拒绝后不得落盘任何文件
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_untrusted_peer_requires_approval_and_rejection_leaves_no_file() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    let mut approval_rx = b.approval_rx.resubscribe();

    // 故意不配对 -> 必须触发审批
    let src = a.write_out("sneaky.bin", 4096);
    let send_task = {
        let engine = a.engine.clone();
        let port = b.port();
        let dev_id = b.engine.identity.device_id.clone();
        tokio::spawn(async move {
            engine
                .send_files("127.0.0.1", port, &dev_id, "node-B", vec![src])
                .await
        })
    };

    let req = tokio::time::timeout(PAIR_TIMEOUT, approval_rx.recv())
        .await
        .expect("应收到审批请求")
        .expect("审批通道不应关闭");
    assert_eq!(req.sender_id, a.engine.identity.device_id);
    assert_eq!(req.file_count, 1);
    assert!(!req.total_size_formatted.is_empty());

    assert!(b
        .engine
        .respond_approval(&req.approval_id, ApprovalAction::Reject));

    let outcome = tokio::time::timeout(SETTLE_TIMEOUT, send_task)
        .await
        .expect("发送任务应当结束")
        .expect("任务不应 panic");
    let err = outcome.expect_err("被拒绝的传输必须返回错误");
    assert!(
        err.to_string().contains("拒绝"),
        "错误信息应说明被拒绝, 实际: {}",
        err
    );

    assert!(
        !b.inbox().join("sneaky.bin").exists(),
        "被拒绝的传输绝不允许落盘"
    );
    assert_eq!(
        std::fs::read_dir(b.inbox()).unwrap().count(),
        0,
        "落盘目录必须保持为空"
    );

    cleanup(&[a, b]);
}

// ==============================================================
// 7. 审批通过 -> 落盘; 重复审批请求必须被拒(幂等)
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_approval_allows_and_is_single_shot() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    let mut approval_rx = b.approval_rx.resubscribe();

    let src = a.write_out("allowed.bin", 2048);
    let send_task = {
        let engine = a.engine.clone();
        let port = b.port();
        let dev_id = b.engine.identity.device_id.clone();
        tokio::spawn(async move {
            engine
                .send_files("127.0.0.1", port, &dev_id, "node-B", vec![src])
                .await
        })
    };

    let req = tokio::time::timeout(PAIR_TIMEOUT, approval_rx.recv())
        .await
        .expect("应收到审批请求")
        .expect("审批通道不应关闭");

    assert!(b
        .engine
        .respond_approval(&req.approval_id, ApprovalAction::AllowOnce));
    // 同一请求重复处理必须返回 false, 而不是重复放行
    assert!(
        !b
            .engine
            .respond_approval(&req.approval_id, ApprovalAction::AllowOnce),
        "同一审批请求不得被重复消费"
    );

    let outcome = tokio::time::timeout(SETTLE_TIMEOUT, send_task)
        .await
        .expect("发送任务应当结束")
        .expect("任务不应 panic");
    outcome.expect("被允许的传输应当成功");

    let landed = b.inbox().join("allowed.bin");
    assert!(landed.exists(), "被允许的传输必须落盘");
    assert_eq!(std::fs::metadata(&landed).unwrap().len(), 2048);

    // `AllowOnce` **不得**写入长期信任。
    //
    // 这条断言以前是反的（"审批通过后应写入受信条目"），它编码的是旧实现：
    // 一次批准被 `bind_device(is_trusted=true)` 静默升级成"永久免密"。
    // 用户点了"允许这一次"，得到的却是"以后都不用再点了" ——
    // 用户没有选择权，而界面上「允许一次」与「允许并信任」是**两个按钮**
    // （App.vue 的 `handleApproval('allow_once')` / `('allow_and_trust')`），
    // 写成一样等于把那个选择作废。
    //
    // "一次配对、终生免密"由「允许并信任」交付，见下面的
    // `test_allow_and_trust_persists_for_good`。
    assert!(
        !b.engine
            .trust_store
            .is_device_trusted(&a.engine.identity.device_id)
            .unwrap(),
        "「允许一次」绝不能写入长期信任 —— 那是「允许并信任」按钮的职责, \
         两者混同等于剥夺用户的选择权"
    );

    cleanup(&[a, b]);
}

/// 传输路径的审批必须是「注册 → 通知 → 等待」，顺序**不可交换**。
///
/// 早先写成"先 `approval_tx.send()`、再在循环顶部 `register()`"。
/// UI 收到通知后立刻点「允许」是完全正常的（人比 IPC 快），
/// 而那一刻 `pending` 里若还没有 oneshot sender，`resolve` 会返回 false
/// —— 审批被**静默丢弃**，连接一直等到超时才被拒。症状是
/// "点了允许，什么也没发生，过一分钟说传输失败"，且**只在首次配对时出现**
/// （只有未配对设备才走审批）。
///
/// ## 为什么断言源码顺序，而不是实跑一次审批
///
/// 我先写的是"收到通知后立即应答"的运行时用例，然后试着证明它能抓到缺陷：
/// 把 `send` 移到 `register` 之前 → **仍然通过**；再在 `register` 里注入
/// `yield_now()` 放大窗口 → **仍然通过**。
///
/// 原因是 `register()` 与 `send()` 之间**没有 await 点**，
/// 而 tokio 单线程 worker 不会在同步代码中间让出。于是那个窗口在测试里
/// 根本不存在，这个用例证明不了它想证明的事。
///
/// 真实环境里窗口来自**跨进程**：UI 在另一个进程，通知经 Tauri 事件循环
/// 回来才变成一次 `respond_approval` 调用，那中间有真正的调度延迟。
/// 这一点无法用进程内集成测试复现。
///
/// 所以只能直接断言源码顺序。这类断言确实会在重构时误报，
/// 但本项目的失败模式是"两份实现漂移"（`await_approval` 与传输内联那份），
/// 而顺序漂移正是漂移的一种 —— 宁可误报也不能让它再漂回去。
#[test]
fn transfer_approval_must_register_before_notifying() {
    // `CARGO_MANIFEST_DIR` 对 core 的集成测试**就是 `core/` 本身**，
    // 所以 `src/transport/server.rs` 只往上退一级。
    // 早先按"从 workspace 根算"写了 `../..`，直接退到仓库外面，
    // 报错是"读取失败"—— 读不到文件这件事本身毫无信息量。
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("transport")
        .join("server.rs");
    let code = std::fs::read_to_string(&src)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}", src.display(), e));

    // 只看传输路径那一段（`let (action, ... ) = loop {` 到 `let decision`）。
    let start = code
        .find("let (action, _unused_grant_code) = loop {")
        .expect("找不到传输路径的审批循环");
    let end = code[start..]
        .find("let decision =")
        .map(|i| start + i)
        .expect("找不到审批循环里的等待点");
    let window = &code[start..end];

    let reg = window
        .find("approval_manager.register(")
        .expect("审批循环里没有 register —— 通知发出后无人应答");
    let notify = window
        .find("approval_tx.send(")
        .expect("审批循环里没有 send —— 界面永远收不到审批请求");

    assert!(
        reg < notify,
        "传输路径必须**先 register 再 send**（register 在第 {reg} 字节, \
         send 在第 {notify} 字节）。反过来会留下『UI 已经能点、但点了没人接』\
         的窗口：审批被静默丢弃, 用户表现为『点了允许但什么都没发生』, \
         且只在首次配对时出现。浏览/取回走 `await_approval`, 那边就是这个顺序。"
    );

    // 重试路径不得自己再 send 一次 —— 那样"通知"有两处、"注册"只有一处,
    // 于是重试又回到"先发后注册"。
    let retry_region = &code[end..];
    let next_loop = retry_region
        .find("let decision =")
        .map(|i| end + i)
        .unwrap_or(code.len());
    assert!(
        !code[end..next_loop.min(code.len())].contains("approval_tx.send("),
        "重试分支不得自己 send 审批请求 —— 交给循环顶部统一『register + send』, \
         否则通知有两处而注册只有一处, 重试路径上又会出现先发后注册的窗口"
    );
}

// ==============================================================
// 7b. 「允许并信任」: 必须写入长期信任, 且之后**零弹窗**
// ==============================================================
//
// 为什么单独一条: 「允许并信任」是"一次配对、终生免密"这个产品承诺
// **唯一**的兑现点（AGENTS.md §1.2）。而它此前**零覆盖** ——
// 上一条测试只验了 AllowOnce，于是"点了允许并信任却没写成永久信任"
// 这种缺陷不会让任何一条测试变红，而它一旦发生就是核心承诺失效。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_allow_and_trust_persists_for_good() {
    let a = Node::spawn("TRUST-A", true).await;
    let b = Node::spawn("TRUST-B", true).await;
    let mut approval_rx = b.approval_rx.resubscribe();

    // 第 1 次：未配对 → 必弹窗；用户选「允许并信任」
    let src1 = a.write_out("first.bin", 1024);
    let send1 = {
        let engine = a.engine.clone();
        let port = b.port();
        let dev_id = b.engine.identity.device_id.clone();
        tokio::spawn(async move {
            engine
                .send_files("127.0.0.1", port, &dev_id, "TRUST-B", vec![src1])
                .await
        })
    };
    let req = tokio::time::timeout(PAIR_TIMEOUT, approval_rx.recv())
        .await
        .expect("首次传输应收到审批请求")
        .expect("审批通道不应关闭");
    assert!(
        b.engine
            .respond_approval(&req.approval_id, ApprovalAction::AllowAndTrust),
        "「允许并信任」必须被接受"
    );
    tokio::time::timeout(SETTLE_TIMEOUT, send1)
        .await
        .expect("发送任务应当结束")
        .expect("任务不应 panic")
        .expect("被允许的传输应当成功");
    assert!(
        b.inbox().join("first.bin").exists(),
        "第 1 次传输必须落盘"
    );

    // 长期信任必须真的写进去了
    assert!(
        b.engine
            .trust_store
            .is_device_trusted(&a.engine.identity.device_id)
            .unwrap(),
        "「允许并信任」必须写入长期信任 —— 这是『一次配对终生免密』的唯一兑现点"
    );

    // 第 2 次：**不得**再弹窗。
    //
    // 只断言"信任写进去了"是不够的: 授权判定可能在别处仍然要求审批,
    // 那样用户每次都要点 —— 而"零弹窗"正是这个承诺的内容。
    // 这里用一个短超时当探针: 弹窗了就立刻收到事件(反而说明有问题);
    // 不弹窗则超时, 两者可区分。
    let src2 = a.write_out("second.bin", 1024);
    // `send_files` 返回 `Result<u32>`（断点续传跳过的文件数），
    // 所以 `timeout` 之后只有**一层** Result 可解 —— 之前多写了一层
    // `.expect("任务不应 panic")`，编译期就报 `method not found in u32`。
    let skipped = tokio::time::timeout(
        SETTLE_TIMEOUT,
        a.engine.send_files(
            "127.0.0.1",
            b.port(),
            &b.engine.identity.device_id,
            "TRUST-B",
            vec![src2],
        ),
    )
    .await
    .expect("已信任设备的传输不应卡在审批上（说明仍在弹窗）")
    .expect("已信任设备的传输应当成功");

    // 顺带确认：真的没有第二个审批请求被发出来
    let no_approval = tokio::time::timeout(Duration::from_millis(300), approval_rx.recv()).await;
    assert!(
        no_approval.is_err(),
        "已写入长期信任后不得再弹审批窗口（却收到了: {:?}）",
        no_approval.ok()
    );
    assert!(
        b.inbox().join("second.bin").exists(),
        "第 2 次传输必须落盘（断点续传跳过 {} 个）",
        skipped
    );

    cleanup(&[a, b]);
}

// ==============================================================
// 8. ~~黑名单 fail-closed~~ —— 该维度已整体删除（§14.11 决策 1）
// ==============================================================
//
// 原来这里有一条 `test_blacklist_blocks_incoming_transfer`。
// `blocked`（黑名单）是**实现期自己加的**：原始提交 `735dd66` 只有 6 个文件，
// `grep -i blacklist` 零命中，需求里也从未要求过（详见
// `DESIGN_TRUST_SHUTTLE.md` §14.0 的溯源）。
//
// 它替代不了"我不想它给我发文件"这个诉求 —— 那个诉求现在由
// **解除配对**（§14.5，已做成双向）覆盖；而"我不想看见它"由**隐藏**覆盖。
// 所以整条用例连同 `blacklisted_devices` 表、`TrustLevel::Blocked`、
// `DenyCode::{DeviceBlocked, BlocklistCheckFailed}`、`Presence::Blocked`、
// 审批弹窗的 `[阻止该设备]` 一并删除。
//
// **删除的守卫**：`scripts/guard_blocked_dimension.py` 从"写入方数量不变"
// 改成"写入方必须为 0" —— 任何新增都会让 CI 变红。

// ==============================================================
// 9. 取回请求不得越权读取接收目录之外的文件
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pull_cannot_escape_receive_dir() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    // b 落盘目录之外造一个"机密"文件
    let secret = b.dir.join("secret.txt");
    std::fs::write(&secret, b"top secret").unwrap();

    for evil in ["../secret.txt", "..\\secret.txt", "sub/../../secret.txt"] {
        let res = a
            .engine
            .request_pull("127.0.0.1", b.port(), vec![evil.to_string()], "")
            .await;
        assert!(
            res.is_err(),
            "取回越权路径必须被拒绝: {:?}, 实际 {:?}",
            evil,
            res
        );
    }

    // 不存在的文件同样必须报错
    assert!(a
        .engine
        .request_pull("127.0.0.1", b.port(), vec!["nope.bin".to_string()], "")
        .await
        .is_err());

    // 空列表必须被拒
    assert!(a
        .engine
        .request_pull("127.0.0.1", b.port(), vec![], "")
        .await
        .is_err());

    assert!(secret.exists(), "越权请求不得删除或改动源文件");

    cleanup(&[a, b]);
}

// ==============================================================
// 10. 配对码: 单次有效 + 按 IP 限速锁定
// ==============================================================
#[test]
fn test_pair_pin_single_use_and_rate_limited() {
    let dir = temp_dir("pin");
    let store = TrustStore::open_at(dir.join("trust_store.db")).unwrap();

    let (pin, _) = store.generate_pair_pin();
    assert!(store.verify_pair_pin(&pin), "首次校验应当通过");
    assert!(
        !store.verify_pair_pin(&pin),
        "配对码必须单次有效, 不得可重放"
    );

    let (_p2, _) = store.generate_pair_pin();
    for _ in 0..5 {
        store.register_pin_failure("10.0.0.9");
    }
    assert!(
        store.is_pin_locked("10.0.0.9"),
        "连续失败达到上限后必须锁定该 IP"
    );
    assert!(
        !store.is_pin_locked("10.0.0.10"),
        "锁定必须按 IP 维度隔离, 不能误伤其他设备"
    );

    store.clear_pin_failures("10.0.0.9");
    assert!(!store.is_pin_locked("10.0.0.9"), "清空计数后应恢复");

    let _ = std::fs::remove_dir_all(&dir);
}

// ==============================================================
// 11. 配对不可顶替: 已绑定 device_id 换公钥必须被拒
// ==============================================================
#[test]
fn test_pairing_cannot_hijack_existing_device_id() {
    let dir = temp_dir("hijack");
    let store = TrustStore::open_at(dir.join("trust_store.db")).unwrap();

    let victim = DeviceIdentity::load_or_generate_at(dir.join("victim.key")).unwrap();
    let attacker = DeviceIdentity::load_or_generate_at(dir.join("attacker.key")).unwrap();

    let entry = |who: &DeviceIdentity, name: &str, ip: &str| TrustedDevice {
        device_id: who.device_id.clone(),
        device_name: name.to_string(),
        public_key_hex: who.public_key_hex(),
        last_ip: ip.to_string(),
        bound_at: chrono::Utc::now().to_rfc3339(),
        is_trusted: true,
        trust_level: feisuo_core::security::TrustLevel::Permanent,
        visible: true,
        last_seen_at: chrono::Utc::now().timestamp(),
        pairing_epoch: String::new(),
    };

    assert_eq!(
        store.bind_device(&entry(&victim, "victim", "10.0.0.1")).unwrap(),
        feisuo_core::BindOutcome::Added
    );
    assert_eq!(
        store.bind_device(&entry(&victim, "victim", "10.0.0.9")).unwrap(),
        feisuo_core::BindOutcome::Refreshed,
        "同公钥重复配对只应刷新元数据"
    );

    let hijack = store
        .bind_device(&entry_of_attacker(&victim, &attacker))
        .unwrap();
    assert_eq!(
        hijack,
        feisuo_core::BindOutcome::KeyConflict,
        "绝不允许用已存在的 device_id 覆盖公钥"
    );
    assert_eq!(
        store.get_device_pubkey(&victim.device_id).unwrap().unwrap(),
        victim.public_key_hex(),
        "信任库中的公钥必须保持为受害者的"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn entry_of_attacker(victim: &DeviceIdentity, attacker: &DeviceIdentity) -> TrustedDevice {
    TrustedDevice {
        device_id: victim.device_id.clone(),
        device_name: "attacker".to_string(),
        public_key_hex: attacker.public_key_hex(),
        last_ip: "10.0.0.66".to_string(),
        bound_at: chrono::Utc::now().to_rfc3339(),
        is_trusted: true,
        trust_level: feisuo_core::security::TrustLevel::Permanent,
        visible: true,
        last_seen_at: chrono::Utc::now().timestamp(),
        pairing_epoch: String::new(),
    }
}

// ==============================================================
// 12. device_id 必须由公钥派生 (否则可自造已信任身份)
// ==============================================================
#[test]
fn test_identity_creates_its_own_parent_directory() {
    // 旧实现建的是 `AppConfig::get_app_dir()`(全局默认目录) 而不是
    // key_path 自己的父目录, 于是:
    // - Android 注入 filesDir 时父目录没被创建, 落盘直接失败;
    // - 集成测试用临时路径时, 反而在**用户真实目录**里凭空建出空脚手架,
    //   跑完还留在那儿 —— 属于污染用户环境。
    // 这里用一个**尚不存在**的多级父目录来验证修复。
    let root = temp_dir("keyparent");
    let nested = root.join("level1").join("level2").join("level3");
    assert!(
        !nested.exists(),
        "前置条件: 多级父目录此时必须还不存在, 否则测不出问题"
    );

    let key_path = nested.join("device_identity.key");
    let id = DeviceIdentity::load_or_generate_at(key_path.clone())
        .expect("应当能自动创建 key_path 的父目录并落盘");

    assert!(nested.exists(), "父目录必须被自动创建");
    assert!(key_path.exists(), "私钥文件必须落盘");
    assert_eq!(std::fs::metadata(&key_path).unwrap().len(), 32);

    // 不得留下临时文件 (先写 .tmp 再 rename, 成功路径上必须已被消费)
    let leftovers: Vec<String> = std::fs::read_dir(&nested)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "原子写的临时文件不得残留, 实际: {:?}",
        leftovers
    );

    // 再加载一次必须得到同一身份
    let again = DeviceIdentity::load_or_generate_at(key_path).unwrap();
    assert_eq!(id.device_id, again.device_id);
}

// ==============================================================
#[test]
fn test_device_id_is_derived_from_public_key() {
    let dir = temp_dir("derive");
    let a = DeviceIdentity::load_or_generate_at(dir.join("a.key")).unwrap();
    let b = DeviceIdentity::load_or_generate_at(dir.join("b.key")).unwrap();

    assert_eq!(
        DeviceIdentity::device_id_from_pubkey_hex(&a.public_key_hex()).unwrap(),
        a.device_id,
        "device_id 必须可由公钥重算得到"
    );
    assert_ne!(a.device_id, b.device_id, "不同私钥必须得到不同指纹");

    // 同一路径重复加载必须复用同一身份
    let a2 = DeviceIdentity::load_or_generate_at(dir.join("a.key")).unwrap();
    assert_eq!(a.device_id, a2.device_id, "身份必须持久化, 不得每次重启都变");

    // 私钥文件损坏应自动恢复而不是永久失败
    std::fs::write(dir.join("a.key"), b"corrupted").unwrap();
    let a3 = DeviceIdentity::load_or_generate_at(dir.join("a.key")).unwrap();
    assert_ne!(
        a.device_id, a3.device_id,
        "损坏后应生成全新身份并可继续运行"
    );
    assert!(
        dir.join("a.key.corrupt").exists(),
        "损坏的私钥文件应被备份保留, 实际目录: {:?}",
        std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>()
    );

    assert!(DeviceIdentity::device_id_from_pubkey_hex("not-hex").is_err());
    assert!(DeviceIdentity::device_id_from_pubkey_hex("aabb").is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

// ==============================================================
// 13. 零字节文件 + total_size == 0 不得产生 NaN / null 进度
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_empty_file_transfer_and_no_nan_progress() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    let mut b_progress = b.progress_rx.resubscribe();

    let empty = a.write_out("empty.txt", 0);
    a.engine
        .send_files(
            "127.0.0.1",
            b.port(),
            &b.engine.identity.device_id,
            "node-B",
            vec![empty],
        )
        .await
        .expect("空文件传输应当成功");

    let mut seen = 0;
    while let Ok(p) = b_progress.try_recv() {
        seen += 1;
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains("null"),
            "进度事件被序列化成 null (NaN 会破坏前端反序列化): {}",
            json
        );
        assert!(
            p.progress_percent.is_finite(),
            "progress_percent 出现非有限值: {}",
            p.progress_percent
        );
        assert!((0.0..=100.0).contains(&p.progress_percent));
    }
    assert!(seen > 0, "应当至少收到一条接收进度事件");

    let landed = b.inbox().join("empty.txt");
    assert!(landed.exists(), "空文件也必须落盘");
    assert_eq!(std::fs::metadata(&landed).unwrap().len(), 0);

    cleanup(&[a, b]);
}

// ==============================================================
// 14. 配置文件: 字段往返 + 缺省字段必须有合理取值
// ==============================================================
#[test]
fn test_config_roundtrip_and_defaults() {
    let cfg = AppConfig {
        device_name: "test".into(),
        transfer_port: 42100,
        discovery_port: 42101,
        auto_receive: true,
        autostart: false,
        receive_dir: PathBuf::from("C:/tmp/feisuo-test"),
        log_level: "DEBUG".into(),
        max_log_size_mb: 5,
        max_history_records: 500,
        record_retention_days: 30,
        max_concurrent_transfers: 3,
        discovery_bind: "0.0.0.0".into(),
        transfer_bind: "0.0.0.0".into(),
        close_action: "tray".into(),
        theme: "light".into(),
        auto_check_update: true,
        allowed_peer_subnets: Vec::new(),
        approval_timeout_secs: 90,
        session_grant_ttl_secs: 300,
    };
    let text = serde_json::to_string(&cfg).unwrap();
    let back: AppConfig = serde_json::from_str(&text).unwrap();
    assert_eq!(back.close_action, "tray");
    assert_eq!(back.theme, "light");
    assert_eq!(back.log_level, "DEBUG");
    assert!(back.auto_check_update);

    // 缺省字段: serde(default) 兜底必须给出**合法**取值,
    // 不能是空字符串, 否则前端会拿到 theme="" 这种无法渲染的状态
    let partial = r#"{"device_name":"old","transfer_port":42100,"discovery_port":42101,
        "auto_receive":true,"autostart":true,"receive_dir":"C:/tmp/feisuo"}"#;
    let parsed: AppConfig = serde_json::from_str(partial).unwrap();
    assert_eq!(parsed.close_action, "ask", "缺省应为每次询问");
    assert_eq!(parsed.theme, "dark", "缺省主题必须是合法值而非空串");
    assert_eq!(parsed.log_level, "INFO");
    assert_eq!(parsed.max_history_records, 500);
    assert_eq!(parsed.max_log_size_mb, 5);
    assert_eq!(parsed.record_retention_days, 30);
    assert_eq!(parsed.discovery_bind, "0.0.0.0", "缺省必须监听所有网卡");
    assert_eq!(parsed.transfer_bind, "0.0.0.0", "缺省必须监听所有网卡");
    // 老版本配置里根本没有这个字段, 必须缺省为 true (开启自动检查),
    // 否则升级上来的用户会莫名其妙丢掉静默检查更新的能力
    assert!(parsed.auto_check_update, "老配置缺省必须仍然自动检查更新");
    // §2.3.1：老配置里没有这个字段，必须缺省为 300（5 分钟）而不是 0 ——
    // 缺 0 意味着升级后第三个按钮静默消失，**已升级的用户会以为
    // 自己记错了**（"我明明点过那个按钮"）。而 0 的语义是"主动关闭"。
    assert_eq!(
        parsed.session_grant_ttl_secs, 300,
        "老配置缺省必须是 300s（默认开启），不是 0（那会让功能静默消失）"
    );
}

/// §2.3.1 的 TTL 上界是**安全属性**，所以守卫要钉住的是
/// 「配多大都只生效 10 分钟」以及「配 0 = 关闭」。
///
/// 这条守的是 `effective_session_grant_ttl` 这一个函数 ——
/// 上界必须由**它**说了算，而不是靠每个调用方记得 `min()`。
/// 调用方少写一个 `min`，安全边界就没了，而且不会有编译错误。
#[test]
fn session_grant_ttl_is_bounded_and_zero_means_off() {
    use feisuo_core::config::{
        effective_session_grant_ttl, SESSION_GRANT_DEFAULT_SECS, SESSION_GRANT_MAX_SECS,
    };

    assert_eq!(SESSION_GRANT_MAX_SECS, 600, "上界是 10 分钟 —— 改它是一次安全决策");
    assert_eq!(SESSION_GRANT_DEFAULT_SECS, 300);

    // 0 = 关闭（弹窗里不出现第三个按钮）
    assert_eq!(effective_session_grant_ttl(0), None);

    // 正常值原样通过
    assert_eq!(effective_session_grant_ttl(300), Some(300));

    // 超过上界必须被夹住 —— 这是本条守卫的核心
    assert_eq!(effective_session_grant_ttl(3600), Some(SESSION_GRANT_MAX_SECS));
    assert_eq!(
        effective_session_grant_ttl(u64::MAX),
        Some(SESSION_GRANT_MAX_SECS),
        "极端值也必须夹住，不能溢出成一张过期的授权"
    );
}

// ==============================================================
// 15. 协议常量保持稳定 (协议兼容性)
// ==============================================================
#[test]
fn test_protocol_constants_are_stable() {
    use feisuo_core::protocol;
    assert_eq!(protocol::PROTOCOL_VERSION, 1);
    assert_eq!(protocol::CHUNK_SIZE, 4 * 1024 * 1024);
    assert_eq!(protocol::MSG_PAIR, 1);
    assert_eq!(protocol::MSG_TRANSFER, 2);
    assert_eq!(protocol::MSG_BROWSE, 3);
    assert_eq!(protocol::MSG_PULL, 4);
    assert!(protocol::MAX_JSON_FRAME <= 8 * 1024 * 1024, "帧上限必须有限");
    assert!(protocol::MAX_FILES_PER_BATCH > 0);
    assert!(protocol::MAX_CONCURRENT_CONNECTIONS > 0);
    assert!(protocol::MAX_HANDSHAKE_SKEW_SECS > 0);
    assert_eq!(DISCOVERY_MULTICAST_ADDR, "239.255.42.100");
    assert_eq!(DEFAULT_TRANSFER_PORT, 42100);
}

// ==============================================================
// 16. 跨平台适配: 设备名必须唯一且可读, 目录必须可持久化
// ==============================================================
#[test]
fn test_device_name_is_sanitized_and_stable() {
    let dir = temp_dir("devname");
    // 走一次完整身份生成, 确认兜底名与身份关联且可重复得到
    let id = DeviceIdentity::load_or_generate_at(dir.join("id.key")).unwrap();
    let hint = DeviceIdentity::current_device_id_hint();
    // 当前进程未设置 FEISUO_APP_DIR 时读不到, 允许为 None
    if let Some(h) = hint {
        assert!(h.starts_with("feisuo-"), "指纹格式不对: {}", h);
    }
    assert!(!id.device_id.is_empty());

    // 同一路径重复加载必须得到同一身份(名字兜底也因此稳定)
    let id2 = DeviceIdentity::load_or_generate_at(dir.join("id.key")).unwrap();
    assert_eq!(id.device_id, id2.device_id);

    let _ = std::fs::remove_dir_all(&dir);
}

// ==============================================================
// 16.1 设备名消毒: 控制字符/超长/纯空白必须被处理
// ==============================================================
#[test]
fn test_sanitize_device_name() {
    // 控制字符(tab / 换行)必须被剔除, 前后空白被裁掉
    assert_eq!(sanitize_device_name("  我的\t设备\n  "), "我的设备");
    // 保留普通内部空格(用户可能就叫"书房 台式")
    assert_eq!(sanitize_device_name("书房 台式"), "书房 台式");
    // 限长 32 字符
    let long = "超长设备名称".repeat(50);
    assert_eq!(sanitize_device_name(&long).chars().count(), 32);
    // 空串/纯控制字符不得产出空名字
    assert_eq!(sanitize_device_name(""), "飞梭设备");
    assert_eq!(sanitize_device_name("   \t\n  "), "飞梭设备");
    // NUL 字节必须被剔除
    assert!(!sanitize_device_name("a\0b").contains('\0'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_beacon_never_carries_unsanitized_name() {
    let _serial = discovery_lock();
    // 设备名会广播给局域网内所有对端, 并写进对端的传输记录与日志。
    // 带 `\n` 的名字能让对端 UI 排版错乱, 还能在对端日志里伪造记录。
    // 旧实现直接 clone 配置里的原始值, 消毒只存在于一个没有调用点的函数里。
    let a = Node::spawn_with_bind("NAME-A", false, "127.0.0.1").await;
    let b = Node::spawn_with_bind("NAME-B", false, "127.0.0.2").await;

    // 往 A 的配置里塞一个带控制字符的名字, 模拟"手工改过配置文件"或
    // "旧版本写入过脏数据"两种情况
    {
        let mut cfg = a.engine.config.write().await;
        cfg.device_name = "我的\n设备\r\u{7}偷偷的名字".to_string();
    }

    a.engine.discovery.start().await.expect("A 启动发现");
    b.engine.discovery.start().await.expect("B 启动发现");

    // 等 B 发现 A
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut seen: Option<feisuo_core::DiscoveredDevice> = None;
    while std::time::Instant::now() < deadline && seen.is_none() {
        if let Some(d) = b
            .engine
            .discovery
            .get_online_devices()
            .await
            .into_iter()
            .find(|d| d.device_id == a.engine.identity.device_id)
        {
            seen = Some(d);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let dev = seen.expect("B 应当发现 A");
    let name = dev.device_name;
    assert!(
        !name.contains('\n') && !name.contains('\r'),
        "广播出去的名字不得含换行 (可被用于伪造对端日志): {:?}",
        name
    );
    assert!(
        !name.chars().any(|c| c.is_control()),
        "广播出去的名字不得含控制字符: {:?}",
        name
    );
    assert_eq!(name, "我的设备偷偷的名字", "控制字符应被剔除而非整体拒绝");

    // 重新加载配置时也必须消毒, 保证"本机看到的"与"对端看到的"一致
    let saved = std::fs::read_to_string(a.dir.join("config.json")).unwrap_or_default();
    if !saved.is_empty() {
        let parsed: AppConfig = serde_json::from_str(&saved).unwrap();
        assert!(
            !parsed.device_name.contains('\n'),
            "落盘的配置也必须已消毒, 实际: {:?}",
            parsed.device_name
        );
    }

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_probe_reply_never_carries_unsanitized_name() {
    // 主动探测走的是**回信**路径而不是广播路径 —— 两条路径各自构造信标,
    // 修一条漏一条。这个用例专门盯回信那条。
    let _serial = discovery_lock();
    let a = Node::spawn_with_bind("PNAME-A", false, "127.0.0.1").await;
    let b = Node::spawn_with_bind("PNAME-B", false, "127.0.0.2").await;
    {
        let mut cfg = b.engine.config.write().await;
        cfg.device_name = "回信\n注入\u{7}测试".to_string();
    }
    a.engine.discovery.start().await.expect("A 启动发现");
    b.engine.discovery.start().await.expect("B 启动发现");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline
        && a.engine.discovery.get_online_devices().await.is_empty()
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A 主动探测 B 所在的回环地址 —— 走回信路径
    let probed = a
        .engine
        .probe_device("127.0.0.2")
        .await
        .expect("探测应当成功");
    assert_eq!(probed.device_id, b.engine.identity.device_id);
    assert!(
        !probed.device_name.chars().any(|c| c.is_control()),
        "回信里的设备名不得含控制字符: {:?}",
        probed.device_name
    );
    assert_eq!(probed.device_name, "回信注入测试");

    cleanup(&[a, b]);
}

/// 设备名在广播给对端前必须限长, 避免超长名字撑坏对端侧边栏排版
#[test]
fn test_device_name_length_is_bounded() {
    let long = "超长设备名称".repeat(50);
    let bounded: String = long.chars().take(32).collect();
    assert!(bounded.chars().count() <= 32, "设备名必须被截断到 32 字符以内");
}

// ==============================================================
// 17. 应用目录可被宿主显式覆盖 (Android 宿主靠它指向私有目录)
// ==============================================================
#[test]
fn test_app_dir_can_be_overridden() {
    let dir = temp_dir("appdir");
    // 环境变量注入是 core 唯一允许的"外部指定数据目录"入口,
    // 缺失时必须仍能返回可用路径而不能 panic
    let resolved = feisuo_core::resolve_app_dir();
    assert!(
        !resolved.as_os_str().is_empty(),
        "应用目录不得为空: {:?}",
        resolved
    );
    assert!(
        resolved.is_absolute() || resolved.to_string_lossy().contains("feisuo"),
        "应用目录应当是可持久化位置, 实际: {:?}",
        resolved
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ==============================================================
// 19. init_in: 显式指定数据目录必须端到端生效 (Android 宿主入口)
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_init_in_honors_explicit_app_dir() {
    let root = temp_dir("init-in");

    // 1. 首次初始化: 应当在指定目录下生成 私钥 / 配置 / 信任库
    let handles = FeisuoEngine::init_in(root.clone())
        .await
        .expect("init_in 应当成功");
    let engine = handles.engine;
    assert_eq!(engine.app_dir, root, "app_dir 必须等于显式传入的目录");
    for name in ["device_identity.key", "config.json", "trust_store.db"] {
        assert!(
            root.join(name).exists(),
            "init_in 必须在指定目录生成 {} (实际目录内容: {:?})",
            name,
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        );
    }

    let first_id = engine.identity.device_id.clone();
    let first_recv = engine.config.read().await.receive_dir.clone();
    drop(engine);

    // 2. 二次初始化: 指纹与配置必须复用, 而不是在别处重新生成
    let again = FeisuoEngine::init_in(root.clone())
        .await
        .expect("二次 init_in 应当成功");
    assert_eq!(
        again.engine.identity.device_id, first_id,
        "同一目录二次启动必须复用同一设备指纹, 否则等于每次都换新身份"
    );
    assert_eq!(
        again.engine.config.read().await.receive_dir, first_recv,
        "收件目录必须从已有配置读回"
    );

    // 3. 不同目录必须得到不同身份 (证明目录隔离真的生效)
    let other = temp_dir("init-in-other");
    let third = FeisuoEngine::init_in(other.clone())
        .await
        .expect("另一个目录也应当能初始化");
    assert_ne!(
        third.engine.identity.device_id, first_id,
        "不同数据目录必须产生不同设备身份"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&other);
}

// ==============================================================
// 20. init_in 对非法目录给出可读错误, 而不是 panic
// ==============================================================
#[tokio::test]
async fn test_init_in_rejects_unwritable_path_with_clear_error() {
    // 试图在"文件"下面建目录, 必然失败
    let blocker = temp_dir("init-in-bad").join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();

    let err = match FeisuoEngine::init_in(blocker.join("child")).await {
        Ok(_) => panic!("非法路径必须返回错误, 实际却初始化成功了"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("数据目录") || msg.contains("I/O"),
        "错误信息必须指出是数据目录问题, 实际: {}",
        msg
    );
}

// ==============================================================
// 20. 局域网发现: 两个独立节点必须能互相发现
// ==============================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_discovery_finds_peer() {
    let _serial = discovery_lock();
    // 同机两实例必须绑不同回环地址, 否则第二个绑同一端口必然失败
    let a = Node::spawn_with_bind("DISC-A", false, "127.0.0.1").await;
    let b = Node::spawn_with_bind("DISC-B", false, "127.0.0.2").await;

    a.engine.discovery.start().await.expect("A 启动发现");
    b.engine.discovery.start().await.expect("B 启动发现");

    // 两侧都必须在超时内看到对方
    let mut a_sees_b = false;
    let mut b_sees_a = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !(a_sees_b && b_sees_a) {
        if !a_sees_b
            && a.engine
                .discovery
                .get_online_devices()
                .await
                .iter()
                .any(|d| d.device_id == b.engine.identity.device_id)
        {
            a_sees_b = true;
        }
        if !b_sees_a
            && b.engine
                .discovery
                .get_online_devices()
                .await
                .iter()
                .any(|d| d.device_id == a.engine.identity.device_id)
        {
            b_sees_a = true;
        }
        if !(a_sees_b && b_sees_a) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    assert!(
        a_sees_b,
        "A 未发现 B; A 列表: {:?}",
        a.engine.discovery.get_online_devices().await
    );
    assert!(
        b_sees_a,
        "B 未发现 A; B 列表: {:?}",
        b.engine.discovery.get_online_devices().await
    );

    // 双方都不得把自己算进在线列表
    let a_online = a.engine.discovery.get_online_devices().await;
    assert!(
        !a_online.iter().any(|d| d.device_id == a.engine.identity.device_id),
        "不得把自己列入在线列表"
    );
    assert!(
        !a_online.iter().any(|d| d.is_trusted),
        "未配对设备不得显示为已信任"
    );

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_probe_reports_failure_for_silent_peer() {
    let _serial = discovery_lock();
    let a = Node::spawn_with_bind("PROBE-A", false, "127.0.0.1").await;
    a.engine.discovery.start().await.expect("A 启动发现");

    // 探测一个确定没人监听的地址: 必须明确失败, 不能恒返回成功
    // (旧实现只发包就 return Ok(()), UI 上"已识别设备"永远是假的)
    // 探测一个本机没有实例监听的地址(同样是回环, 但该端口上无节点)
    let res = a.engine.probe_device("127.0.0.1").await;
    assert!(
        res.is_err(),
        "无人应答的探测必须报错, 实际: {:?}",
        res.map(|d| d.device_name)
    );
    let msg = res.unwrap_err().to_string();
    assert!(
        msg.contains("未能发现") || msg.contains("无应答"),
        "错误信息必须说明是无应答, 实际: {}",
        msg
    );

    // 非法 IP 必须在发出任何包之前就被拒
    assert!(a.engine.probe_device("不是IP").await.is_err());
    assert!(a.engine.probe_device("999.999.999.999").await.is_err());
    assert!(a.engine.probe_device("").await.is_err());

    cleanup(&[a]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_probe_finds_running_peer() {
    let _serial = discovery_lock();
    let a = Node::spawn_with_bind("PROBE2-A", false, "127.0.0.1").await;
    let b = Node::spawn_with_bind("PROBE2-B", false, "127.0.0.2").await;
    a.engine.discovery.start().await.expect("A 启动发现");
    b.engine.discovery.start().await.expect("B 启动发现");

    // 先等广播互相看见, 再走主动探测, 避免只测到广播路径
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !a.engine.discovery.get_online_devices().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // 主动探测 B 所在的回环地址, 必须拿到 B 的应答
    let probed = a.engine.probe_device("127.0.0.2").await;
    assert!(
        probed.is_ok(),
        "探测运行中的对端应当成功, 实际错误: {:?}",
        probed.err()
    );
    let dev = probed.expect("必须有应答");
    assert_eq!(dev.device_id, b.engine.identity.device_id);

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stop_releases_port_and_silences_discovery() {
    // stop() 必须是**真的**停: 旧实现里三个循环(广播/监听/过期清理)都是
    // 无出口的 loop {}, 于是同一进程第二次 start() 会起第二套网络栈去抢
    // 同一个端口, Android Service 重建时更会持续累积。
    let _serial = discovery_lock();
    let a = Node::spawn_with_bind("STOP-A", false, "127.0.0.1").await;
    let b = Node::spawn_with_bind("STOP-B", false, "127.0.0.2").await;

    a.engine.discovery.start().await.expect("A 启动发现");
    b.engine.discovery.start().await.expect("B 启动发现");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline
        && b.engine.discovery.get_online_devices().await.is_empty()
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !b.engine.discovery.get_online_devices().await.is_empty(),
        "停止前 B 应当已经看到 A"
    );

    // 停掉 A 的发现服务
    a.engine.discovery.stop();
    a.engine.server.stop();

    // 1) 过期清理应把 A 从 B 的在线列表里剔除 (TTL 20s, 清理每 5s 一次)
    let drop_deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < drop_deadline
        && b.engine
            .discovery
            .get_online_devices()
            .await
            .iter()
            .any(|d| d.device_id == a.engine.identity.device_id)
    {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        !b.engine
            .discovery
            .get_online_devices()
            .await
            .iter()
            .any(|d| d.device_id == a.engine.identity.device_id),
        "A 已停止发现, B 的在线列表里不应再有它"
    );

    // 2) 端口必须能被重新绑定 —— 这才是 stop() 真正释放了套接字的证据
    let restart = a.engine.discovery.start().await;
    assert!(
        restart.is_ok(),
        "stop() 之后必须能重新绑定发现端口, 实际错误: {:?}",
        restart.err()
    );

    // 3) 重新启动后仍能被发现, 说明没有残留任务把状态搅乱
    b.engine.discovery.stop();
    let c = Node::spawn_with_bind("STOP-C", false, "127.0.0.2").await;
    c.engine.discovery.start().await.expect("C 启动发现");
    let see_deadline = std::time::Instant::now() + Duration::from_secs(12);
    let mut ok = false;
    while std::time::Instant::now() < see_deadline {
        if c.engine
            .discovery
            .get_online_devices()
            .await
            .iter()
            .any(|d| d.device_id == a.engine.identity.device_id)
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(ok, "A 重启后应当重新出现在对端的在线列表里");

    cleanup(&[a, b, c]);
}

// ==============================================================
// 21. 传输记录保留策略: 条数上限与保留天数都要生效
// ==============================================================
#[test]
fn test_transfer_history_retention_policy() {
    let dir = temp_dir("history");
    let store = TrustStore::open_at(dir.join("trust_store.db")).unwrap();

    for i in 0..10 {
        store
            .add_transfer_record(
                &format!("f{}.bin", i),
                i as u64,
                if i % 2 == 0 { "send" } else { "recv" },
                "peer",
                "127.0.0.1",
                "completed",
                5,     // 最多保留 5 条
                30,    // 30 天
                feisuo_core::security::TransferMetrics::default(),
                &[],
            )
            .expect("写入历史");
    }
    let rows = store.list_transfer_records(100).unwrap();
    assert_eq!(rows.len(), 5, "超出上限必须裁剪到最旧的被删除");
    // 应保留最近的 5 条, 即 f5..f9
    assert!(rows.iter().any(|r| r.file_name == "f9.bin"));
    assert!(!rows.iter().any(|r| r.file_name == "f0.bin"));

    // limit 参数必须生效
    assert_eq!(store.list_transfer_records(2).unwrap().len(), 2);

    store.clear_transfer_records().unwrap();
    assert!(store.list_transfer_records(100).unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

// ==============================================================
// 22. 拒绝必须**出站**: 发送方要拿到原因, 而不是一句 "early eof"
// ==============================================================
//
// ## 这组测试在防什么
//
// 用户报的现象: 界面弹一个红色 toast —— "I/O error: early eof"。
// 这句话由两半拼成:
//   - `FeisuoError::Io` 的 Display 前缀 "I/O error: "
//   - tokio 给 `read_exact` 撞上对端关闭时的字面量文案 "early eof"
//     (`tokio/src/io/util/read_exact.rs`)
//
// 也就是说: **对端在应答还没发出来之前就把连接关了**。
// 而当时的服务端在握手准入这一段有十来条裸 `return Err`
// —— 只关连接、不给对端任何东西。被拉黑、版本不兼容、签名不过、
// 时效偏差……全都长成同一个 "early eof",
// 真实原因只存在于**对方**的日志里。
//
// 断言写成"消息里必须含原因关键词"而不是"必须等于某句文案",
// 因为文案会改; 但 `early eof` **永远不许出现** ——
// 它一旦出现就说明又有一条拒绝路径绕过了 `refuse_transfer`。

/// 给对端配一套**会拒绝对端**的可访问范围。
///
/// 之所以选它当载体：`blacklisted_devices` 已整体删除，而"拒绝必须出站"
/// 这个缺陷（服务端只关连接、不给对端任何东西 ⇒ 对端只看到 `early eof`）
/// 依然存在，必须用一个**还活着**的拒绝路径来钉。
///
/// `can_push=false` → 传输被拒（走 `refuse_transfer`）
/// `can_pull=false` → 取回被拒（走 `refuse_by_msg_type(MSG_PULL)`）
/// `mode=ReceiveOnly` → 收件目录仍可浏览，收件目录之外被拒
fn deny_scope(push: bool, pull: bool, mode: feisuo_core::security::AccessMode) -> feisuo_core::security::AccessScope {
    feisuo_core::security::AccessScope {
        mode,
        allow_volumes: vec![],
        allow_paths: vec![],
        deny_paths: vec![],
        can_pull: pull,
        can_push: push,
        updated_at: 0,
    }
}

/// 断言这条错误是**有原因的拒绝**，而不是裸的 `early eof`。
fn assert_tells_the_sender_why(err: &feisuo_core::FeisuoError, why: &str, ctx: &str) {
    let msg = err.to_string();
    assert!(
        !msg.contains("early eof"),
        "{}: 拒绝必须给出可读原因, 不能退化成裸 errno (实际: {})",
        ctx,
        msg
    );
    assert!(
        msg.contains(why),
        "{}: 拒绝原因里应包含 {:?} (实际: {})",
        ctx,
        why,
        msg
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_transfer_rejection_reaches_sender_with_a_reason() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    // B 撤销 A 的写入权 —— 这是**用户自己配的授权**
    b.engine
        .trust_store
        .set_access_scope(
            &a.engine.identity.device_id,
            &deny_scope(false, true, feisuo_core::security::AccessMode::All),
        )
        .expect("写访问范围应成功");

    let src = a.write_out("rejected.bin", 1024);
    let err = a
        .engine
        .send_files("127.0.0.1", b.port(), &b.engine.identity.device_id, "node-B", vec![src])
        .await
        .expect_err("无写入权的设备不得传输成功");

    assert_tells_the_sender_why(&err, "无权写入", "传输被可访问范围拒绝");
    assert_eq!(
        std::fs::read_dir(b.inbox()).unwrap().count(),
        0,
        "被拒的传输绝不允许落盘"
    );

    cleanup(&[a, b]);
}

/// 「每次匹配码」协商回合必须以**结构化**方式回到发送方。
///
/// 这条守的不只是"要出站"，而是**分类要正确**：`GrantCodeRequired` 是一次
/// 正常回合（对方应保留待发队列并弹码重试），而"硬拒绝"会让用户以为失败
/// 并反复重试。§14.10 记录了它一度只靠 `message.contains("传输码")` 判断 ——
/// 那正是本轮在 core 里拆掉的反模式。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_grant_code_round_trip_is_reported_as_a_negotiation_not_a_rejection() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    // 把 B 对 A 的信任降到「每次认证」
    b.engine
        .trust_store
        .set_trust_level(&a.engine.identity.device_id, feisuo_core::security::TrustLevel::Session)
        .expect("降级为每次认证应成功");

    // 「每次认证」下服务端会**先弹审批**，再发现对端没出示码 ⇒ 回
    // `requires_grant_code: true`。所以这里必须有人**点允许**，
    // 否则服务端会一直等到审批超时（30s），测试只会看到
    // 「读取 握手应答 长度超时」—— 那恰恰是本轮修掉的症状，
    // 不是我们要断言的东西。
    let mut approval_rx = b.approval_rx.resubscribe();
    let approver = tokio::spawn(async move {
        let req = match tokio::time::timeout(Duration::from_secs(8), approval_rx.recv()).await {
            Ok(Ok(r)) => r,
            other => panic!("应当收到审批请求, 实际: {:?}", other),
        };
        req.approval_id
    });

    let src = a.write_out("needs-code.bin", 512);
    let send = tokio::spawn({
        let port = b.port();
        let id = b.engine.identity.device_id.clone();
        let e = a.engine.clone();
        let s = src.clone();
        async move { e.send_files("127.0.0.1", port, &id, "node-B", vec![s]).await }
    });
    // 收到审批请求就点「允许一次」
    let approval_id = approver.await.expect("审批任务应完成");
    b.engine
        .respond_approval(&approval_id, ApprovalAction::AllowOnce);

    let err = send
        .await
        .expect("发送任务应结束")
        .expect_err("「每次认证」等级下第一次不该直接成功");

    assert!(
        matches!(err, feisuo_core::FeisuoError::GrantCodeRequired(_)),
        "必须是 GrantCodeRequired（正常协商回合）, 实际: {}",
        err
    );
    assert_eq!(
        std::fs::read_dir(b.inbox()).unwrap().count(),
        0,
        "没出示码之前绝不允许落盘"
    );

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_browse_rejection_reaches_requester_in_browse_shape() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    // 「仅收件目录」拒绝的是真实磁盘，不是收件目录自己。
    //
    // 旧断言要求浏览收件根也被拒。那会把这个模式做成"什么都看不见"：
    // 用户选了最严一档之后，对方连刚收下的文件都打不开。
    // 这里改成：收件根必须能打开，收件根之外的卷必须被拒，
    // 并且拒绝原因要出站（不能退化成 early eof / 结构错位）。
    b.engine
        .trust_store
        .set_access_scope(
            &a.engine.identity.device_id,
            &deny_scope(true, true, feisuo_core::security::AccessMode::ReceiveOnly),
        )
        .expect("写访问范围应成功");

    let listing = a
        .engine
        .list_remote_files("127.0.0.1", b.port(), &feisuo_core::BrowseTarget::legacy(""))
        .await
        .expect("仅收件目录时，收件根必须能浏览");
    assert!(
        !listing.volume_mode,
        "仅收件目录不得回真实卷模式"
    );

    // 盘根一定在收件目录之外。用它而不是临时目录：临时目录有可能
    // 恰好落在测试收件箱里，断言就会被跳过，守卫变成空的。
    let outside = feisuo_core::storage::volumes::split_browse_path(std::path::Path::new(
        &std::env::temp_dir(),
    ))
    .map(|(vol, _)| (vol, String::new()));
    let Some((vol, rel)) = outside else {
        panic!("本机临时目录必须能拆出卷号，否则这条守卫测不到「盘外拒绝」");
    };
    assert!(
        !feisuo_core::storage::is_within(
            feisuo_core::storage::volumes::resolve_browse_path(&vol, &rel)
                .expect("盘根必须能解析")
                .as_path(),
            &b.inbox()
        ),
        "测试收件目录不得就是盘根，否则「盘外拒绝」没有可拒的目标"
    );
    let err = a
        .engine
        .list_remote_files(
            "127.0.0.1",
            b.port(),
            &feisuo_core::transport::BrowseTarget {
                volume: vol,
                rel_path: rel,
                offset: 0,
                limit: 0,
                grant_code: String::new(),
            },
        )
        .await
        .expect_err("仅收件目录时，收件目录之外的卷必须被拒");
    assert_tells_the_sender_why(&err, "无权浏览", "浏览被可访问范围拒绝");

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pairing_rejection_reaches_requester_in_pair_shape() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    let (_pin, _) = b.engine.trust_store.generate_pair_pin();

    // 配对码错误 —— 走 `refuse_by_msg_type(MSG_PAIR)`。
    // 回错结构的话对端会撞 `Serialization error: missing field 'device_id'`。
    let err = a
        .engine
        .pair_with_device("127.0.0.1", b.port(), "000000")
        .await
        .expect_err("错误配对码必须被拒");

    assert_tells_the_sender_why(&err, "配对码", "配对码错误");

    cleanup(&[a, b]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pull_rejection_reaches_requester() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;
    let f = write_test_file(&b.inbox(), "take-me.txt", 64);

    b.engine
        .trust_store
        .set_access_scope(
            &a.engine.identity.device_id,
            &deny_scope(true, false, feisuo_core::security::AccessMode::All),
        )
        .expect("写访问范围应成功");

    let err = a
        .engine
        .request_pull(
            "127.0.0.1",
            b.port(),
            vec![f.file_name().unwrap().to_string_lossy().to_string()],
            "",
        )
        .await
        .expect_err("无取回权的设备不得取回");

    assert_tells_the_sender_why(&err, "无权取走", "取回被可访问范围拒绝");

    cleanup(&[a, b]);
}

/// 对端**不是飞梭**（或干脆挂了）时, 客户端必须说清楚是"对方关的连接"
/// 并带上**具体阶段**, 而不是把 tokio 的裸文案透给用户。
///
/// 这是本轮修掉的第二个缺口: `read_json_frame` 以前把 `label` 丢在
/// 半路, 于是握手的"读取握手应答长度"和分块传输的"读取清单内容"
/// 在用户那里长得一模一样。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_a_peer_that_hangs_up_is_reported_by_stage_not_by_errno() {
    let a = Node::spawn("A", false).await; // 不起传输服务, 我们自己当"假对端"

    // 假对端: **把请求读完再挂断**, 一个应答字节都不回。
    //
    // 为什么必须先读完: 一上来就 drop 的话, 对端那次 `write_all` 自己
    // 就先撞 10053（连接在请求还在途时被中止），于是测到的是
    // "写帧长度" 而不是 "读应答时对方关的连接" —— 那是另一条路径,
    // 而且它掩盖了我们真正想守的那个缺口。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定假对端端口");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut type_byte = [0u8; 1];
            let mut len_buf = [0u8; 4];
            if sock.read_exact(&mut type_byte).await.is_ok()
                && sock.read_exact(&mut len_buf).await.is_ok()
            {
                let mut body = vec![0u8; u32::from_be_bytes(len_buf) as usize];
                let _ = sock.read_exact(&mut body).await;
            }
            // 读完了, 一个字节也不回 —— 这就是"对方挂了/不是飞梭"
            drop(sock);
        }
    });

    let err = a
        .engine
        .list_remote_files("127.0.0.1", port, &feisuo_core::BrowseTarget::legacy(""))
        .await
        .expect_err("对端直接挂断, 浏览必须失败");

    let msg = err.to_string();
    assert!(
        !msg.contains("early eof"),
        "裸 errno 绝不许透给用户 (实际: {})",
        msg
    );
    assert!(
        msg.contains("浏览应答"),
        "错误必须指出卡在哪个阶段, 实际: {}",
        msg
    );
    assert!(
        msg.contains("对方已关闭连接"),
        "错误必须说明是对方关的连接, 实际: {}",
        msg
    );

    cleanup(&[a]);
}
// ==============================================================
// 22. 双向解除配对（§14.5）：「一方解除，双方都变成未信任」
// ==============================================================
//
// ## 这组测试在防什么
//
// 旧实现里解除配对是**纯本地**的 `remove_device`：不发任何网络帧
// （协议里当时连 MSG_UNPAIR 都没有）。于是 A 解除之后 ——
//   A 那边: B 变成未信任 ✅
//   B 那边: 仍是永久信任, 并继续静默收 A 的文件 ❌
// 而两边界面都显示「永久信任」，用户完全无从察觉。
// 不对称的方向恰好是危险的那一侧。
//
// ## 为什么要有"配对世代"
//
// 解除配对必须双向，而对端可能不在线 ⇒ 消息必然要排队
// ⇒ 排队就意味着**可能重放**。攻击者录下旧的解除帧，等双方重新配对后
// 重放，就能把**新绑定**拆掉。`bound_at` 救不了：它是各自本地时钟，
// 两边各写各的，从来不相等。所以配对时必须协商出一个**共同**世代。

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unpair_is_bidirectional() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    // 双方都必须是永久信任，且**必须有共同世代**
    let epoch_a = a
        .engine
        .trust_store
        .pairing_epoch(&b.engine.identity.device_id)
        .unwrap()
        .expect("A 侧应记录了配对世代");
    let epoch_b = b
        .engine
        .trust_store
        .pairing_epoch(&a.engine.identity.device_id)
        .unwrap()
        .expect("B 侧应记录了配对世代");
    assert_eq!(epoch_a, epoch_b, "两端必须持有**同一个**配对世代");
    assert!(!epoch_a.is_empty(), "配对握手必须协商出非空世代");

    // A 解除配对 B
    let report = a
        .engine
        .unpair_device("127.0.0.1", b.port(), &b.engine.identity.device_id)
        .await
        .expect("解除配对不应报错");
    assert!(
        report.peer_notified,
        "对端在线时必须真的通知到: {} / {}",
        report.reason_code, report.user_message
    );
    assert!(report.local_applied, "本机必须降级");
    assert_eq!(
        report.reason_code, "applied",
        "对端在线且世代一致时必须是 applied"
    );

    // **两边都**降级 —— 这是本测试的全部意义
    assert_eq!(
        a.engine
            .trust_store
            .trust_level(&b.engine.identity.device_id)
            .unwrap(),
        Some(feisuo_core::security::TrustLevel::Pending),
        "A 这边应变成未信任"
    );
    assert_eq!(
        b.engine
            .trust_store
            .trust_level(&a.engine.identity.device_id)
            .unwrap(),
        Some(feisuo_core::security::TrustLevel::Pending),
        "**对端也必须**变成未信任 —— 旧实现下这条会失败, 对方仍是永久信任"
    );

    cleanup(&[a, b]);
}

/// 守卫：重放一帧**旧的**解除配对，**绝不能**拆掉新绑定。
///
/// 这是整个设计里唯一真正有难度的部分：解除配对必须双向 ⇒ 对端可能
/// 不在线 ⇒ 消息要排队 ⇒ 排队就有重放。而重放的危害不是"多解除一次"
/// （幂等无害），是**把双方重新建立的新信任拆掉**。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_replayed_unpair_cannot_break_a_new_binding() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;

    let a_id = a.engine.identity.device_id.clone();
    let b_id = b.engine.identity.device_id.clone();

    let old_epoch = a
        .engine
        .trust_store
        .pairing_epoch(&b_id)
        .unwrap()
        .unwrap();

    // A 解除配对（第一次，生效）
    let first = a
        .engine
        .unpair_device("127.0.0.1", b.port(), &b_id)
        .await
        .expect("首次解除应成功");
    assert!(first.peer_notified && first.local_applied);

    // 双方**重新配对** —— 世代必须换新
    setup_pair(&a, &b).await;
    let new_epoch = a
        .engine
        .trust_store
        .pairing_epoch(&b_id)
        .unwrap()
        .unwrap();
    assert_ne!(old_epoch, new_epoch, "重新配对必须产生新的世代");

    // **显式**把 B 侧的信任恢复成永久。
    //
    // 这里踩到一处真实的设计问题：`bind_device` 的 Refreshed 分支
    // 刻意**不写回** `trust_level`（理由见那里的注释：不能把用户
    // 主动降级成的 `session` 改回去）。但"被对端解除"造成的 `pending`
    // 和"用户自己降级"造成的 `session` 被同一处逻辑一起跳过了 ——
    // 于是**重新配对不会恢复信任**。那该由配对弹窗（§2.5）来问，
    // 而不是 `bind_device` 默默决定，所以本测试显式设置。
    //
    // 这个取舍本身值得单独拍板（见 DESIGN_TRUST_SHUTTLE §14.11 决策 5）。
    b.engine
        .trust_store
        .set_trust_level(&a_id, feisuo_core::security::TrustLevel::Permanent)
        .expect("恢复信任应成功");


    // 攻击者拿着**旧世代**来解除
    let outcome = b
        .engine
        .trust_store
        .apply_peer_unpair(&a_id, &old_epoch)
        .expect("apply_peer_unpair 不应报错");
    assert_eq!(
        outcome,
        feisuo_core::security::UnpairOutcome::EpochMismatch,
        "旧世代的解除帧必须被判定为**过期**, 而不是生效"
    );

    // **关键断言**：新绑定必须完好
    assert_eq!(
        b.engine.trust_store.trust_level(&a_id).unwrap(),
        Some(feisuo_core::security::TrustLevel::Permanent),
        "重放**绝不能**拆掉重新建立的新绑定"
    );

    // 而用**新**世代解除仍然要生效（证明上一条不是因为"压根不生效"）
    let outcome = b
        .engine
        .trust_store
        .apply_peer_unpair(&a_id, &new_epoch)
        .expect("apply_peer_unpair 不应报错");
    assert_eq!(outcome, feisuo_core::security::UnpairOutcome::Applied);

    cleanup(&[a, b]);
}

/// 守卫：幂等 —— 重复解除回成功而不是报错。
///
/// 「两边都点一次解除」是很自然的操作，若第二次报错，
/// 用户会以为没解干净而反复重试。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unpair_is_idempotent() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;
    let a_id = a.engine.identity.device_id.clone();
    let epoch = b
        .engine
        .trust_store
        .pairing_epoch(&a_id)
        .unwrap()
        .unwrap();

    assert_eq!(
        b.engine.trust_store.apply_peer_unpair(&a_id, &epoch).unwrap(),
        feisuo_core::security::UnpairOutcome::Applied
    );
    assert_eq!(
        b.engine.trust_store.apply_peer_unpair(&a_id, &epoch).unwrap(),
        feisuo_core::security::UnpairOutcome::AlreadyUnpaired,
        "第二次必须是幂等成功, 不是报错"
    );

    cleanup(&[a, b]);
}

/// 守卫：解除配对**必须保留「隐藏」偏好**（§14.3.1 的 I9）。
///
/// 旧实现用 `DELETE`，把 `visible` 一起删了，于是用户
/// 「隐藏 + 解除配对」之后那台设备下次重新配对会**重新出现在主列表** ——
/// 而他明确说过不想看见它。隐藏是显示偏好，与信任强度无关。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unpair_keeps_the_hidden_preference() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;
    let b_id = b.engine.identity.device_id.clone();

    a.engine.trust_store.set_visible(&b_id, false).expect("隐藏应成功");

    let report = a
        .engine
        .unpair_device("127.0.0.1", b.port(), &b_id)
        .await
        .expect("解除配对不应报错");
    assert!(report.peer_notified && report.local_applied);

    let after = a
        .engine
        .trust_store
        .list_devices()
        .unwrap()
        .into_iter()
        .find(|d| d.device_id == b_id)
        .expect("解除配对**不应删掉这一行**, 否则隐藏偏好会一起丢失");
    assert!(!after.visible, "解除配对必须保留 visible=false");
    assert_eq!(
        after.trust_level,
        feisuo_core::security::TrustLevel::Pending
    );
    assert!(
        after.pairing_epoch.is_empty(),
        "解除后世代必须清空 —— 否则重放旧帧仍能命中"
    );

    cleanup(&[a, b]);
}

/// 守卫：解除配对**必须连带清掉授权**（`access_scope` / `session_grants`）。
///
/// 那两者是**授权**；解除信任后留着它们等于"没解除" ——
/// 用户以为关上了门，实际对方还能按旧范围浏览/取回。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unpair_clears_access_scope_and_grants() {
    let a = Node::spawn("A", true).await;
    let b = Node::spawn("B", true).await;
    setup_pair(&a, &b).await;
    let a_id = a.engine.identity.device_id.clone();

    b.engine
        .trust_store
        .set_access_scope(
            &a_id,
            &feisuo_core::security::AccessScope {
                // 刻意用**非默认**的范围（默认是 `All` + 空列表）：
                // 解除配对是"删掉这一行"，之后读回来的是**默认值**。
                // 若断言落在默认值也恰好为真的字段上（如 `can_pull` 默认 true），
                // 那条断言就永远绿 —— 测不出"到底删没删"。
                mode: feisuo_core::security::AccessMode::Allowlist,
                allow_volumes: vec!["X:".into()],
                allow_paths: vec!["机密".into()],
                deny_paths: vec![],
                can_pull: true,
                can_push: true,
                updated_at: 0,
            },
        )
        .expect("写访问范围应成功");

    // ⚠️ 本守卫的名字一直写着「and grants」，但早先**只断言了 access_scope**。
    // `session_grants` 那一半从没人测 —— 于是「解除配对不清 session_grants」
    // 这条变异可以畅通无阻地被注入（变异测试实测：未被抓到）。
    //
    // 它之所以重要：清掉之后 `has_valid_grant` 必须回 false，
    // 否则对方仍能凭**解除前**那次授权进来 —— 用户以为关上了门。
    // TTL 给足 3600s，确保不是"恰好过期"造成的假绿。
    b.engine
        .trust_store
        .grant_session(&a_id, "browse", 3600)
        .expect("授予会话授权应成功");
    assert!(
        b.engine
            .trust_store
            .has_valid_grant(&a_id, "browse")
            .expect("查询授权应成功"),
        "前置条件：授权此刻应当有效"
    );

    let epoch = a
        .engine
        .trust_store
        .pairing_epoch(&b_id_of(&b))
        .unwrap()
        .unwrap();
    assert_eq!(
        b.engine
            .trust_store
            .apply_peer_unpair(&a_id, &epoch)
            .unwrap(),
        feisuo_core::security::UnpairOutcome::Applied
    );

    let scope = b
        .engine
        .trust_store
        .get_access_scope(&a_id)
        .unwrap();
    assert_ne!(
        scope.mode,
        feisuo_core::security::AccessMode::Allowlist,
        "解除配对后不得保留旧的可访问范围授权: {:?}",
        scope
    );
    assert!(
        scope.allow_volumes.is_empty() && scope.allow_paths.is_empty(),
        "旧的白名单必须一并清掉, 否则重新配对后旧授权会复活: {:?}",
        scope
    );

    // ---- `session_grants` 那一半（本守卫的名字一直承诺要测）----
    assert!(
        !b.engine
            .trust_store
            .has_valid_grant(&a_id, "browse")
            .expect("查询授权应成功"),
        "❌ 解除配对后仍持有有效的会话授权：对方能凭**解除前**那次授权进来"
    );

    cleanup(&[a, b]);
}

fn b_id_of(n: &Node) -> String {
    n.engine.identity.device_id.clone()
}