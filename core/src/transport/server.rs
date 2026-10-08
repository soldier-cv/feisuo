use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot, RwLock, Semaphore};
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::error::{FeisuoError, Result};
use crate::protocol::*;
use crate::security::{
    format_bytes, BindOutcome, DeviceIdentity, Op, SecurityEventKind, TrustLevel, TrustStore,
};
use crate::storage::{ChunkStore, PathManager};
use crate::transport::client::TransferClient;
use crate::transport::session::{TransferDirection, TransferProgress, TransferStatus};

/// 拒绝一条连接时, 用来"偷看"消息类型那 1 个字节的上限（毫秒）。
///
/// 短到可以忽略（局域网内一个 RTT 就够了）、长到足以让正常客户端把
/// 类型字节送出来。它**不是** [`crate::protocol::IO_TIMEOUT_SECS`] ——
/// 拒绝路径上的等待是纯粹的损耗，30 秒会让一条本该立刻释放的连接
/// 占着并发额度不放。
const REFUSE_PEEK_TIMEOUT_MS: u64 = 300;

/// 回应答后"读干净对端剩余字节"的时长上限（毫秒）。
///
/// 存在的理由只有一个: 避免 Windows 上带未读数据关 socket 触发 RST,
/// 把刚写出去的拒绝帧作废（详见 [`TransferServer::refuse_after_peek`]）。
/// 它不需要长 —— 对端在拿到拒绝帧后本来就会停手。
const REFUSE_DRAIN_TIMEOUT_MS: u64 = 300;

/// 同时进行的「善意拒绝应答」额度。
///
/// ## 为什么需要它
///
/// 拒绝路径**不能**变成新的资源放大器 —— 尤其"并发已满"那条,
/// 它跑得最勤(每一次被拒都要答一次), 而每次拒绝都要占着一个 socket
/// 直到"偷看 + 写 + 读干净"结束, 最长约 600ms。
/// 不设上限的话, 一条连接洪泛就能把"拒绝以卸载压力"
/// 变成"为每一条被拒连接再养一个任务和一个 socket"——
/// 比修复之前更糟。
///
/// 拿不到额度就直接掐连接: 那正是这些路径在修复之前的**既有**行为,
/// 所以这不构成新的退化, 只是不让修复引入一个。
///
/// 进程级共享(而不是每个 `TransferServer` 一份)是有意的:
/// "我们同时愿意写多少条拒绝帧"本来就是**整个进程**的资源预算,
/// 而一个进程里正常也只有一个传输服务。
static REFUSAL_BUDGET: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(32)));

/// 读取一个长度前缀的 JSON 帧, 并强制上限。
/// 旧实现直接 `vec![0u8; 对端声明的长度]`, 一个 4 字节的 0xFFFFFFFF
/// 就能让进程分配 4 GiB 内存 —— 而且这段读取发生在任何认证之前。
///
/// ## `label` 必须进入错误文案
///
/// 帧读取失败是这个系统里最常见的错误, 而它的原始形态是 tokio 的字面量
/// `"early eof"`。不带上 `label` 时用户在界面上看到的是
/// `"I/O error: early eof"` —— 分不清是握手应答、浏览应答还是分块回执,
/// 更看不出是谁关的连接（见 [`FeisuoError::PeerClosed`]）。
pub(crate) async fn read_json_frame<T: serde::de::DeserializeOwned>(
    stream: &mut TcpStream,
    label: &str,
) -> Result<T> {
    let mut len_buf = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(IO_TIMEOUT_SECS), stream.read_exact(&mut len_buf))
        .await
        .map_err(|_| FeisuoError::Protocol(format!("读取 {} 长度超时", label)))?
        .map_err(|e| super::io_error_with(&format!("读取{}长度", label), e))?;

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_JSON_FRAME {
        return Err(FeisuoError::Security(format!(
            "{} 长度 {} 超过上限 {} 字节, 已拒绝",
            label, len, MAX_JSON_FRAME
        )));
    }

    let mut buf = vec![0u8; len];
    if len > 0 {
        tokio::time::timeout(Duration::from_secs(IO_TIMEOUT_SECS), stream.read_exact(&mut buf))
            .await
            .map_err(|_| FeisuoError::Protocol(format!("读取 {} 内容超时", label)))?
            .map_err(|e| super::io_error_with(&format!("读取{}内容", label), e))?;
    }
    Ok(serde_json::from_slice(&buf)?)
}

pub(crate) async fn write_json_frame<T: serde::Serialize>(
    stream: &mut TcpStream,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    // 每一段写都必须有超时: 对端挂机时 write_all 会永久阻塞
    super::write_with_timeout(stream, &(bytes.len() as u32).to_be_bytes(), "写帧长度").await?;
    super::write_with_timeout(stream, &bytes, "写帧内容").await?;
    tokio::time::timeout(
        Duration::from_secs(IO_TIMEOUT_SECS),
        stream.flush(),
    )
    .await
    .map_err(|_| FeisuoError::Network("刷新传输缓冲区超时".into()))?
    .map_err(|e| super::io_error_with("刷新传输缓冲区", e))?;
    Ok(())
}

/// 审批失败的**结构化**原因。
///
/// ## 为什么不能只回一句文案
///
/// 「需要本次传输码」是一次**协商回合**而不是拒绝：对方收到它应当
/// **保留待发队列**、弹码让用户抄过来、带码重试。
/// 而"用户点了拒绝 / 审批超时 / 码连错三次"是**最终判决**。
///
/// 旧实现两者都塞进 `FeisuoError::Security(String)`，于是调用方只能
/// 去文案里判断 —— `client.rs` 的取回路径真的那么干了：
///
/// ```text
/// if resp.message.contains("传输码") || resp.message.contains("匹配码")
/// ```
///
/// 协议其实早就给传输配了结构化字段 `HandshakeResponse.requires_grant_code`，
/// 但**浏览与取回的应答没配**，所以那两条路径只能退回匹配中文。
/// 一旦对端换了措辞（哪怕只是升级到另一个构建），一次正常的
/// 「请出示本次传输码」就会被当成硬拒绝报给用户，而用户能做的只有重试 ——
/// 于是永远卡在这一步（这正是 `error.rs` 里 `GrantCodeRequired`
/// 那段注释描述过的失败模式）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApprovalError {
    /// 需要对端出示本次传输码。**不是**最终拒绝。
    GrantCodeNeeded { user_message: String },
    /// 最终拒绝：用户拒绝 / 超时 / 通道关闭 / 码连错 N 次 / 对方无审批界面。
    Rejected { user_message: String },
}

impl ApprovalError {
    /// 给人看的说明（写进安全事件与应答的 `message`）。
    pub fn user_message(&self) -> &str {
        match self {
            ApprovalError::GrantCodeNeeded { user_message }
            | ApprovalError::Rejected { user_message } => user_message,
        }
    }

    fn rejected(msg: impl Into<String>) -> Self {
        ApprovalError::Rejected {
            user_message: msg.into(),
        }
    }
}

pub struct ApprovalManager {
    pending: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    /// `approval_id` -> 本次审批应当比对的码。**只在本进程内比对，不外发。**
    by_approval: Mutex<HashMap<String, String>>,
    /// `(peer_id, 操作指纹)` -> 已签发的码，用于让**重试沿用同一个码**。
    issued: Mutex<HashMap<String, IssuedChallenge>>,
}

struct IssuedChallenge {
    code: String,
    at: std::time::Instant,
}

/// 同一 `(设备, 操作指纹)` 在这个窗口内重试时沿用同一个码。
///
/// # 为什么必须有它
///
/// 真实时序是：接收方弹出窗口并显示码 → 发起方收到「需要码」→
/// **用户去读接收方屏幕上的码** → 发起方带码重试。
///
/// 而"带码重试"是一次**全新的 TCP 连接**，在接收方看来是一个
/// **全新的审批**。如果每次审批都新生成一个码，那么用户在第一个窗口
/// 上读到的码在第二个窗口上永远对不上 —— 这个功能**一次都不可能成功**。
/// （这是把方向从"发起方出码"改成"接收方出码"时必须一并解决的东西：
/// 旧方向下码由发起方生成，重试带的是同一个码，天然对得上。）
///
/// 所以码的复用边界是"同一个设备的**同一个请求**"：
/// 指纹由发起请求的具体内容构成（传输 = 文件数+总大小+首文件名，
/// 浏览 = 卷+路径，取回 = 卷+子路径列表），换一个请求就是新码。
///
/// # 代价（诚实说明）
///
/// 同一台设备在窗口内**重复发送完全相同的内容**时会拿到同一个码。
/// 也就是说：拿到过这个码的人，在 TTL 内可以重放**同一个请求**。
/// 但他仍然需要该设备的有效 Ed25519 签名才能通过握手，
/// 而签名验证通过意味着**那台设备此刻真的在发起连接** ——
/// 也就是说这道码此时要挡的"无人值守静默推送"场景已经不成立了。
/// TTL 取 120 秒，覆盖"用户读完屏幕、念过去、对方敲进去"所需的时间，
/// 不做更长。
const CHALLENGE_REUSE_TTL: std::time::Duration = std::time::Duration::from_secs(120);

impl ApprovalManager {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            by_approval: Mutex::new(HashMap::new()),
            issued: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<ApprovalDecision>>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn register(&self, approval_id: &str) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        self.lock().insert(approval_id.to_string(), tx);
        rx
    }

    /// 取出（或签发）本次审批的匹配码，返回它**显示给本机用户**。
    ///
    /// # 方向：为什么码由**接收方**生成，而不是发起方
    ///
    /// 「每次匹配码」要回答的问题是"发起方现在真的在吗"，而不是
    /// "发起方是不是它自称的那台机器"——后者由握手里的 Ed25519 签名
    /// 回答，根本不需要码。
    ///
    /// 那么码必须由**不信任发起方的那一方**（接收方）来发：
    ///
    /// 1. **凭证的方向**。接收方不信任发起方，所以该由接收方出题、
    ///    发起方应答。反过来（发起方出题、接收方抄）等于让接收方
    ///    去核对一个**发起方自己编的答案**：发起方知道答案，
    ///    接收方手上却没有任何独立信息可用于判断。
    /// 2. **静默推送挡不住**。接收方出码时，码只出现在接收方屏幕上。
    ///    一台被攻陷的旧设备读不到它，于是**无法在不惊动接收方的情况下
    ///    完成传输**。发起方出码则相反：那台设备自己就能填上。
    ///
    /// 代价是码必须由人从接收方屏幕搬到发起方（当面看、或念一遍）。
    /// 这正是"每次匹配码"这个等级**本来就要**的东西 ——
    /// 它和「永久信任」的区别正是"每次都要有人在旁边"。
    ///
    /// # 安全性
    ///
    /// - 码**只在本进程内、只给本机 UI**，绝不放进任何回给对端的帧。
    ///   一旦写进应答，发起方就能自动填上，"有人在接收方旁边"
    ///   这件事就完全不存在了。
    /// - 用 CSPRNG 生成（见 [`generate_grant_challenge`]）。
    /// - 换一个请求（指纹不同）一定是新码，所以旧码无法用于别的操作。
    pub fn challenge_for(&self, peer_id: &str, fingerprint: &str) -> String {
        let key = format!("{}|{}", peer_id, fingerprint);
        let mut map = self.issued.lock().unwrap_or_else(|e| e.into_inner());
        // 顺手清过期项：这张表只按"设备 × 请求"增长，不清会一直长。
        if map.len() > 128 {
            map.retain(|_, v| v.at.elapsed() < CHALLENGE_REUSE_TTL);
        }
        let code = match map.get(&key) {
            Some(v) if v.at.elapsed() < CHALLENGE_REUSE_TTL => v.code.clone(),
            _ => {
                let c = generate_grant_challenge();
                map.insert(
                    key,
                    IssuedChallenge {
                        code: c.clone(),
                        at: std::time::Instant::now(),
                    },
                );
                c
            }
        };
        drop(map);
        code
    }

    /// 把本次审批的码记下来，供 [`ApprovalManager::expected_for`] 比对。
    pub fn bind(&self, approval_id: &str, code: &str) {
        self.by_approval
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(approval_id.to_string(), code.to_string());
    }

    /// 取本次审批应当比对的码。
    pub fn expected_for(&self, approval_id: &str) -> Option<String> {
        self.by_approval.lock().unwrap_or_else(|e| e.into_inner()).get(approval_id).cloned()
    }

    /// 移除待审批条目 (超时/取消时调用, 否则 map 会无限增长)
    pub fn discard(&self, approval_id: &str) {
        self.lock().remove(approval_id);
        self.by_approval
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(approval_id);
    }

    /// 提交一次审批结果。
    ///
    /// **没有 `grant_code` 参数**：匹配码是本进程生成的（见
    /// [`ApprovalManager::challenge_for`]），发起方出示的码由调用方
    /// 拿 `expected_for` 比对。审批人只负责"允许一次 / 允许并永久信任 / 拒绝"，
    /// 不需要（也不应该）替对端"输入"什么。
    pub fn resolve(&self, approval_id: &str, action: ApprovalAction) -> bool {
        if let Some(tx) = self.lock().remove(approval_id) {
            let _ = tx.send(ApprovalDecision {
                action,
                grant_code: String::new(),
            });
            true
        } else {
            false
        }
    }
}

/// 生成 6 位数字的「本次匹配码」。
///
/// 用 `rand::thread_rng` 而不是 `SystemTime` 之类的：这是**凭据**，
/// 可预测的码等于没有码（有人能算出"下一位是什么"就能填对）。
///
/// 6 位 = 100 万种组合。配合"每次审批换码 + 3 次上限 + 60 秒审批超时"，
/// 穷举需要 100 万次签名握手，实际不可行；而位数再多用户在电话里
/// 念错的比例明显上升（这是可用性与强度的真实取舍点）。
fn generate_grant_challenge() -> String {
    use rand::Rng;
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{:06}", n)
}

/// 给人读的码：`123 456`（念/抄都更容易，错的概率更低）。
fn format_grant_challenge_for_display(code: &str) -> String {
    let t = code.trim();
    if t.len() == 6 && t.chars().all(|c| c.is_ascii_digit()) {
        format!("{} {}", &t[..3], &t[3..])
    } else {
        t.to_string()
    }
}

/// 比码通过后，按用户选的档位决定要不要写一张**同类操作**的短期授权（§2.3.1）。
///
/// ## 为什么单独一个函数、而不是三处各写一遍
///
/// 传输 / 浏览 / 取回三条路都要判 `RequireGrant` 并比码。
/// 分散写的话，"写了授权"这个**有副作用**的动作会出现三份 ——
/// 而 §14.10 已经记过"一个功能实现了三遍、只有一遍是通的"，
/// §14.9 也把 `session`↔`session_grants`↔`access_scope` 列为三层叠加。
/// **同一个副作用只能有一个落点。**
///
/// ## 安全性质（三条都在这里强制，调用方无法绕过）
///
/// 1. **只在比码通过后**写。`AllowOnce` 不写 —— 它的语义就是"只这一次"。
/// 2. **scope 就是本次的操作类别**（§2.3）。`receive` 的授权
///    **不能**用来 `browse` 或 `pull`：`auth_policy` 查的是
///    `has_valid_grant(peer_id, op.as_str())`，scope 不同即落空。
/// 3. **TTL 上界在 [`crate::config::effective_session_grant_ttl`] 里**
///    强制（10 分钟），调用方传多大都会被夹住。
///
/// 返回 `None` = 没写（用户选了「只允许本次」，或功能被配成 0）。
fn maybe_write_session_grant(
    trust_store: &TrustStore,
    cfg: &AppConfig,
    peer_id: &str,
    peer_name: &str,
    scope: Op,
    action: ApprovalAction,
) -> Option<u64> {
    if !action.wants_grant() {
        return None;
    }
    let ttl = crate::config::effective_session_grant_ttl(cfg.session_grant_ttl_secs)?;
    if peer_id.is_empty() {
        // 没有对端身份就没有归属，写了也没人能查、也永远清不掉。
        return None;
    }
    match trust_store.grant_session(peer_id, scope.as_str(), ttl as i64) {
        Ok(()) => {
            info!(
                "已为 {} 的「{}」写入 {}s 短期授权（用户选了免重复确认）",
                peer_name,
                scope.as_str(),
                ttl
            );
            Some(ttl)
        }
        Err(e) => {
            // 写授权失败**不能**影响本次传输 —— 用户的首要意图是
            // "把东西传过去"，授权只是省下一次码。
            // fail-open 记在**传输**上、fail-closed 记在**授权**上。
            warn!(
                "写入短期授权失败（本次传输仍放行）: {} / {} / {}",
                peer_id,
                scope.as_str(),
                e
            );
            None
        }
    }
}

/// 定长（补零后）**常数时间**比较两个匹配码。
///
/// 6 位数字的逐字节比较其实很难被实用地计时攻击（网络抖动远大于
/// 单次比较的时间差），但这是凭据比对，写成常数时间不额外花什么，
/// 而"以后有人把它改成 `==`"这件事会很自然 —— 现在就把形状定死。
fn grant_code_eq(a: &str, b: &str) -> bool {
    let a = a.trim().as_bytes();
    let b = b.trim().as_bytes();
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

pub struct TransferServer {
    identity: Arc<DeviceIdentity>,
    config: Arc<RwLock<AppConfig>>,
    trust_store: Arc<TrustStore>,
    /// 处理"取回"请求时需要反向发起传输
    client: Arc<TransferClient>,
    progress_tx: broadcast::Sender<TransferProgress>,
    approval_tx: broadcast::Sender<ApprovalRequest>,
    approval_manager: Arc<ApprovalManager>,
    /// 并发连接上限
    connection_limiter: Arc<Semaphore>,
    /// 关闭信号。accept 循环 select 在它上面, 收到即退出。
    ///
    /// 旧实现的 accept 循环是无出口的 `loop {}`, 没有任何办法停它。
    /// 后果: 进程内第二次 `start()` 会起第二个监听任务去抢同一个端口,
    /// 失败后只留一条 warn 日志; Android Service 重建时会持续累积。
    ///
    /// **必须配合 `stopped` 标志**: `Notify::notify_waiters()` 只唤醒
    /// "调用那一刻已注册"的等待者, accept 循环若恰好在两次 accept 之间
    /// 就会丢掉这次唤醒, 永远停不下来。
    shutdown: Arc<tokio::sync::Notify>,
    /// 关闭标志, accept 循环每轮开头检查
    stopped: Arc<std::sync::atomic::AtomicBool>,
    /// accept 循环的 JoinHandle。
    ///
    /// 必须持有它才能在 stop() 里 abort: 只置标志的话, 监听端口要等到
    /// accept 循环下一次醒来才释放, "停止后立刻重启"必然因端口被占而失败
    /// (实测 `boot -> shutdown -> boot` 第二次直接返回 INVALID_HANDLE)。
    /// 该任务除持有 listener 外没有收尾工作, 直接 abort 是安全的。
    accept_task: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// 已被用户撤销的 `transfer_id` 集合（§5.2 直发撤销）。
    ///
    /// ## 为什么必须有它
    ///
    /// 早先的撤销实现是在**另一条连接**上直接
    /// `remove_dir_all(.feisuo-incoming/<id>)`，然后回一句"已撤销"。
    /// 而**正在跑的那条传输连接完全不知情**：
    ///
    /// - Windows 上文件正被打开时 `remove_dir_all` 会失败，而那行
    ///   写的是 `let _ =`（忽略错误）⇒ **回"已撤销"但什么都没删掉**；
    /// - 即使删成功，传输循环仍会继续写、继续校验，最后照常 commit ——
    ///   用户点了"撤销"、界面说"已撤销"，**文件照样落进收件目录**。
    ///
    /// 这不是边界情况，是要求 ②「拖拽直发 + 5 秒撤销」的**主路径**。
    ///
    /// 传输循环每收一个分块查一次这个集合（4 MiB 一次，锁持有时间
    /// 远小于一次网络往返），命中就中断并让 `StagingGuard` 清理。
    ///
    /// 值是「用户点撤销的时刻」。
    ///
    /// 不能在读到清单时无条件删掉这个 id：取消帧走的是**另一条连接**，
    /// 经常比清单更早到达。删掉之后，这次传输就再也看不到撤销，
    /// 文件照样落盘。只丢掉**早于本次连接建立**的旧标记
    /// （同一批文件重试时，上次的撤销不能毒化这一次）。
    cancelled: Arc<std::sync::Mutex<std::collections::HashMap<String, Instant>>>,
    /// 已经 commit（文件已落到收件目录）的 `transfer_id`。
    ///
    /// 撤销请求到达时据此判断"还撤得掉吗"。集合在传输开始时清、
    /// commit 成功后写入、保留期结束后由维护任务清理 ——
    /// 只需覆盖"传输正在进行"这个窗口，所以不需要长期留存。
    committed_transfers: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// 断点续传决策的产物（`plan_resume` 的返回值）。
struct ResumePlan {
    /// 已完整落盘、可跳过的文件
    completed: Vec<CompletedPart>,
    /// 本次真正要传的清单下标
    needed: Vec<u32>,
    /// 与 `needed` 等长、同序的目标相对路径
    dest_paths: Vec<String>,
    /// 本次真正要传的字节数
    send_bytes: u64,
    /// 分块级断点续传：针对 needed 中的文件，已在暂存区收到的有效起始分块进度
    file_resumes: Vec<crate::protocol::FileResumeProgress>,
}

impl TransferServer {
    pub fn new(
        identity: Arc<DeviceIdentity>,
        config: Arc<RwLock<AppConfig>>,
        trust_store: Arc<TrustStore>,
        client: Arc<TransferClient>,
        progress_tx: broadcast::Sender<TransferProgress>,
        approval_tx: broadcast::Sender<ApprovalRequest>,
    ) -> Self {
        Self {
            identity,
            config,
            trust_store,
            client,
            progress_tx,
            approval_tx,
            approval_manager: Arc::new(ApprovalManager::new()),
            connection_limiter: Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            accept_task: Arc::new(std::sync::Mutex::new(None)),
            cancelled: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            committed_transfers: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    // 注：`cancelled` / `committed_transfers` 的读写都发生在
    // `handle_connection` 内部（撤销登记在 `handle_cancel`，轮询在分块循环），
    // 所以这里**刻意不提供** `is_cancelled()` 之类的公开方法 ——
    // 一个没人调用的公开方法等于对外承诺一个不存在的用法。
    // 真需要暴露时再加，那时它会有真实的调用方。

    /// 请求停止传输服务 (accept 循环退出)。
    ///
    /// 先置标志再唤醒, 然后 abort 掉 accept 循环并**等它真正结束**。
    ///
    /// ## 为什么 abort 之后还要等
    ///
    /// `JoinHandle::abort()` 只是**请求**取消。accept 循环持有 `listener`,
    /// 而它要等 task 被调度一次、future 被 drop 之后才真的释放 ——
    /// 端口因此还要再晚**若干毫秒**才空出来。
    ///
    /// 紧接着的 `start()` 就会拿到 `AddrInUse`, 而报错文案写的是
    /// "是否已有另一个飞梭实例在运行" —— 占用者恰恰是刚刚退出的**他自己**。
    /// 真实后果: 托盘"退出再打开"、Android 前台服务被系统杀掉后重启。
    ///
    /// 这与 `DiscoveryService::stop()` 是**同一个缺陷**（那里已修，见该文件
    /// `stop()` 的长注释），两边的写法当时是一起写的，所以也一起中招。
    ///
    /// ## 为什么等的方式是"轮询 + 让出"
    ///
    /// 不能用 `runtime().block_on`：调用方（Android JNI 关闭、桌面退出钩子）
    /// 拿到的 runtime 可能**已经关闭**，在上面 await 等一个永不完成的
    /// future 等于把进程挂死。也不能用 `blocking_write` 那类会 panic 的
    /// 原语 —— `stop()` 可能运行在 tokio worker 线程**内部**
    /// （`protocol_integration` 里就有 async 测试直接调 `stop()`）。
    ///
    /// `stopped` 标志已经置好、accept 循环也在 `select!` 里等着
    /// `shutdown.notified()`，所以它本来就会很快醒来；这里只是把
    /// "已经醒来并 drop 掉 listener"这件事**确认**一下。
    pub fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.shutdown.notify_waiters();
        let handle = match self.accept_task.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        };
        if let Some(t) = handle {
            t.abort();
            // 等它真正被丢弃 —— 那才是 listener 被 drop、端口空出来的时刻。
            // 2 秒是防挂死的上限, 正常是微秒级。
            const DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
            let deadline = std::time::Instant::now() + DEADLINE;
            while !t.is_finished() {
                if std::time::Instant::now() >= deadline {
                    tracing::error!(
                        "等待传输服务 accept 循环退出超时（2s），传输端口可能仍被本进程占用"
                    );
                    return;
                }
                std::thread::yield_now();
            }
        }
    }

    /// 提交一次审批结果。
    ///
    /// **没有 `grant_code` 参数**：匹配码由本进程生成并显示给审批人看
    /// （见 [`ApprovalManager::issue_challenge`]），由发起方出示回来。
    /// 审批人只决定"允许一次 / 允许并永久信任 / 拒绝"。
    pub fn resolve_approval(&self, approval_id: &str, action: ApprovalAction) -> bool {
        self.approval_manager.resolve(approval_id, action)
    }

    pub async fn start(&self) -> Result<()> {
        let port = self.config.read().await.transfer_port;
        // 先重置关闭标志, 否则 stop() 之后无法重新启动。
        // 绑定失败时把标志恢复回去, 保持"已停止"语义不被破坏。
        let was_stopped = self
            .stopped
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            Ok(l) => l,
            Err(e) => {
                if was_stopped {
                    self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                return Err(e.into());
            }
        };
        info!("Transfer server listening on TCP port {}", port);

        let identity = self.identity.clone();
        let config = self.config.clone();
        let trust_store = self.trust_store.clone();
        let pull_client = self.client.clone();
        let progress_tx = self.progress_tx.clone();
        let approval_tx = self.approval_tx.clone();
        let approval_manager = self.approval_manager.clone();
        let limiter = self.connection_limiter.clone();
        let s_shutdown = self.shutdown.clone();
        let s_stopped = self.stopped.clone();
        let cancelled = self.cancelled.clone();
        let committed_transfers = self.committed_transfers.clone();

        let accept_task = tokio::spawn(async move {
            let mut consecutive_errors: u32 = 0;
            loop {
                // 标志检查放在 select 之前: notify_waiters 只唤醒调用那一刻
                // 已注册的等待者, 循环停在两次 accept 之间时那次唤醒就丢了。
                if s_stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    info!("传输服务 accept 循环已停止");
                    break;
                }
                // accept 必须与关闭信号竞争, 否则 stop() 之后任务永远阻塞,
                // 紧接着的 start() 会与它抢同一个监听端口。
                let accepted = tokio::select! {
                    _ = s_shutdown.notified() => {
                        info!("传输服务 accept 循环已停止");
                        break;
                    }
                    a = listener.accept() => a,
                };
                if s_stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    info!("传输服务 accept 循环已停止");
                    break;
                }
                match accepted {
                    Ok((stream, peer_addr)) => {
                        consecutive_errors = 0;
                        let _ = stream.set_nodelay(true);

                        let id = identity.clone();
                        let cfg = config.clone();
                        let ts = trust_store.clone();
                        let pc = pull_client.clone();
                        let ptx = progress_tx.clone();
                        let atx = approval_tx.clone();
                        let am = approval_manager.clone();

                        // 并发上限: 防御"开 N 条连接各发 1 字节然后静默"的资源耗尽
                        let permit = match limiter.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                let msg = format!(
                                    "对方并发连接数已达上限 {}，请稍后重试",
                                    MAX_CONCURRENT_CONNECTIONS
                                );
                                warn!("拒绝来自 {} 的连接: {}", peer_addr, msg);
                                // 必须给对端一条明确应答。裸 `continue` 会把
                                // stream 直接丢掉, 对端那边就成了
                                // "对方已关闭连接: 读取握手应答长度时…" ——
                                // 一个既没有 errno 也没有原因的错误,
                                // 而真实原因(并发已满)就在手边。
                                //
                                // 同样不能在这条路径上阻塞: 这一刻正是要**卸载**
                                // 压力的时候, 让 accept 循环停下来等对端发字节
                                // 等于把防御变成新的瓶颈。
                                tokio::spawn(Self::refuse_after_peek(
                                    stream,
                                    id.clone(),
                                    cfg.clone(),
                                    msg,
                                ));
                                continue;
                            }
                        };

                        // 撤销/已落盘两个集合必须**在 spawn 之前**克隆。
                        // `async move` 会按值把捕获的变量搬进闭包,
                        // 而它位于 `loop` 里 —— 第二轮迭代就变成
                        // "use of moved value"。在闭包**内**写 `.clone()`
                        // 救不了: 捕获动作本身就已经是移动了。
                        let conn_cancelled = cancelled.clone();
                        let conn_committed = committed_transfers.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) = Self::handle_connection(
                                stream,
                                id,
                                cfg,
                                ts,
                                pc,
                                ptx,
                                atx,
                                am,
                                conn_cancelled,
                                conn_committed,
                            )
                            .await
                            {
                                // 对端正常断开不是错误, 不应刷 warn 冲掉日志轮转配额
                                //
                                // `PeerClosed` 必须一并算进来: 帧读取现在会把它
                                // 单独归类(见 `io_error_with`), 不加这一条的话
                                // "对端正常关连接" 会突然开始刷 warn ——
                                // 一个纯日志侧的回归, 而且它会在最常见的场景
                                // (用户点取消 / 对端先行关闭) 上发生。
                                let is_normal_close = matches!(
                                    e,
                                    FeisuoError::PeerClosed(_)
                                ) || matches!(
                                    e,
                                    FeisuoError::Io(ref io)
                                        if io.kind() == std::io::ErrorKind::UnexpectedEof
                                            || io.kind() == std::io::ErrorKind::ConnectionReset
                                            || io.kind() == std::io::ErrorKind::BrokenPipe
                                );
                                if is_normal_close {
                    info!("Connection from {} closed by peer", peer_addr);
                } else {
                    warn!("Connection from {} failed: {}", peer_addr, e);
                }
                            }
                        });
                    }
                    Err(e) => {
                        // 旧实现只是 error! 然后立刻回到 accept(),
                        // 在 EMFILE / WSAEMFILE 下会变成 100% CPU 紧循环, 饿死该 worker。
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        let backoff =
                            Duration::from_millis((50u64 << consecutive_errors.min(6)).min(3_000));
                        error!(
                            "TCP accept error ({}x): {}; retrying in {:?}",
                            consecutive_errors, e, backoff
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        });

        // 登记 accept 循环, 使 stop() 能 abort 它并立刻释放传输端口
        if let Ok(mut slot) = self.accept_task.lock() {
            *slot = Some(accept_task);
        }

        Ok(())
    }

    

    async fn handle_connection(
        mut stream: TcpStream,
        identity: Arc<DeviceIdentity>,
        config: Arc<RwLock<AppConfig>>,
        trust_store: Arc<TrustStore>,
        pull_client: Arc<TransferClient>,
        progress_tx: broadcast::Sender<TransferProgress>,
        approval_tx: broadcast::Sender<ApprovalRequest>,
        approval_manager: Arc<ApprovalManager>,
        cancelled: Arc<Mutex<std::collections::HashMap<String, Instant>>>,
        committed_transfers: Arc<Mutex<std::collections::HashSet<String>>>,
    ) -> Result<()> {
        let peer_ip = match stream.peer_addr() {
            Ok(a) => a.ip().to_string(),
            Err(_) => "unknown".to_string(),
        };
        // 诊断用的连接起点与本机 IP（§9.6 / §9.7）
        let conn_started = Instant::now();
        let local_ip = local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_default();
        // 审批超时改为可配（§9.8.3：跨地域时 60s 偏短, 超时即拒会让用户频繁被拒）
        let approval_timeout = Duration::from_secs(config.read().await.approval_timeout_secs);
        // 仅在走了人工审批分支时才有值
        // 审批超时（§9.8.3；默认值 60s 偏短，超时后用户只能重试）

        // 1. 读消息类型
        let mut msg_type = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(IO_TIMEOUT_SECS), stream.read_exact(&mut msg_type))
            .await
            .map_err(|_| FeisuoError::Network("等待握手超时".into()))?
            .map_err(|e| super::io_error_with("读取消息类型", e))?;

        match msg_type[0] {
            MSG_PAIR => {
                return Self::handle_pairing(
                    &mut stream,
                    &identity,
                    &config,
                    &trust_store,
                    &peer_ip,
                )
                .await
            }
            MSG_BROWSE => {
                return Self::handle_browse(
                    &mut stream,
                    &identity,
                    &config,
                    &trust_store,
                    &approval_manager,
                    &approval_tx,
                    &peer_ip,
                )
                .await
            }
            MSG_PULL => {
                return Self::handle_pull(
                    &mut stream,
                    &identity,
                    &config,
                    &trust_store,
                    &pull_client,
                    &approval_manager,
                    &approval_tx,
                    &peer_ip,
                )
                .await
            }
            MSG_CANCEL => {
                return Self::handle_cancel(
                    &mut stream,
                    &identity,
                    &config,
                    &trust_store,
                    &peer_ip,
                    &cancelled,
                    &committed_transfers,
                )
                .await;
            }
            MSG_UNPAIR => {
                return Self::handle_unpair(
                    &mut stream,
                    &identity,
                    &config,
                    &trust_store,
                    &peer_ip,
                )
                .await;
            }
            MSG_TRANSFER => {}
            other => {
                // 未知类型照样回应答 —— 类型都看不懂, 更不能只掐连接。
                let msg = format!("未知的消息类型 {}", other);
                Self::refuse_by_msg_type(&mut stream, other, &identity, &config, &msg, false)
                    .await;
                return Err(FeisuoError::Protocol(msg));
            }
        }

        // 3. 握手 (带长度上限)
        let handshake: HandshakeRequest = read_json_frame(&mut stream, "握手请求").await?;

        if handshake.version != PROTOCOL_VERSION {
            // 版本不兼容必须**说出来**: 这是最需要用户动手的一类拒绝
            // （升级对端），却曾经只表现为一个无来由的连接中断。
            let e = FeisuoError::Protocol(format!(
                "协议版本不兼容: 对端 {} / 本机 {}",
                handshake.version, PROTOCOL_VERSION
            ));
            Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
            return Err(e);
        }

        // ---- 下面每一条拒绝路径都必须先 `refuse_transfer` ----
        //
        // 早先这一整段是裸 `return Err`: 只关连接、不给对端任何东西。
        // 于是发送方在 `read_json_frame("握手应答")` 上收到 `UnexpectedEof`,
        // 界面显示的是 `"I/O error: early eof"` —— 既没有原因, 也看不出
        // 是谁关的连接。用户能做的只有反复重试一个注定失败的操作,
        // 而真实原因(被拉黑 / 版本不兼容 / 签名不过)只存在于**对方**的日志里。
        //
        // 下面这一段拒绝理由的**细节**都通过 `refuse_transfer` 出站,
        // 因为调用者正是它自称的那台设备 —— 没有额外泄露。
        // device_id 必须与握手自带的公钥一致, 否则攻击者可以拿一个
        // "已信任的 device_id + 自己的公钥"顶替既有身份。
        if handshake.sender_public_key_hex.is_empty() {
            let e = FeisuoError::Security("握手缺少发送方公钥".into());
            Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
            return Err(e);
        }
        // 这不是"顺手补一个 ? 的错误" —— 对端发来一串垃圾公钥是**外部输入**,
        // 它触发的早退和对端自己版本不兼容在性质上一样(对端该升级/该检查),
        // 所以必须一样地把原因说出来。
        let claimed_id = match DeviceIdentity::device_id_from_pubkey_hex(
            &handshake.sender_public_key_hex,
        ) {
            Ok(id) => id,
            Err(e) => {
                let msg = format!("握手公钥无法解析: {}", e);
                Self::refuse_transfer(&mut stream, &identity, &config, msg.clone()).await;
                return Err(FeisuoError::Security(msg));
            }
        };
        if claimed_id != handshake.sender_id {
            let e = FeisuoError::Security(format!(
                "设备指纹与公钥不匹配: 自称 {} / 实际 {}",
                handshake.sender_id, claimed_id
            ));
            Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
            return Err(e);
        }

        // 已绑定时公钥必须与库中记录完全一致
        if let Some(pubkey) = trust_store.get_device_pubkey(&handshake.sender_id)? {
            if !pubkey.eq_ignore_ascii_case(&handshake.sender_public_key_hex) {
                let e = FeisuoError::Security("设备指纹已绑定到另一把公钥, 拒绝连接".into());
                Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
                return Err(e);
            }
        }

        // ---- 统一鉴权（§2.4）----
        // 旧实现在 4 个 handler 里各写了一遍 is_device_trusted / screen_peer,
        // 引入"信任等级 × 访问范围 × 会话授权"四维后必然漏改某一个。
        // 现在所有准入判断都收敛到 auth_policy::authorize 这一个函数。
        let decision = {
            let c = config.read().await.clone();
            // `path` / `volume` 传空串是**有意的**，不是漏填。
            //
            // `check_scope` 对 `Op::Receive` 只查 `can_push`，不查 `can_read`
            // —— 因为 `can_read` 的语义是"能不能**读**那个路径"，
            // 而入站落点恒在收件目录之内（由紧随其后的
            // `normalize_sub_path` + `is_within` 强制），不适用它。
            //
            // 刻意**不要**把 `dest_sub_path` 填进这里：真填了，
            // `AccessMode::ReceiveOnly` 会让 `can_read` 对**任何**路径
            // 返回 false，于是将来谁在 Receive 分支加上 `can_read` 检查，
            // 就会得到"只允许写收件目录的设备一律收不到任何文件"。
            // 那是一个等着被踩的陷阱，而不是纵深防御。
            crate::security::authorize(
                &trust_store,
                &c,
                crate::security::Op::Receive,
                &crate::security::AuthContext {
                    peer_id: &handshake.sender_id,
                    peer_ip: &peer_ip,
                    volume: "",
                    path: "",
                },
            )
        };
        let challenge = format!("{}:{}", handshake.nonce, handshake.timestamp);

        // 是否属于"可以走人工审批"的原因; 其余 Deny 直接拒绝。
        // 三类: 未配对 / 关了自动接收 / **「每次匹配码」等级**。
        //
        // 加上第三类是这轮的修复: 早先 `RequireGrant` 直接 `return Err`,
        // 而界面上**照样有**「每次匹配码」开关。给了开关却不生效比不给更糟 ——
        // 用户会以为配好了, 实际每次传输都被拒, 而且完全不知道原因。
        // ## 旧写法靠 `contains()` 猜, 已修
        //
        // ```text
        // matches!(&decision, Deny(r) if r.contains("未配对") || r.contains("自动接收"))
        // ```
        // 那是 `Deny(&'static str)` 逼出来的: 类型里只有一个文案字段,
        // 下游分不清"这个拒绝能被审批救回"和"这个拒绝就是最终判决",
        // 只能去匹配中文。**安全行为因此依赖于措辞** —— 把「已关闭自动接收」
        // 改成「已关闭自动接收文件」, 一台永久信任设备就会从弹审批
        // 变成硬拒绝, 且无任何编译期错误。
        //
        // 现在改成读 `DenyCode::is_approvable()`: 那是"一个 code 一个答案"
        // 的纯函数, 而 `authorize` 只有唯一的写入方。
        let needs_approval = decision.is_approvable_deny()
            || matches!(decision, crate::security::Decision::RequireGrant);
        // 「每次匹配码」等级: 本次审批必须由用户输入传输码
        let requires_grant_code =
            matches!(decision, crate::security::Decision::RequireGrant);

        match decision {
            crate::security::Decision::Allow => {
                // 静默放行: 永久信任 + 已开自动接收。仍必须验签。
                let pubkey = match trust_store.get_device_pubkey(&handshake.sender_id)? {
                    Some(pk) => pk,
                    // 鉴权说"永久信任"但库里没有公钥 —— 状态自相矛盾,
                    // fail-closed 并把矛盾告诉对端（否则它只会看到连接被掐）。
                    None => {
                        let e = FeisuoError::Untrusted(handshake.sender_id.clone());
                        Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
                        return Err(e);
                    }
                };
                if !DeviceIdentity::verify(&pubkey, challenge.as_bytes(), &handshake.signature)? {
                    let e = FeisuoError::Security("握手签名校验失败".into());
                    Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
                    return Err(e);
                }
                // 时效偏差 = 疑似重放。**必须让对端知道**是这一条:
                // 用户在两台机器时钟不同步时会毫无征兆地一直撞这堵墙,
                // 而"连接被掐"不会给他任何去校时钟的线索。
                if let Err(e) = Self::check_freshness(handshake.timestamp) {
                    Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
                    return Err(e);
                }
            }
            crate::security::Decision::RequireGrant => {
                // 落到下面的审批分支 —— 不在这里返回。
                info!(
                    "Incoming {} from {} ({}) is in「每次匹配码」mode, 需要人工输入传输码",
                    "transfer", handshake.sender_name, peer_ip
                );
            }
            crate::security::Decision::Deny {
                user_message: reason,
                ..
            } if !needs_approval => {
                // 硬拒绝（未配对 / 自动接收关闭 / 无写入权限 / 网段或范围不允许）
                // **必须留安全事件**。
                //
                // 早先这里只 `return Err`, 上层打一条 warn 就关连接 ——
                // 用户在安全事件列表里什么都看不到, 于是只能理解为
                // "对方设备坏了", 而真实情况是"对方只是不让我写而已"。
                // §3.9 的要求是"必须让用户知情, 不能只写日志"。
                //
                // 同时给对端一条明确应答: 只回 Err 的话发送方看到的是
                // **连接被掐**, 分不清"被拒绝了"和"网络断了" ——
                // 而这两种情况用户该做的事完全不同（前者去查安全事件，
                // 后者重试就行）。
                Self::refuse_transfer(&mut stream, &identity, &config, reason.clone()).await;
                let _ = trust_store.add_security_event(
                    crate::security::SecurityEventKind::RejectedAttempt,
                    &handshake.sender_id,
                    &handshake.sender_name,
                    &reason,
                );
                return Err(FeisuoError::Security(reason));
            }
            crate::security::Decision::Deny { code, .. } => {
                info!(
                    "Incoming transfer from {} ({}) requires approval (reason={:?})",
                    handshake.sender_name, peer_ip, code
                );
            }
        }

        // 审批分支（未配对 / 关了自动接收 / 每次匹配码 三者共用）
        //
        // 块**返回**审批耗时而不是写进一个预先声明的变量：预声明 `None`
        // 会在被读到之前就被覆盖，编译器会如实报"死赋值"。而让块
        // 直接产出这个值，"没走审批"与"审批花了 0ms"在类型上就分得开 ——
        // 前者根本不执行这个块。
        //
        // ## 这里必须有 `if !needs_approval` 闸门（实测发现的严重缺陷）
        //
        // 早先这个块是**无条件**执行的：上面 `match decision` 在
        // `Allow` 分支验完签就往下走，然后**照样**进了审批流程 ——
        // 于是"永久信任 + 已开自动接收"的设备每次发文件都会弹窗，
        // 30 秒无人应答后被拒。整条"配对一次、终生无人值守"的承诺
        // **完全失效**，而且从界面上看不出任何异常（弹窗照弹、
        // 允许按钮照能点，只是本来不该问）。
        //
        // 编译期抓不到它：`approval_wait_ms` 两条路径都有值，
        // 类型完全一致。这类"控制流走错分支"的缺陷只能靠端到端
        // 实跑发现 —— 断言"永久信任时鉴权决议 = Allow"还不够，
        // 必须真的发一个文件、看它是不是零弹窗落地。
        // 需要人工确认的路径，时钟只在**进审批之前**查这一次。
        //
        // 查晚了会把等人的时间和发送方算哈希的时间算进 120 秒。
        // 审批框默认 60 秒，一个稍大的文件再算几十秒哈希，
        // 一次正常发送就会被判成「时间戳偏差过大」。
        // 重放窗口仍是请求刚到时的 120 秒，不是确认之后再量一次。
        if needs_approval {
            if let Err(e) = Self::check_freshness(handshake.timestamp) {
                Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
                return Err(e);
            }
        }

        let approval_wait_ms: Option<u64> = if !needs_approval {
            // 永久信任 + 自动接收: 静默放行。签名与时效已在上面的
            // `Decision::Allow` 分支里验过, 这里不重复验。
            info!(
                "Incoming transfer from {} ({}) auto-accepted（永久信任, 零弹窗）",
                handshake.sender_name, peer_ip
            );
            None
        } else {
            info!(
                "Incoming transfer from {} ({}) requires approval",
                handshake.sender_name, peer_ip
            );

            let approval_id = uuid::Uuid::new_v4().to_string();
            // 「每次匹配码」等级：**本机（接收方）出码**并显示在审批窗口上，
            // 由发起方敲回来。方向论证见 `ApprovalManager::challenge_for`。
            //
            // 指纹让"带码重试"（一次全新连接）复用同一个码；否则用户在
            // 第一个窗口读到的码在第二个窗口上永远对不上。
            let grant_challenge = if requires_grant_code {
                let fp = format!(
                    "transfer|{}|{}|{}",
                    handshake.file_count,
                    handshake.total_size,
                    handshake.first_file_name
                );
                let code = approval_manager.challenge_for(&handshake.sender_id, &fp);
                approval_manager.bind(&approval_id, &code);
                code
            } else {
                String::new()
            };
            let app_req = ApprovalRequest {
                approval_id: approval_id.clone(),
                sender_id: handshake.sender_id.clone(),
                sender_name: handshake.sender_name.clone(),
                sender_ip: peer_ip.clone(),
                file_count: handshake.file_count,
                total_size: handshake.total_size,
                total_size_formatted: format_bytes(handshake.total_size),
                first_file_name: handshake.first_file_name.clone(),
                created_at: chrono::Utc::now().timestamp(),
                requires_grant_code,
                grant_challenge: grant_challenge.clone(),
            };

            // ⚠️ 这里**只构造** `app_req`，不发送。发送在下面的循环里、
            // 且必须在 `register` 之后 —— 理由见那里的注释。
            //
            // 早先的写法是"先 send、再在循环顶部 register"，两者之间有窗口：
            // UI 收到通知后**立刻**点「允许」是完全正常的（人比 IPC 快），
            // 而那一刻 `pending` 里还没有 oneshot sender，`resolve` 返回 false ——
            // 审批被**静默丢弃**，连接一直等到超时才被拒。
            // 表现是"点了允许，什么也没发生，过一分钟说传输失败"，
            // 而且**只在第一次配对时出现**（只有未配对设备才走审批）。

            // 审批等待单独计时：它必须被排除在"速度"之外（§9.6.3），
            // 否则"对方开会审批 60 秒"会被算成 185 KB/s 这种假慢。
            let approval_started = Instant::now();
            // 码输错时**重新弹窗**而不是直接拒绝。
            //
            // ## 为什么允许重试（这不是一个可以被暴力破解的口子）
            //
            // 重试的输入**只能来自人**：审批决策由 UI 提交，协议里
            // 没有"自动重试"这条通路。所以对端无法用 6 位码去撞 ——
            // 60 秒内人也不可能手敲 100 万种组合。
            //
            // 不重试的代价却很大：输错一位就得让对方重新发起、
            // 自己重新批准。而输错一位是极常见的操作。
            //
            // 上限 3 次：既给了改正空间, 又让"一直输不对"这件事
            // 最终有个明确的失败而不是无限等下去（审批超时是 60s，
            // 3 次交互也用不了多久）。
            const MAX_CODE_ATTEMPTS: u32 = 3;
            let mut code_attempt: u32 = 0;
            let mut last_mismatch_hint = String::new();
            let (action, _unused_grant_code) = loop {
                let rx = approval_manager.register(&approval_id);
                let remaining = approval_timeout
                    .saturating_sub(approval_started.elapsed());
                if remaining.is_zero() {
                    approval_manager.discard(&approval_id);
                    break (ApprovalAction::Reject, String::new());
                }
                // 「注册 → 通知 → 等待」三步在**同一次迭代**内完成，顺序不可交换。
                //
                // 先通知后注册会留下一个"UI 已经能点、但点了没人接"的窗口
                // （见上面 `app_req` 处的说明）；先注册后通知则不可能出现。
                // 浏览 / 取回走 `await_approval`，那边就是这个顺序 ——
                // 早先这两份实现在这一行恰好漂移了。
                //
                // 通知失败必须 fail-closed：无人能审批就绝不能放行。
                // 但报错文案不能说"前端未监听"：那是实现细节，对端用户看不懂，
                // 只会以为对方设备坏了。Android 端还没有审批界面，
                // 属于这种情况的典型场景。
                if approval_tx.send(app_req.clone()).is_err() {
                    approval_manager.discard(&approval_id);
                    let e = FeisuoError::Security(format!(
                        "对方暂不支持人工确认, 已拒绝来自 {} 的传输; 请先完成配对, 或让对方开启自动接收",
                        handshake.sender_name
                    ));
                    // 同样必须出站: "对方没有审批界面"是一条**对方用户
                    // 完全能靠自己解决**的问题(升级 / 开自动接收),
                    // 只写进对方日志等于把可执行的建议扔掉。
                    Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
                    return Err(e);
                }
                let decision =
                    match tokio::time::timeout(remaining, rx).await {
                        Ok(Ok(d)) => d,
                        Ok(Err(_)) => {
                            approval_manager.discard(&approval_id);
                            break (ApprovalAction::Reject, String::new());
                        }
                        Err(_) => {
                            // 超时后必须清理 pending 条目, 否则 map 无限增长
                            approval_manager.discard(&approval_id);
                            break (ApprovalAction::Reject, String::new());
                        }
                    };
                let act = decision.action;

                // ---- 匹配码核对（§2.3）----
                //
                // 顺序很重要: **先验签再比码**。反过来会让未验签的连接
                // 消耗对方的码校验次数, 并让"签名无效"这条更严重的
                // 问题被"码不对"掩盖。
                if requires_grant_code {
                    let pubkey = match trust_store
                        .get_device_pubkey(&handshake.sender_id)?
                    {
                        Some(pk) => pk,
                        None => {
                            let e = FeisuoError::Untrusted(handshake.sender_id.clone());
                            Self::refuse_transfer(
                                &mut stream,
                                &identity,
                                &config,
                                e.to_string(),
                            )
                            .await;
                            return Err(e);
                        }
                    };
                    if !DeviceIdentity::verify(
                        &pubkey,
                        challenge.as_bytes(),
                        &handshake.signature,
                    )? {
                        let e = FeisuoError::Security("握手签名校验失败".into());
                        Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
                        return Err(e);
                    }
                    // 时钟只在请求刚到时查过（本分支开头）。
                    // 这里再查会把「等人点确认 + 对端算哈希」算进 120 秒，
                    // 一次正常发送会被判成重放。

                    if act.is_allow() {
                        // 比的是**本进程生成、显示在本机窗口上的那个码**。
                        // 不能用 `decision.grant_code`（那是旧实现里
                        // "审批人替对端输入的码"，方向反了）。
                        let expected = approval_manager
                            .expected_for(&approval_id)
                            .unwrap_or_else(|| grant_challenge.clone());
                        if handshake.grant_code.trim().is_empty() {
                            // 必须**回一条带 requires_grant_code 的应答**,
                            // 不能只 `return Err`。
                            //
                            // 旧实现直接 return, 于是上层只打一条 warn 就
                            // 关闭连接 —— 发送方那边等不到任何应答, 得到的是
                            // "读取 握手应答 长度超时"。用户看到的是**网络故障**,
                            // 而真实原因是"你该把本机窗口上的 6 位码念给对方,
                            // 让对方输入后重试"。
                            // 协商回合被伪装成连接故障, 用户能做的只有重试,
                            // 于是永远卡死在这一步。
                            let _ = Self::reject_with_message(
                                &mut stream,
                                &identity,
                                &config,
                                &format!(
                                    "对方尚未出示本次匹配码; 请把这台设备窗口上的 {} 念给对方, 让对方输入后重试",
                                    format_grant_challenge_for_display(&expected)
                                ),
                                true,
                            )
                            .await;
                            return Err(FeisuoError::Security(
                                "对方未出示本次匹配码".into(),
                            ));
                        }
                        if !grant_code_eq(&expected, &handshake.grant_code) {
                            code_attempt += 1;
                            // **只记长度不记内容** —— 码只有 6 位,
                            // 落进数据库就等于把一个正在使用中的凭据存盘。
                            let _ = trust_store.add_security_event(
                                SecurityEventKind::GrantCodeMismatch,
                                &handshake.sender_id,
                                &handshake.sender_name,
                                &format!(
                                    "第 {} 次匹配码不匹配（对方出示 {} 位）",
                                    code_attempt,
                                    handshake.grant_code.trim().len()
                                ),
                            );
                            warn!(
                                "匹配码不匹配({}/{}): {} ({}); 同一窗口内可重输",
                                code_attempt,
                                MAX_CODE_ATTEMPTS,
                                handshake.sender_name,
                                peer_ip
                            );
                            if code_attempt >= MAX_CODE_ATTEMPTS {
                                // 同上: 必须回一条明确应答。
                                // 只 return Err 的话发送方看到的是超时,
                                // 分不清"对方输错了码"和"网络断了" ——
                                // 而这两种情况的用户动作完全不同。
                                let msg = format!(
                                    "匹配码连续 {} 次不匹配, 已拒绝本次传输; 请与对方重新核对",
                                    code_attempt
                                );
                                let _ = Self::reject_with_message(
                                    &mut stream,
                                    &identity,
                                    &config,
                                    &msg,
                                    true,
                                )
                                .await;
                                return Err(FeisuoError::Security(msg));
                            }
                            last_mismatch_hint = format!(
                                "对方输入的码不匹配（第 {} 次）。本机码不变，请让对方再核对一次。",
                                code_attempt
                            );
                            // 重新弹窗由**循环顶部**统一负责（沿用同一个
                            // approval_id 与**同一个码**）。
                            //
                            // 早先这里自己 `send` 一份 `retry_req`，于是
                            // "通知"有两处、"注册"只有一处 —— 重试路径上
                            // 又回到"先发后注册"的竞态：对方改完码重发时，
                            // 接收方窗口上可能已经能点、而 map 里还没有接收端。
                            // 少写一份构造代码，代价是这类漂移少一个来源。
                            continue;
                        }
                        info!(
                            "匹配码核对通过, 放行来自 {} ({}) 的本次传输",
                            handshake.sender_name, peer_ip
                        );
                        // §2.3.1：按用户选的档位决定要不要写短期授权。
                        // 传输这条路的审批循环是**独立**的（不走
                        // `await_approval`），所以这里必须自己调一次
                        // 那个共用落点 —— 副作用只写在一个函数里。
                        maybe_write_session_grant(
                            &trust_store,
                            &*config.read().await,
                            &handshake.sender_id,
                            &handshake.sender_name,
                            Op::Receive,
                            act,
                        );
                    }
                }
                // 第二返回值曾经是"审批人输入的码"，现已无意义（方向反了），
                // 统一传空串；下面所有 `let _ = &grant_code;` 只是为了
                // 明确标记"这里曾经有个码参数"，一并清掉。
                break (act, String::new());
            };
            let approval_wait_ms = Some(approval_started.elapsed().as_millis() as u64);
            if !last_mismatch_hint.is_empty() {
                info!(
                    "本次审批经历 {} 次码重试: {}",
                    code_attempt, last_mismatch_hint
                );
            }
            match action {
                ApprovalAction::AllowOnce => {
                    // 只放行本次, **不写入长期信任**（§2.4）。
                    // 旧实现这里直接 bind_device(is_trusted=true), 等于把
                    // "这一次允许"静默升级成"永久免密", 用户没有选择权。
                    info!(
                        "Incoming transfer from {} ({}) approved ONCE by user",
                        handshake.sender_name, peer_ip
                    );
                }
                ApprovalAction::AllowWithGrant => {
                    // 与 `AllowOnce` **同样不写长期信任** —— 差别只有
                    // `maybe_write_session_grant` 已经写下了一张
                    // **同类操作、短期、可见可撤销**的授权（§2.3.1）。
                    //
                    // 刻意不把它并进 `AllowOnce` 的分支：那个分支的
                    // 语义是"只这一次"，而这一档会留下状态。并进去
                    // 就等于让 `AllowOnce` 悄悄有了副作用。
                    info!(
                        "Incoming transfer from {} ({}) approved ONCE + short grant",
                        handshake.sender_name, peer_ip
                    );
                }
                ApprovalAction::AllowAndTrust => {
                    info!(
                        "Incoming transfer from {} ({}) approved AND TRUSTED by user",
                        handshake.sender_name, peer_ip
                    );
                    let dev = crate::security::TrustedDevice {
                        device_id: handshake.sender_id.clone(),
                        device_name: if handshake.sender_name.trim().is_empty() {
                            "未知设备".to_string()
                        } else {
                            handshake.sender_name.trim().to_string()
                        },
                        public_key_hex: handshake.sender_public_key_hex.clone(),
                        last_ip: peer_ip.clone(),
                        bound_at: chrono::Utc::now().to_rfc3339(),
                        is_trusted: true,
                        trust_level: TrustLevel::Permanent,
                        visible: true,
                        last_seen_at: chrono::Utc::now().timestamp(),
                        // 审批里写入长期信任时**不生成**新世代：
                        // 那会让本地世代与对端脱钩, 双向解除配对随之失效。
                        // 只有真正的配对握手才协商世代。
                        pairing_epoch: trust_store
                            .pairing_epoch(&handshake.sender_id)?
                            .unwrap_or_default(),
                    };
                    match trust_store.bind_device(&dev) {
                        Ok(BindOutcome::Added) => {
                            info!("已建立长期信任 {}", dev.device_id)
                        }
                        Ok(BindOutcome::Refreshed) => {
                            info!("已刷新受信设备元数据 {}", dev.device_id)
                        }
                        Ok(BindOutcome::KeyConflict) => {
                            // 公钥与既有绑定不一致: 绝不覆盖, 本次仅放行一次。
                            // bind_device 内部已记安全事件(§3.9), 这里只告警。
                            warn!(
                                "设备 {} 提交了与既有绑定不同的公钥, 仅放行本次传输, 不写入信任库",
                                dev.device_id
                            );
                        }
                        Err(e) => tracing::warn!("写入受信条目失败: {}", e),
                    }
                }
                ApprovalAction::Reject => {
                    let _ = Self::reject_with_message(
                        &mut stream,
                        &identity,
                        &config,
                        "传输已被对方拒绝",
                        // 用户主动拒绝 => **不是**"缺码"，
                        // 别让发送方以为还能再试一次
                        false,
                    )
                    .await;
                    return Err(FeisuoError::Security("传输已被对方拒绝".into()));
                }
                }

            // 审批通过: 验签 + 时效检查。
            // 用库中公钥（已绑定）或对端自证公钥（首次绑定）——
            // 设备 id 与公钥的绑定关系上面已校验过, 因此自证公钥是可信的。
            let verify_pubkey = trust_store
                .get_device_pubkey(&handshake.sender_id)?
                .unwrap_or_else(|| handshake.sender_public_key_hex.clone());
            if !DeviceIdentity::verify(&verify_pubkey, challenge.as_bytes(), &handshake.signature)?
            {
                // 审批已经通过，但签名不过。只 `return Err` 的话发送方
                // 在读握手应答时撞上连接关闭，界面上像是网络断了，
                // 而真实原因是身份对不上 —— 重试也不会好。
                let e = FeisuoError::Security("握手签名校验失败".into());
                Self::refuse_transfer(&mut stream, &identity, &config, e.to_string()).await;
                return Err(e);
            }
            // 时钟在进审批之前查（见下方）。这里不再查：
            // 审批框默认 60 秒，发送方还要在握手之后算整文件哈希，
            // 120 秒的窗口装不下这两段合法等待。
            approval_wait_ms
        };

        // ---- 落点子目录归一 + 越界拦截（§7.7 / §2.4）----
        //
        // 必须在**握手应答之前**做完，理由是"越界要明确拒绝"而不是
        // "先建立连接再失败"：发送方在 `GrantCodeRequired` 那一回合会
        // 重试整条握手，落点非法时每一回合都该立刻拿到明确拒绝，
        // 而不是让它连上再在后面某处失败。
        //
        // 校验与取回方向（`handle_pull`）**完全相同**：
        //   normalize_sub_path 挡 `..` / 绝对路径 / 盘符 / 隐藏目录 / 超深
        //   → is_within(receive_dir) 兜住 junction / 符号链接
        // 外加 `authorize(Op::Receive)` 已经查过的强制敏感路径排除清单。
        //
        // **它不能被用来写到收件目录之外** —— 那是 §2.4 给「允许写」
        // 定的语义（允许写，落点强制在收件目录）。UI 侧因此只在对方
        // 地址栏位于**收件目录镜像**里时才传值；浏览对方真实卷时不传。
        let dest_prefix = match Self::normalize_sub_path(&handshake.dest_sub_path) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("落点子目录非法, 已拒绝: {}", e);
                let _ = Self::reject_with_message(&mut stream, &identity, &config, &msg, false)
                    .await;
                return Err(FeisuoError::Security(msg));
            }
        };
        let dest_dir = if dest_prefix.is_empty() {
            config.read().await.receive_dir.clone()
        } else {
            let base = config.read().await.receive_dir.clone();
            let target = base.join(&dest_prefix);
            // ⚠️ 校验必须在**建目录之前**。
            //
            // `is_within` 内部的 `canonicalize_lenient` 对不存在的路径会
            // 逐级上溯到最深的已存在祖先、再把剩余段接回去，所以它
            // **不要求目标存在**（storage::paths 的
            // `within_true_for_nonexistent_child` 锁住了这个语义）。
            //
            // 原先这里先 `create_dir_all(&target)` 再校验，理由是"这样
            // canonicalize 才有东西可解析"——那是在给一个不需要前提的
            // 检查硬造前提。代价是**逃逸的落点在校验失败时已经被建出来了**：
            // 落点被拒，可那个目录真的出现在了收件目录之外。
            // 这条守卫本该是纵深防御，却自己在攻击面留了痕迹。
            if !crate::storage::is_within(&target, &base) {
                let msg = format!(
                    "落点子目录逃逸出收件目录, 已拒绝: {}",
                    handshake.dest_sub_path
                );
                let _ = Self::reject_with_message(&mut stream, &identity, &config, &msg, false)
                    .await;
                return Err(FeisuoError::Security(msg));
            }
            // 建目录交给真正的落盘阶段（`dest_dir` 由写块时按需创建）。
            // 这里是**握手**阶段：此刻还不能确定这批文件真会落下来
            // （审批可能拒绝、断点续传可能全部跳过），
            // 先建目录等于给一次"什么都不会发生"的传输留下空壳。
            target
        };

        // 5. 下发握手应答 (携带接收方公钥 + 对握手挑战的签名)
        let receiver_name = config.read().await.device_name.clone();
        let responder_challenge = format!("{}:{}", handshake.nonce, handshake.timestamp);
        let resp = HandshakeResponse {
            success: true,
            receiver_id: identity.device_id.clone(),
            receiver_name: receiver_name.clone(),
            message: "OK".into(),
            receiver_public_key_hex: identity.public_key_hex(),
            receiver_signature: identity.sign(responder_challenge.as_bytes()),
            // 断点续传靠这个位协商: 发送方看到它才读清单应答。
            caps: crate::protocol::local_caps(),
            // 已经过了审批阶段, 说明码核对通过（或不需要码）
            requires_grant_code: false,
        };
        write_json_frame(&mut stream, &resp).await?;

        // ---- 诊断建档（§9.6 / §9.7）----
        // 建档点选在"握手应答已发出"这一刻: 此刻握手（含人工审批等待）
        // 刚好结束, 后面读清单的时间可以单独归到 manifest 阶段。
        // 关键: **审批等待绝不能混进数据流时间**, 否则"对方开会审批 60 秒"
        // 会被算成 185 KB/s 这种假慢（§9.6.3）。
        let mut diag = crate::transport::TransferDiagnostics::start(
            uuid::Uuid::new_v4().to_string(),
            "recv",
            &handshake.sender_id,
            &handshake.sender_name,
            &peer_ip,
            0,
        );
        diag.mark_hello(conn_started.elapsed().as_millis() as u64);
        // 接收侧没有独立的"建连"阶段, 用握手总耗时近似链路延迟上界
        let recv_connect_ms = conn_started.elapsed().as_millis() as u64;
        diag.mark_connect(&local_ip, recv_connect_ms);
        // 显式 socket buffer（§9.2）。
        //
        // **接收侧同样必须设** —— 有效吞吐由收发两侧 buffer 的**较小**者决定,
        // 只调发送侧等于没调。放在握手之后而不是 accept 处, 是因为这里才有
        // RTT 代理值（accept 立即返回, 拿不到任何链路信息）。
        //
        // 握手字节量只有几百字节, 此时改 buffer 不会丢数据;
        // 真正的数据流阶段还没开始。
        //
        // `bdp_est` / `bdp_regrow_tried` 在此声明（而不是提前给默认值）:
        // 提前 `Default::default()` 会让初始值在覆盖前从未被读到 ——
        // 编译器会如实报"死赋值"。真实值只能来自 tune_and_log。
        let mut bdp_est = {
            let local_ip_for_buf = local_ip.clone();
            let (tuned, est) = crate::transport::sockopt::tune_and_log(
                stream,
                &local_ip_for_buf,
                &peer_ip,
                recv_connect_ms,
            )?;
            stream = tuned;
            diag.set_bdp(&est);
            est
        };
        // 传完 64MB 后按**实测**带宽二次调大 —— 建连时两端都还不知道带宽,
        // 只按 RTT 猜; 而有效吞吐取收发两侧 buffer 的较小者,
        // 所以发送侧与接收侧必须**同时**调, 只调一侧等于没调。
        let mut bdp_regrow_tried = false;
        if let Some(wait_ms) = approval_wait_ms {
            diag.mark_approval_wait(wait_ms);
        }

        // 6. 读清单并做完整校验
        let manifest_started = Instant::now();
        let manifest: TransferManifest = read_json_frame(&mut stream, "传输清单").await?;
        if let Err(e) = Self::validate_manifest(&manifest, &handshake, &trust_store) {
            // 发送方此刻正在等清单应答。只关连接，它看到的是
            // 「对方已关闭连接」，分不清是清单被拒还是网络断了。
            // 签名不过、时钟偏差、分块对不上，都该把原因写进这条应答。
            let ack = ManifestAck {
                success: false,
                receiver_id: identity.device_id.clone(),
                receiver_name: config.read().await.device_name.clone(),
                message: e.to_string(),
                completed: Vec::new(),
                needed: Vec::new(),
                dest_paths: Vec::new(),
                file_resumes: Vec::new(),
            };
            let _ = write_json_frame(&mut stream, &ack).await;
            return Err(e);
        }
        diag.mark_manifest(manifest_started.elapsed().as_millis() as u64);
        // 清单到手后补齐诊断的传输要素（transfer_id 与发送侧记录对齐）
        diag.transfer_id = manifest.transfer_id.clone();
        // 只丢掉**早于本次连接**的旧撤销。
        //
        // `transfer_id` 是内容派生的，同一批文件重试是同一个 id。
        // 上次的撤销如果还留着，会让这次「明明重发了却被说已撤销」。
        //
        // 但不能无条件删：取消帧经常比清单更早到达（用户在哈希 / 握手时
        // 就点了撤销）。那种标记的时刻**晚于** `conn_started`，必须留下，
        // 否则这次传输看不到撤销，文件照样落盘。
        if let Ok(mut s) = cancelled.lock() {
            if let Some(at) = s.get(&manifest.transfer_id).copied() {
                if at < conn_started {
                    s.remove(&manifest.transfer_id);
                }
            }
        }
        diag.set_manifest_info(
            manifest.files.len() as u32,
            manifest.total_size,
            manifest.chunk_size,
            manifest.files.iter().map(|f| f.chunk_count).sum(),
        );

        // 落点基准 = 握手里已校验过的 `dest_dir`（收件根 或 收件根下的子目录）。
        //
        // 为什么用 `dest_dir` 而不是重新读配置：握手阶段已经把
        // `dest_sub_path` 归一化、并确认仍在收件目录之内。
        // 这里再读一次配置会**丢掉那个子目录**，而且多一次读锁。
        //
        // 它此时**可能还不存在**（握手不再提前建目录，见上面那段注释）：
        // 真正创建发生在下面的落盘路径里，由写块按需建 —— 审批可能拒绝、
        // 断点续传可能全部跳过，提前建只会留下一批空目录。
        let receive_base_dir = dest_dir;

        // ---- 6.5 断点续传：算出"这次真的还要传哪些"（P1 ⑪）----
        // 复用处理清单的这一轮往返回话, 不额外问一次 ——
        // 在 RTT 200ms 的链路上, 多一轮就是 200ms。
        let resume = Self::plan_resume(&manifest, &trust_store);
        let ack = ManifestAck {
            success: true,
            receiver_id: identity.device_id.clone(),
            receiver_name: config.read().await.device_name.clone(),
            message: if resume.completed.is_empty() {
                "OK".into()
            } else {
                format!(
                    "已跳过 {} 个此前已完整接收的文件, 只需补传 {} 个",
                    resume.completed.len(),
                    resume.needed.len()
                )
            },
            completed: resume.completed.clone(),
            needed: resume.needed.clone(),
            dest_paths: resume.dest_paths.clone(),
            file_resumes: resume.file_resumes.clone(),
        };
        write_json_frame(&mut stream, &ack).await?;
        if !resume.completed.is_empty() {
            info!(
                "断点续传: transfer={} 已有 {} 个文件, 本次只收 {} 个",
                manifest.transfer_id,
                resume.completed.len(),
                resume.needed.len()
            );
        }
        // 收完只剩 0 个文件的情况: 全部都已存在, 直接回成功。
        // 不这么做的话接收端会等一个永远不会来的分块, 最终超时失败 ——
        // 用户看到的是"明明文件都在却说传输失败"。
        if resume.needed.is_empty() {
            diag.mark_data(0);
            diag.mark_verify(0);
            diag.mark_commit(0);
            diag.mark_ack(0);
            diag.integrity_ok = true;
            let _ = progress_tx.send(TransferProgress {
                transfer_id: manifest.transfer_id.clone(),
                direction: TransferDirection::Receive,
                peer_device_id: manifest.sender_id.clone(),
                peer_device_name: handshake.sender_name.clone(),
                current_file: "全部已存在".to_string(),
                file_index: 0,
                total_files: manifest.files.len() as u32,
                bytes_transferred: 0,
                total_bytes: 0,
                progress_percent: 100.0,
                speed_bytes_per_sec: 0,
                status: TransferStatus::Completed,
            });
            let ok = TransferAck {
                success: true,
                error_msg: None,
                transfer_id: manifest.transfer_id.clone(),
                file_index: 0,
                chunk_index: 0,
            };
            write_json_frame(&mut stream, &ok).await?;
            diag.finish("completed", None);
            return Ok(());
        }

        info!(
            "Receiving batch transfer: {} ({} new / {} total files, {} bytes to send)",
            manifest.transfer_id,
            resume.needed.len(),
            manifest.files.len(),
            resume.send_bytes
        );

        let started_at = Instant::now();
        let mut total_transferred = 0u64;
        // 失败也要进传输记录，而且必须用用户设的保留条数。
        // 写死 500 的话，用户把上限调高之后，一次失败就会把更早的记录删掉。
        let history_limits = {
            let cfg = config.read().await;
            (cfg.max_history_records, cfg.record_retention_days)
        };
        // 整文件校验累计耗时（不计入速度, §9.6.4）
        let mut verify_total_ms: u64 = 0;

        // 7. 逐文件逐分块落盘
        let mut last_chunk_at = Instant::now();
        // ---- 暂存区 → 提交（§5.2）----
        // 先写 receive_dir/.feisuo-incoming/<transfer_id>/，全部校验通过后
        // 再 rename 到最终位置。同卷内 rename 是原子的。
        //
        // 这样做的两个理由：
        // 1. **撤销彻底**：删除 staging 目录即可，磁盘上不留任何痕迹
        //    （旧实现直接写最终路径，中断会留下半成品文件）；
        // 2. 目录本身在收件目录内部 ⇒ 同卷 ⇒ rename 一定成功，不会
        //    触发跨卷复制。放在临时目录（如 %TEMP%）会跨卷，rename 失败。
        let staging_root = receive_base_dir.join(".feisuo-incoming").join(&manifest.transfer_id);
        if let Err(e) = tokio::task::spawn_blocking({
            let d = staging_root.clone();
            move || std::fs::create_dir_all(&d)
        })
        .await
        .map_err(|e| FeisuoError::Internal(format!("创建暂存目录任务异常: {}", e)))?
        {
            let e = FeisuoError::IoContext {
                context: format!("创建暂存目录失败: {}", staging_root.display()),
                source: e,
            };
            return Err(Self::receive_failure(
                &progress_tx,
                &trust_store,
                history_limits,
                &peer_ip,
                &manifest,
                &handshake.sender_name,
                "-",
                total_transferred,
                std::path::Path::new(""),
                &e.to_string(),
                e,
            ));
        }
        // (staged 路径, 最终路径, **清单原始下标**)
        //
        // ⚠️ 必须带原始下标。断点续传让 `staged_paths` 只装"要传的那几个",
        // 于是它的**位置**与 `manifest.files` 的下标不再一一对应 ——
        // 按位置取 FileMeta 会拿到错误的相对路径, 把文件提交到别人的位置上,
        // 而整文件校验仍然通过（因为 hash 是对的）。这类 bug 极难发现。
        let mut staged_paths: Vec<(std::path::PathBuf, std::path::PathBuf, u32)> =
            Vec::with_capacity(manifest.files.len());
        // 暂存区守卫：无论走哪条失败路径, 离开本函数时把 staging 目录清空。
        // 旧实现直接写最终路径, 中断会在用户目录里留下截断的半成品文件,
        // 而下一次重传又会生成 "name (1).ext" —— 残缺的那份永久留着（§5.2）。
        let mut staging_guard = StagingGuard {
            root: staging_root.clone(),
            armed: true,
            keep_for_resume: false,
        };

        // 只收接收方点名要的文件（断点续传）。`needed` 存的是**清单原始下标**,
        // 不能重编号 —— 发送方在分块头里带的是原始 `file_index`,
        // 重编号会让落盘路径与 FileMeta 对不上, 而分块校验仍会通过。
        let needed: std::collections::HashSet<u32> = resume.needed.iter().copied().collect();
        for file in &manifest.files {
            if !needed.contains(&file.file_index) {
                continue;
            }
            // relative_path 允许多级 ("2026/报表/1月.csv"): 穿梭取回要保留
            // 对端目录结构。逐段穿越校验在 validate_relative_subpath 内完成。
            PathManager::validate_relative_subpath(&file.relative_path)?;
            let file_name = file.relative_path.replace('\\', "/");

            let chunk_resume = resume.file_resumes.iter().find(|r| r.file_index == file.file_index);
            let start_chunk_idx = chunk_resume.map(|r| r.next_chunk_index).unwrap_or(0);
            let resumed_bytes = chunk_resume.map(|r| r.bytes_resumed).unwrap_or(0);

            // 如果该文件已有暂存断点且文件还在，则直接复用已有暂存文件路径；否则新分配占位
            let staged_path = if start_chunk_idx > 0 {
                if let Ok(Some(rec)) = trust_store.get_chunk_progress(&manifest.transfer_id, file.file_index) {
                    let existing_p = std::path::PathBuf::from(rec.staged_path);
                    if existing_p.is_file() {
                        existing_p
                    } else {
                        PathManager::resolve_unique_subpath(&staging_root, &file_name)?
                    }
                } else {
                    PathManager::resolve_unique_subpath(&staging_root, &file_name)?
                }
            } else {
                PathManager::resolve_unique_subpath(&staging_root, &file_name)?
            };

            info!("Writing to staging: {:?} (从分块 {} / 字节 {} 开始续收)", staged_path, start_chunk_idx, resumed_bytes);
            staged_paths.push((staged_path.clone(), std::path::PathBuf::new(), file.file_index));

            // 仅在从头开始 (start_chunk_idx == 0) 或文件尚未存在时预分配文件长度
            if start_chunk_idx == 0 || !staged_path.exists() {
                let prepare_res = {
                    let p = staged_path.clone();
                    let size = file.file_size;
                    tokio::task::spawn_blocking(move || ChunkStore::prepare_file(&p, size))
                        .await
                        .map_err(|e| FeisuoError::Internal(format!("建文件任务异常: {}", e)))?
                };
                if let Err(e) = prepare_res {
                    return Err(Self::receive_failure(
                        &progress_tx,
                        &trust_store,
                        history_limits,
                        &peer_ip,
                        &manifest,
                        &handshake.sender_name,
                        &file.relative_path,
                        total_transferred,
                        &staged_path,
                        &e.to_string(),
                        e,
                    ));
                }
            }

            // 整个文件只开一次写句柄（§9.3 第 2 行）。旧实现每块
            // `OpenOptions::open` 一次 —— 4 GiB 文件就是 1024 次 open + 1024 次 seek。
            let writer = match tokio::task::spawn_blocking({
                let p = staged_path.clone();
                move || crate::storage::ChunkWriter::open(&p, CHUNK_SIZE)
            })
            .await
            .map_err(|e| FeisuoError::Internal(format!("打开写句柄任务异常: {}", e)))? {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    return Err(Self::receive_failure(
                        &progress_tx,
                        &trust_store,
                        history_limits,
                        &peer_ip,
                        &manifest,
                        &handshake.sender_name,
                        &file.relative_path,
                        total_transferred,
                        &staged_path,
                        &format!("打开目标文件失败: {}", e),
                        e,
                    ))
                }
            };

            let mut file_transferred = resumed_bytes;
            for chunk_idx in (start_chunk_idx as u64)..file.chunk_count {
                // 读分块头**之前**先看撤销。发送方点撤销后不再写下一块，
                // 若这里先 `read_json_frame`，会干等 30 秒直到超时，
                // 界面上像是网络断了，而不是「已撤销」。
                if Self::stop_if_cancelled(
                    &cancelled,
                    &mut stream,
                    &progress_tx,
                    &manifest,
                    &handshake.sender_name,
                    &file.relative_path,
                    file.file_index,
                    chunk_idx,
                    total_transferred,
                )
                .await?
                {
                    let _ = trust_store.remove_all_chunk_progress(&manifest.transfer_id);
                    Self::record_receive_outcome(
                        &trust_store,
                        history_limits,
                        &peer_ip,
                        &manifest,
                        &handshake.sender_name,
                        &file.relative_path,
                        total_transferred,
                        "cancelled",
                    );
                    return Err(FeisuoError::Cancelled);
                }
                let chunk_header: ChunkHeader = match read_json_frame(&mut stream, "分块头").await {
                    Ok(h) => h,
                    Err(e) => {
                        return Err(Self::receive_failure(
                            &progress_tx,
                            &trust_store,
                            history_limits,
                            &peer_ip,
                            &manifest,
                            &handshake.sender_name,
                            &file.relative_path,
                            total_transferred,
                            &staged_path,
                            &e.to_string(),
                            e,
                        ))
                    }
                };

                // 分块头必须与清单严格对位, 乱序/重复/跳号一律拒绝
                if chunk_header.transfer_id != manifest.transfer_id
                    || chunk_header.file_index != file.file_index
                    || chunk_header.chunk_index != chunk_idx
                {
                    let e = FeisuoError::Security(format!(
                        "分块 {} 顺序异常 (期望 file={} chunk={}, 实际 file={} chunk={})",
                        chunk_idx,
                        file.file_index,
                        chunk_idx,
                        chunk_header.file_index,
                        chunk_header.chunk_index
                    ));
                    return Err(Self::receive_failure(
                        &progress_tx,
                        &trust_store,
                        history_limits,
                        &peer_ip,
                        &manifest,
                        &handshake.sender_name,
                        &file.relative_path,
                        total_transferred,
                        &staged_path,
                        &e.to_string(),
                        e,
                    ));
                }

                if chunk_header.data_length as usize > CHUNK_SIZE {
                    let e = FeisuoError::Security(format!(
                        "分块 {} 长度 {} 超过分块上限",
                        chunk_idx, chunk_header.data_length
                    ));
                    return Err(Self::receive_failure(
                        &progress_tx,
                        &trust_store,
                        history_limits,
                        &peer_ip,
                        &manifest,
                        &handshake.sender_name,
                        &file.relative_path,
                        total_transferred,
                        &staged_path,
                        &e.to_string(),
                        e,
                    ));
                }

                // 读进 Arc 共享的 buffer, 避免为了交给 spawn_blocking 再 clone 4 MiB
                let chunk_data: Arc<Vec<u8>> = if chunk_header.data_length == 0 {
                    Arc::new(Vec::new())
                } else {
                    let mut buf = vec![0u8; chunk_header.data_length as usize];
                    let read_result = tokio::time::timeout(
                        Duration::from_secs(IO_TIMEOUT_SECS),
                        stream.read_exact(&mut buf),
                    )
                    .await;

                    let read_err = match read_result {
                        Ok(Ok(_)) => None,
                        // 这里会进 `receive_failure` 并作为失败原因上报到 UI,
                        // 所以标签不是给日志看的, 是给**用户**看的:
                        // 裸 `"early eof"` 会让他以为本机磁盘出问题。
                        Ok(Err(io)) => Some(super::io_error_with("接收分块数据", io)),
                        Err(_) => Some(FeisuoError::Network("读取分块数据超时".into())),
                    };
                    if let Some(e) = read_err {
                        return Err(Self::receive_failure(
                            &progress_tx,
                            &trust_store,
                            history_limits,
                            &peer_ip,
                            &manifest,
                            &handshake.sender_name,
                            &file.relative_path,
                            total_transferred,
                            &staged_path,
                            &e.to_string(),
                            e,
                        ));
                    }
                    Arc::new(buf)
                };
                let chunk_data_arc = chunk_data.clone();

                // BLAKE3 分块校验
                let actual_hash = ChunkStore::hash_chunk(&chunk_data);
                if actual_hash != chunk_header.chunk_blake3 {
                    let e = FeisuoError::ChecksumMismatch {
                        chunk_index: chunk_idx,
                        expected: chunk_header.chunk_blake3,
                        actual: actual_hash,
                    };
                    return Err(Self::receive_failure(
                        &progress_tx,
                        &trust_store,
                        history_limits,
                        &peer_ip,
                        &manifest,
                        &handshake.sender_name,
                        &file.relative_path,
                        total_transferred,
                        &staged_path,
                        &e.to_string(),
                        e,
                    ));
                }

                // 单文件 / 总体积上限: 防止对端无视清单无限灌数据撑爆磁盘
                file_transferred += chunk_data.len() as u64;
                total_transferred += chunk_data.len() as u64;
                if file_transferred > file.file_size || total_transferred > manifest.total_size {
                    let e = FeisuoError::Security(format!(
                        "接收数据超出清单声明的体积 (文件 {} 收 {} / 声明 {}, 累计 {} / {})",
                        file.relative_path,
                        file_transferred,
                        file.file_size,
                        total_transferred,
                        manifest.total_size
                    ));
                    return Err(Self::receive_failure(
                        &progress_tx,
                        &trust_store,
                        history_limits,
                        &peer_ip,
                        &manifest,
                        &handshake.sender_name,
                        &file.relative_path,
                        total_transferred,
                        &staged_path,
                        &e.to_string(),
                        e,
                    ));
                }

                // 4 MiB 同步落盘: 同上, 不能占住 runtime worker。
                // 整个文件只开一次写句柄（§9.3 第 2 行）, 旧实现每块重开。
                // data 用 Arc 共享, 避免每块再 clone 一份 4 MiB（§9.3 第 3 行）。
                let write_res = {
                    let writer = writer.clone();
                    let data = chunk_data_arc.clone();
                    tokio::task::spawn_blocking(move || writer.write_chunk(chunk_idx, &data))
                        .await
                        .map_err(|e| FeisuoError::Internal(format!("写分块任务异常: {}", e)))?
                };
                if let Err(e) = write_res {
                    return Err(Self::receive_failure(
                        &progress_tx,
                        &trust_store,
                        history_limits,
                        &peer_ip,
                        &manifest,
                        &handshake.sender_name,
                        &file.relative_path,
                        total_transferred,
                        &staged_path,
                        &e.to_string(),
                        e,
                    ));
                }

                // 实时持久化分块进度，确保任何时刻断线都能从当前分块接续
                let next_chunk = (chunk_idx + 1) as u32;
                let _ = trust_store.record_chunk_progress(
                    &manifest.transfer_id,
                    file.file_index,
                    &file.relative_path,
                    &file.blake3_hash,
                    file.file_size,
                    &staged_path.to_string_lossy(),
                    next_chunk,
                    file_transferred,
                );

                let elapsed = started_at.elapsed().as_secs_f64().max(0.001);
                let speed = (total_transferred as f64 / elapsed) as u64;
                // 逐块采样吞吐 + 记录停顿（区分"链路慢"与"磁盘写阻塞"）
                diag.sample_throughput(
                    chunk_data.len() as u64,
                    last_chunk_at.elapsed().as_millis() as u64,
                );
                last_chunk_at = Instant::now();
                let progress = TransferProgress {
                    transfer_id: manifest.transfer_id.clone(),
                    direction: TransferDirection::Receive,
                    peer_device_id: manifest.sender_id.clone(),
                    peer_device_name: handshake.sender_name.clone(),
                    current_file: file.relative_path.clone(),
                    file_index: file.file_index,
                    total_files: manifest.files.len() as u32,
                    bytes_transferred: total_transferred,
                    total_bytes: manifest.total_size,
                    // 旧实现直接做除法: total_size == 0 时得到 NaN,
                    // serde_json 会写成 null, 前端 f32 反序列化从此每条事件都抛错。
                    progress_percent: if manifest.total_size == 0 {
                        100.0
                    } else {
                        (total_transferred as f64 / manifest.total_size as f64 * 100.0).min(100.0)
                            as f32
                    },
                    speed_bytes_per_sec: speed,
                    status: TransferStatus::Transferring,
                };
                if progress_tx.send(progress).is_err() {
                    // 无订阅者不是致命错误, 但必须可观测
                    tracing::debug!("transfer-progress 通道已关闭 (接收侧无订阅者)");
                }

                // ---- 接收侧二次调大 socket buffer（§9.2）----
                //
                // **必须与发送侧同时做**：有效吞吐由收发 buffer 的较小者决定,
                // 只调一侧等于没调 —— 发送侧调到 2MB、接收侧仍是 256KB 时,
                // 吞吐依然被 256KB 卡住, 而日志会显示"已调优"（最坏情况:
                // 日志与事实相反）。
                //
                // 换手柄只能发生在**一块读完之后**: 中途替换会丢掉内核
                // 接收队列里已到达但尚未交给应用层的字节。
                if !bdp_regrow_tried && total_transferred >= 64 * 1024 * 1024 {
                    bdp_regrow_tried = true;
                    // **先打标记再调用**：regrow_stream 可能因为
                    // "现有 buffer 已经覆盖 BDP" 而返回 None ——
                    // 那是**正确决策**，不是"什么都没发生"。
                    // 不打标记的话，事后数据里"没触发"与
                    // "触发了但判定不需要"长得一模一样。
                    diag.mark_bdp_regrow_tried();
                    let rtt_ms = crate::transport::sockopt::rtt_from_profile(&diag.link)
                        .max(recv_connect_ms / 2)
                        .max(1);
                    let measured = diag.throughput.avg_bps();
                    let (new_stream, new_est) = crate::transport::sockopt::regrow_stream(
                        stream,
                        &bdp_est,
                        rtt_ms,
                        measured,
                    )?;
                    stream = new_stream;
                    if let Some(est) = new_est {
                        bdp_est = est;
                        diag.set_bdp(&bdp_est);
                        diag.mark_bdp_regrown();
                    }
                }
            }

            // 落盘后立刻校验整文件哈希: 截断/损坏的文件绝不能被当作成功。
            // 整文件哈希是纯 CPU + 磁盘读, 大文件可达数秒, 同样必须离开 worker。
            // ⚠️ 这段时间单独计入 `verify_ms`, **不得**并入数据流时间:
            //    BLAKE3 是 GB/s 级, 算进"网络速度"会凭空制造假慢（§9.6.4）。
            let verify_started = Instant::now();
            // 收完该文件: **仅此一次** sync_all（§9.3 每文件 2 次 -> 1 次）
            if let Err(e) = writer.finalize() {
                warn!("落盘 sync 失败 {}: {}", file.relative_path, e);
            }
            let verify_path = staged_path.clone();
            let actual_file_hash: String = tokio::task::spawn_blocking(move || {
                ChunkStore::hash_file(&verify_path)
            })
            .await
            .map_err(|e| FeisuoError::Internal(format!("校验任务异常: {}", e)))??;
            verify_total_ms += verify_started.elapsed().as_millis() as u64;
            if actual_file_hash != file.blake3_hash {
                let msg = format!(
                    "文件 {} 整文件校验失败 (期望 {}, 实际 {})",
                    file.relative_path, file.blake3_hash, actual_file_hash
                );
                let e = FeisuoError::ChecksumMismatch {
                    chunk_index: file.chunk_count,
                    expected: file.blake3_hash.clone(),
                    actual: actual_file_hash,
                };
                return Err(Self::receive_failure(
                    &progress_tx,
                    &trust_store,
                    history_limits,
                    &peer_ip,
                    &manifest,
                    &handshake.sender_name,
                    &file.relative_path,
                    total_transferred,
                    &staged_path,
                    &msg,
                    e,
                ));
            }

            // 该文件所有分块已完整且整文件 BLAKE3 校验通过，清理其暂存分块进度
            let _ = trust_store.remove_chunk_progress(&manifest.transfer_id, file.file_index);
        }

        // 提交前再看一次撤销。校验整文件哈希可能要几秒，
        // 撤销请求经常就落在这个窗口里。若此时已经开始 rename，
        // 文件就进了收件目录，而界面还说「已撤销」。
        if Self::stop_if_cancelled(
            &cancelled,
            &mut stream,
            &progress_tx,
            &manifest,
            &handshake.sender_name,
            "-",
            0,
            0,
            total_transferred,
        )
        .await?
        {
            Self::record_receive_outcome(
                &trust_store,
                history_limits,
                &peer_ip,
                &manifest,
                &handshake.sender_name,
                "-",
                total_transferred,
                "cancelled",
            );
            return Err(FeisuoError::Cancelled);
        }

        // ---- 8.5 提交：暂存 → 最终位置（§5.2）----
        // 走到这里说明**每个文件都已通过整文件 BLAKE3 复核**，
        // 现在才计算最终路径（此时才需要"避免同名覆盖"）并同卷 rename。
        //
        // rename 失败（极少见：例如目标被另一个程序独占）会退化为
        // "复制 + 删除暂存"，保证数据不丢。
        let commit_started = Instant::now();
        for idx in 0..staged_paths.len() {
            let staged: std::path::PathBuf = staged_paths[idx].0.clone();
            // 清单**原始下标**, 不是位置（见 staged_paths 的注释）
            let file_index = staged_paths[idx].2;
            let rel = &manifest.files
                [file_index as usize]
                .relative_path;
            let final_path = match PathManager::resolve_unique_subpath(&receive_base_dir, rel) {
                Ok(p) => p,
                Err(e) => {
                    return Err(Self::receive_failure(
                        &progress_tx,
                        &trust_store,
                        history_limits,
                        &peer_ip,
                        &manifest,
                        &handshake.sender_name,
                        rel,
                        total_transferred,
                        &staged,
                        &format!("解析最终落盘路径失败: {}", e),
                        e,
                    ));
                }
            };
            // Windows 的 rename 不能覆盖刚占好的空文件。
            // `commit_staged_file` 会替换它；真失败时把空占位删掉，
            // 不在收件目录里留一个 0 字节文件。
            let commit_res = tokio::task::spawn_blocking({
                let from = staged.clone();
                let to = final_path.clone();
                move || crate::storage::commit_staged_file(&from, &to)
            })
            .await
            .map_err(|e| FeisuoError::Internal(format!("提交任务异常: {}", e)))?;
            if let Err(commit_err) = commit_res {
                return Err(Self::receive_failure(
                    &progress_tx,
                    &trust_store,
                    history_limits,
                    &peer_ip,
                    &manifest,
                    &handshake.sender_name,
                    rel,
                    total_transferred,
                    &staged,
                    &format!("提交到最终位置失败: {}", commit_err),
                    FeisuoError::Io(commit_err),
                ));
            }
            if let Some(slot) = staged_paths.get_mut(idx) {
                slot.1 = final_path;
            }
        }
        // 暂存根目录已空, 删掉; `.feisuo-incoming` 本身保留以复用（隐藏目录, 对端不可见）
        // 提交成功后解除守卫: 此刻暂存区理应为空, 但 rename 失败走复制分支时
        // 可能残留 —— 那种情况由下面的 remove_dir_all 兜底。
        if let Err(e) = std::fs::remove_dir_all(&staging_root) {
            tracing::debug!("清理暂存目录 {}: {}", staging_root.display(), e);
        }
        staging_guard.armed = false;
        diag.mark_commit(commit_started.elapsed().as_millis() as u64);
        info!(
            "Committed {} file(s) from staging to {:?}",
            staged_paths.len(),
            receive_base_dir
        );

        // ---- 8.6 登记断点续传记录（P1 ⑪）----
        // **必须在提交成功之后**才写。提前写 = 把截断文件当成完整的,
        // 下次续传会跳过它, 而用户永远发现不了内容是坏的。
        for (_, final_path, file_index) in &staged_paths {
            let meta = &manifest.files[*file_index as usize];
            if let Err(e) = trust_store.record_completed_part(
                &manifest.transfer_id,
                &manifest.sender_id,
                &meta.relative_path,
                &meta.blake3_hash,
                meta.file_size,
                &final_path.to_string_lossy(),
            ) {
                // 登记失败 = 下次要重传, 不影响本次结果。只记 warn。
                tracing::warn!("登记断点续传记录失败 {}: {}", meta.relative_path, e);
            }
        }

        // 8. 总量必须与**本次实际要收的量**一致。
        //
        // ⚠️ 断点续传下不能用 `manifest.total_size` —— 那是全量,
        // 而跳过的文件根本没传, 拿它比会永远"不完整"并报传输失败。
        if total_transferred != resume.send_bytes {
            let e = FeisuoError::Security(format!(
                "传输不完整: 本次应收 {} 字节, 实际收到 {} 字节",
                resume.send_bytes, total_transferred
            ));
            return Err(Self::receive_failure(
                &progress_tx,
                &trust_store,
                history_limits,
                &peer_ip,
                &manifest,
                &handshake.sender_name,
                "-",
                total_transferred,
                std::path::Path::new(""),
                &e.to_string(),
                e,
            ));
        }

        // 9. 终态进度 + 最终 Ack
        // 数据流阶段到此为止: 下面的 ack 往返不属于"把数据送出去"的时间
        diag.mark_data(started_at.elapsed().as_millis() as u64);
        diag.mark_verify(verify_total_ms);
        diag.bytes_transferred = total_transferred;
        // 到这里文件已经 commit 落盘 ⇒ **撤不掉了**。
        // 登记进去, 让此后到达的撤销请求如实回"无法撤销"
        // 而不是回"已撤销"然后文件照样在（那正是旧实现的谎）。
        if let Ok(mut s) = committed_transfers.lock() {
            s.insert(manifest.transfer_id.clone());
        }
        let elapsed = started_at.elapsed().as_secs_f64().max(0.001);
        let _ = progress_tx.send(TransferProgress {
            transfer_id: manifest.transfer_id.clone(),
            direction: TransferDirection::Receive,
            peer_device_id: manifest.sender_id.clone(),
            peer_device_name: handshake.sender_name.clone(),
            current_file: format!("{} 个文件", manifest.files.len()),
            file_index: 0,
            total_files: manifest.files.len() as u32,
            bytes_transferred: total_transferred,
            total_bytes: manifest.total_size,
            progress_percent: 100.0,
            speed_bytes_per_sec: (total_transferred as f64 / elapsed) as u64,
            status: TransferStatus::Completed,
        });

        let ack = TransferAck {
            transfer_id: manifest.transfer_id.clone(),
            file_index: manifest.files.len().saturating_sub(1) as u32,
            chunk_index: 0,
            success: true,
            error_msg: None,
        };
        if let Err(e) = write_json_frame(&mut stream, &ack).await {
            // 文件已经完整落盘并通过校验, 仅回执发送失败, 记录告警后仍按成功处理
            warn!("传输已完成但回执发送失败: {}", e);
        }

        // 收尾诊断并落库（§9.7：这是"用户测完把报告发给我"的数据来源）
        diag.finish("completed", None);
        info!("{}", diag.to_log_line());
        info!("[诊断] 接收侧归因: {}", diag.attribution());
        if let Err(e) = trust_store.insert_diagnostics(&diag) {
            warn!("写入接收诊断失败: {}", e);
        }
        // 撤销标记用完即清：它只需要覆盖"传输正在进行"这个窗口。
        // 不清的话集合会随传输次数无界增长（每次一个 id）。
        //
        // `committed_transfers` 也要清 —— 它只对"传输刚结束、撤销请求
        // 还在路上"这个瞬间有意义。留着会让**下一次同 id 传输**被误判成
        // "已落盘，撤不掉"，而 `transfer_id` 是内容派生的，同一批文件
        // 重试必然撞上。
        if let Ok(mut s) = committed_transfers.lock() {
            s.remove(&manifest.transfer_id);
        }
        if let Ok(mut s) = cancelled.lock() {
            s.remove(&manifest.transfer_id);
        }

        // 10. 记录传输历史
        let (max_records, retention_days) = {
            let c = config.read().await;
            (c.max_history_records, c.record_retention_days)
        };
        let first_name = manifest
            .files
            .first()
            .map(|f| f.relative_path.clone())
            .unwrap_or_else(|| "未知文件".into());
        let history_label = if manifest.files.len() > 1 {
            format!("{} 等 {} 个文件", first_name, manifest.files.len())
        } else {
            first_name
        };
        let file_paths: Vec<String> = if !staged_paths.is_empty() {
            staged_paths
                .iter()
                .map(|(_, final_p, _)| final_p.to_string_lossy().to_string())
                .collect()
        } else {
            manifest
                .files
                .iter()
                .map(|f| receive_base_dir.join(&f.relative_path).to_string_lossy().to_string())
                .collect()
        };
        let _ = trust_store.add_transfer_record(
            &history_label,
            total_transferred,
            "recv",
            &handshake.sender_name,
            &peer_ip,
            "completed",
            max_records,
            retention_days,
            crate::security::TransferMetrics::from_diagnostics(&diag),
            &file_paths,
        );

        info!(
            "Batch transfer completed and fully verified ({} bytes)",
            total_transferred
        );
        Ok(())
    }

    /// 命中撤销标记时：回一条明确的失败 Ack、发 `Cancelled` 进度，然后返回 `Ok(true)`。
    ///
    /// 调用方必须立刻 `return Err(Cancelled)`，让 `StagingGuard` 清掉暂存目录。
    /// 返回 `Ok(false)` = 没有撤销，继续收。
    #[allow(clippy::too_many_arguments)]
    async fn stop_if_cancelled(
        cancelled: &Mutex<std::collections::HashMap<String, Instant>>,
        stream: &mut TcpStream,
        progress_tx: &broadcast::Sender<TransferProgress>,
        manifest: &TransferManifest,
        peer_name: &str,
        current_file: &str,
        file_index: u32,
        chunk_index: u64,
        bytes: u64,
    ) -> Result<bool> {
        let hit = cancelled
            .lock()
            .map(|s| s.contains_key(&manifest.transfer_id))
            .unwrap_or(false);
        if !hit {
            return Ok(false);
        }
        let _ = write_json_frame(
            stream,
            &TransferAck {
                transfer_id: manifest.transfer_id.clone(),
                file_index,
                chunk_index,
                success: false,
                error_msg: Some("对方撤销了本次传输".into()),
            },
        )
        .await;
        let _ = progress_tx.send(TransferProgress {
            transfer_id: manifest.transfer_id.clone(),
            direction: TransferDirection::Receive,
            peer_device_id: manifest.sender_id.clone(),
            peer_device_name: peer_name.to_string(),
            current_file: current_file.to_string(),
            file_index,
            total_files: manifest.files.len() as u32,
            bytes_transferred: bytes,
            total_bytes: manifest.total_size,
            progress_percent: 0.0,
            speed_bytes_per_sec: 0,
            status: TransferStatus::Cancelled,
        });
        info!(
            "接收侧因撤销中断: transfer={} file={} chunk={}",
            manifest.transfer_id, current_file, chunk_index
        );
        Ok(true)
    }

    /// 接收侧失败收尾: 删除半成品文件 + 广播 Failed 终态 + 写入传输历史。
    ///
    /// 成功路径会写一条「已完成」。失败只发进度事件的话，窗口一关这条就没了，
    /// 用户事后只能看到成功记录，以为没收成的那次根本没发生。
    #[allow(clippy::too_many_arguments)]
    fn receive_failure(
        progress_tx: &broadcast::Sender<TransferProgress>,
        trust_store: &TrustStore,
        history_limits: (u32, u32),
        peer_ip: &str,
        manifest: &TransferManifest,
        peer_name: &str,
        current_file: &str,
        bytes: u64,
        partial_path: &std::path::Path,
        reason: &str,
        err: FeisuoError,
    ) -> FeisuoError {
        // 网络瞬断/超时错误保留暂存文件以供断点续传；仅在明确失败/篡改时清理损坏文件
        let is_retriable_network_err = matches!(err, FeisuoError::Network(_) | FeisuoError::Io(_));
        if !partial_path.as_os_str().is_empty() && !is_retriable_network_err {
            if let Err(rm_err) = std::fs::remove_file(partial_path) {
                if rm_err.kind() != std::io::ErrorKind::NotFound {
                    warn!("清理半成品文件 {:?} 失败: {}", partial_path, rm_err);
                }
            }
        }

        let _ = progress_tx.send(TransferProgress {
            transfer_id: manifest.transfer_id.clone(),
            direction: TransferDirection::Receive,
            peer_device_id: manifest.sender_id.clone(),
            peer_device_name: peer_name.to_string(),
            current_file: current_file.to_string(),
            file_index: 0,
            total_files: manifest.files.len() as u32,
            bytes_transferred: bytes,
            total_bytes: manifest.total_size,
            progress_percent: 0.0,
            speed_bytes_per_sec: 0,
            status: TransferStatus::Failed(reason.to_string()),
        });

        // 和成功路径同一张表。窗口关掉之后，失败仍能在传输记录里看到。
        Self::record_receive_outcome(
            trust_store,
            history_limits,
            peer_ip,
            manifest,
            peer_name,
            current_file,
            bytes,
            "failed",
        );

        error!("Incoming transfer {} failed: {}", manifest.transfer_id, reason);
        err
    }

    /// 接收未完成时写一条历史。`status` 只用 `failed` 或 `cancelled`。
    ///
    /// 成功路径在提交之后另写「已完成」。这里不写成功，避免一次接收留下两条。
    #[allow(clippy::too_many_arguments)]
    fn record_receive_outcome(
        trust_store: &TrustStore,
        history_limits: (u32, u32),
        peer_ip: &str,
        manifest: &TransferManifest,
        peer_name: &str,
        current_file: &str,
        bytes: u64,
        status: &str,
    ) {
        let label = if current_file.is_empty() || current_file == "-" {
            manifest
                .files
                .first()
                .map(|f| f.relative_path.clone())
                .unwrap_or_else(|| "未知文件".into())
        } else if manifest.files.len() > 1 {
            format!("{} 等 {} 个文件", current_file, manifest.files.len())
        } else {
            current_file.to_string()
        };
        let file_paths: Vec<String> = manifest
            .files
            .iter()
            .map(|f| f.relative_path.clone())
            .collect();
        if let Err(e) = trust_store.add_transfer_record(
            &label,
            bytes,
            "recv",
            peer_name,
            peer_ip,
            status,
            history_limits.0,
            history_limits.1,
            crate::security::TransferMetrics {
                declared_size: manifest.total_size,
                ..Default::default()
            },
            &file_paths,
        ) {
            warn!("写入接收记录失败 ({}): {}", status, e);
        }
    }

    fn check_freshness(timestamp: i64) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        if (now - timestamp).abs() > MAX_HANDSHAKE_SKEW_SECS {
            return Err(FeisuoError::Security(format!(
                "握手时间戳偏差过大, 疑似重放攻击 (对端 {} / 本机 {})",
                timestamp, now
            )));
        }
        Ok(())
    }

    /// 清单级校验: 时间戳 / 签名 / 发送方一致性 / 分块参数 / 数量与体积自洽
    /// 断点续传决策：算出"这次真的需要传哪些文件"（P1 ⑪）。
    ///
    /// ## 跳过的判据是 **BLAKE3 全等**，不是文件名
    ///
    /// 用户改了文件内容、或者另一台机器上恰好有同名文件时，
    /// 按文件名跳过会让用户**永远收不到更新后的文件**，
    /// 而界面上还显示"传输成功"。哈希对不上就必须重传。
    ///
    /// ## 已经传过的文件必须复核哈希，而不是直接信数据库
    ///
    /// `transfer_parts` 里记着提交时的哈希，但用户在两次传输之间
    /// 可能改了那个文件。**重新算一遍**是唯一能保证"已存在 = 内容相同"
    /// 的办法。代价是每个已存在文件一次全量哈希（GB/s 级，几百 MB
    /// 也就零点几秒），换来的是不会静默跳过一个已被改动的文件。
    fn plan_resume(
        manifest: &TransferManifest,
        trust_store: &TrustStore,
    ) -> ResumePlan {
        let recorded = match trust_store.completed_parts(&manifest.transfer_id) {
            Ok(v) => v,
            Err(e) => {
                // 读不出来就当没有断点 —— 退化成全量重传, 绝不因此出错
                tracing::warn!("读取断点续传记录失败, 退化为全量传输: {}", e);
                Vec::new()
            }
        };
        let mut by_path: std::collections::HashMap<&str, &CompletedPart> =
            std::collections::HashMap::with_capacity(recorded.len());
        for p in &recorded {
            by_path.insert(p.relative_path.as_str(), p);
        }

        let mut completed: Vec<CompletedPart> = Vec::new();
        let mut needed: Vec<u32> = Vec::with_capacity(manifest.files.len());
        let mut dest_paths: Vec<String> = Vec::with_capacity(manifest.files.len());
        let mut file_resumes: Vec<crate::protocol::FileResumeProgress> = Vec::new();
        let mut send_bytes: u64 = 0;

        for (idx, f) in manifest.files.iter().enumerate() {
            let recorded = by_path.get(f.relative_path.as_str());
            let skip = match recorded {
                Some(part) => {
                    // 记录里的哈希必须与清单一致 —— 清单由发送方签名,
                    // 记录是本地状态。两者不一致说明清单变了或记录过期,
                    // 一律重传。
                    part.blake3_hash == f.blake3_hash
                        && part.file_size == f.file_size
                        && Self::existing_file_matches(
                            std::path::Path::new(&part.committed_path),
                            &f.blake3_hash,
                        )
                }
                None => false,
            };
            if skip {
                let path = recorded
                    .map(|p| p.committed_path.clone())
                    .unwrap_or_default();
                completed.push(CompletedPart {
                    relative_path: f.relative_path.clone(),
                    blake3_hash: f.blake3_hash.clone(),
                    file_size: f.file_size,
                    committed_path: path,
                });
                continue;
            }
            needed.push(idx as u32);
            dest_paths.push(f.relative_path.clone());

            // 检查是否有分块断点续传（未完成文件的分块记录）
            let mut file_needed_bytes = f.file_size;
            if let Ok(Some(chunk_rec)) = trust_store.get_chunk_progress(&manifest.transfer_id, f.file_index) {
                if chunk_rec.blake3_hash == f.blake3_hash
                    && chunk_rec.file_size == f.file_size
                    && chunk_rec.next_chunk_index > 0
                    && std::path::Path::new(&chunk_rec.staged_path).is_file()
                {
                    tracing::info!(
                        "断点续传检测到分块进度: transfer={} file={} 已有 {} 块 ({} 字节)",
                        manifest.transfer_id,
                        f.relative_path,
                        chunk_rec.next_chunk_index,
                        chunk_rec.bytes_resumed
                    );
                    file_needed_bytes = file_needed_bytes.saturating_sub(chunk_rec.bytes_resumed);
                    file_resumes.push(crate::protocol::FileResumeProgress {
                        file_index: f.file_index,
                        next_chunk_index: chunk_rec.next_chunk_index,
                        bytes_resumed: chunk_rec.bytes_resumed,
                    });
                }
            }
            send_bytes += file_needed_bytes;
        }
        ResumePlan {
            completed,
            needed,
            dest_paths,
            send_bytes,
            file_resumes,
        }
    }

    /// 已落盘文件是否仍与清单哈希一致。
    ///
    /// **必须真的重新算一遍哈希**，不能只看数据库里记的值。
    /// 两次传输之间用户可能改了那个文件（很常见：改完再发一次），
    /// 此刻数据库记的仍是"提交时的哈希"，而文件内容已经变了。
    /// 只比对记录 ⇒ 跳过一个内容已变的文件 ⇒ 用户永远收不到更新，
    /// 而界面显示"传输成功"。**这是最难自查的一类错误。**
    ///
    /// 代价：每个已存在文件一次全量 BLAKE3（GB/s 级，几百 MB 零点几秒）。
    /// 这个不对称（廉价的校验 vs 静默的数据丢失）决定了必须重算。
    ///
    /// 同步磁盘 I/O —— 调用方已在 `spawn_blocking` 之外，这里同步执行
    /// 会阻塞 runtime worker。**由调用方保证**：本函数只在传输开始前
    /// 调用一次，且已确认在 worker 外。
    fn existing_file_matches(
        committed_path: &std::path::Path,
        expected_hash: &str,
    ) -> bool {
        if !committed_path.is_file() {
            return false;
        }
        match crate::storage::ChunkStore::hash_file(committed_path) {
            Ok(h) => h == expected_hash,
            Err(e) => {
                tracing::warn!("复核已存在文件失败 {}: {}", committed_path.display(), e);
                false
            }
        }
    }

    fn validate_manifest(
        manifest: &TransferManifest,
        handshake: &HandshakeRequest,
        trust_store: &TrustStore,
    ) -> Result<()> {
        Self::check_freshness(manifest.timestamp)?;

        if manifest.sender_id != handshake.sender_id {
            return Err(FeisuoError::Security(format!(
                "清单发送方 {} 与握手发送方 {} 不一致",
                manifest.sender_id, handshake.sender_id
            )));
        }

        if manifest.chunk_size as usize != CHUNK_SIZE {
            return Err(FeisuoError::Security(format!(
                "分块大小 {} 与协议约定的 {} 不符",
                manifest.chunk_size, CHUNK_SIZE
            )));
        }

        if manifest.files.is_empty() {
            return Err(FeisuoError::Security("传输清单为空".into()));
        }
        if manifest.files.len() > MAX_FILES_PER_BATCH {
            return Err(FeisuoError::Security(format!(
                "单批文件数 {} 超过上限 {}",
                manifest.files.len(),
                MAX_FILES_PER_BATCH
            )));
        }

        // 对已受信设备, 清单必须由其私钥签名, 否则 ARP 欺骗即可任意篡改文件名/内容
        if let Some(pubkey) = trust_store.get_device_pubkey(&manifest.sender_id)? {
            if !DeviceIdentity::verify(
                &pubkey,
                manifest.signing_payload().as_bytes(),
                &manifest.signature,
            )? {
                return Err(FeisuoError::Security("传输清单签名校验失败".into()));
            }
        }

        // 逐文件自洽性校验
        let mut sum: u64 = 0;
        for (idx, f) in manifest.files.iter().enumerate() {
            if f.file_index as usize != idx {
                return Err(FeisuoError::Security(format!(
                    "清单文件索引不连续: 期望 {}, 实际 {}",
                    idx, f.file_index
                )));
            }
            if f.file_size > MAX_FILE_SIZE {
                return Err(FeisuoError::Security(format!(
                    "文件 {} 超过单文件体积上限",
                    f.relative_path
                )));
            }
            // 允许多级相对路径 (穿梭取回要保留对端目录结构), 逐段穿越校验
            PathManager::validate_relative_subpath(&f.relative_path)?;

            let expected_chunks = if f.file_size == 0 {
                1
            } else {
                (f.file_size + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64
            };
            if f.chunk_count != expected_chunks {
                return Err(FeisuoError::Security(format!(
                    "文件 {} 的分块数 {} 与体积 {} 不符 (应为 {})",
                    f.relative_path, f.chunk_count, f.file_size, expected_chunks
                )));
            }
            sum = sum
                .checked_add(f.file_size)
                .ok_or_else(|| FeisuoError::Security("清单总体积溢出".into()))?;
        }

        if sum != manifest.total_size {
            return Err(FeisuoError::Security(format!(
                "清单声明总量 {} 与逐文件求和 {} 不符",
                manifest.total_size, sum
            )));
        }

        Ok(())
    }

    /// 传输握手被拒时给对端一条 `HandshakeResponse{success:false}`。
    ///
    /// ## 为什么单独抽一个
    ///
    /// 这一段准入判定里有十来条拒绝路径, 早先它们全是裸 `return Err` ——
    /// 只关连接、不回应答。后果是发送方在 `read_json_frame("握手应答")`
    /// 上拿到 `UnexpectedEof`, 界面上是一句 `"I/O error: early eof"`:
    /// 没有原因, 没有来源, 用户只能反复重试。
    ///
    /// 抽出来是为了让"拒绝必须出站"这件事在调用点一眼可见
    /// （`Self::refuse_transfer(...).await; return Err(e);` 两行成对出现）,
    /// 而不是藏在某个 `?` 后面。
    ///
    /// ## 为什么不把 `requires_grant_code` 做成参数
    ///
    /// 「需要本次传输码」是一次**协商回合**而不是拒绝, 它的应答必须带
    /// `requires_grant_code: true` 才能让发送方保留待发队列。
    /// 塞进同一个函数就等于给拒绝路径开了一个可以传错标志位的口子,
    /// 而那个标志位传错的后果是"用户被当成失败了"而不是"请重试"。
    /// 所以协商路径继续用 [`Self::reject_with_message`] 显式传参。
    async fn refuse_transfer(
        stream: &mut TcpStream,
        identity: &Arc<DeviceIdentity>,
        config: &Arc<RwLock<AppConfig>>,
        message: String,
    ) {
        let _ = Self::reject_with_message(stream, identity, config, &message, false).await;
    }

    /// 在**还不知道消息类型**的前提下给一条拒绝应答。
    ///
    /// 先用极短超时偷看类型字节, 再按类型选结构。见 [`Self::refuse_by_msg_type`]
    /// 的注释 —— 那里解释了"为什么不能统一发 `HandshakeResponse`"。
    ///
    /// 刻意**不**占用调用方的等待预算：调用点在 accept 循环里（正是要卸载
    /// 压力的时候）或在刚拒绝完的位置, 让它同步等对端发字节是错的。
    ///
    /// ## 为什么写完应答还要把对方剩下的字节读干净
    ///
    /// Windows 上"带未读数据关闭 socket"发的是 **RST 而不是 FIN**。
    /// 而拒绝发生在读完整请求**之前**, 于是对端通常还有半帧在途:
    /// 我们写完拒绝帧就 drop, 栈上立刻发出 RST, 把刚写出去的那条
    /// 应答一起作废 —— 对端随后那次 `write_all` 撞上
    /// `10054 远程主机强迫关闭了一个现有的连接`。
    ///
    /// 实测就是这个顺序: 用户看到的错误是
    /// `"I/O error: 写帧内容 (…os error 10054)"` ——
    /// 一句关于**本机发送**的错, 而真实情况是我们掐掉了自己刚写的回复。
    /// 而且它是竞态: 传输路径赢, 配对路径输, 同一段代码时好时坏。
    ///
    /// 所以顺序固定为: 半关写端（发 FIN，表明"我说完了"）→ 把对端
    /// 剩余字节读干净 → 再 drop。读的长度必须有上限：对方随时可能
    /// 只发一半就停住，那正是这条路径必须扛住的形态。
    async fn refuse_after_peek(
        mut stream: TcpStream,
        identity: Arc<DeviceIdentity>,
        config: Arc<RwLock<AppConfig>>,
        message: String,
    ) {
        // 额度用尽就直接掐连接（见 `REFUSAL_BUDGET`）。
        let Ok(_permit) = REFUSAL_BUDGET.clone().try_acquire_owned() else {
            return;
        };
        let mut one = [0u8; 1];
        let msg_type = tokio::time::timeout(
            Duration::from_millis(REFUSE_PEEK_TIMEOUT_MS),
            stream.read_exact(&mut one),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .map(|_| one[0])
        // 读不到就按传输的形状回 —— 那是绝大多数请求的形状, 且
        // `refuse_by_msg_type` 对未知类型本来就走 `HandshakeResponse`。
        .unwrap_or(MSG_TRANSFER);
        Self::refuse_by_msg_type(&mut stream, msg_type, &identity, &config, &message, false).await;

        // 半关写端: 让对端知道"没有后续应答了"。
        let _ = stream.shutdown().await;
        let mut sink = vec![0u8; 8 * 1024];
        let drain = async {
            loop {
                match stream.read(&mut sink).await {
                    // 0 = 对端也关了; Err 包含 RST —— 都已经不需要再等了
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_millis(REFUSE_DRAIN_TIMEOUT_MS), drain).await;
    }

    /// 按**请求的消息类型**回一条拒绝应答。
    ///
    /// ## 为什么不能统一发 `HandshakeResponse`
    ///
    /// 四个响应 DTO 的必填字段不一样, 交叉解析是**硬失败**而不是降级:
    /// - `BrowseResponse.files` 没有 `#[serde(default)]` —— 缺字段直接报
    ///   `missing field 'files'`;
    /// - `PairResponse.device_id` 同样必填, 而且原因字段叫 `error_msg`
    ///   而不是 `message`。
    ///
    /// 于是"统一发握手应答"在浏览/配对路径上把一句能看懂的拒绝理由
    /// 换成了 `Serialization error: missing field 'files'` ——
    /// 比"对方已关闭连接"更让人一头雾水。
    ///
    /// 代价是每加一种消息类型都要在这里补一个分支, 所以下面用
    /// `PairResponse` / `PullResponse` 那种同构结构去覆盖同类形状,
    /// 而不是为每条消息再抽一层。
    async fn refuse_by_msg_type(
        stream: &mut TcpStream,
        msg_type: u8,
        identity: &Arc<DeviceIdentity>,
        config: &Arc<RwLock<AppConfig>>,
        message: &str,
        requires_grant_code: bool,
    ) {
        let receiver_name = config.read().await.device_name.clone();
        let device_id = identity.device_id.clone();
        // 四个 DTO 是四个不同类型, 没有公共父类型可以塞进同一个
        // `let resp = match ...`, 所以每条分支自己写帧。
        // 写成 `Box<dyn Serialize>` 能省三行, 但代价是**写错字段名
        // 编译不出来** —— 那正是这里最该被编译挡住的一类错。
        match msg_type {
            MSG_PAIR => {
                let _ = write_json_frame(
                    stream,
                    &PairResponse {
                        success: false,
                        device_id,
                        device_name: receiver_name,
                        public_key_hex: identity.public_key_hex(),
                        error_msg: Some(message.to_string()),
                        pairing_epoch: String::new(),
                    },
                )
                .await;
            }
            MSG_BROWSE => {
                let _ = write_json_frame(
                    stream,
                    &BrowseResponse {
                        success: false,
                        receiver_id: device_id,
                        receiver_name,
                        message: message.to_string(),
                        requires_grant_code,
                        files: Vec::new(),
                        current_path: String::new(),
                        parent_path: None,
                        truncated: false,
                        volumes: Vec::new(),
                        places: Vec::new(),
                        total: 0,
                        offset: 0,
                        volume_mode: false,
                        volume: String::new(),
                        caps: crate::protocol::local_caps(),
                    },
                )
                .await;
            }
            // 取回与撤销的应答结构完全相同, 一起处理。
            MSG_PULL | MSG_CANCEL => {
                let _ = write_json_frame(
                    stream,
                    &PullResponse {
                        success: false,
                        receiver_id: device_id,
                        receiver_name,
                        message: message.to_string(),
                        requires_grant_code,
                    },
                )
                .await;
            }
            // 传输 / 未知类型: 走握手应答（对端本来就在等它）
            _ => {
                let _ = Self::reject_with_message(
                    stream,
                    identity,
                    config,
                    message,
                    requires_grant_code,
                )
                .await;
            }
        }
    }

    /// 回一条失败的握手应答。
    ///
    /// `requires_grant_code` 决定发送方怎么反应：
    /// - `true` = 保留待发队列，提示用户带码重试（**不**报"失败"）；
    /// - `false` = 真失败，报错。
    async fn reject_with_message(
        stream: &mut TcpStream,
        identity: &Arc<DeviceIdentity>,
        config: &Arc<RwLock<AppConfig>>,
        message: &str,
        requires_grant_code: bool,
    ) -> Result<()> {
        let resp = HandshakeResponse {
            success: false,
            receiver_id: identity.device_id.clone(),
            receiver_name: config.read().await.device_name.clone(),
            message: message.to_string(),
            receiver_public_key_hex: identity.public_key_hex(),
            receiver_signature: String::new(),
            requires_grant_code,
            // 拒绝路径也要带 caps: 发送方在收到失败应答后就结束了,
            // 但保持字段齐全可避免"成功/失败两条路径的 DTO 不一致"
            // 这类以后一定会咬人的疏漏。
            caps: crate::protocol::local_caps(),
        };
        let _ = write_json_frame(stream, &resp).await;
        Ok(())
    }

    /// 校验 timestamp:nonce 挑战签名
    fn verify_challenge_signature(
        trust_store: &TrustStore,
        device_id: &str,
        timestamp: i64,
        nonce: &str,
        signature: &str,
    ) -> Result<bool> {
        let pk = match trust_store.get_device_pubkey(device_id)? {
            Some(pk) => pk,
            None => return Ok(false),
        };
        let expected_id = DeviceIdentity::device_id_from_pubkey_hex(&pk)?;
        if expected_id != device_id {
            return Err(FeisuoError::Security("设备指纹与公钥不匹配".into()));
        }
        let challenge = format!("{}:{}", nonce, timestamp);
        DeviceIdentity::verify(&pk, challenge.as_bytes(), signature)
    }

    /// 等待一次人工审批，并在「每次匹配码」等级下核对传输码（§2.3）。
    ///
    /// ## 码的方向
    ///
    /// **本函数（接收方）生成码并显示给本机用户；发起方把码输回来。**
    /// 理由见 [`ApprovalManager::issue_challenge`] 的长注释 ——
    /// 简言之：不信任发起方的那一方出题，才叫"凭证"。
    ///
    /// 因此 `presented_code` 是**发起方出示的**码，方向与旧实现相反
    /// （旧实现是发起方出码、接收方在弹窗里输入，等于让接收方去核对
    /// 一个对方自选的答案）。
    ///
    /// ## 抽出来是因为三个入口需要**完全相同**的语义
    ///
    /// 传输接收 / 目录浏览 / 取回，三处都要判 `RequireGrant`。
    /// 早先三处各写一遍，且只有传输那一处真的能走通 ——
    /// 浏览与取回的分支只是回一句"请先出示授权码"，
    /// 而协议里**根本没有申请授权码的通路**。
    /// 用户看到的是"配了每次匹配码就什么都用不了"。
    ///
    /// 统一到这里之后，"配了就能用"这句话才是真的。
    ///
    /// ## 返回值
    ///
    /// `Ok(())` = 放行；`Err(msg)` = 拒绝，`msg` 是可以直接展示给用户的原因。
    #[allow(clippy::too_many_arguments)]
    async fn await_approval(
        approval_manager: &ApprovalManager,
        approval_tx: &broadcast::Sender<ApprovalRequest>,
        trust_store: &TrustStore,
        cfg: &AppConfig,
        peer_id: &str,
        peer_name: &str,
        peer_ip: &str,
        op: Op,
        op_label: &str,
        file_count: u32,
        total_size: u64,
        first_file_name: &str,
        requires_grant_code: bool,
        presented_code: &str,
        timeout: Duration,
    ) -> std::result::Result<(), ApprovalError> {
        let approval_id = uuid::Uuid::new_v4().to_string();
        // 「每次匹配码」等级：**本机（接收方）出码**并显示给本机用户，
        // 发起方把看到的码敲回来。方向论证见 `challenge_for`。
        //
        // 指纹 = 本次请求的具体内容，让"带码重试"（一次全新连接）
        // 复用同一个码 —— 否则用户在第一个窗口读到的码在第二个窗口上
        // 永远对不上，这个功能一次都不可能成功。见 CHALLENGE_REUSE_TTL。
        let grant_challenge = if requires_grant_code {
            let fp = format!(
                "{}|{}|{}|{}",
                op_label, file_count, total_size, first_file_name
            );
            let code = approval_manager.challenge_for(peer_id, &fp);
            approval_manager.bind(&approval_id, &code);
            code
        } else {
            String::new()
        };
        let mut req = ApprovalRequest {
            approval_id: approval_id.clone(),
            sender_id: peer_id.to_string(),
            sender_name: peer_name.to_string(),
            sender_ip: peer_ip.to_string(),
            file_count,
            total_size,
            total_size_formatted: format_bytes(total_size),
            first_file_name: first_file_name.to_string(),
            created_at: chrono::Utc::now().timestamp(),
            requires_grant_code,
            grant_challenge: grant_challenge.clone(),
        };

        // 审批等待单独计时：它必须被排除在"速度"之外（§9.6.3），
        // 否则"对方开会审批 60 秒"会被算成 185 KB/s 这种假慢。
        let started = Instant::now();
        // 码输错时**重新弹窗**而不是直接拒绝。
        //
        // ## 为什么允许重试（这不是一个可以被暴力破解的口子）
        //
        // 重试的输入**只能来自人**：审批决策由 UI 提交，协议里没有
        // "自动重试"这条通路。所以对端无法用 6 位码去撞 ——
        // 60 秒内人也不可能手敲 100 万种组合。
        //
        // 不重试的代价却很大：输错一位就得让对方重新发起、自己重新批准。
        // 而输错一位是极常见的操作。
        const MAX_CODE_ATTEMPTS: u32 = 3;
        let mut attempts: u32 = 0;

        loop {
            let rx = approval_manager.register(&approval_id);
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                approval_manager.discard(&approval_id);
                return Err(ApprovalError::rejected("等待人工确认超时, 已拒绝"));
            }
            // 通知 UI 的发送结果不能被静默丢弃: 没有订阅者时对端会干等。
            // 这里 fail-closed 是刻意的 —— 无人能审批就绝不能放行。
            // 但报错文案不能说"前端未监听": 那是实现细节, 对端用户看不懂,
            // 只会以为对方设备坏了。
            if approval_tx.send(req.clone()).is_err() {
                approval_manager.discard(&approval_id);
                return Err(ApprovalError::rejected(format!(
                    "对方暂不支持人工确认, 已拒绝来自 {} 的{}请求; 请先完成配对, 或让对方开启自动接收",
                    peer_name, op_label
                )));
            }

            let decision = match tokio::time::timeout(remaining, rx).await {
                Ok(Ok(d)) => d,
                Ok(Err(_)) => {
                    approval_manager.discard(&approval_id);
                    return Err(ApprovalError::rejected("审批通道已关闭"));
                }
                Err(_) => {
                    // 超时后必须清理 pending 条目, 否则 map 无限增长
                    approval_manager.discard(&approval_id);
                    return Err(ApprovalError::rejected("等待人工确认超时, 已拒绝"));
                }
            };

            let action = decision.action;
            if action == ApprovalAction::Reject {
                return Err(ApprovalError::rejected("已被对方拒绝"));
            }
            if !requires_grant_code {
                return Ok(());
            }

            // ---- 传输码核对（§2.3）----
            //
            // 顺序很重要: **先验签再比码**（调用方负责验签，这里只比码）。
            //
            // 比的是**本进程生成、显示在接收机屏幕上的那个码**。
            // 注意这里绝不能回落到 `decision.grant_code` ——
            // 那是旧实现里"审批人替对端输入的码"，方向反了。
            let expected = approval_manager
                .expected_for(&approval_id)
                .unwrap_or_else(|| grant_challenge.clone());
            if presented_code.trim().is_empty() {
                // 关键: 归类为 GrantCodeNeeded 而不是 Rejected。
                // 调用方据此回 `requires_grant_code: true`, 对方才会
                // **保留待发队列**并弹码重试 —— 归错类就变成"硬拒绝",
                // 而用户能做的只有重试, 于是永远卡在这一步。
                return Err(ApprovalError::GrantCodeNeeded {
                    user_message: format!(
                        "对方尚未出示本次匹配码; 请让对方在本机窗口里看到 {} 后输入再重试",
                        format_grant_challenge_for_display(&expected)
                    ),
                });
            }
            if grant_code_eq(&expected, presented_code) {
                info!(
                    "匹配码核对通过, 放行来自 {} ({}) 的{}",
                    peer_name, peer_ip, op_label
                );
                // §2.3.1：按用户选的档位决定要不要写短期授权。
                maybe_write_session_grant(
                    trust_store,
                    cfg,
                    peer_id,
                    peer_name,
                    op,
                    action,
                );
                return Ok(());
            }

            attempts += 1;
            // **只记长度不记内容** —— 码只有 6 位, 落进数据库就等于
            // 把一个正在使用中的凭据存盘。
            let _ = trust_store.add_security_event(
                SecurityEventKind::GrantCodeMismatch,
                peer_id,
                peer_name,
                &format!(
                    "第 {} 次匹配码不匹配（对方出示 {} 位）",
                    attempts,
                    presented_code.trim().len()
                ),
            );
            warn!(
                "匹配码不匹配({}/{}): {} ({}); 同一窗口内可重输",
                attempts, MAX_CODE_ATTEMPTS, peer_name, peer_ip
            );
            if attempts >= MAX_CODE_ATTEMPTS {
                return Err(ApprovalError::rejected(format!(
                    "匹配码连续 {} 次不匹配, 已拒绝本次{}; 请与对方重新核对",
                    attempts, op_label
                )));
            }
            // 沿用同一个 approval_id —— 前端靠"id 相同"判定这是重输请求,
            // 从而**保留弹窗和同一个码**（码不换, 否则对方得重新去读屏幕）。
            req.created_at = chrono::Utc::now().timestamp();
        }
    }

    /// 目录浏览: 仅允许已配对且已受信的设备查询, 复用同一套签名认证
    /// 目录浏览: 仅允许已配对且已受信的设备查询, 复用同一套签名认证
    ///
    /// 是**方法**而不是关联函数: 「每次匹配码」等级下要走人工审批
    /// （[`Self::await_approval`]）, 而审批需要 `approval_manager` /
    /// `approval_tx` / 超时配置 —— 早先的关联函数签名里没有这三样,
    /// 所以那条分支只能干巴巴地回一句"请出示授权码"。
    #[allow(clippy::too_many_arguments)]
    async fn handle_browse(
        stream: &mut TcpStream,
        identity: &Arc<DeviceIdentity>,
        config: &Arc<RwLock<AppConfig>>,
        trust_store: &Arc<TrustStore>,
        approval_manager: &ApprovalManager,
        approval_tx: &broadcast::Sender<ApprovalRequest>,
        peer_ip: &str,
    ) -> Result<()> {
        let req: BrowseRequest = read_json_frame(stream, "浏览请求").await?;

        let mut resp = BrowseResponse {
            success: false,
            receiver_id: identity.device_id.clone(),
            receiver_name: config.read().await.device_name.clone(),
            message: String::new(),
            requires_grant_code: false,
            files: Vec::new(),
            current_path: String::new(),
            parent_path: None,
            truncated: false,
            volumes: Vec::new(),
            places: Vec::new(),
            total: 0,
            offset: 0,
            volume_mode: false,
            volume: String::new(),
            caps: crate::protocol::local_caps(),
        };

        // ---- 版本不再"不同就拒"（§7.3 / CAPS_HINT）----
        // 旧实现在这里直接 `return 协议版本不兼容`, 一升版 1.x 与 2.x
        // 彻底不通。而"多台设备同步"恰恰要求新旧版本能在同一网络里共存 ——
        // 用户不会为了升级而停用。改成：只记日志 + 降级到交集能力。
        if req.version != PROTOCOL_VERSION {
            let peer_caps = if req.caps == 0 {
                crate::protocol::legacy_caps()
            } else {
                req.caps
            };
            let negotiated = crate::protocol::negotiate_caps(peer_caps, resp.caps);
            tracing::info!(
                "Browse from {} ({}): version {} != {} -> 降级协商, 对方能力={} 交集={}",
                req.requester_name, peer_ip, req.version, PROTOCOL_VERSION,
                crate::protocol::describe_caps(peer_caps),
                crate::protocol::describe_caps(negotiated),
            );
        }

        // ---- 真实卷模式 vs 1.x 收件目录模式 ----
        let volume_mode = req.is_volume_mode();
        let peer_caps = if req.caps == 0 {
            crate::protocol::legacy_caps()
        } else {
            req.caps
        };
        if volume_mode && peer_caps & crate::protocol::caps::BROWSE_VOLUMES == 0 {
            // 请求方自己没这个能力却发了 volume 字段 —— 或者是伪造/畸形包。
            // 不静默降级（降级会让它以为是收件目录），直接说清楚。
            resp.message = "对端未声明支持真实卷浏览能力, 已拒绝该请求".into();
            write_json_frame(stream, &resp).await?;
            return Ok(());
        }

        let receive_dir = config.read().await.receive_dir.clone();
        // 统一鉴权（§2.4 / §8）：信任等级 + 会话授权 + 可访问范围一次判完。
        // 必须在解析卷之前取 —— `volume = "*"` 的解析依赖 scope 决定
        // 哪些卷可读。
        let scope = match trust_store.get_access_scope(&req.requester_id) {
            Ok(s) => s,
            Err(e) => {
                resp.message = format!("无法读取访问范围配置: {}", e);
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
        };
        // scope 内第一个可读卷（`volume = "*"` 时用它）
        let resolved_volume = if volume_mode && req.volume == crate::storage::volumes::VOLUME_ANY {
            match crate::storage::volumes::first_readable_volume(&scope) {
                Some(v) => v,
                None => {
                    resp.message = "该设备没有开放任何可浏览的磁盘（可访问范围为「仅收件目录」或白名单为空）".into();
                    write_json_frame(stream, &resp).await?;
                    return Ok(());
                }
            }
        } else {
            req.volume.clone()
        };
        // 本次要解析的 (卷, 卷内相对路径)
        let (browse_volume, raw_rel) = if volume_mode {
            (resolved_volume, req.effective_rel().to_string())
        } else {
            (Self::volume_of(&receive_dir), req.sub_path.clone())
        };

        let browse_abs = if volume_mode {
            crate::storage::volumes::resolve_browse_path(&browse_volume, &raw_rel)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("{}:{}", browse_volume, raw_rel))
        } else {
            // 1.x 语义的根就是收件目录本身，不是盘根。
            // 拼成 `C:/子路径` 会让「仅收件目录」把合法的收件根判成盘外路径。
            receive_dir
                .join(raw_rel.replace('/', std::path::MAIN_SEPARATOR_STR))
                .to_string_lossy()
                .to_string()
        };
        let decision = {
            let c = config.read().await.clone();
            crate::security::authorize(
                &trust_store,
                &c,
                crate::security::Op::Browse,
                &crate::security::AuthContext {
                    peer_id: &req.requester_id,
                    peer_ip,
                    volume: &browse_volume,
                    path: &browse_abs,
                },
            )
        };
        match decision {
            crate::security::Decision::Allow => {}
            crate::security::Decision::RequireGrant => {
                // 「每次匹配码」等级：走与传输完全相同的审批 + 码核对。
                // 早先这里只回一句"请先出示授权码"，而协议里根本没有
                // 申请授权码的通路 —— 用户实际体验是"配了就什么都用不了"。
                //
                // 时钟在等人**之前**查。查完再等：确认框默认 60 秒，
                // 等人的时间不能算进 120 秒，否则一次正常确认会被判成重放。
                if let Err(e) = Self::check_freshness(req.timestamp) {
                    resp.message = e.to_string();
                    let _ = write_json_frame(stream, &resp).await;
                    return Err(e);
                }
                if let Err(e) = Self::await_approval(
                    &approval_manager,
                    &approval_tx,
                    &trust_store,
                    &*config.read().await,
                    &req.requester_id,
                    &req.requester_name,
                    peer_ip,
                    Op::Browse,
                    "目录浏览",
                    0,
                    0,
                    &browse_abs,
                    true,
                    &req.grant_code,
                    Duration::from_secs(config.read().await.approval_timeout_secs),
                )
                .await
                {
                    // 结构化信号必须带上, 否则对端只能去 `message` 里
                    // 匹配"传输码"三个字来分辨这是协商回合还是硬拒绝 ——
                    // 而对端一改文案就静默失效（见 ApprovalError 的注释）。
                    if matches!(e, ApprovalError::GrantCodeNeeded { .. }) {
                        resp.requires_grant_code = true;
                    }
                    resp.message = e.user_message().to_string();
                    write_json_frame(stream, &resp).await?;
                    return Ok(());
                }
            }
            crate::security::Decision::Deny { user_message, .. } => {
                resp.message = user_message;
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
        }
        // mode=all 时写访问审计（§8.2 第 2 层：只做限制不做记录不完整）
        if scope.mode == crate::security::AccessMode::All {
            let _ = trust_store.add_access_audit(
                &req.requester_id,
                &req.requester_name,
                &browse_volume,
                &browse_abs,
                "browse",
            );
        }

        match Self::verify_challenge_signature(trust_store, &req.requester_id, req.timestamp, &req.nonce, &req.signature) {
            Ok(true) => {}
            Ok(false) => {
                resp.message = "浏览请求签名校验失败".into();
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
            Err(e) => {
                // 库损坏这类内部错误也要写进应答。只 `return Err` 的话，
                // 浏览方在读应答时撞上连接关闭，界面上像是网络断了。
                resp.message = e.to_string();
                let _ = write_json_frame(stream, &resp).await;
                return Err(e);
            }
        }

        // 「每次匹配码」已经在进审批前查过时钟。这里只补永久信任那条：
        // 它没有等人，所以到达时查一次就够了。两条路都不能在确认之后再查。
        if !matches!(decision, crate::security::Decision::RequireGrant) {
            if let Err(e) = Self::check_freshness(req.timestamp) {
                resp.message = e.to_string();
                let _ = write_json_frame(stream, &resp).await;
                return Err(e);
            }
        }
        trust_store.update_last_ip(&req.requester_id, peer_ip)?;
        // 记录"最后一次在线"（§3.6）。必须落库, 否则重启后离线时间无从判断。
        let _ = trust_store.mark_seen(&req.requester_id, peer_ip);

        // 1) 归一化子路径（拒绝 .. / 绝对路径 / 隐藏目录 / 超深）
        let rel = match Self::normalize_sub_path(&raw_rel) {
            Ok(r) => r,
            Err(e) => {
                resp.message = e.to_string();
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
        };

        // 2) 解析目标目录
        let dir = if volume_mode {
            // 真实卷模式：按 (卷, rel) 拼绝对路径。
            // 这里**不复用 resolve_browse_dir** —— 那个函数的纵深防御是
            // "必须仍在收件目录内"，对真实卷浏览会误杀。真实卷模式的
            // 边界保证来自 `authorize()`（scope + 强制排除清单），不是路径前缀。
            match crate::storage::volumes::resolve_browse_path(&browse_volume, &rel) {
                Some(d) => d,
                None => {
                    resp.message = format!("未知的卷: {}", browse_volume);
                    write_json_frame(stream, &resp).await?;
                    return Ok(());
                }
            }
        } else {
            match Self::resolve_browse_dir(&receive_dir, &rel) {
                Ok(d) => d,
                Err(e) => {
                    resp.message = e.to_string();
                    write_json_frame(stream, &resp).await?;
                    return Ok(());
                }
            }
        };

        // 3) 读目录是同步磁盘 I/O, 必须在 worker 之外执行
        let (offset, limit) = crate::storage::volumes::normalize_page(req.offset, req.limit);
        let paged = peer_caps & crate::protocol::caps::BROWSE_VOLUMES != 0;
        let listed = {
            let list_dir = dir.clone();
            let inbox_dir = receive_dir.clone();
            // 卷根可能不存在（U 盘拔了 / 权限不足）。静默返回空列表会被
            // 用户当成"文件夹是空的"，必须明确报错。
            // 列举要按该设备的 `scope` 过滤（`deny_paths` / `allow_paths`），
            // 而 `scope` 在闭包外还要用于后面的 `volume_mode` 判定，所以
            // 这里给闭包一份 clone，不 move 走。
            let browse_scope = scope.clone();
            tokio::task::spawn_blocking(move || -> Result<(Vec<RemoteFileEntry>, bool, u32, u32)> {
                if !list_dir.exists() {
                    if paged {
                        return Err(FeisuoError::Protocol(format!(
                            "路径不存在: {}",
                            list_dir.to_string_lossy()
                        )));
                    }
                    // 1.x 语义: 收件目录不存在就补建, 让界面至少能正常打开
                    let _ = std::fs::create_dir_all(&list_dir);
                }
                if list_dir.exists() && !list_dir.is_dir() {
                    return Err(FeisuoError::Protocol("目标不是目录".into()));
                }
                if paged {
                    let (items, total, hidden) = Self::list_dir_entries_paged(
                        &list_dir,
                        offset,
                        limit,
                        &inbox_dir,
                        &browse_scope,
                    );
                    let shown_end = offset + items.len();
                    Ok((items, shown_end < total as usize, total, hidden))
                } else {
                    let (items, truncated, hidden) =
                        Self::list_dir_entries(&list_dir, &inbox_dir, &browse_scope);
                    let total = items.len() as u32;
                    Ok((items, truncated, total, hidden))
                }
            })
            .await
            .map_err(|e| FeisuoError::Internal(format!("列目录任务异常: {}", e)))?
        };
        let (files, truncated, total, hidden_by_policy) = match listed {
            Ok(v) => v,
            Err(e) => {
                resp.message = e.to_string();
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
        };

        resp.files = files;
        resp.truncated = truncated;
        resp.total = total;
        resp.offset = offset as u32;
        resp.current_path = rel.clone();
        resp.parent_path = if rel.is_empty() {
            None
        } else {
            Some(Self::parent_of(&rel))
        };
        resp.volume_mode = volume_mode;
        resp.volume = if volume_mode { browse_volume.clone() } else { String::new() };
        // 真实卷模式才回卷列表 —— 1.x 对端收到这个字段只会忽略（serde default），
        // 而老版本客户端的 UI 不会因为多出来的字段崩（它按 index 访问数组）。
        if volume_mode {
            let mut vols = crate::storage::volumes::list_volumes();
            crate::storage::volumes::filter_volumes_by_scope(&mut vols, &scope);
            resp.volumes = vols;
        }
        // 常用位置与卷列表**分开判断**：卷受限时（ReceiveOnly）仍然要
        // 能列出常用位置 —— 收件目录本身就是被允许浏览的那一处，
        // 而它常常就落在 `Users\\<u>\\Downloads` 之类路径下。
        // 逐个按 scope 判，才不会把"其实可读"的入口一起砍掉。
        resp.places = crate::storage::known_places()
            .into_iter()
            .filter(|p| {
                // 收件目录豁免，与目录列举同一套规则 —— 否则配对后
                // 「只能收件」的用户连自己刚收下的东西都找不到地方看。
                let abs = std::path::Path::new(&p.path);
                crate::storage::is_within(abs, &receive_dir)
                    || scope.can_read(&p.volume, &p.path)
            })
            .collect();
        resp.success = true;
        resp.message = if truncated {
            if paged {
                format!(
                    "共 {} 项, 已显示第 {}-{} 项（滚动加载更多）",
                    total,
                    offset + 1,
                    offset + resp.files.len()
                )
            } else {
                format!(
                    "目录条目过多, 仅显示前 {} 项 (受 {} 上限保护)",
                    MAX_BROWSE_ENTRIES, MAX_BROWSE_ENTRIES
                )
            }
        } else {
            "OK".into()
        };
        // 被安全清单隐藏的条目必须说出来。
        //
        // 静默隐藏时用户看到的是一个"空文件夹"，会以为对端没东西可拿，
        // 于是去检查网络、重新配对、甚至以为文件发丢了 ——
        // 而真实原因是"这台设备的访问范围把这些路径设成了不可见"。
        // 两者需要的后续动作完全不同，所以不能说"OK"。
        if hidden_by_policy > 0 {
            resp.message = if resp.message == "OK" {
                format!("其中 {hidden_by_policy} 项因该设备的访问范围设置而未显示")
            } else {
                format!("{}；另有 {hidden_by_policy} 项因访问范围设置而未显示", resp.message)
            };
        }
        write_json_frame(stream, &resp).await?;
        Ok(())
    }

    /// 处理"取回"请求: 校验通过后由本机反向发起一次传输, 把对方点名的文件推回去。
    /// 复用既有发送链路, 因此进度 / 校验 / 落盘策略完全一致。
    async fn handle_pull(
        stream: &mut TcpStream,
        identity: &Arc<DeviceIdentity>,
        config: &Arc<RwLock<AppConfig>>,
        trust_store: &Arc<TrustStore>,
        client: &Arc<TransferClient>,
        approval_manager: &ApprovalManager,
        approval_tx: &broadcast::Sender<ApprovalRequest>,
        peer_ip: &str,
    ) -> Result<()> {
        let req: PullRequest = read_json_frame(stream, "取回请求").await?;

        let mut resp = PullResponse {
            success: false,
            receiver_id: identity.device_id.clone(),
            receiver_name: config.read().await.device_name.clone(),
            message: String::new(),
            requires_grant_code: false,
        };

        fn fail(resp: &mut PullResponse, msg: &str) {
            resp.message = msg.to_string();
        }

        if req.version != PROTOCOL_VERSION {
            fail(
                &mut resp,
                &format!(
                    "协议版本不兼容: 对端 {} / 本机 {}",
                    req.version, PROTOCOL_VERSION
                ),
            );
            write_json_frame(stream, &resp).await?;
            return Ok(());
        }
        // 统一鉴权（§2.4 / §8.3）：取走文件比浏览更敏感, 额外受 `can_pull` 约束。
        // 默认 `can_pull = true` 但 `can_push = false`（D2 决策）。
        let base_dir_for_scope = config.read().await.receive_dir.clone();
        let scope_volume = Self::volume_of(&base_dir_for_scope);
        let scope = match trust_store.get_access_scope(&req.requester_id) {
            Ok(s) => s,
            Err(e) => {
                fail(&mut resp, &format!("无法读取访问范围配置: {}", e));
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
        };
        // 逐个目标判定范围: 任何一个越界就整体拒绝, 不做部分放行
        //
        // 「每次匹配码」单独收集在下面统一处理: 它**不是**拒绝,
        // 而是要走一次人工审批 + 码核对。多个目标时只弹**一次**窗,
        // 否则用户要点 N 次"允许"（而且每次都要重输同一个码）。
        let mut any_require_grant = false;
        for name in &req.sub_paths {
            // 预检用的绝对路径必须与后面**真正读取的路径**一致。
            //
            // 早先一律用 `join_volume_path(receive_dir 的卷, name)`，
            // 于是卷模式请求 `Windows/System32` 拿到的是
            // `C:/Windows/System32` —— 碰巧也是 C 盘所以看着对，
            // 但请求 `D 盘的东西` 时预检的是 `C:/D盘的东西`：
            // 该拒的没拒（真正读取阶段会拒，于是表现为"能浏览但取不回"），
            // 该拒的也可能在预检阶段被误拒。
            //
            // 判据与解析必须**同一个函数**，否则两处结论必然对不上。
            // 卷号必须和真正要读的那条路径是同一个盘。
            //
            // 早先无论请求哪个卷，这里都把 `volume` 填成收件目录所在盘。
            // 白名单只开了 D: 时，取回 D: 上的文件会被当成 C: 拒绝；
            // 只开了 C: 时，取回 D: 又会被当成 C: 放行。
            // 用户看到的是"能浏览这个盘，点取回却说无权"，或者反过来。
            let (auth_volume, abs) = if req.volume.is_empty() {
                // 1.x 取回读的是收件目录下的相对路径，不是盘根下的同名路径。
                (
                    scope_volume.clone(),
                    base_dir_for_scope
                        .join(name.replace('/', std::path::MAIN_SEPARATOR_STR))
                        .to_string_lossy()
                        .to_string(),
                )
            } else {
                // 卷模式下用浏览器同款解析；解析不出来就让它落到
                // 展开阶段去报错（那里有更完整的文案）。
                let resolved = crate::storage::volumes::resolve_browse_path(
                    &req.volume,
                    &name.replace('\\', "/"),
                )
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| join_volume_path(&req.volume, name));
                (req.volume.clone(), resolved)
            };
            let decision = {
                let c = config.read().await.clone();
                crate::security::authorize(
                    &trust_store,
                    &c,
                    crate::security::Op::Pull,
                    &crate::security::AuthContext {
                        peer_id: &req.requester_id,
                        peer_ip,
                        volume: &auth_volume,
                        path: &abs,
                    },
                )
            };
            match decision {
                crate::security::Decision::Allow => {}
                crate::security::Decision::RequireGrant => {
                    any_require_grant = true;
                }
                crate::security::Decision::Deny { user_message, .. } => {
                    fail(
                        &mut resp,
                        &format!("{}（目标: {}）", user_message, name),
                    );
                    write_json_frame(stream, &resp).await?;
                    return Ok(());
                }
            }
        }
        if any_require_grant {
            // 时钟在等人之前查，理由与浏览相同：
            // 确认框的等待时间不能算进 120 秒的重放窗口。
            if let Err(e) = Self::check_freshness(req.timestamp) {
                fail(&mut resp, &e.to_string());
                let _ = write_json_frame(stream, &resp).await;
                return Err(e);
            }
            if let Err(e) = Self::await_approval(
                approval_manager,
                approval_tx,
                trust_store,
                &*config.read().await,
                &req.requester_id,
                &req.requester_name,
                peer_ip,
                Op::Pull,
                "取回",
                req.sub_paths.len() as u32,
                0,
                req.sub_paths.first().map(|s| s.as_str()).unwrap_or(""),
                true,
                &req.grant_code,
                Duration::from_secs(config.read().await.approval_timeout_secs),
            )
            .await
            {
                // 同 browse: 结构化信号必须带上（见 ApprovalError 的注释）
                if matches!(e, ApprovalError::GrantCodeNeeded { .. }) {
                    resp.requires_grant_code = true;
                }
                fail(&mut resp, e.user_message());
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
        }
        if scope.mode == crate::security::AccessMode::All {
            for name in &req.sub_paths {
                let _ = trust_store.add_access_audit(
                    &req.requester_id,
                    &req.requester_name,
                    &scope_volume,
                    &join_volume_path(&scope_volume, name),
                    "pull",
                );
            }
        }
        match Self::verify_challenge_signature(
            trust_store,
            &req.requester_id,
            req.timestamp,
            &req.nonce,
            &req.signature,
        ) {
            Ok(true) => {}
            Ok(false) => {
                fail(&mut resp, "取回请求签名校验失败");
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
            Err(e) => {
                fail(&mut resp, &e.to_string());
                let _ = write_json_frame(stream, &resp).await;
                return Err(e);
            }
        }
        // 需要匹配码的已经在进审批前查过。这里只补不弹窗的那条。
        if !any_require_grant {
            if let Err(e) = Self::check_freshness(req.timestamp) {
                fail(&mut resp, &e.to_string());
                let _ = write_json_frame(stream, &resp).await;
                return Err(e);
            }
        }
        trust_store.update_last_ip(&req.requester_id, peer_ip)?;

        // 只允许取回本机落盘目录下的文件, 逐段做路径穿越校验。
        // sub_paths 可以含 "/" 分隔的层级, 用户在穿梭里进了子文件夹也能取回。
        if req.sub_paths.is_empty() {
            fail(&mut resp, "未指定要取回的文件");
            write_json_frame(stream, &resp).await?;
            return Ok(());
        }
        if req.requester_port == 0 {
            fail(&mut resp, "对端未提供有效的传输端口");
            write_json_frame(stream, &resp).await?;
            return Ok(());
        }
        if req.sub_paths.len() > MAX_FILES_PER_BATCH {
            fail(
                &mut resp,
                &format!("单次取回文件数超过 {} 上限", MAX_FILES_PER_BATCH),
            );
            write_json_frame(stream, &resp).await?;
            return Ok(());
        }

        let base_dir = config.read().await.receive_dir.clone();
        // (读取绝对路径, 交给请求方的目标相对名) —— 目录保留完整层级
        let mut targets: Vec<(std::path::PathBuf, String)> = Vec::new();
        // 目录展开会**放大**目标数量，所以上限要在展开**之后**再查一次。
        // 只在展开前查 `sub_paths.len()` 的话，一个只含 1 个目录的请求
        // 就能拉走 2000 个文件 —— 上限形同虚设。
        let mut expanded_total = 0usize;
        // 这个目录一个文件都没贡献时记下来。只写日志的话，请求方
        // 看到的仍是「已受理」，不知道哪一个目录是空的或超出了访问范围。
        let mut skipped_empty_dirs: Vec<String> = Vec::new();
        for name in &req.sub_paths {
            // 逐段校验：`..` / 绝对路径 / 盘符 / 控制字符 一律拒绝。
            // 卷模式下 `name` 必须是**卷内**相对路径 —— 带盘符说明调用方
            // 搞混了语义，宁可拒掉也不能让它去解析成别的卷。
            if let Err(e) = PathManager::validate_relative_subpath(name) {
                fail(&mut resp, &format!("非法路径已拒绝: {}", e));
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
            if !req.volume.is_empty() {
                if name.contains(':') {
                    fail(
                        &mut resp,
                        "卷模式下目标必须是卷内相对路径, 不能带盘符",
                    );
                    write_json_frame(stream, &resp).await?;
                    return Ok(());
                }
            }
            let normalized = name.replace('\\', "/");
            // 解析成绝对路径：卷模式走 `resolve_browse_path`（它只认白名单卷
            // + 通配 `*` 的展开结果），1.x 模式落在收件目录下。
            let candidate = if req.volume.is_empty() {
                base_dir.join(&normalized)
            } else {
                match crate::storage::volumes::resolve_browse_path(&req.volume, &normalized) {
                    Some(d) => d,
                    None => {
                        fail(&mut resp, &format!("未知卷: {}", req.volume));
                        write_json_frame(stream, &resp).await?;
                        return Ok(());
                    }
                }
            };
            // 规范路径必须仍落在允许的根内（防 junction / `..` 逃逸）
            let allowed_root = if req.volume.is_empty() {
                base_dir.clone()
            } else {
                match crate::storage::volumes::resolve_browse_path(&req.volume, "") {
                    Some(d) => d,
                    None => {
                        fail(&mut resp, &format!("未知卷: {}", req.volume));
                        write_json_frame(stream, &resp).await?;
                        return Ok(());
                    }
                }
            };
            // `is_within` 内部已处理 Windows `\\?\` 前缀与"目标尚不存在"，
            // 见 storage::paths 的模块注释。手写 canonicalize+starts_with
            // 会在 Windows 上把合法路径误判成逃逸。
            if !crate::storage::is_within(&candidate, &allowed_root) {
                fail(
                    &mut resp,
                    &format!("取回路径逃逸出允许的目录, 已拒绝: {}", name),
                );
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
            // 显式拒绝符号链接 / junction：它们能让"看起来在允许目录内"
            // 的路径指向别处（D11）。`folder_scan` 内部也不跟随。
            if let Ok(meta) = std::fs::symlink_metadata(&candidate) {
                if meta.file_type().is_symlink() {
                    fail(&mut resp, &format!("不能取回符号链接: {}", name));
                    write_json_frame(stream, &resp).await?;
                    return Ok(());
                }
            }
            if candidate.is_dir() {
                // ---- 目录：递归展开，保留层级（§7.6 同款语义）----
                let dir_name = normalized.trim_end_matches('/').to_string();
                // 展开失败必须先写进应答。`?` 会直接拆掉这条连接，
                // 对方看到的是「对方已关闭连接」，不知道是这个目录
                // 没有权限、不存在，或者正在被占用。
                let (files, scan) = match crate::storage::folder_scan::expand_all(&[candidate.clone()]) {
                    Ok(v) => v,
                    Err(e) => {
                        fail(&mut resp, &format!("无法展开目录 {}: {}", name, e));
                        write_json_frame(stream, &resp).await?;
                        return Ok(());
                    }
                };
                expanded_total += files.len();
                if expanded_total > MAX_FILES_PER_BATCH {
                    fail(
                        &mut resp,
                        &format!(
                            "取回目标展开后超过 {} 个文件（已取 {} 个; 跳过 {} 项，其中符号链接 {} 个）。请缩小选择范围",
                            MAX_FILES_PER_BATCH,
                            expanded_total,
                            scan.skipped,
                            scan.symlinks_skipped
                        ),
                    );
                    write_json_frame(stream, &resp).await?;
                    return Ok(());
                }
                // 只看**这个目录**有没有贡献文件。用整个 `targets` 是否为空来判，
                // 会受选择顺序影响：空目录排在前面就把后面还能取的文件整批拒绝；
                // 排在后面又会被前面的文件掩盖，什么都不说。
                let added_before = targets.len();
                for (abs, rel_in_dir) in files {
                    // 目标相对名 = 目录名 + 目录内层级，取回后结构不变
                    let rel = format!("{}/{}", dir_name, rel_in_dir.replace('\\', "/"));
                    // ⚠️ 展开出来的文件必须**逐个**重过范围判定。
                    // 只判目录本身是不够的：目录里可能有落在强制排除清单下的
                    // 子树（用户没察觉），而"目录允许"不蕴含"目录里每个文件允许"。
                    let abs_str = abs.to_string_lossy().to_string();
                    // 与上面逐目标预检同一条规则：1.x（空卷）读的是收件目录
                    // 所在盘，卷模式读的是请求指定的盘。填空串会让白名单
                    // 把收件目录里展开出来的文件全部判成"不在允许的卷上"。
                    let auth_volume = if req.volume.is_empty() {
                        scope_volume.as_str()
                    } else {
                        req.volume.as_str()
                    };
                    let decision = {
                        let c = config.read().await.clone();
                        crate::security::authorize(
                            &trust_store,
                            &c,
                            crate::security::Op::Pull,
                            &crate::security::AuthContext {
                                peer_id: &req.requester_id,
                                peer_ip,
                                volume: auth_volume,
                                path: &abs_str,
                            },
                        )
                    };
                    if let crate::security::Decision::Deny {
                        user_message: reason, ..
                    } = decision
                    {
                        // 单个文件被拒**不整体失败**：把能取的取走，
                        // 被拒的那些在回执里如实说明。整体失败会让
                        // "目录里混了一个受限文件"变成"什么都拿不到"。
                        tracing::info!(
                            "取回展开时跳过被拒文件 {}: {}",
                            abs_str, reason
                        );
                        continue;
                    }
                    targets.push((abs, rel));
                }
                if targets.len() == added_before {
                    tracing::info!(
                        "取回跳过目录 {}：里面没有可取的文件（可能全部超出访问范围）",
                        name
                    );
                    skipped_empty_dirs.push(name.clone());
                }
            } else if candidate.is_file() {
                targets.push((candidate, normalized));
            } else {
                fail(&mut resp, &format!("文件不存在: {}", name));
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
        }

        // ---- 落点子目录（§7.7）----
        // 请求方可以指定"把 A 的文件取到我的 `D:\工作\`"。
        // 归一化后拼到每个目标相对路径前面，于是接收端会重建出
        // `工作/2026/报表/1月.csv`。空串 = 落收件根（= 旧行为）。
        let dest_prefix = match Self::normalize_sub_path(&req.dest_sub_path) {
            Ok(p) => p,
            Err(e) => {
                fail(
                    &mut resp,
                    &format!("落点子目录非法, 已拒绝: {}", e),
                );
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
        };
        if !dest_prefix.is_empty() {
            // 落点子目录由**请求方**指定。它绝不能逃出收件目录 ——
            // 否则一个已配对设备就能让本机往任意路径写文件
            // （写权限与读取解耦，D2 决策在这里必须再守一次）。
            let probe = base_dir.join(&dest_prefix);
            // ⚠️ 这里**不能**先 `create_dir_all` 再校验。
            //
            // `is_within` 内部的 `canonicalize_lenient` 对不存在的路径会
            // 逐级上溯到最深的已存在祖先、再把剩余段接回去（storage::paths
            // 有测试 `within_true_for_nonexistent_child` 锁住这个语义），
            // 所以它**本来就不要求目标存在**。
            //
            // 早先的写法是"先建目录让 canonicalize 有东西可解析"——
            // 那是在给一个不需要前提的检查硬造前提。代价是**校验失败时
            // 目录已经被建出来了**：`probe` 逃逸被拒，可那个路径已经
            // 真的出现在磁盘上。也就是说这条"防御"本身在攻击面留了痕迹。
            // 目录交给真正的落盘阶段建（`dest_dir` 那段）。
            if !crate::storage::is_within(&probe, &base_dir) {
                fail(
                    &mut resp,
                    &format!("落点子目录逃逸出收件目录, 已拒绝: {}", req.dest_sub_path),
                );
                write_json_frame(stream, &resp).await?;
                return Ok(());
            }
        }
        if !dest_prefix.is_empty() {
            for (_, rel) in targets.iter_mut() {
                *rel = format!("{}/{}", dest_prefix, rel);
            }
        }

        // 每个目录单独跳过之后，仍可能一个文件都没有。
        // 这时不能回「已受理，正在推送 0 个文件」——对方会以为取回已经开始。
        if targets.is_empty() {
            fail(
                &mut resp,
                "没有可取回的文件（目录为空，或里面的文件全部超出访问范围）",
            );
            write_json_frame(stream, &resp).await?;
            return Ok(());
        }

        resp.success = true;
        let mut extra = if dest_prefix.is_empty() {
            String::new()
        } else {
            format!("（落点: {}）", dest_prefix)
        };
        if !skipped_empty_dirs.is_empty() {
            // 只点名头几个。一次取回可以选很多目录，整份名单塞进一句
            // 提示会把真正要看的文件数挤掉。其余的仍在日志里。
            let listed = if skipped_empty_dirs.len() <= 3 {
                skipped_empty_dirs.join("、")
            } else {
                format!(
                    "{} 等 {} 个",
                    skipped_empty_dirs[..3].join("、"),
                    skipped_empty_dirs.len()
                )
            };
            extra.push_str(&format!("；未取回目录: {}", listed));
        }
        resp.message = format!(
            "已受理, 正在向 {} 推送 {} 个文件{}",
            req.requester_name,
            targets.len(),
            extra
        );
        let message = resp.message.clone();
        write_json_frame(stream, &resp).await?;

        // 异步推送, 不占用本次连接的读写
        let client = client.clone();
        let requester_id = req.requester_id.clone();
        let requester_name = req.requester_name.clone();
        let requester_ip = peer_ip.to_string();
        let requester_port = req.requester_port;
        tokio::spawn(async move {
            info!(
                "Pull request from {} ({}): serving {} file(s)",
                requester_name,
                requester_ip,
                targets.len()
            );
            // 保留目录层级: 用户在对端 2026/报表/1月.csv 点了取回,
            // 本机也必须落在 2026/报表/1月.csv, 否则"进子文件夹选文件"是假的。
            if let Err(e) = client
                .send_files_as(
                    &requester_ip,
                    requester_port,
                    &requester_id,
                    &requester_name,
                    targets,
                )
                .await
            {
                tracing::error!("反向推送至 {} 失败: {}", requester_name, e);
            }
        });

        info!("Pull request accepted: {}", message);
        Ok(())
    }

    /// 归一化目录浏览的子路径。
    ///
    /// 只允许落盘目录之下的普通子目录, 拒绝:
    /// - `..` 任何形式的路径穿越
    /// - 绝对路径与盘符
    /// - 以 `.` 开头的隐藏目录 (与列表过滤保持一致)
    /// - 超过 `MAX_BROWSE_DEPTH` 层的深路径
    ///
    /// 返回使用 `/` 分隔的相对路径; 根目录返回空串。
    fn normalize_sub_path(raw: &str) -> Result<String> {
        let trimmed = raw.trim().replace('\\', "/");
        if trimmed.is_empty() || trimmed == "." || trimmed == "/" {
            return Ok(String::new());
        }
        if trimmed.starts_with('/') {
            return Err(FeisuoError::Security("浏览路径必须是相对路径".into()));
        }
        if trimmed.contains(':') {
            return Err(FeisuoError::Security("浏览路径不得包含盘符".into()));
        }
        let mut parts: Vec<&str> = Vec::new();
        for seg in trimmed.split('/') {
            match seg {
                "" | "." => continue,
                ".." => {
                    return Err(FeisuoError::Security(
                        "浏览路径不得包含上级目录 (..)".into(),
                    ))
                }
                s if s.starts_with('.') => {
                    return Err(FeisuoError::Security(
                        "浏览路径不得进入隐藏目录".into(),
                    ))
                }
                s => parts.push(s),
            }
        }
        if parts.is_empty() {
            return Ok(String::new());
        }
        if parts.len() > MAX_BROWSE_DEPTH {
            return Err(FeisuoError::Security(format!(
                "浏览路径层级超过 {} 层上限",
                MAX_BROWSE_DEPTH
            )));
        }
        Ok(parts.join("/"))
    }

    /// 把归一化后的子路径解析成落盘目录之下的真实路径, 并再次确认没有逃逸。
    fn resolve_browse_dir(receive_dir: &std::path::Path, rel: &str) -> Result<std::path::PathBuf> {
        let mut dir = receive_dir.to_path_buf();
        if !rel.is_empty() {
            for seg in rel.split('/') {
                dir.push(seg);
            }
        }
        // 二次确认: 拼接后的路径必须仍在落盘根目录之内。
        // normalize_sub_path 已经挡掉了 `..`, 这里是纵深防御 ——
        // 万一以后有人改了归一化逻辑, 也不会立刻变成任意目录读取。
        //
        // 必须用 `is_within` 而不是手写 canonicalize+starts_with:
        // Windows 上 canonicalize 返回带 `\\?\` 前缀的逐字路径, 而目标
        // 子目录不存在时又会回落成普通写法, 两者前缀不同 → 永远判 false
        // → 浏览任何尚未创建的子目录都被误报成"逃逸"。详见 storage::paths。
        if !crate::storage::is_within(&dir, receive_dir) {
            return Err(FeisuoError::Security(
                "浏览路径逃逸出落盘目录, 已拒绝".into(),
            ));
        }
        let probe = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if probe.exists() && !probe.is_dir() {
            return Err(FeisuoError::Protocol("目标不是目录".into()));
        }
        Ok(dir)
    }

    /// 从绝对路径推出卷标识。
    ///
    /// Windows 取盘符（`"C:"`）；其它平台没有盘符概念，
    /// 统一返回 `"local"`（Android 侧由 SAF 虚拟卷体系另行映射）。
    fn volume_of(path: &std::path::Path) -> String {
        let s = path.to_string_lossy();
        let bytes = s.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            return s[..2].to_ascii_uppercase();
        }
        "local".to_string()
    }

    /// 取相对路径上一级; 已在根目录时返回空串。
    fn parent_of(rel: &str) -> String {
        match rel.rfind('/') {
            Some(i) => rel[..i].to_string(),
            None => String::new(),
        }
    }

    /// 列举目录内容。返回 `(条目, 是否被条数上限截断, 被安全清单隐藏的条数)`。
    ///
    /// 截断是必须的: 落盘目录里堆几万个文件时, 完整序列化会超过
    /// `MAX_JSON_FRAME`, 对端读帧直接被拒, 界面表现为"设备在线但穿梭报错"。
    ///
    /// ## 收件目录**必须**豁免强制排除清单
    ///
    /// `MANDATORY_DENY` 里有 `C:\Users\*\AppData`（凭据与系统数据），
    /// 而飞梭**自己的默认收件目录**恰好是 `%LOCALAPPDATA%\feisuo\inbox`，
    /// 就在 AppData 之下。
    ///
    /// 于是默认配置下：鉴权判 `Allow`（那条路不看清单），但列举时每一条
    /// 都被 `is_mandatory_denied` 过滤掉 —— **穿梭右栏永远是空的**，
    /// 而界面报的是"该目录下没有可发送的文件"。用户看到的是一个空文件夹，
    /// 完全无从知道是安全策略把它藏了。
    ///
    /// 这条豁免不是"开后门"：收件目录是**用户自己指定用来收文件的地方**，
    /// D2 定的语义就是"允许写，落点强制在收件目录"—— 它是唯一一处
    /// 按设计就该被对端看见的目录。排除清单针对的是"对方浏览我的磁盘"，
    /// 而收件目录**本来就是给对方看的**。
    ///
    /// 真实卷浏览（C:\Users\me\AppData\...）仍然照常隐藏，那才是清单的用途。
    pub(crate) fn list_dir_entries(
        dir: &std::path::Path,
        receive_dir: &std::path::Path,
        scope: &crate::security::AccessScope,
    ) -> (Vec<RemoteFileEntry>, bool, u32) {
        let (list, total, hidden) = Self::read_and_sort_dir(dir, dir, receive_dir, Some(scope));
        let truncated = total > list.len();
        (list, truncated, hidden)
    }

    /// 分页列举（§7.3 第 1 点）。返回 `(本页条目, 总条目数, 被安全清单隐藏的条数)`。
    ///
    /// ## 为什么先全读再切片, 而不是靠迭代器 `skip(offset).take(limit)`
    ///
    /// 两个原因：
    /// 1. **稳定顺序**。目录项的返回顺序由文件系统决定（NTFS 的 B-tree、
    ///    ext4 的 hash、ext4 的 dirent 顺序都不一样），不排序的话"第 2 页"
    ///    和"第 1 页"会重复或漏掉同一批文件 —— 用户表现为"往下滚有重复文件"。
    /// 2. **分页必须在排序之后**。`skip/take` 作用在未排序的原始顺序上，
    ///    每次请求的分界点都在漂移。
    ///
    /// 代价是一个大目录要全量 `read_dir` + `metadata`。这是**每次请求一次**，
    /// 而不是每条记录一次；实测万级目录的 `read_dir` 在 10ms 量级，
    /// 相对一次网络往返可以忽略。
    pub(crate) fn list_dir_entries_paged(
        dir: &std::path::Path,
        offset: usize,
        limit: usize,
        receive_dir: &std::path::Path,
        scope: &crate::security::AccessScope,
    ) -> (Vec<RemoteFileEntry>, u32, u32) {
        let (mut list, total, hidden) = Self::read_and_sort_dir(dir, dir, receive_dir, Some(scope));
        let total_u32 = total.min(u32::MAX as usize) as u32;
        if offset >= list.len() {
            return (Vec::new(), total_u32, hidden);
        }
        list = list.split_off(offset);
        list.truncate(limit);
        (list, total_u32, hidden)
    }

    /// 读全目录 + 排序（目录在前, 同类按名）。
    /// 返回 `(全部条目, 过滤后总数, 因安全清单被隐藏的条数)`。
    ///
    /// `abs_base` = 本目录的**绝对路径**，用于把条目名拼成绝对路径后
    /// 套用强制排除清单。真实卷浏览下这是"点进去会被拒"的目录，
    /// 列表里**必须直接不出现**（§8.2）—— 灰掉或只靠点击后报错都等于
    /// 泄露"这里有东西"，而且用户会反复点一个必然失败的条目。
    ///
    /// `receive_dir` 下的条目**豁免**该清单，理由见 `list_dir_entries`。
    ///
    /// 隐藏条数必须一并返回：静默隐藏会让用户以为目录是空的，
    /// 而"这里是空的"与"这里的东西被安全策略藏了"需要的后续动作
    /// 完全不同（前者去放文件，后者去改访问范围）。
    fn read_and_sort_dir(
        dir: &std::path::Path,
        abs_base: &std::path::Path,
        receive_dir: &std::path::Path,
        scope: Option<&crate::security::AccessScope>,
    ) -> (Vec<RemoteFileEntry>, usize, u32) {
        let mut list = Vec::new();
        let mut hidden: u32 = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // 隐藏的内部文件 (如暂存目录 `.feisuo-incoming`) 不应出现
                    if name.starts_with('.') {
                        continue;
                    }
                    let child_abs = abs_base.join(&name);
                    let child_str = child_abs.to_string_lossy().to_string();
                    let in_receive = crate::storage::is_within(&child_abs, receive_dir);
                    // 访问范围过滤（收件目录豁免，由 scope.can_read 统筹裁决全部/黑名单/白名单）
                    if !in_receive {
                        if let Some(sc) = scope {
                            let vol = Self::volume_of(&child_abs);
                            if !sc.can_read(&vol, &child_str) {
                                hidden += 1;
                                tracing::debug!(
                                    "列举时按该设备的访问范围隐藏: {}",
                                    child_str
                                );
                                continue;
                            }
                        }
                    }
                    let is_dir = meta.is_dir();
                    let size = if is_dir { 0 } else { meta.len() };
                    let modified = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| {
                            chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                                .unwrap_or_default()
                                .format("%Y-%m-%d %H:%M")
                                .to_string()
                        })
                        .unwrap_or_else(|| "-".into());

                    list.push(RemoteFileEntry {
                        name,
                        is_dir,
                        size,
                        size_formatted: if is_dir {
                            "文件夹".to_string()
                        } else {
                            format_bytes(size)
                        },
                        modified,
                    });
                }
            }
        }
        // 先排序再截断, 否则"前 N 条"随文件系统返回顺序漂移, 同一个目录
        // 两次打开看到的文件集不一样, 用户会以为是随机丢文件。
        list.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        let total = list.len();
        // 旧路径（1.x 对端 / 无分页）仍受 MAX_BROWSE_ENTRIES 保护
        if total > MAX_BROWSE_ENTRIES {
            list.truncate(MAX_BROWSE_ENTRIES);
        }
        (list, total, hidden)
    }

    async fn handle_pairing(
        stream: &mut TcpStream,
        identity: &Arc<DeviceIdentity>,
        config: &Arc<RwLock<AppConfig>>,
        trust_store: &Arc<TrustStore>,
        peer_ip: &str,
    ) -> Result<()> {
        let pair_req: PairRequest = read_json_frame(stream, "配对请求").await?;
        let local_device_name = config.read().await.device_name.clone();

        // 配对码爆破限速
        if trust_store.is_pin_locked(peer_ip) {
            return Self::send_pair_response(
                stream,
                identity,
                local_device_name,
                false,
                Some("配对码尝试次数过多, 请稍后再试".into()),
                "",
            )
            .await;
        }

        // device_id 必须由公钥派生, 绝不允许客户端自称
        let derived_id = match DeviceIdentity::device_id_from_pubkey_hex(&pair_req.public_key_hex) {
            Ok(id) => id,
            Err(e) => {
                return Self::send_pair_response(
                    stream,
                    identity,
                    local_device_name,
                    false,
                    Some(e.to_string()),
                    "",
                )
                .await
            }
        };
        if derived_id != pair_req.device_id {
            return Self::send_pair_response(
                stream,
                identity,
                local_device_name,
                false,
                Some("设备指纹与公钥不匹配, 配对被拒绝".into()),
                "",
            )
            .await;
        }

        if !trust_store.verify_pair_pin(&pair_req.pin_code) {
            let attempts = trust_store.register_pin_failure(peer_ip);
            let msg = if attempts >= MAX_PIN_ATTEMPTS {
                "配对码错误次数过多, 已临时锁定该地址".to_string()
            } else {
                "配对码错误或已失效".to_string()
            };
            return Self::send_pair_response(stream, identity, local_device_name, false, Some(msg), "").await;
        }
        trust_store.clear_pin_failures(peer_ip);

        // 配对世代（§14.5.1）：**双方必须拿到同一个串**，否则解除配对无法
        // 防重放。发起方带了就用它；没带（旧对端）就本地生成一个并回填 ——
        // 这样"新发起方 ↔ 旧接收方"仍然有共同世代，而
        // "旧发起方 ↔ 新接收方"会由旧发起方存空串，于是双向解除配对
        // **明确失败并提示升级**，而不是静默只改一边。
        let pairing_epoch = if pair_req.pairing_epoch.trim().is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            pair_req.pairing_epoch.trim().to_string()
        };

        let dev = crate::security::TrustedDevice {
            device_id: pair_req.device_id.clone(),
            device_name: if pair_req.device_name.trim().is_empty() {
                "未知设备".to_string()
            } else {
                pair_req.device_name.trim().to_string()
            },
            public_key_hex: pair_req.public_key_hex.clone(),
            last_ip: peer_ip.to_string(),
            bound_at: chrono::Utc::now().to_rfc3339(),
            is_trusted: true,
            // 配对完成默认给"永久信任"（§2.5：配对弹窗会让用户在
            // 永久 / 每次需匹配码之间二选一, 这是默认值）
            trust_level: crate::security::TrustLevel::Permanent,
            visible: true,
            last_seen_at: chrono::Utc::now().timestamp(),
            pairing_epoch: pairing_epoch.clone(),
        };

        // 绝不覆盖既有身份的公钥 (防身份顶替)
        match trust_store.bind_device(&dev)? {
            BindOutcome::KeyConflict => {
                warn!(
                    "拒绝配对: 设备 {} 已绑定到另一把公钥, 请先在本机解除信任后重试",
                    dev.device_id
                );
                return Self::send_pair_response(
                    stream,
                    identity,
                    local_device_name,
                    false,
                    Some("该设备指纹已绑定其他密钥, 请先在受信列表中解除后重新配对".into()),
                    "",
                )
                .await;
            }
            BindOutcome::Added | BindOutcome::Refreshed => {}
        }

        Self::send_pair_response(
            stream,
            identity,
            local_device_name.clone(),
            true,
            None,
            &pairing_epoch,
        )
        .await?;
        info!(
            "Successfully paired device: {} ({})",
            dev.device_name, dev.device_id
        );
        Ok(())
    }

    async fn send_pair_response(
        stream: &mut TcpStream,
        identity: &Arc<DeviceIdentity>,
        local_device_name: String,
        success: bool,
        error_msg: Option<String>,
        pairing_epoch: &str,
    ) -> Result<()> {
        let resp = PairResponse {
            success,
            device_id: identity.device_id.clone(),
            device_name: local_device_name,
            public_key_hex: identity.public_key_hex(),
            error_msg,
            // 必须回填: 发起方靠它拿到共同世代, 从而**双向**解除配对。
            // 失败路径也回填同一个值 —— 否则"配对失败后重试"会每次
            // 生成一个新世代, 而发起方存的是第一次那个, 两边对不上。
            pairing_epoch: pairing_epoch.to_string(),
        };
        write_json_frame(stream, &resp).await
    }
}

/// 把卷标识与相对路径拼成用于范围判定的绝对路径（仅比较用, 不落盘）。
///
/// Windows 盘符形式是 `C:` + `/子路径`；其它平台的 `local` 直接用 `/子路径`。
/// **这个字符串只喂给 `AccessScope::can_read` 做比较, 绝不是用来 open 的路径** ——
/// 真正的落盘解析仍然走 `PathManager::resolve_browse_dir` 的逐段穿越校验。
/// 暂存区守卫：Drop 时清空 staging 目录。
///
/// 存在的唯一理由是**失败路径有十几条**，每条都手写清理必然漏一条，
/// 而漏一条就意味着用户在收件目录里看到一个截断的半成品文件，
/// 下一次重传又会生成 `name (1).ext` —— 残缺的那份永久留着（§5.2）。
/// 用 RAII 把"离开作用域 = 清干净"变成语言级保证。
impl TransferServer {
    /// 处理"撤销本次传输"（直发的 5 秒撤销窗口，§5.2）。
///
/// ## 语义边界
///
/// 撤销**只在接收端还没开始落盘时有效**。一旦第一块数据落进
/// staging 目录，就撤不掉了 —— 只能由用户在目标设备上删除。
/// 这里如实回 `success: false`，**不假装撤销成功**。
/// 处理对端发来的解除配对（§14.5）。
///
/// ## 这是"双方都有变成未信任"里**对端那半边**
///
/// 旧实现里解除配对是纯本地的 `remove_device`，于是 A 解除之后
/// B 仍然是永久信任并继续静默收 A 的文件，而两边界面都显示
/// 「永久信任」—— 用户完全无从察觉。
///
/// ## 拒绝时的应答一律走 `refuse_by_msg_type`
///
/// 否则对端读到的又是 `early eof`，也就是本轮最开始那个症状。
/// 处理对端发来的解除配对（§14.5）。
///
/// ## 这是"双方都有变成未信任"里**对端那半边**
///
/// 旧实现里解除配对是纯本地的 `remove_device`，于是 A 解除之后
/// B 仍然是永久信任并继续静默收 A 的文件，而两边界面都显示
/// 「永久信任」—— 用户完全无从察觉。
///
/// ## 拒绝时也必须回应答
///
/// 否则对端读到的又是 `early eof` —— 也就是本轮最开始那个症状。
async fn handle_unpair(
    stream: &mut TcpStream,
    identity: &Arc<DeviceIdentity>,
    config: &Arc<RwLock<AppConfig>>,
    trust_store: &Arc<TrustStore>,
    peer_ip: &str,
) -> Result<()> {
    /// 回一条 `UnpairResponse` 并结束本次连接。
    async fn reply(
        stream: &mut TcpStream,
        identity: &Arc<DeviceIdentity>,
        receiver_name: &str,
        success: bool,
        reason_code: &str,
        message: String,
        pairing_epoch: String,
    ) -> Result<()> {
        let resp = UnpairResponse {
            success,
            target_id: identity.device_id.clone(),
            receiver_name: receiver_name.to_string(),
            message,
            pairing_epoch,
            reason_code: reason_code.to_string(),
        };
        let _ = write_json_frame(stream, &resp).await;
        Ok(())
    }

    let receiver_name = config.read().await.device_name.clone();

    let req: UnpairRequest = match read_json_frame(stream, "解除配对请求").await {
        Ok(v) => v,
        Err(e) => {
            // 解析失败时连设备号都没有，无从回应答，只能关连接。
            warn!("解除配对请求解析失败（来自 {}）: {}", peer_ip, e);
            return Err(e);
        }
    };

    if req.target_id != identity.device_id {
        return reply(
            stream,
            identity,
            &receiver_name,
            false,
            "target_mismatch",
            format!(
                "解除配对目标不符：本机是 {}，请求指向 {}",
                identity.device_id, req.target_id
            ),
            String::new(),
        )
        .await;
    }

    // **不能伪造**：验签用的公钥必须来自本机信任库里**已绑定**的那一把,
    // 而不是请求自带的公钥 —— 否则任何人自称 device_id 就能拆掉别人的关系。
    let Some(pubkey) = trust_store.get_device_pubkey(&req.initiator_id)? else {
        return reply(
            stream,
            identity,
            &receiver_name,
            false,
            "not_paired",
            format!("本机没有与设备 {} 的绑定记录", req.initiator_id),
            String::new(),
        )
        .await;
    };

    if req.version != PROTOCOL_VERSION {
        return reply(
            stream,
            identity,
            &receiver_name,
            false,
            "version_mismatch",
            format!(
                "协议版本不兼容：对端 {} / 本机 {}",
                req.version, PROTOCOL_VERSION
            ),
            String::new(),
        )
        .await;
    }

    // 先验签再比世代。反过来会让未验签的帧消耗世代比对的机会,
    // 并把"签名无效"这条更严重的问题被"世代不符"掩盖。
    if !DeviceIdentity::verify(&pubkey, req.signing_payload().as_bytes(), &req.signature)? {
        warn!(
            "解除配对帧签名校验失败, 已忽略 {} 自称的设备 {}",
            peer_ip, req.initiator_id
        );
        return reply(
            stream,
            identity,
            &receiver_name,
            false,
            "signature_invalid",
            "解除配对请求签名校验失败, 已拒绝".into(),
            String::new(),
        )
        .await;
    }

    if let Err(e) = Self::check_freshness(req.timestamp) {
        return reply(
            stream,
            identity,
            &receiver_name,
            false,
            "stale",
            e.to_string(),
            String::new(),
        )
        .await;
    }

    let outcome = trust_store.apply_peer_unpair(&req.initiator_id, &req.peer_epoch)?;
    // 用已有的 `TrustRevoked` 而不是新增一种：语义完全一致（信任被解除），
    // 而多一种 kind 只会在安全事件列表里多一行用户看不懂的分类。
    // "是谁发起的"由 detail 承担。
    let _ = trust_store.add_security_event(
        crate::security::SecurityEventKind::TrustRevoked,
        &req.initiator_id,
        peer_ip,
        &format!("**对端要求**解除配对: {:?}", outcome),
    );

    match outcome {
        crate::security::UnpairOutcome::Applied => {
            info!(
                "已按对端 {} 的要求解除配对（{}）",
                req.initiator_id, peer_ip
            );
            reply(
                stream,
                identity,
                &receiver_name,
                true,
                "applied",
                "已解除配对".into(),
                String::new(),
            )
            .await
        }
        // 幂等：本来就没配对，回成功。报错会让"两边都点一次"里
        // 有一边看到失败，而那个失败不含任何信息。
        crate::security::UnpairOutcome::AlreadyUnpaired
        | crate::security::UnpairOutcome::NotPaired => {
            reply(
                stream,
                identity,
                &receiver_name,
                true,
                "already_unpaired",
                "本机已是未信任状态".into(),
                String::new(),
            )
            .await
        }
        // 世代不符：极可能是**重放**。明确告诉对方"你的帧过期了",
        // 让它不要重试 —— 本地状态一个字都不动。
        crate::security::UnpairOutcome::EpochMismatch => {
            let current = trust_store.pairing_epoch(&req.initiator_id)?.unwrap_or_default();
            reply(
                stream,
                identity,
                &receiver_name,
                false,
                "epoch_mismatch",
                "该解除配对请求已过期（本机已与该设备重新绑定），已忽略".into(),
                current,
            )
            .await
        }
        // 无共同世代（旧对端）：**不**盲目降级 —— 那等于承诺了一个
        // 我们无法验证来源的双向解除。明确失败，让用户知道要升级。
        crate::security::UnpairOutcome::NoSharedEpoch => {
            reply(
                stream,
                identity,
                &receiver_name,
                false,
                "no_shared_epoch",
                "本机与该设备没有共同配对世代（对方可能版本过旧），\
                 无法安全地双向解除配对; 请先升级对方"
                    .into(),
                String::new(),
            )
            .await
        }
    }
}

async fn handle_cancel(
    stream: &mut TcpStream,
    identity: &Arc<DeviceIdentity>,
    config: &Arc<RwLock<AppConfig>>,
    trust_store: &TrustStore,
    peer_ip: &str,
    // `cancelled`: 传输循环每分块轮询的撤销集合（`TransferServer::cancelled`）
    // `committed`: 已经 commit 落盘、撤不掉的 `transfer_id`
    cancelled: &Arc<Mutex<std::collections::HashMap<String, Instant>>>,
    committed: &Arc<Mutex<std::collections::HashSet<String>>>,
) -> Result<()> {
    let req: CancelRequest = read_json_frame(stream, "撤销请求").await?;
    let receiver_name = config.read().await.device_name.clone();
    let mut resp = CancelResponse {
        success: false,
        receiver_id: identity.device_id.clone(),
        receiver_name,
        message: String::new(),
    };

    if req.version != PROTOCOL_VERSION {
        resp.message = format!(
            "协议版本不兼容: 对端 {} / 本机 {}",
            req.version, PROTOCOL_VERSION
        );
        let _ = write_json_frame(stream, &resp).await;
        return Ok(());
    }
    if let Err(e) = Self::check_freshness(req.timestamp) {
        // 两台机器时钟不同步时，撤销会毫无征兆地变成「对方已关闭连接」。
        // 用户分不清是撤销没送到，还是该去校时钟。
        resp.message = e.to_string();
        let _ = write_json_frame(stream, &resp).await;
        return Err(e);
    }

    // 签名必须验：否则局域网内任何人只要知道 transfer_id 就能
    // 撤销别人的传输（DoS）。未配对设备没有绑定公钥 —— 它的传输
    // 本来还停在审批阶段, 撤销一个"还没被批准的请求"没有风险。
    let challenge = format!("{}:{}", req.nonce, req.timestamp);
    let signature_ok = match trust_store.get_device_pubkey(&req.sender_id)? {
        Some(pk) => DeviceIdentity::verify(&pk, challenge.as_bytes(), &req.signature)?,
        None => true,
    };
    if !signature_ok {
        resp.message = "撤销请求签名校验失败".into();
        let _ = write_json_frame(stream, &resp).await;
        return Ok(());
    }

    // 登记撤销：**不碰 staging 目录**。
    //
    // 早先在这里 `remove_dir_all(staging_base)`，那是错的：
    // 正在跑的那条传输连接不知情, 会继续写、继续校验、最后照常 commit ——
    // 用户点了"撤销"、界面回了"已撤销", **文件照样落进收件目录**。
    // 而 Windows 上文件正被打开时 `remove_dir_all` 还会直接失败,
    // 那行写的是 `let _ =`（忽略错误）⇒ 连"删掉"都是假的。
    //
    // 现在只登记, 由**传输循环**在下一个分块边界发现并中断,
    // 清理交给它自己的 `StagingGuard` —— 状态的**唯一所有者**
    // 必须是正在写它的那一方；任何"从旁边删掉"的尝试都会和写者打架。
    let already = {
        if req.transfer_id.is_empty() {
            true
        } else {
            cancelled
                .lock()
                .map(|mut s| {
                    s.insert(req.transfer_id.clone(), Instant::now()).is_some()
                })
                .unwrap_or(true)
        }
    };
    let committed = committed
        .lock()
        .map(|s| s.contains(&req.transfer_id))
        .unwrap_or(false);
    if committed {
        // 已 commit（文件已改名落到收件目录）⇒ 撤不掉, 如实说。
        // 判据不能用"staging 目录是否存在" —— 目录早就被 Guard 删了,
        // 而文件已经在收件目录里, 用目录判断会把"已落盘"误报成"已撤销"。
        resp.success = false;
        resp.message = "对方已开始落盘, 无法撤销; 请在收件目录里手动删除".into();
    } else {
        resp.success = true;
        resp.message = if already {
            "已撤销（本次为重复请求）".into()
        } else {
            "已撤销, 不会落盘".into()
        };
    }
    info!(
        "Cancel from {} ({}): transfer={} success={} committed={} repeated={}",
        req.sender_id, peer_ip, req.transfer_id, resp.success, committed, already
    );
    let _ = write_json_frame(stream, &resp).await;
    Ok(())
}
}

struct StagingGuard {
    root: std::path::PathBuf,
    /// 提交成功后置 false，让 Drop 不再删除（此时目录已被搬空）
    armed: bool,
    /// 是否因网络异常断开而保留半成品用于断点续传
    keep_for_resume: bool,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.armed || self.keep_for_resume {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.root) {
            // 目录不存在是最常见情况（提交成功路径已删过），不算错误
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("清理暂存目录 {} 失败: {}", self.root.display(), e);
            }
        }
    }
}

fn join_volume_path(volume: &str, rel: &str) -> String {
    let unified = rel.replace('\\', "/");
    let rel = unified.trim_start_matches('/');
    if volume == "local" {
        format!("/{}", rel)
    } else {
        format!("{}/{}", volume, rel)
    }
}
