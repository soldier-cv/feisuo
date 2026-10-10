//! 端到端冒烟：把本轮改过的传输链路真正跑一遍（一次性工具，跑完即删）。
//!
//! ## 为什么需要它
//!
//! AGENTS.md 规则 2 禁止跑测试套件，所以 `core/tests/` 里的集成测试
//! **从未执行过**。而本轮给传输路径加了大量新代码：staging 原子提交、
//! StagingGuard、断点续传、socket buffer 二次调大、目录递归展开、
//! caps 协商、每次匹配码、MSG_CANCEL 取消帧、**内容派生的 transfer_id**。
//! `cargo check` 全绿**不代表**它们能跑通 —— 编译期看不见索引错位、
//! 字段漏填、清单与回执对不上、时序竞态。
//!
//! 本工具在**同一进程内起多个独立节点**（系统临时目录 + 127.0.0.1 +
//! 操作系统分配的端口），走真实协议栈。
//! 只写临时目录、只连回环，不触碰真实配置 / 真实文件 / 外部网络，跑完自删。
//!
//! 设 `FEISUO_SMOKE_LOG=1` 打开详细日志（排查失败时用）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use feisuo_core::config::AppConfig;
use feisuo_core::security::{DeviceIdentity, TrustLevel, TrustedDevice};
use feisuo_core::transport::BrowseTarget;
use feisuo_core::{
    ApprovalAction, ApprovalRequest, BindOutcome, FeisuoEngine, FeisuoError, TrustStore,
    TransferDiagnostics, TransferProgress,
};

const SETTLE: Duration = Duration::from_secs(25);

struct Node {
    engine: Arc<FeisuoEngine>,
    port: u16,
    inbox: PathBuf,
    dir: PathBuf,
    _progress_rx: tokio::sync::broadcast::Receiver<TransferProgress>,
    _approval_rx: tokio::sync::broadcast::Receiver<ApprovalRequest>,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("分配端口")
        .local_addr()
        .unwrap()
        .port()
}

async fn spawn(tag: &str, listen: bool) -> Node {
    let dir = std::env::temp_dir().join(format!(
        "feisuo-smoke-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let identity = Arc::new(
        DeviceIdentity::load_or_generate_at(dir.join("id.key")).expect("生成身份"),
    );
    let trust_store = Arc::new(TrustStore::open_at(dir.join("trust.db")).expect("开信任库"));
    let receive_dir = dir.join("inbox");
    std::fs::create_dir_all(&receive_dir).expect("建收件目录");

    // 用 Default 打底, 只覆盖必须隔离的项 —— 显式列全字段会在
    // AppConfig 增字段时静默漏掉, 而漏掉的那个字段会退回真实默认值
    // (比如 autostart=true 会去写真实注册表)。
    let cfg = AppConfig {
        device_name: format!("smoke-{}", tag),
        transfer_port: free_port(),
        discovery_port: free_port(),
        autostart: false,
        receive_dir: receive_dir.clone(),
        log_level: "ERROR".into(),
        max_log_size_mb: 1,
        // 审批等待要够长: 冒烟里审批是异步任务, 太短会误报超时
        approval_timeout_secs: 30,
        discovery_bind: "127.0.0.1".into(),
        transfer_bind: "127.0.0.1".into(),
        ..Default::default()
    };

    let handles = FeisuoEngine::assemble(
        identity,
        Arc::new(tokio::sync::RwLock::new(cfg)),
        trust_store,
    )
    .expect("组装引擎");
    let engine = Arc::new(handles.engine);
    let port = engine.config.read().await.transfer_port;
    let node = Node {
        engine: engine.clone(),
        port,
        inbox: receive_dir,
        dir,
        _progress_rx: handles.progress,
        _approval_rx: handles.approval,
    };
    if listen {
        // 只起传输服务, **不起发现** —— 冒烟不测 mDNS, 起了反而会
        // 往真实局域网发包、污染用户正在用的飞梭实例的设备列表。
        node.engine.server.start().await.expect("启动传输服务");
    }
    node
}

/// 建立永久互信（跳过配对流程, 直接写信任库）
fn trust(a: &Node, b: &Node) {
    let now = chrono::Utc::now();
    for (self_node, peer_id, peer_name, peer_pub) in [
        (a, b.engine.identity.device_id.clone(), "peer-b", b.engine.identity.public_key_hex()),
        (b, a.engine.identity.device_id.clone(), "peer-a", a.engine.identity.public_key_hex()),
    ] {
        let out = self_node
            .engine
            .trust_store
            .bind_device(&TrustedDevice {
                device_id: peer_id.clone(),
                device_name: peer_name.into(),
                public_key_hex: peer_pub,
                last_ip: "127.0.0.1".into(),
                bound_at: now.to_rfc3339(),
                is_trusted: true,
                trust_level: TrustLevel::Permanent,
                visible: true,
                last_seen_at: now.timestamp(),
                // 这个冒烟脚本直接写库、不走配对握手，所以没有协商世代。
                // 与真实运行一致的后果：双向解除配对会拒绝（no_shared_epoch）。
                pairing_epoch: String::new(),
            })
            .expect("写入信任库");
        // 不检查 bind 结果的话, KeyConflict 会被静默吞掉, 后面表现为
        // "对端明明配对了却一直要审批" —— 极难定位。
        assert_eq!(
            out,
            BindOutcome::Added,
            "绑定 {} 失败: {:?}（KeyConflict 说明公钥与 device_id 不匹配）",
            peer_id,
            out
        );
    }
}

async fn wait_until<F: FnMut() -> bool>(mut f: F, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    false
}

fn hash_of(p: &Path) -> String {
    match std::fs::read(p) {
        Ok(b) => blake3::hash(&b).to_hex().to_string(),
        Err(_) => String::new(),
    }
}

fn staging_is_empty(inbox: &Path) -> bool {
    std::fs::read_dir(inbox.join(".feisuo-incoming"))
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

fn count_tree(p: &Path) -> usize {
    std::fs::read_dir(p)
        .map(|d| {
            d.flatten()
                .map(|e| {
                    if e.path().is_dir() {
                        1 + count_tree(&e.path())
                    } else {
                        1
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

fn dump_tree(p: &Path, depth: usize) {
    if let Ok(d) = std::fs::read_dir(p) {
        let mut v: Vec<_> = d.flatten().collect();
        v.sort_by_key(|e| e.file_name());
        for e in v {
            println!("       {}{}", "  ".repeat(depth), e.file_name().to_string_lossy());
            if e.path().is_dir() {
                dump_tree(&e.path(), depth + 1);
            }
        }
    }
}

/// 后台审批器: 收到请求后按脚本动作回应。
///
/// 返回收到的**全部**请求, 供主流程断言 `requires_grant_code`
/// 与"码输错时的重输提示"这两个协议行为。
///
/// 注意脚本的粒度是"**每次弹窗**": 输错码时对端会用**同一个**
/// `approval_id` 重新弹窗, 所以要观察重输行为, 脚本里就得给多个步骤。
fn spawn_approver(
    engine: Arc<FeisuoEngine>,
    script: Vec<Action>,
) -> tokio::task::JoinHandle<Vec<ApprovalRequest>> {
    tokio::spawn(async move {
        let mut rx = engine.approval_tx.subscribe();
        let mut seen = Vec::new();
        let mut step = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while step < script.len() {
            // `recv()` **必须**带超时。
            //
            // 脚本步数多于协议实际产生的审批次数时（例如"连续输错 3 次"
            // 只产生 3 次，脚本却写了 4 步），`recv()` 会**永远阻塞** ——
            // broadcast 的 recv 不自己超时，而 `while` 的条件只有在
            // `recv()` 返回之后才会被重新检查。
            //
            // 后果是测试**挂死**而不是失败，而挂死的测试与挂死的产品
            // 在报告里长得一模一样：都是"没输出、CPU 占用 0"。
            // 这个仓库已经因此浪费过一轮排查，所以在这里钉死。
            let now = tokio::time::Instant::now();
            if now >= deadline {
                println!(
                    "       [审批] 脚本还剩 {} 步未等到审批（可能协议不再产生该审批）",
                    script.len() - step
                );
                break;
            }
            let Ok(req) = tokio::time::timeout_at(deadline, rx.recv()).await else {
                break;
            };
            let Ok(req) = req else { break };
            seen.push(req.clone());
            let action = script[step];
            step += 1;
            // 真实审批走 UI; 这里直接调引擎接口, 等价于"用户在弹窗里点了允许"。
            //
            // 「每次匹配码」等级下**审批人不再输入任何码** ——
            // 码是引擎生成、显示在弹窗上的, 由**发起方**敲回来。
            // 所以 `Action::AllowWithCode` 里的码现在的语义是
            // "模拟一个已经知道本机码的发起方" —— 见 usage() 里的说明。
            let act = match action {
                Action::Allow | Action::AllowWithCode(_) => ApprovalAction::AllowOnce,
                Action::Reject => ApprovalAction::Reject,
            };
            if !engine.respond_approval(&req.approval_id, act) {
                println!("       [审批] 回应失败（approval_id 已被消费或过期）");
            }
        }
        seen
    })
}

#[derive(Clone, Copy)]
enum Action {
    Allow,
    Reject,
    /// 允许，并在**回应里带上一个码**。
    ///
    /// 只用来触发"码不匹配"的重输回路。
    ///
    /// 注意：正常路径下审批人**什么都不用带** —— 匹配码是接收方自己生成、
    /// 显示在审批窗口上的，由发起方敲回来。脚本里要用"正确的码"，
    /// 应该从 `ApprovalRequest.grant_challenge` 里读，而不是写死。
    #[allow(dead_code)]
    AllowWithCode(&'static str),
}

/// 从审批请求里取出接收方生成的匹配码。
///
/// 这是"接收方出码、发起方输入"这条链路的**唯一**通道：真实环境下
/// 这个码是用户**看着接收方屏幕**敲进去的，测试里我们能直接读到，
/// 所以测试验证的是**协议与状态机**，人眼那一环无法也不该自动化。
fn challenge_of(reqs: &[ApprovalRequest]) -> String {
    reqs.iter()
        .find_map(|r| {
            let c = r.grant_challenge.trim();
            if c.len() == 6 && c.chars().all(|ch| ch.is_ascii_digit()) {
                Some(c.to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn main() {
    if std::env::var("FEISUO_SMOKE_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();
    }
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("无法建 runtime: {}", e);
            std::process::exit(2);
        }
    };
    let code = rt.block_on(run());
    std::process::exit(code);
}

async fn run() -> i32 {
    let mut fail = 0usize;
    macro_rules! check {
        ($cond:expr, $($arg:tt)*) => {{
            if $cond { println!("  OK   {}", format!($($arg)*)); }
            else { println!("  FAIL {}", format!($($arg)*)); fail += 1; }
        }};
    }

    // 发送方。**必须监听** —— 取回（pull）是反向的：请求方先发请求，
    // 然后由**对端**连回请求方的 transfer_port 推文件。
    // 早先这里 listen=false，于是取回路径整条没被执行过。
    let a = spawn("a", true).await;
    let b = spawn("b", true).await;
    trust(&a, &b);
    let b_id = b.engine.identity.device_id.clone();
    let a_id = a.engine.identity.device_id.clone();

    // ================================================================
    println!("\n== 0. 永久信任的入站传输必须**零弹窗**（产品核心承诺）==");
    check!(
        b.engine
            .trust_store
            .is_device_trusted(&a_id)
            .unwrap_or(false),
        "B 的信任库里 A 是受信设备"
    );
    check!(
        b.engine
            .trust_store
            .trust_level(&a_id)
            .ok()
            .flatten()
            .map(|l| l == TrustLevel::Permanent)
            .unwrap_or(false),
        "信任等级读回来是 Permanent（写-读一致）"
    );
    check!(
        b.engine.config.read().await.auto_receive,
        "自动接收已开启"
    );
    let auto = b
        .engine
        .trust_store
        .get_access_scope(&a_id)
        .map(|s| s.can_push)
        .unwrap_or(false);
    check!(auto, "访问范围允许写入（can_push=true）");
    // 决议必须是 Allow —— 这一步直接调鉴权函数, 绕开网络,
    // 用来把"策略错了"与"传输链路错了"分开
    let cfg = b.engine.config.read().await.clone();
    let decision = feisuo_core::security::authorize(
        &b.engine.trust_store,
        &cfg,
        feisuo_core::security::Op::Receive,
        &feisuo_core::security::AuthContext {
            peer_id: &a_id,
            peer_ip: "127.0.0.1",
            volume: "",
            path: "",
        },
    );
    check!(
        matches!(decision, feisuo_core::security::Decision::Allow),
        "鉴权决议 = {:?}（必须是 Allow，否则每次传输都要弹窗 = 违背『无人值守』承诺）",
        decision
    );

    // ================================================================
    println!("\n== 1. 单文件传输（走 staging 原子提交路径）==");
    let src = a.dir.join("hello.txt");
    std::fs::write(&src, "hello feisuo 2026").unwrap();
    let r = a
        .engine
        .send_files("127.0.0.1", b.port, &b_id, "b", vec![src.clone()])
        .await;
    check!(
        r.is_ok(),
        "发送成功{}",
        r.as_ref()
            .err()
            .map(|e| format!("（错误: {}）", e))
            .unwrap_or_default()
    );
    let got = b.inbox.join("hello.txt");
    check!(wait_until(|| got.is_file(), SETTLE).await, "文件落到收件目录");
    check!(
        got.is_file()
            && std::fs::read_to_string(&got).unwrap_or_default() == "hello feisuo 2026",
        "内容逐字节一致"
    );
    check!(
        wait_until(|| staging_is_empty(&b.inbox), Duration::from_secs(5)).await,
        "staging 目录已清空（原子提交后不留残留）"
    );

    // ================================================================
    println!("\n== 2. 目录递归传输（多级相对路径 + 隐藏文件跳过）==");
    let proj = a.dir.join("项目");
    std::fs::create_dir_all(proj.join("子")).unwrap();
    std::fs::write(proj.join("顶层.csv"), "aaa").unwrap();
    std::fs::write(proj.join("子/深层.txt"), "bbb").unwrap();
    std::fs::write(proj.join(".hidden"), "xxx").unwrap();
    let r = a
        .engine
        .send_files("127.0.0.1", b.port, &b_id, "b", vec![proj.clone()])
        .await;
    check!(r.is_ok(), "整夹发送成功");
    let p1 = b.inbox.join("项目/顶层.csv");
    let p2 = b.inbox.join("项目/子/深层.txt");
    check!(wait_until(|| p1.is_file(), SETTLE).await, "顶层文件按层级落盘");
    check!(wait_until(|| p2.is_file(), SETTLE).await, "子目录文件按层级落盘");
    check!(
        !b.inbox.join("项目/.hidden").exists(),
        "隐藏文件被跳过且未落盘"
    );
    // 展开统计必须能说清"发了多少、跳过了多少"
    let scan = a.engine.take_last_scan().expect("取到展开结果");
    check!(
        scan.skipped >= 1,
        "展开结果报告跳过了 {} 项（静默跳过会让用户不知道为什么少文件）",
        scan.skipped
    );

    // ================================================================
    println!("\n== 3. 断点续传：同一批重发应跳过已存在文件 ==");
    // 这是本轮修掉的**功能性 bug**: 早先 transfer_id 每次都是新 uuid,
    // 而 transfer_parts 按 transfer_id 索引 ⇒ 续传永不可能触发。
    let _ = std::fs::remove_file(&p2);
    let again = a
        .engine
        .send_files("127.0.0.1", b.port, &b_id, "b", vec![proj.clone()])
        .await;
    match again {
        Ok(n) => check!(
            n >= 1,
            "第二次跳过 {} 个已存在文件（断点续传真的生效了）",
            n
        ),
        Err(e) => {
            println!("  FAIL 重发失败: {}", e);
            fail += 1;
        }
    }
    check!(
        wait_until(|| p2.is_file() && hash_of(&p2) == hash_of(&proj.join("子/深层.txt")), SETTLE)
            .await,
        "缺失的那个被重新传来且内容正确"
    );

    println!("\n== 3b. 内容变了必须重传（绝不能误跳过）==");
    std::fs::write(proj.join("顶层.csv"), "CONTENT-CHANGED").unwrap();
    let r3 = a
        .engine
        .send_files("127.0.0.1", b.port, &b_id, "b", vec![proj.clone()])
        .await;
    check!(r3.is_ok(), "改动后重发成功");
    // ⚠️ 同名**不覆盖**是刻意的产品行为（§1.3: `file (1).ext`），
    // 所以新内容落到**新文件**，老文件保持原样。
    // 早先这里直接断言"p1 内容变了"，那是**我断言写错了**，
    // 不是工具错了 —— 真按"覆盖"实现才是 bug。
    check!(
        wait_until(
            || std::fs::read_dir(b.inbox.join("项目"))
                .map(|d| {
                    d.flatten().any(|e| {
                        e.file_name().to_string_lossy() != "顶层.csv"
                            && std::fs::read_to_string(e.path())
                                .map(|s| s == "CONTENT-CHANGED")
                                .unwrap_or(false)
                    })
                })
                .unwrap_or(false),
            SETTLE
        )
        .await,
        "内容变化的文件被重传并落成新文件（未被误判为已收到）"
    );
    check!(
        std::fs::read_to_string(&p1).unwrap_or_default() == "aaa",
        "原有同名文件**未被覆盖**（绝不覆盖用户已有数据）"
    );

    println!("\n== 3c. 改了内容 ⇒ resume_key 必须变（这是正确性的根）==");
    let k1 = FeisuoEngine::compute_resume_key(
        &b_id,
        "",
        &[(proj.join("顶层.csv"), "项目/顶层.csv".into())],
    );
    let k2 = FeisuoEngine::compute_resume_key(
        &b_id,
        "",
        &[(proj.join("深层.txt"), "项目/深层.txt".into())],
    );
    check!(k1 != k2, "不同文件 ⇒ 不同 key（不会互相误跳过）");
    let k3 = FeisuoEngine::compute_resume_key(
        &b_id,
        "",
        &[(proj.join("顶层.csv"), "项目/顶层.csv".into())],
    );
    check!(k1 == k3, "同一文件重算 ⇒ 相同 key（续传能触发）");
    // 顺序无关: 换个顺序拖入不该导致全量重传
    let items2 = vec![
        (proj.join("子/深层.txt"), "项目/子/深层.txt".to_string()),
        (proj.join("顶层.csv"), "项目/顶层.csv".to_string()),
    ];
    let items1 = vec![
        (proj.join("顶层.csv"), "项目/顶层.csv".to_string()),
        (proj.join("子/深层.txt"), "项目/子/深层.txt".to_string()),
    ];
    check!(
        FeisuoEngine::compute_resume_key(&b_id, "", &items1)
            == FeisuoEngine::compute_resume_key(&b_id, "", &items2),
        "拖入顺序不同 ⇒ 同一个 key（换顺序重发不会全量重传）"
    );
    check!(
        FeisuoEngine::compute_resume_key("other-device", "", &items1)
            != FeisuoEngine::compute_resume_key(&b_id, "", &items1),
        "不同对端 ⇒ 不同 key（不会把给甲的文件当成给乙的）"
    );

    // ---- 3d. 落点不同 ⇒ 必须不同 key（否则换个落点会被静默跳过）----
    //
    // 这条不是"锦上添花"：key 不含落点时，同一批文件发给同一台设备的
    // **另一个落点**，接收端会查到上一次的 transfer_parts 记录，
    // 复核时读到的是上次的文件（存在、哈希一致）⇒ 判定"已完整接收"跳过。
    // 用户看到"已跳过 1 个此前已完整接收的文件"，而新落点里什么都没有。
    // 静默少一个文件比多传一次糟得多。
    let items_at = vec![(proj.join("顶层.csv"), "顶层.csv".to_string())];
    let k_root = FeisuoEngine::compute_resume_key(&b_id, "", &items_at);
    let k_2026 = FeisuoEngine::compute_resume_key(&b_id, "2026", &items_at);
    check!(
        k_root != k_2026,
        "同一批文件、同一对端、不同落点 ⇒ 不同 key（不会在收件根被跳过后 \
         误以为 2026/ 下也已收到）"
    );
    // 同义写法必须归一到同一个 key，否则换个写法就全量重传。
    let k_2026_slash = FeisuoEngine::compute_resume_key(&b_id, "2026/", &items_at);
    let k_2026_back = FeisuoEngine::compute_resume_key(&b_id, "2026\\", &items_at);
    let k_2026_dot = FeisuoEngine::compute_resume_key(&b_id, "./2026", &items_at);
    check!(
        k_2026 == k_2026_slash && k_2026 == k_2026_back && k_2026 == k_2026_dot,
        "同义落点写法（尾部分隔符 / 反斜杠 / 前导 .）⇒ 同一个 key（不无谓全量重传）"
    );

    // ================================================================
    println!("\n== 4. 诊断记录往返（用户后续分析的数据源）==");
    let diags = b.engine.trust_store.list_diagnostics(50).expect("读诊断");
    check!(!diags.is_empty(), "接收侧落库 {} 条诊断", diags.len());
    let completed: Vec<&TransferDiagnostics> =
        diags.iter().filter(|d| d.outcome == "completed").collect();
    check!(
        !completed.is_empty(),
        "存在 outcome=completed 的记录（{} 条）",
        completed.len()
    );
    let with_data = diags
        .iter()
        .any(|d| d.timings.data_ms > 0 && d.bytes_transferred > 0);
    check!(with_data, "存在带真实 data_ms / 字节数的记录");
    // 两侧诊断要能用 transfer_id 串起来 —— 否则事后没法对齐两侧
    let send_ids: Vec<String> = a
        .engine
        .trust_store
        .list_diagnostics(50)
        .unwrap_or_default()
        .iter()
        .map(|d| d.transfer_id.clone())
        .collect();
    let recv_ids: Vec<String> = diags.iter().map(|d| d.transfer_id.clone()).collect();
    check!(
        send_ids.iter().any(|id| recv_ids.contains(id)),
        "发送侧与接收侧的 transfer_id 能对上（事后可串起两侧记录）"
    );
    if let Some(d) = completed.first() {
        let line = d.to_log_line();
        for field in [
            "sockbuf_eff=",
            "connect=",
            "data=",
            "link=[",
            "overlay=",
            "integrity=",
        ] {
            check!(line.contains(field), "日志行含字段 {}", field);
        }
        let attr = d.attribution();
        check!(!attr.trim().is_empty(), "归因非空");
        println!("       样例归因: {}", attr);
        check!(
            d.integrity_ok,
            "BLAKE3 全程校验通过（integrity_ok=true）"
        );
        check!(d.chunk_count > 0, "分块计数非零（chunk_count={}）", d.chunk_count);
    }
    let report = b
        .engine
        .export_diagnostics_report(5)
        .expect("导出报告");
    // 报告的**唯一用途**是别人拿到后不必再追问，所以这几项必须都在
    check!(report.contains("飞梭传输诊断报告"), "报告有抬头");
    check!(
        report.contains(env!("CARGO_PKG_VERSION")) || report.contains("飞梭版本"),
        "报告带版本号（否则跨版本的数字无法归因）"
    );
    check!(report.contains("成功解析"), "报告显式报告解析成功/失败条数");
    check!(report.contains("摘要"), "报告先给摘要（不必逐行扫原始日志）");
    check!(report.contains("归因"), "报告逐条含归因");
    check!(
        report.contains("regrow_tried"),
        "逐条记录含 regrow_tried（否则分不清「没触发」与「触发但判定不需要」）"
    );
    check!(
        report.contains("安全事件"),
        "报告含安全事件（被拒/被拉黑常常正是「为什么没传成」的答案）"
    );
    check!(report.chars().count() > 400, "报告非空（{} 字符）", report.chars().count());

    // ================================================================
    println!("\n== 5. socket buffer 真的有被设置（否则归因会撒谎）==");
    let est = feisuo_core::transport::sockopt::estimate_buffer(180, 0);
    check!(
        est >= feisuo_core::transport::sockopt::MIN_SOCK_BUF,
        "BDP 估算 {} 字节 ≥ 下限 {}",
        est,
        feisuo_core::transport::sockopt::MIN_SOCK_BUF
    );
    // 诊断里必须有回读到的生效值 —— 记"设置值"而不记"生效值"时,
    // 归因会把"内核把它砍了一半"说成"我们配小了", 结论完全反了
    check!(
        diags.iter().any(|d| d.bdp.effective > 0),
        "诊断记录了 socket buffer 的**回读生效值**（而非仅设置值）"
    );
    check!(
        diags.iter().any(|d| d.bdp.requested > 0),
        "诊断记录了请求设置的 buffer 大小（requested）"
    );

    // ================================================================
    println!("\n== 6. 对端真实卷浏览（右栏穿梭）==");
    let listing = a
        .engine
        .list_remote_files("127.0.0.1", b.port, &BrowseTarget::volume_root("*"))
        .await;
    match listing {
        Ok(l) => {
            check!(l.volume_mode, "对端进入真实卷模式（volume_mode=true）");
            check!(!l.volumes.is_empty(), "返回了卷列表（{} 个）", l.volumes.len());
            check!(l.total > 0, "根目录列举到 {} 项", l.total);
        }
        Err(e) => {
            println!("  FAIL 卷列举失败: {}", e);
            fail += 1;
        }
    }
    let legacy = a
        .engine
        .list_remote_files_legacy("127.0.0.1", b.port, "")
        .await;
    check!(legacy.is_ok(), "1.x 语义浏览（只看收件目录）仍可用（向后兼容）");

    // ================================================================
    println!("\n== 7. 「每次匹配码」：完整握手链路 ==");
    b.engine
        .trust_store
        .set_trust_level(&a_id, TrustLevel::Session)
        .expect("把 A 降为每次匹配码");

    // 7a. 发送方**没出示码**（Android 端还没做、或 UI 忘了弹码）
    //     → 必须拿到 GrantCodeRequired 这个**协商回合**，
    //       而不是超时、也不是一句"对方拒绝传输"。
    let ap_a = spawn_approver(b.engine.clone(), vec![Action::Allow]);
    let r7 = a
        .engine
        .send_files("127.0.0.1", b.port, &b_id, "b", vec![src.clone()])
        .await;
    let seen_a = ap_a.await.unwrap_or_default();
    check!(
        matches!(r7, Err(FeisuoError::GrantCodeRequired(_))),
        "未出示码 → GrantCodeRequired（实得 {:?}）",
        r7.as_ref().err().map(|e| e.to_string())
    );
    check!(
        seen_a.iter().all(|r| r.requires_grant_code) && !seen_a.is_empty(),
        "接收侧审批请求标了 requires_grant_code（界面上必须显示这个码）"
    );
    // **方向守卫**：码必须是接收方生成的，且随审批请求送到本机 UI。
    // 早先的实现是发起方生成、接收方在弹窗里输入 —— 那样等于让接收方
    // 去核对一个对方自选的答案，这道门形同虚设。
    let code_a = challenge_of(&seen_a);
    check!(
        code_a.len() == 6,
        "接收方为本次请求生成了 6 位匹配码（实得 {:?}）",
        code_a
    );
    check!(
        seen_a.iter().all(|r| r.grant_challenge == seen_a[0].grant_challenge),
        "同一请求的重试沿用**同一个**码（否则用户在第一个窗口读到的码 \
         在第二个窗口上永远对不上，这个功能一次都不会成功）"
    );

    // 7b. 发起方**出示接收方给的码** → 必须放行。
    //     这一条是整条链路的成败点：码从"接收方窗口"出发，经过网络回到
    //     接收方，比对必须成立。
    let ap_b = spawn_approver(b.engine.clone(), vec![Action::Allow]);
    let r7b = a
        .engine
        .send_files_with_code("127.0.0.1", b.port, &b_id, "b", vec![src.clone()], &code_a)
        .await;
    let seen_b = ap_b.await.unwrap_or_default();
    check!(
        r7b.is_ok(),
        "发起方出示接收方的码后放行（实得 {:?}）",
        r7b.as_ref().err().map(|e| e.to_string())
    );
    check!(
        !seen_b.is_empty() && seen_b[0].grant_challenge == code_a,
        "重试拿到的还是同一个码（复用生效）"
    );

    // 7c. 出示**错误的**码三次 → 必须给出明确失败，而不是无限等。
    //     注意这是"对方输错"，不是"接收方输错" —— 方向改了以后，
    //     输错的人变成发起方那一侧。
    let ap_c = spawn_approver(
        b.engine.clone(),
        // 恰好 3 步 = MAX_CODE_ATTEMPTS。第 3 次不匹配后服务端直接拒绝，
        // 不会再弹第 4 次 —— 脚本多写一步只会让审批任务空等。
        vec![Action::Allow, Action::Allow, Action::Allow],
    );
    let r7c = a
        .engine
        .send_files_with_code("127.0.0.1", b.port, &b_id, "b", vec![src.clone()], "999999")
        .await;
    let seen_c = ap_c.await.unwrap_or_default();
    check!(
        r7c.is_err(),
        "连续 3 次出示错误码后明确拒绝（不会无限等下去）"
    );
    check!(
        seen_c.len() >= 2,
        "码不匹配时在同一窗口内重弹而不是从头审批（收到 {} 次请求）",
        seen_c.len()
    );
    // 码不匹配必须留下安全事件，且**只记长度不记内容**
    let evs = b.engine.trust_store.list_security_events(20).unwrap_or_default();
    let mism = evs
        .iter()
        .find(|e| e.kind.contains("grant") || e.kind.contains("mismatch"));
    check!(
        mism.is_some(),
        "码不匹配留下了安全事件（kind={:?}）",
        mism.map(|e| e.kind.clone())
    );
    check!(
        mism.map(|e| !e.detail.contains("999999")).unwrap_or(false),
        "安全事件**没有记录码内容**（落盘等于泄露正在使用的凭据）"
    );

    // 7d. **换一个请求必须换码** —— 这是复用窗口的边界。
    //     同内容重复发送会在 120s 内复用同一个码（CHALLENGE_REUSE_TTL），
    //     那是明说的代价；而**内容不同**必须拿到不同的码，否则一个码
    //     就能覆盖任意内容，窄化就完全没有意义了。
    let ap_d2 = spawn_approver(b.engine.clone(), vec![Action::Allow]);
    // 名字和内容都与 `src` 不同：指纹变了，必须换码。
    let other = a.dir.join("另一个内容.txt");
    std::fs::write(&other, "完全不同的内容，长度也不同").unwrap();
    let r7d = a
        .engine
        .send_files("127.0.0.1", b.port, &b_id, "b", vec![other.clone()])
        .await;
    let seen_d2 = ap_d2.await.unwrap_or_default();
    let code_d2 = challenge_of(&seen_d2);
    check!(
        r7d.is_err() && code_d2.len() == 6 && code_d2 != code_a,
        "换一个请求 → 换一个码（旧码 {:?}，新码 {:?}）",
        code_a,
        code_d2
    );

    println!("\n== 7b. 出示正确码后可以放行 ==");
    b.engine
        .trust_store
        .set_trust_level(&a_id, TrustLevel::Permanent)
        .expect("恢复永久信任");

    // ================================================================
    println!("\n== 8. 用户在审批弹窗点「拒绝」==");
    // 未配对设备 → 走人工审批。拒绝后必须:
    //   1. 发送方拿到明确的"被拒绝"（不是超时）
    //   2. **不能**顺手把对方写进长期信任库
    //     —— 早先的实现在这里 bind_device(is_trusted=true), 等于把
    //     "这一次允许"静默升级成"永久免密", 用户根本没有选择权。
    let d = spawn("d", true).await;
    let d_id = d.engine.identity.device_id.clone();
    let ap_d = spawn_approver(d.engine.clone(), vec![Action::Reject]);
    let r9 = a
        .engine
        .send_files("127.0.0.1", d.port, &d_id, "d", vec![src.clone()])
        .await;
    let _ = ap_d.await;
    check!(r9.is_err(), "拒绝后发送失败（实得 {:?}）", r9.as_ref().err().map(|e| e.to_string()));
    let err_text = r9.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
    check!(
        err_text.contains("拒绝"),
        "错误文案说清了是**被拒绝**（实得「{}」）—— 超时的话用户会以为是网络问题",
        err_text
    );
    check!(
        !d.engine.trust_store.is_device_trusted(&a_id).unwrap_or(false),
        "拒绝**没有**把对方写进长期信任库（『允许一次』不等于『永久免密』）"
    );
    check!(
        d.inbox.join("hello.txt").exists() == false,
        "被拒的传输没有落盘"
    );
    let _ = d.engine.stop();

    // ================================================================
    println!("\n== 9. 可访问范围 fail-closed：撤销写入权后立即拒绝入站 ==");
    // 原来这一步用的是「黑名单 fail-closed」。那个维度是**实现期自己加的**
    // （原始提交 `735dd66` 里 `grep -i blacklist` 零命中，需求从未要求过），
    // 已按 `DESIGN_TRUST_SHUTTLE.md` §14.11 决策 1 整体删除 ——
    // 它的两个真实诉求现在分别由「解除配对」（§14.5，已双向）与
    // 「隐藏」覆盖。详见 `core/tests/protocol_integration.rs` 第 8 节的说明。
    //
    // 这里换成**仍然活着**的拒绝路径：撤销对端的 `can_push`。
    // 它同样是"用户自己配的授权"，同样会在服务端留安全事件。
    let c = spawn("c", true).await;
    let c_id = c.engine.identity.device_id.clone();
    trust(&a, &c);
    c.engine
        .trust_store
        .set_access_scope(
            &a_id,
            &feisuo_core::security::AccessScope {
                mode: feisuo_core::security::AccessMode::All,
                allow_volumes: vec![],
                allow_paths: vec![],
                deny_paths: vec![],
                can_pull: true,
                can_push: false,
                updated_at: 0,
            },
        )
        .expect("C 撤销 A 的写入权");
    let r8 = a
        .engine
        .send_files("127.0.0.1", c.port, &c_id, "c", vec![src.clone()])
        .await;
    check!(
        r8.is_err(),
        "无写入权的设备发送被拒（fail-closed）: {:?}",
        r8.as_ref().err().map(|e| e.to_string())
    );
    // 必须留下安全事件 —— 静默拒绝会让用户以为"对方坏了"。
    let events = c.engine.trust_store.list_security_events(20).unwrap_or_default();
    let ev = events
        .iter()
        .find(|e| e.kind == "rejected_attempt")
        .or_else(|| events.iter().find(|e| e.peer_device_id == a_id));
    check!(
        ev.is_some(),
        "被拒时留下了安全事件（kind=rejected_attempt, 共 {} 条事件）",
        events.len()
    );
    if let Some(e) = ev {
        println!(
            "       样例事件: kind={} peer={}({}) detail={}",
            e.kind, e.peer_name, e.peer_device_id, e.detail
        );
    }
    let _ = c.engine.stop();

    // ================================================================
    // ================================================================
    println!("\n== 10. 取回（pull）：请求方收对端推来的文件 ==");
    // 取回是**反向**数据流：请求方发请求，对端连回来推文件。
    // 这条路径此前从未被执行过，而它同时压了 scope 校验、
    // dest_sub_path 落点、以及反向连接的端口协商。
    let pulled = b.inbox.join("hello.txt");
    let sub = "从B取回";
    let rp = a
        .engine
        .request_pull("127.0.0.1", b.port, vec!["hello.txt".into()], sub)
        .await;
    check!(
        rp.is_ok(),
        "取回请求被接受{}",
        rp.as_ref().err().map(|e| format!("（{}）", e)).unwrap_or_default()
    );
    let landed = a.inbox.join(sub).join("hello.txt");
    check!(
        wait_until(|| landed.is_file(), SETTLE).await,
        "文件落到本机收件目录的 dest_sub_path 下（实得路径 {:?}）",
        landed
    );
    check!(
        landed.is_file() && hash_of(&landed) == hash_of(&pulled),
        "取回内容与对端源文件逐字节一致"
    );

    // ---- 10b. 发送方向同样支持指定落点，且必须真的落在那一层 ----
    //
    // 取回方向（上面）早就压着 `dest_sub_path` 的端到端验证，而**发送**方向
    // 一直没有。缺它意味着：穿梭里把文件拖到对方某个子目录、界面也照着
    // 地址栏显示落点，但实际全落在收件根 —— 而没有任何一处会报错。
    // 这正是 §7.7 承诺的核心行为，必须端到端验一次。
    let send_sub = "送到B的子目录";
    // 源文件必须在**本机真实存在**。
    //
    // 早先这里复用了 `b.inbox.join(sub).join("hello.txt")` —— 那是上面
    // 取回测试里"应该被 A 收到"的目标路径，落在 B 的收件目录里、并不存在。
    // 于是 `expand_all` 直接报"文件不存在"，测试是**因为错误的原因**通过的：
    // 越界那条也一样，它被拒是因为读不到文件，不是真因为挡住了 `..`。
    // 断言被满足、行为完全没被验证 —— 这类假绿比失败更贵。
    let sent = a.inbox.join("落点测试源.txt");
    std::fs::write(&sent, "送到指定落点的内容").expect("写落点测试源文件");
    let sp = a
        .engine
        .send_files_to_dest(
            "127.0.0.1",
            b.port,
            &b_id,
            "b",
            vec![sent.clone()],
            send_sub,
        )
        .await;
    check!(
        sp.is_ok(),
        "带落点的发送被接受{}",
        sp.as_ref().err().map(|e| format!("（{}）", e)).unwrap_or_default()
    );
    // 落点只决定**目录**，文件名仍来自源文件名。
    // 早先这里写死 `hello.txt`，于是断言永远落在一条**永不存在**的路径上 ——
    // 传输其实成功了，测试却报"没落到位"。断言写错时的失败信息
    // 会把人引向完全错误的排查方向（去查落点逻辑，而它是对的）。
    let sent_landed = b.inbox.join(send_sub).join("落点测试源.txt");
    check!(
        wait_until(|| sent_landed.is_file(), SETTLE).await,
        "文件真的落在对方收件目录下的 {}/落点测试源.txt（实得路径 {:?}）",
        send_sub,
        sent_landed
    );
    check!(
        sent_landed.is_file() && hash_of(&sent_landed) == hash_of(&sent),
        "带落点发送的内容逐字节一致"
    );

    // ---- 10c. 越界落点必须被拒（落点不能逃出收件目录）----
    //
    // 只测成功路径是不够的：落点是从**对端**发过来的字段，
    // 发送方完全可以声称 `../../Windows`。`normalize_sub_path` 挡 `..`，
    // 但端到端必须验一次"它真的挡得住" —— 因为这条防线一旦失效，
    // 后果是已配对设备可往本机任意路径写文件。
    let escape = a
        .engine
        .send_files_to_dest(
            "127.0.0.1",
            b.port,
            &b_id,
            "b",
            vec![sent.clone()],
            "../../escape",
        )
        .await;
    // 必须**因为落点非法**而被拒。只断言 `is_err()` 等于什么都没验：
    // 源文件不存在、网络断了、对方没监听 —— 全都会让 `is_err()` 成立，
    // 而 `normalize_sub_path` 是否真的挡住了 `..` 完全没被触及。
    // 断言错误信息里出现"落点"，把"因正确的原因被拒"变成可检查的。
    let escape_msg = escape.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
    check!(
        escape.is_err() && escape_msg.contains("落点"),
        "越界落点（../../escape）因**落点非法**被拒（实得 {:?}）",
        escape_msg
    );
    check!(
        !b.inbox
            .parent()
            .map(|p| p.join("escape").exists())
            .unwrap_or(false),
        "越界落点没有在收件目录之外创建任何目录"
    );

    // ================================================================
    println!("\n== 11. 敏感路径：端到端必须被拒（且必须**因正确的原因**被拒）==");
    // `is_mandatory_denied` 的 37 条断言验证过**判定函数**本身，
    // 但没验证过"请求真的发出去、对端真的拒绝"。
    //
    // ## 第一版这里写错了，而且错得很隐蔽
    //
    // 我传的是 `sub_paths: ["C:\\Windows"]`，断言"被拒"——过了。
    // 但错误是**"路径不得包含盘符"**：命中的是路径格式校验器，
    // **强制排除清单根本没被执行**。断言通过，测的却是另一件事。
    //
    // 真实形状是**卷内相对路径**：`join_volume_path("C:", "Windows")`
    // 才拼出 `C:/Windows` 交给 `can_read`。所以必须传不带盘符的相对路径，
    // 并**断言错误文案**来自范围判定 —— 只断言"失败了"永远抓不到这种错。
    for (label, rel) in [
        ("系统根", "Windows"),
        ("系统深层", "Windows/System32"),
        ("SAM 影子副本", "Windows/System32/config/SAM"),
        ("程序目录", "Program Files"),
        ("反斜杠写法", "Windows\\System32\\config\\SAM"),
    ] {
        // 浏览（v2 卷模式）
        let mut t = BrowseTarget::volume_root("C:");
        t.rel_path = rel.to_string();
        let r = a.engine.list_remote_files("127.0.0.1", b.port, &t).await;
        let err = r.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
        check!(
            r.is_err() && err.contains("无权"),
            "浏览{}（{}）被**范围判定**拒绝（实得「{}」）",
            label,
            rel,
            err
        );
        check!(
            !err.contains("盘符"),
            "浏览{} 不是被格式校验器拒的（说明确实走到了范围判定）",
            label
        );
        // 取回
        let rp2 = a
            .engine
            .request_pull("127.0.0.1", b.port, vec![rel.to_string()], "")
            .await;
        let err2 = rp2.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
        check!(
            rp2.is_err() && err2.contains("无权"),
            "取回{}（{}）被**范围判定**拒绝（实得「{}」）",
            label,
            rel,
            err2
        );
        check!(
            !err2.contains("盘符"),
            "取回{} 不是被格式校验器拒的（说明确实走到了范围判定）",
            label
        );
    }

    // 反面对照：不在排除清单里的路径**必须能浏览**，否则说明排除清单
    // 写成了"全都不给"，那不是保护是把功能关了
    let mut ok_t = BrowseTarget::volume_root("C:");
    ok_t.rel_path = "Users".to_string();
    let ok_r = a.engine.list_remote_files("127.0.0.1", b.port, &ok_t).await;
    check!(
        ok_r.is_ok(),
        "反面对照：C:\\Users 不在排除清单里，必须能浏览（实得 {:?}）",
        ok_r.as_ref().err().map(|e| e.to_string())
    );

    // ================================================================
    println!("\n== 12. access_scope：收窄后必须真的生效 ==");
    // 之前只验证过 `set_access_scope` 能写进库，没验证过它**改变了行为**。
    // 一个不生效的权限配置比没有配置更危险：用户会以为已经收窄了。
    let mut narrow = b
        .engine
        .trust_store
        .get_access_scope(&a_id)
        .expect("读范围");
    check!(narrow.can_pull, "初始允许取回");
    narrow.can_pull = false;
    b.engine
        .trust_store
        .set_access_scope(&a_id, &narrow)
        .expect("收窄取回权限");
    let r12 = a
        .engine
        .request_pull("127.0.0.1", b.port, vec!["hello.txt".into()], "不该出现")
        .await;
    check!(
        r12.is_err(),
        "关掉 can_pull 后取回被拒（实得 {:?}）",
        r12.as_ref().err().map(|e| e.to_string())
    );
    check!(
        !a.inbox.join("不该出现").exists(),
        "被拒的取回没有落盘"
    );
    narrow.can_pull = true;
    b.engine
        .trust_store
        .set_access_scope(&a_id, &narrow)
        .expect("恢复取回权限");
    // 恢复后必须**真的**又能用了 —— 只验证"关得掉"不够，
    // 也要验证"关不掉"（权限一旦写入就再也打不开是最糟的故障）
    let r12b = a
        .engine
        .request_pull("127.0.0.1", b.port, vec!["hello.txt".into()], "恢复后")
        .await;
    check!(r12b.is_ok(), "恢复 can_pull 后取回重新可用");
    check!(
        wait_until(|| a.inbox.join("恢复后").join("hello.txt").is_file(), SETTLE).await,
        "恢复后的取回确实落了盘"
    );

    // ================================================================
    println!("\n== 13. 大文件：多分块偏移 + BDP 二次调大 ==");
    // 到这里为止所有传输都是几个字节、1~2 个分块。而用户实测是
    // **跨地域 ZeroTier 传大文件**。两段代码路径此前**不可能**跑过：
    //
    // - `write_chunk_at` 的非零偏移数学（分块 0 之外的每一个）；
    // - **BDP 二次调大**：阈值是 `total_transferred >= 64 MiB`，
    //   小文件永远触发不到。也就是说"传输中途按实测带宽调大 buffer"
    //   这项优化，在用户实测前**从未执行过一行**。
    let big = a.dir.join("big.bin");
    // 70 MiB：跨过 64 MiB 阈值，又不至于让冒烟跑太久
    const BIG_SIZE: usize = 70 * 1024 * 1024;
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&big).expect("建大文件");
        // 用可复现的伪随机内容：全 0 会让 BLAKE3 走快路径，
        // 也无法验证"某一分块写错位置"这类偏移错误
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut buf = vec![0u8; 1024 * 1024];
        for _ in 0..70 {
            for b in buf.iter_mut() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *b = (seed >> 24) as u8;
            }
            f.write_all(&buf).expect("写大文件");
        }
        f.sync_all().ok();
    }
    let big_dst = b.inbox.join("big.bin");
    let t0 = std::time::Instant::now();
    let rbig = a
        .engine
        .send_files("127.0.0.1", b.port, &b_id, "b", vec![big.clone()])
        .await;
    let secs = t0.elapsed().as_secs_f64();
    check!(
        rbig.is_ok(),
        "70 MiB 单文件传输成功{}",
        rbig.as_ref()
            .err()
            .map(|e| format!("（{}）", e))
            .unwrap_or_default()
    );
    check!(
        wait_until(|| big_dst.is_file() && hash_of(&big_dst) == hash_of(&big), Duration::from_secs(120))
            .await,
        "70 MiB 到达且 BLAKE3 与源文件一致（分块偏移数学正确）"
    );
    check!(
        big_dst.metadata().map(|m| m.len() as usize).ok() == Some(BIG_SIZE),
        "落盘大小精确等于 {} 字节（实得 {:?}）",
        BIG_SIZE,
        big_dst.metadata().map(|m| m.len()).ok()
    );
    println!(
        "       回环吞吐 {:.1} MB/s（{} 块 × 4 MiB，耗时 {:.2}s）",
        BIG_SIZE as f64 / 1e6 / secs.max(0.001),
        BIG_SIZE / (4 * 1024 * 1024),
        secs
    );
    // 这次传输的诊断必须记录到多分块数据，否则用户实测时读不到东西
    let dsend = a.engine.trust_store.list_diagnostics(10).unwrap_or_default();
    let bigd = dsend
        .iter()
        .find(|d| d.bytes_transferred >= BIG_SIZE as u64)
        .or_else(|| dsend.iter().find(|d| d.chunk_count >= 16));
    check!(
        bigd.is_some(),
        "诊断记录到了这次多分块传输（chunk_count={}）",
        bigd.map(|d| d.chunk_count).unwrap_or(0)
    );
    if let Some(d) = bigd {
        check!(
            d.chunk_count >= 17,
            "分块计数正确（70 MiB / 4 MiB = 18 块，实得 {}）",
            d.chunk_count
        );
        check!(d.integrity_ok, "整文件 BLAKE3 校验通过");
        // 关键：跨过 64 MiB 阈值后**必须尝试过**二次调优。
        //
        // ⚠️ 我第一版断言的是 `regrown == true`，那是个**错的**不变量：
        // `regrow_stream` 在"现有 buffer 已覆盖 BDP"时会**正确地**返回
        // None（不调大）。回环上传输本来就快、BDP 极小，于是调不调大
        // 取决于当次实测带宽 —— 同一份代码有时调大（381KB）有时不调
        // （256KB），断言随机失败。
        //
        // 真正该断言的是**决策**：尝试过，且（调大了 或 判定已够）。
        // 这也正是为什么诊断里要把 `regrow_tried` 与 `regrown` 分开记 ——
        // 否则"没触发"和"触发了但不需要"在数据里长得一样。
        check!(
            d.bdp.regrow_tried,
            "跨过 64 MiB 后**尝试过**二次调优（regrow_tried=true）"
        );
        let grew_or_unnecessary = d.bdp.regrown
            || (d.bdp.bdp_bytes as usize * 2) <= d.bdp.requested.max(1);
        check!(
            grew_or_unnecessary,
            "调优决策正确：要么真的调大了（{}KB），要么判定现有 {}KB 已覆盖 BDP {}KB",
            d.bdp.requested / 1024,
            d.bdp.requested / 1024,
            d.bdp.bdp_bytes / 1024
        );
        // 不管调没调大，**结束时缓冲区必须盖得住 BDP**，
        // 否则就是"飞梭把自己限流了"却归因成链路问题
        check!(
            !d.bdp.under_bdp,
            "结束时缓冲区盖得住 BDP（effective={}KB ≥ bdp={}KB）—— under_bdp=true 意味着飞梭在自我限流",
            d.bdp.effective / 1024,
            d.bdp.bdp_bytes / 1024
        );
        let note = d.bdp.attribution();
        check!(
            note.contains("已中途调大") || note.contains("已评估"),
            "归因能说清调大与否（实得「{}」）",
            note
        );
        check!(
            d.throughput.avg_bps() > 0,
            "吞吐采样有值（{} bps）—— 否则速度栏会显示「不可判定」",
            d.throughput.avg_bps()
        );
        println!("       归因: {}", d.attribution());
        println!("       BDP: requested={}KB effective={}KB bdp={}KB rtt={}ms under_bdp={}",
            d.bdp.requested / 1024, d.bdp.effective / 1024,
            d.bdp.bdp_bytes / 1024, d.bdp.rtt_ms, d.bdp.under_bdp);
    }

    // ================================================================
    println!("\n== 14. 剪贴板：分类 / 拒绝规则 / 暂存 / TTL ==");
    // 这段逻辑此前住在桌面端 bin-only crate 里，**一行都没被执行过**。
    // 现已转到 core（Android 端也要用），可以断言了。
    use feisuo_core::clipboard as cb;
    use feisuo_core::ClipboardContent as CC;

    // --- 扩展名推断 ---
    check!(cb::text_extension("") == "txt", "空文本 → .txt");
    check!(cb::text_extension("   \n  ") == "txt", "纯空白 → .txt");
    check!(cb::text_extension("{\"a\":1}") == "json", "合法 JSON → .json");
    check!(cb::text_extension("# 标题") == "md", "Markdown 标题 → .md");
    check!(cb::text_extension("- 列表项") == "md", "Markdown 列表 → .md");
    check!(cb::text_extension("```rust\nfn main(){}\n```") == "md", "代码围栏 → .md");
    check!(cb::text_extension("普通一句话") == "txt", "普通文本 → .txt");
    // 绝不能生成 .url：那是可执行的 INI 快捷方式，双击会跳转
    check!(
        !["url", "lnk", "exe", "bat", "cmd", "ps1", "html", "svg"]
            .contains(&cb::text_extension("https://example.com")),
        "URL 文本绝不被命名成可执行/可渲染后缀（实得 .{}）",
        cb::text_extension("https://example.com")
    );

    // --- 拒绝规则（安全底线）---
    for (label, raw) in [
        ("SVG", "<svg xmlns=\"http://www.w3.org/2000/svg\"><script/></svg>"),
        ("带 XML 头的 SVG", "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"),
        ("整份 HTML", "<!DOCTYPE html><html><body>hi</body></html>"),
        ("整份 HTML（无 DOCTYPE）", "<html><body>hi</body></html>"),
    ] {
        match cb::classify_text(raw.to_string()) {
            CC::Rejected { reason } => check!(
                reason.contains("安全") || reason.contains("不落盘"),
                "{} 被拒绝且说清了原因：「{}」",
                label,
                reason
            ),
            other => {
                println!("  FAIL {} 竟然没被拒绝（实得 {:?}）", label, std::mem::discriminant(&other));
                fail += 1;
            }
        }
    }
    // 合法文本必须**不被**误伤
    match cb::classify_text("这是一段普通中文文本。".into()) {
        CC::Text { file_name, full_text, .. } => {
            check!(full_text == "这是一段普通中文文本。", "合法文本保留全文");
            check!(file_name.starts_with("剪贴板文本_") && file_name.ends_with(".txt"),
                   "文件名格式正确（{}）", file_name);
        }
        _ => { println!("  FAIL 普通中文文本被误判"); fail += 1; }
    }
    // 片段 HTML **不**该被当整份文档拒（用户复制一段带格式的文字很常见）
    match cb::classify_text("这里有一段 <b>加粗</b> 文字".into()) {
        CC::Text { .. } => check!(true, "片段 HTML 不被误判为整份文档（用户常复制带格式文字）"),
        CC::Rejected { reason } => {
            println!("  FAIL 片段 HTML 被误拒：「{}」—— 用户复制带格式文字是日常操作", reason);
            fail += 1;
        }
        _ => { println!("  FAIL 片段 HTML 分类异常"); fail += 1; }
    }
    check!(matches!(cb::classify_text("   \n\t ".into()), CC::Empty), "纯空白 → Empty");

    // --- 暂存落盘 + TTL + 精确删除 ---
    let app_dir = a.dir.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let sdir = cb::staging_dir(&app_dir);
    check!(
        !sdir.starts_with(&b.inbox),
        "暂存目录在 app 私有目录而非收件目录（明文不该出现在用户可见目录）"
    );
    let txt = CC::Text {
        file_name: "剪贴板文本_probe.txt".into(),
        size: 5,
        preview: String::new(),
        full_text: "机密内容".into(),
    };
    let staged = cb::stage_content(&txt, &sdir).expect("文本落盘");
    check!(staged.len() == 1 && Path::new(&staged[0]).is_file(), "文本落盘成功");
    check!(
        std::fs::read_to_string(&staged[0]).unwrap_or_default() == "机密内容",
        "落盘内容是**全文**而不是预览（只存预览等于把长文本截断）"
    );
    // 暂存文件名必须过路径校验（暂存目录是可写路径，外部字符串不能直接 join）
    let evil = CC::Text {
        file_name: "../../逃逸.txt".into(),
        size: 1,
        preview: String::new(),
        full_text: "x".into(),
    };
    check!(
        cb::stage_content(&evil, &sdir).is_err(),
        "带 `..` 的暂存文件名被拒（防目录逃逸）"
    );
    check!(!a.dir.join("逃逸.txt").exists(), "逃逸文件没有落到暂存目录之外");

    // Rejected / Empty 不得产生任何文件
    let before = std::fs::read_dir(&sdir).map(|d| d.count()).unwrap_or(0);
    let _ = cb::stage_content(&CC::Rejected { reason: "x".into() }, &sdir);
    let _ = cb::stage_content(&CC::Empty, &sdir);
    let after = std::fs::read_dir(&sdir).map(|d| d.count()).unwrap_or(0);
    check!(before == after, "Rejected/Empty 不产生文件（实得 {} -> {}）", before, after);

    // TTL：默认 24h 内不清理，超过才清
    let n_ttl = cb::purge_stale_staging(&app_dir, 24).expect("TTL 清理");
    check!(n_ttl == 0 && Path::new(&staged[0]).is_file(), "24h 内的暂存文件不被清理");
    // TTL 传 0 时下界是 1 小时而不是 0 —— 传 0 若按字面执行会把
    // 刚落的文件立刻删掉，用户表现为"剪贴板发送永远失败"。
    //
    // ⚠️ 我第一版把"TTL 到期后被清理"写成 `purge(..., 0)` 期望清掉，
    // 那与**下一条断言自相矛盾**（同一个 0，一边期望清一边期望不清）。
    // 真正要测的是时间边界，必须**把文件的 mtime 往回拨**——
    // 刚创建的文件永远是"新的"，无论 TTL 填几。
    let n_ttl0 = cb::purge_stale_staging(&app_dir, 0).expect("TTL=0 清理");
    check!(
        n_ttl0 == 0 && Path::new(&staged[0]).is_file(),
        "TTL 传 0 不会把刚落的文件立刻删掉（下界 1 小时，实得清掉 {} 个）",
        n_ttl0
    );
    // 把 mtime 拨回 2 小时前：此时 TTL=1h 应清掉，TTL=24h 应保留
    let aged = cb::stage_content(
        &CC::Text {
            file_name: "两小时前.txt".into(),
            size: 1,
            preview: String::new(),
            full_text: "old".into(),
        },
        &sdir,
    )
    .expect("落一个待 aged 的文件");
    let p_old = Path::new(&aged[0]);
    // 必须以**写**方式打开：Windows 上 `File::open` 只申请
    // FILE_READ_DATA，而 `set_times` 需要 FILE_WRITE_ATTRIBUTES，
    // 否则直接 `PermissionDenied`。踩过这个坑。
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(p_old)
        .expect("以写方式打开以改时间");
    let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
    f.set_times(std::fs::FileTimes::new().set_modified(two_hours_ago))
        .expect("回拨 mtime");
    drop(f);
    let n24 = cb::purge_stale_staging(&app_dir, 24).expect("24h 清理");
    check!(
        n24 == 0 && p_old.is_file(),
        "2 小时前的文件在 24h TTL 下保留（实得清掉 {} 个）",
        n24
    );
    let n1 = cb::purge_stale_staging(&app_dir, 1).expect("1h 清理");
    check!(
        n1 >= 1 && !p_old.exists(),
        "2 小时前的文件在 1h TTL 下被清理（实得清掉 {} 个）",
        n1
    );

    // 精确删除：不能连带删掉排队等待发送的其它暂存
    let keep = cb::stage_content(
        &CC::Text {
            file_name: "排队中.txt".into(),
            size: 1,
            preview: String::new(),
            full_text: "y".into(),
        },
        &sdir,
    )
    .expect("落第二个");
    let gone = cb::stage_content(
        &CC::Text {
            file_name: "已发送.txt".into(),
            size: 1,
            preview: String::new(),
            full_text: "z".into(),
        },
        &sdir,
    )
    .expect("落第三个");
    let n = cb::cleanup_staged(&app_dir, &gone).expect("精确删除");
    check!(
        n == 1 && !Path::new(&gone[0]).exists() && Path::new(&keep[0]).is_file(),
        "只删指定文件，排队的另一个仍在（删掉它就是静默丢数据）"
    );
    // 越界路径必须拒绝删除
    let outside = b.inbox.join("hello.txt");
    let n2 = cb::cleanup_staged(&app_dir, &[outside.to_string_lossy().to_string()])
        .expect("越界删除");
    check!(
        n2 == 0 && outside.is_file(),
        "拒绝删除暂存目录外的文件（否则这条命令就是个任意文件删除器）"
    );

    // ================================================================
    // ================================================================
    println!("\n== 15. 诊断归因：不能说谎 ==");
    // 归因是"事后根据传输信息做分析"的**唯一入口**。它如果说错，
    // 后面所有分析都建立在错误前提上。所以这里断言的是**措辞**，
    // 而不只是"有输出"。
    use feisuo_core::transport::{classify_ip, OverlayKind};

    // --- IP 分类：回环必须有自己的一类 ---
    for (ip, want) in [
        ("127.0.0.1", OverlayKind::Loopback),
        ("127.255.255.255", OverlayKind::Loopback),
        ("100.64.0.9", OverlayKind::CarrierGradeNat100),
        ("100.127.255.255", OverlayKind::CarrierGradeNat100),
        ("10.1.2.3", OverlayKind::PrivateLan),
        ("172.16.0.1", OverlayKind::PrivateLan),
        ("192.168.1.1", OverlayKind::PrivateLan),
        ("203.0.113.7", OverlayKind::Public),
        // 边界：100.63 / 100.128 都**不属于** CGNAT 段
        ("100.63.255.255", OverlayKind::Public),
        ("100.128.0.1", OverlayKind::Public),
        // 172.32 已在 RFC1918 段外
        ("172.32.0.1", OverlayKind::Public),
        // 垃圾输入不得 panic（classify_ip 会被 discovery 直接调用）
        ("", OverlayKind::Unknown),
        ("not-an-ip", OverlayKind::Unknown),
        ("999.1.1.1", OverlayKind::Unknown),
    ] {
        check!(classify_ip(ip) == want, "classify_ip({:?}) = {:?}", ip, classify_ip(ip));
    }
    // 回环在端点优先级里不能垫底：它一定通，lan/public 只是可能通
    check!(
        feisuo_core::endpoint_priority("loopback") >= feisuo_core::endpoint_priority("public"),
        "回环端点优先级不低于公网（单机联调靠它连上）"
    );
    check!(
        feisuo_core::endpoint_priority("overlay") > feisuo_core::endpoint_priority("loopback"),
        "覆盖网仍优先于回环（D6：主场景是跨地域 ZeroTier）"
    );

    // --- 归因措辞 ---
    let dbig = a
        .engine
        .trust_store
        .list_diagnostics(30)
        .unwrap_or_default()
        .into_iter()
        .find(|d| d.chunk_count >= 16)
        .expect("找到那次大文件传输");
    let attr = dbig.attribution();
    check!(
        !attr.contains("非飞梭问题"),
        "归因**不再**无条件替飞梭开脱（实得「{}」）",
        attr
    );
    check!(
        !attr.contains("上限"),
        "归因不再拿观测速度当\"上限\"跟自己比（那恒等于\"不是飞梭的问题\"）"
    );
    // 缓冲区不足时必须说"飞梭可调"，且排在"链路抖动"之前 ——
    // 顺序错了会把飞梭自己的问题说成不可修的"网络抖"
    let mut under = dbig.clone();
    under.bdp.under_bdp = true;
    under.bdp.effective = 64 * 1024;
    under.bdp.bdp_bytes = 8 * 1024 * 1024;
    let a_under = under.attribution();
    check!(
        a_under.contains("飞梭") && a_under.contains("可调"),
        "缓冲区不足时归因指向飞梭且说明可调（实得「{}」）",
        a_under
    );
    let mut stalled_under = under.clone();
    stalled_under.throughput.stalls = 3;
    stalled_under.throughput.worst_gap_ms = 900;
    check!(
        stalled_under.attribution().contains("可调"),
        "缓冲区不足 + 有抖动时，**仍然**先判缓冲区（抖动是它的症状不是原因）"
    );

    // ================================================================
    println!("\n== 16. 收件目录结构 ==");
    let n = count_tree(&b.inbox);
    check!(n >= 4, "收件目录共 {} 个文件/目录", n);
    check!(staging_is_empty(&b.inbox), "结束时 staging 无残留");
    dump_tree(&b.inbox, 1);

    a.engine.stop();
    b.engine.stop();
    let _ = std::fs::remove_dir_all(&a.dir);
    let _ = std::fs::remove_dir_all(&b.dir);
    let _ = std::fs::remove_dir_all(&c.dir);
    let _ = std::fs::remove_dir_all(&d.dir);

    println!("\n结果: {} 项失败", fail);
    if fail > 0 { 1 } else { 0 }
}
