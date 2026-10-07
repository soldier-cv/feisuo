use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4MB per chunk

/// TCP 首字节: 配对握手
pub const MSG_PAIR: u8 = 1;
/// TCP 首字节: 文件传输
pub const MSG_TRANSFER: u8 = 2;
/// TCP 首字节: 目录浏览 (双栏穿梭右栏)
pub const MSG_BROWSE: u8 = 3;
/// TCP 首字节: 请求对端把指定文件推送回本机 (双栏穿梭左栏)
pub const MSG_PULL: u8 = 4;
/// TCP 首字节: 发送方撤销本次传输 (直发的 5 秒撤销窗口, §5.2)
pub const MSG_CANCEL: u8 = 5;

/// 解除配对（§14.5）。
///
/// **为什么必须有一条网络消息**：`remove_device` 是纯本地的，
/// 于是「一方解除配对」之后只有一边变成未信任，而**没解除的那一边
/// 仍然是永久信任并继续静默收文件** —— 而且两边界面都显示「永久信任」，
/// 用户无从察觉。需求是「一方解除配对，**双方**都有变成不信任」。
pub const MSG_UNPAIR: u8 = 6;

/// 单个 JSON 控制帧 (握手 / 清单 / 分块头) 的最大字节数。
/// 不加限制时, 5 个字节就能让接收端按对端声明的长度分配数 GiB 内存。
pub const MAX_JSON_FRAME: usize = 1024 * 1024; // 1 MiB
/// 单个批次允许携带的最大文件数
pub const MAX_FILES_PER_BATCH: usize = 2000;
/// 单个文件允许的最大字节数 (64 GiB)
pub const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024 * 1024;
/// 握手时间戳允许的最大偏差, 超出视为重放并拒绝
pub const MAX_HANDSHAKE_SKEW_SECS: i64 = 120;

// ===========================================================================
// 能力位图（§7.3）
// ===========================================================================

/// 能力位。用位图而不是版本号比较，原因见 [`CAPS_HINT`]。
pub mod caps {
    /// 基本传输（4MiB 分块 + BLAKE3）—— 1.x 全部具备
    pub const TRANSFER_BASIC: u32 = 1 << 0;
    /// 目录浏览（1.x 具备，但只支持收件目录内的相对路径）
    pub const BROWSE: u32 = 1 << 1;
    /// 取回（1.x 具备）
    pub const PULL: u32 = 1 << 2;
    /// **真实卷浏览**（`volume` + 绝对路径 + 分页）—— 2.x 才有
    pub const BROWSE_VOLUMES: u32 = 1 << 3;
    /// 目录递归传输
    pub const FOLDER_TRANSFER: u32 = 1 << 4;
    /// 取回时可指定落点（`dest_sub_path`）
    pub const PULL_DEST: u32 = 1 << 5;
    /// 可访问范围（`access_scope`）约束
    pub const ACCESS_SCOPE: u32 = 1 << 6;
    /// 会话授权（`session` 等级 / 扫码授权）
    pub const SESSION_GRANT: u32 = 1 << 7;
    /// 撤销控制帧
    pub const CANCEL: u32 = 1 << 8;
    /// **断点续传**：清单应答（`ManifestAck`）+ 文件级跳过
    pub const RESUME: u32 = 1 << 9;
}

/// 本机能力集。
pub fn local_caps() -> u32 {
    caps::TRANSFER_BASIC
        | caps::BROWSE
        | caps::PULL
        | caps::BROWSE_VOLUMES
        | caps::FOLDER_TRANSFER
        | caps::PULL_DEST
        | caps::ACCESS_SCOPE
        | caps::SESSION_GRANT
        | caps::CANCEL
        | caps::RESUME
}

/// 1.x 客户端的能力集（用于协商降级）。
pub fn legacy_caps() -> u32 {
    caps::TRANSFER_BASIC | caps::BROWSE | caps::PULL
}

/// 取两端能力交集。
pub fn negotiate_caps(ours: u32, theirs: u32) -> u32 {
    ours & theirs
}

/// 能力位的人类可读列表（写进诊断日志，便于事后分析"为什么没走新路径"）。
pub fn describe_caps(c: u32) -> String {
    let mut names: Vec<&str> = Vec::new();
    let table: &[(u32, &str)] = &[
        (caps::TRANSFER_BASIC, "transfer"),
        (caps::BROWSE, "browse"),
        (caps::PULL, "pull"),
        (caps::BROWSE_VOLUMES, "browse_volumes"),
        (caps::FOLDER_TRANSFER, "folder"),
        (caps::PULL_DEST, "pull_dest"),
        (caps::ACCESS_SCOPE, "scope"),
        (caps::SESSION_GRANT, "session"),
        (caps::CANCEL, "cancel"),
        (caps::RESUME, "resume"),
    ];
    for (bit, name) in table {
        if c & bit != 0 {
            names.push(name);
        }
    }
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join("+")
    }
}

/// 说明为什么用能力位图而不是直接比版本号。
///
/// 旧实现在版本不同就 `协议版本不兼容` **直接拒绝**（三处）。
/// 一升版，1.x 与 2.x 彻底不通 —— 而"多台设备同步"恰恰要求
/// 新旧版本能在同一网络里共存一段时间（用户不会为了升级而停用）。
/// 所以改成：版本只用于**日志与诊断**，真正的互通判断看能力交集。
pub const CAPS_HINT: &str = "版本号不用于互通判断; 用 caps 位图取交集, 只在用到对方不支持的能力时才报错";
/// 传输端口允许的最大并发连接数, 防御连接耗尽型 DoS
pub const MAX_CONCURRENT_CONNECTIONS: usize = 32;
/// 传输帧的读写超时
pub const IO_TIMEOUT_SECS: u64 = 30;
/// 配对码同一 IP 允许的最大连续失败次数
pub const MAX_PIN_ATTEMPTS: u32 = 5;
/// 配对码失败后的锁定时长
pub const PIN_LOCKOUT_SECS: i64 = 60;
/// 单次目录浏览最多返回的条目数。
/// 落盘目录里堆了几万个文件时, 不截断就会拼出超过 MAX_JSON_FRAME 的应答,
/// 对端读帧直接被判为超限拒绝 —— 表现为"设备在线但穿梭右栏一直报错"。
pub const MAX_BROWSE_ENTRIES: usize = 1000;
/// 目录浏览允许的最大相对路径深度 (防止 a/a/a/... 无限深)
pub const MAX_BROWSE_DEPTH: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeaconPacket {
    pub version: u32,
    pub device_id: String,
    pub device_name: String,
    pub os_type: String,
    pub transfer_port: u16,
    pub timestamp: i64,
    /// 对全部公开字段 + nonce 的 Ed25519 签名 (hex)。
    /// 缺少它, 局域网任意主机都能冒充"已信任设备"刷信标并污染 last_ip。
    #[serde(default)]
    pub nonce: String,
    #[serde(default)]
    pub signature: String,
    /// 能力位图（§7.3）。0 = 老版本客户端。
    #[serde(default)]
    pub caps: u32,
    /// 应用版本（仅用于日志与诊断，**不参与互通判断**，见 [`CAPS_HINT`]）。
    #[serde(default)]
    pub app_version: String,
}

impl BeaconPacket {
    /// 待签名内容: 所有公开字段的规范化拼接, 顺序固定。
    ///
    /// ⚠️ `caps` / `app_version` **不在签名载荷里** ——
    /// 老版本客户端算出的是 7 段格式，签名内容若随字段增减而变化，
    /// 新旧两版就互相验不过签，等于把降级路径堵死。
    /// 这两个字段只影响"能做什么"，不影响"是谁"。
    pub fn signing_payload(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.version,
            self.device_id,
            self.device_name,
            self.os_type,
            self.transfer_port,
            self.timestamp,
            self.nonce
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeRequest {
    pub version: u32,
    pub sender_id: String,
    pub sender_name: String,
    pub timestamp: i64,
    pub nonce: String,
    pub signature: String, // Ed25519 signature over nonce + timestamp
    /// 发送方公钥 (hex)。
    /// 首次传输时接收方还没有对方公钥, 审批通过后要靠它建立长期信任;
    /// 缺少它则"审批通过即信任"永远无法生效。
    #[serde(default)]
    pub sender_public_key_hex: String,
    #[serde(default)]
    pub file_count: u32,
    #[serde(default)]
    pub total_size: u64,
    #[serde(default)]
    pub first_file_name: String,
    /// 接收方**收件目录之下**的落点子目录（§7.7）。空串 = 收件根。
    ///
    /// ## 为什么发送方向需要它
    ///
    /// 「双栏穿梭」右栏是对方真实的文件系统，用户在某一层目录里选中文件
    /// 点「穿梭」，期望落在**对方地址栏当前所在的那一层**，而不是每次都被
    /// 扔进收件根。取回方向早就有 `dest_sub_path`（`PullRequest`），
    /// 发送方向却没有 —— 同一个界面的两个方向落点规则不一致。
    ///
    /// ## 安全边界：它只能让写入发生在**收件目录之内**
    ///
    /// 「允许写」的语义是"允许写，但落点强制在收件目录"（§2.4），
    /// 所以这个字段**不能**用来让发送方指定收件目录之外的任何位置。
    /// 接收端的校验与取回方向**完全相同**（`normalize_sub_path` 拒绝
    /// `..`/绝对路径/盘符/隐藏目录/超深 → 逐段创建 → `is_within(receive_dir)`），
    /// 外加强制排除的敏感路径清单。
    ///
    /// 因此 UI 侧的规则是：对方地址栏在**收件目录镜像**里时可以直接传；
    /// 在浏览对方**真实卷**（C:/D:）时不能传，退回收件根并说明原因 ——
    /// 那属于"写到收件目录之外"，是另一个决策（D2），不由这个字段顺带打开。
    #[serde(default)]
    pub dest_sub_path: String,
    /// **本次匹配码**（"每次匹配码"模式，§2.3）—— 发起方**出示**的码。
    ///
    /// ## 谁生成、谁核对
    ///
    /// **接收方生成并显示在自己的审批窗口上**，发起方把看到的码敲进
    /// 自己的界面，由接收端与它本进程持有的那一份比对。
    /// 第一次请求这里是空串。
    ///
    /// ## 为什么方向是"接收方出码"
    ///
    /// 「每次匹配码」要回答的是"发起方**现在真的在吗**"，而不是
    /// "发起方是不是它自称的那台机器"——后者由握手里的 Ed25519
    /// 签名回答，不需要码。
    ///
    /// 不信任发起方的是**接收方**，所以该由接收方出题、发起方应答。
    /// 反过来（发起方出码、接收方抄）时，接收方手上没有任何独立信息：
    /// 它核对的数字是发起方自己给的，等于让接收方去验证对方的自述。
    ///
    /// 方向反过来还直接决定了这道门有没有用：
    /// **接收方出码 ⇒ 码只出现在接收方屏幕上 ⇒ 被入侵的旧设备
    /// 读不到它 ⇒ 无法在不惊动接收方的情况下完成传输。**
    /// 发起方出码则相反：那台设备自己就能填上，这道门形同虚设。
    ///
    /// ## 威胁模型（诚实版）
    ///
    /// 挡得住的是**无人值守的自动发起方**（被入侵的设备、脚本循环投递）——
    /// 它读不到本机屏幕上的码。
    ///
    /// 挡不住的是"能看见你屏幕 / 能让你念数字"的社工 —— 但那种攻击
    /// 同样能直接骗你点"允许"，所以码并没有让情况更糟。
    /// **不要把它当成防社工的凭据。**
    #[serde(default)]
    pub grant_code: String,
}

/// 人工审批的**结果**（含传输码）。
///
/// 刻意与 [`ApprovalAction`] 分开：后者是"用户点了哪个按钮"（可枚举），
/// 这里是"按钮 + 这一次输入的码"（含用户数据）。把码塞进 enum 会让
/// `Copy` + `PartialEq` 全部失效，而那两个 trait 在审批管理器里被用到。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub action: ApprovalAction,
    /// 用户输入的本次传输码。仅 `AllowOnce` 且对端要求码时才有值。
    pub grant_code: String,
}

impl ApprovalDecision {
    pub fn simple(action: ApprovalAction) -> Self {
        Self {
            action,
            grant_code: String::new(),
        }
    }
}

/// 人工审批的动作（§2.4）。
///
/// 旧实现只有 `Allow` / `Reject` / `BlockPermanent`，其中 `Allow` 会
/// **直接把设备写进长期信任库**（`server.rs` 的 `is_trusted: true`）。
/// 于是"我点了这一次允许"被静默升级成"这台设备从此可永久静默投递"——
/// 用户没有选择权。新增 `AllowOnce` 与 `AllowAndTrust` 把它拆开。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAction {
    /// 只放行本次传输，**不写入长期信任**。界面的默认焦点。
    AllowOnce,
    /// 放行本次，并写入长期信任（`trust_level = permanent`）。
    AllowAndTrust,
    /// 放行本次，并给这台设备一张**同类操作**的短期授权（§2.3.1）。
    AllowWithGrant,
    /// 拒绝本次。
    Reject,
}

impl ApprovalAction {
    /// 解析来自宿主层（前端 / Android 通知按钮）的动作字符串。
    ///
    /// **必须向后兼容**：旧版前端仍在发 `"allow"` / `"block_permanent"`。
    /// 这里的映射刻意把旧 `"allow"` 映射到 [`ApprovalAction::AllowOnce`] ——
    /// 旧前端的"允许"按钮在语义上就是"这次允许"，不该继续静默建立长期信任。
    pub fn from_ui_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            // 旧别名
            "allow" | "allow_once" | "allowonce" => Some(ApprovalAction::AllowOnce),
            "allow_and_trust" | "allowandtrust" | "trust" => Some(ApprovalAction::AllowAndTrust),
            "allow_with_grant" | "allowwithgrant" | "grant" => Some(ApprovalAction::AllowWithGrant),
            "reject" | "deny" => Some(ApprovalAction::Reject),
            _ => None,
        }
    }

    /// 是否是"放行"类动作。
    ///
    /// 传输码只对放行有意义 —— 用户点了拒绝时拿码去比对纯属多余，
    /// 还会把"用户主动拒绝"这条更重要的信息冲淡。
    pub fn is_allow(&self) -> bool {
        matches!(
            self,
            ApprovalAction::AllowOnce
                | ApprovalAction::AllowAndTrust
                | ApprovalAction::AllowWithGrant
        )
    }

    /// 是否应当**额外写一张短期授权**（§2.3.1）。
    ///
    /// 刻意与 `is_allow` 分开：前者回答"能不能过"，后者回答
    /// "过完之后要不要记住"。合成一个判断的话，"写授权"这个
    /// **有副作用**的动作就会跟着每一个放行分支顺带发生 ——
    /// 而 `AllowOnce` 的语义恰恰是"只这一次"。
    pub fn wants_grant(&self) -> bool {
        matches!(self, ApprovalAction::AllowWithGrant)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub sender_ip: String,
    pub file_count: u32,
    pub total_size: u64,
    pub total_size_formatted: String,
    pub first_file_name: String,
    pub created_at: i64,
    /// 对端处于「每次匹配码」等级：本次审批**必须**由发起方出示匹配码，
    /// 只点「允许」不算通过。
    ///
    /// 存在的理由：这是用户的原始需求（"每设备可选永久信任或每次匹配码"）。
    /// 早先的实现让 `RequireGrant` 分支直接拒绝，而界面上**照样有**
    /// 「每次匹配码」这个开关 —— 给了开关却不生效比不给更糟：
    /// 用户会以为配好了，实际每次传输都被拒。
    #[serde(default)]
    pub requires_grant_code: bool,
    /// **本机（接收方）生成、显示给本机用户的**「本次匹配码」。
    ///
    /// 空串 = 本次审批不需要码。
    ///
    /// # 它绝不能出现在任何回给对端的帧里
    ///
    /// 一旦发出去，发起方就能自动填上，"有人在接收方旁边"这件事
    /// 就完全不存在了 —— 那正是这个等级唯一要保证的东西。
    /// 传输通道是明文的（本期不做传输层加密，D3），所以"不发出"是
    /// 唯一的保障。
    #[serde(default)]
    pub grant_challenge: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponse {
    pub success: bool,
    pub receiver_id: String,
    pub receiver_name: String,
    pub message: String,
    /// 接收方公钥: 发送方据此确认自己没把文件推给冒充者
    #[serde(default)]
    pub receiver_public_key_hex: String,
    /// 接收方对发送方握手 nonce+timestamp 的签名 (hex)
    #[serde(default)]
    pub receiver_signature: String,
    /// 接收方能力位。
    ///
    /// 握手应答里带 caps 是断点续传能安全降级的前提：发送方据此决定
    /// **要不要读清单应答**。1.x 对端不返回这一帧，读它会解析失败 ——
    /// 而"读失败就当续传"会在最坏情况下把数据发到一个不同步的协议上。
    #[serde(default)]
    pub caps: u32,
    /// 该设备处于「每次匹配码」等级，本次需要出示传输码。
    ///
    /// 发送方据此**保留待发队列并提示用户带上码重试**。
    /// `message` 里虽然也有说明，但那是给人看的文本；
    /// 程序要走对分支就得靠这个 bool。
    #[serde(default)]
    pub requires_grant_code: bool,
}

/// 一次断点续传里"已经完整落盘"的文件（`transfer_parts` 表的一行）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompletedPart {
    pub relative_path: String,
    /// 提交时的整文件 BLAKE3。发送方必须逐个比对 ——
    /// 文件名相同但内容变了（用户重新编辑过）时**不能**跳过。
    pub blake3_hash: String,
    pub file_size: u64,
    /// 本机实际提交路径（可能是 `报表 (1).csv`）。
    ///
    /// **只发给对端自己看, 不作为信任依据** —— 它由接收端决定,
    /// 用来告诉发送方"文件在这"。续传判定会**重新哈希该文件**，
    /// 不依赖这个字符串。
    #[serde(default)]
    pub committed_path: String,
}

/// 接收方在**清单应答**里告知"这些我已经有了"（断点续传，P1 ⑪）。
///
/// ## 流程位置
///
/// ```text
/// 发送方 --Handshake--> 接收方
/// 发送方 --Manifest-->  接收方   ← 接收方此刻查 transfer_parts
/// 发送方 <--ManifestAck-- 接收方  ← 这里回"缺哪些"
/// 发送方 只发缺失的文件
/// ```
///
/// ## 为什么不用"发送方先问"
///
/// 发送方问要额外一轮往返，而接收方在处理 Manifest 时**本来就要做校验**，
/// 顺手回一句省掉一整个 RTT。在 RTT 200ms 的链路上这一轮就是 200ms。
///
/// ## 安全：这份清单必须可信
///
/// 它决定"哪些文件不传"，所以一个被篡改的 `skipped` 列表能让对端
/// 谎称拥有某个文件 —— 实际用户那边是空的，且**没有任何校验会发现**
/// （因为根本没传那个文件）。
///
/// 对策：
/// 1. 只认**已配对且非 blocked** 的设备（`authorize` 已经保证）；
/// 2. 发送方对**每个 skipped 项**按清单里的 `blake3_hash` 去校验
///    本地目标文件（落盘后由接收方提交时记录，双方都验）；
/// 3. UI **必须**把"跳过 N 个文件（已存在）"明说，不能静默。
///
/// 第 2 条是真正兜底的那条 —— 信任对端的诚实不如信任密码学。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestAck {
    pub success: bool,
    pub receiver_id: String,
    pub receiver_name: String,
    pub message: String,
    /// 已完整落盘、可跳过的文件
    #[serde(default)]
    pub completed: Vec<CompletedPart>,
    /// 本次**实际要传**的文件下标（对应 `manifest.files` 的下标）。
    /// 接收方只按这个下标序列收数据。
    #[serde(default)]
    pub needed: Vec<u32>,
    /// 接收方为每个"needed 文件"分配的相对落盘路径。
    /// 与 `needed` 等长且顺序一致。
    ///
    /// 之所以让接收方分配而不是发送方按清单算：收件目录里可能已有
    /// 同名文件（`name (1).ext`），最终下标只有接收端知道。
    #[serde(default)]
    pub dest_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub file_index: u32,
    pub relative_path: String,
    pub file_size: u64,
    pub blake3_hash: String,
    pub chunk_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferManifest {
    pub transfer_id: String,
    pub sender_id: String,
    pub total_size: u64,
    pub chunk_size: u32,
    pub files: Vec<FileMeta>,
    pub timestamp: i64,
    /// 发送方对规范化清单的 Ed25519 签名 (hex)。
    /// 缺少它, 握手通过后 manifest / 分块头 / 分块长度均可被中间人任意篡改。
    #[serde(default)]
    pub signature: String,
}

impl TransferManifest {
    /// 清单签名载荷: 除 signature 外全部字段的规范化表示。
    pub fn signing_payload(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.transfer_id,
            self.sender_id,
            self.total_size,
            self.chunk_size,
            serde_json::to_string(&self.files).unwrap_or_default()
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkHeader {
    pub transfer_id: String,
    pub file_index: u32,
    pub chunk_index: u64,
    pub data_length: u32,
    pub chunk_blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferAck {
    pub transfer_id: String,
    pub file_index: u32,
    pub chunk_index: u64,
    pub success: bool,
    pub error_msg: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub pin_code: String,
    pub device_id: String,
    pub device_name: String,
    pub public_key_hex: String,
    /// **本次配对的世代标识**（§14.5.1 的防重放锚点）。
    ///
    /// 由发起方生成，两端都存一份，`MSG_UNPAIR` 回来时要比对。
    ///
    /// 为什么非要新造一个：解除配对必须双向，而对端可能不在线，
    /// 于是消息必然要排队 —— 排队就意味着**可能重放**。
    /// 攻击者录下旧的解除帧，等双方重新配对后重放，就能把**新绑定**拆掉。
    /// 而 `bound_at` 救不了：它是**各自本地时钟**，两边各写各的，从来不相等。
    ///
    /// `#[serde(default)]` 兼容旧对端：它不发这个字段时对端会自己生成一个
    /// 并在 [`PairResponse`] 里回填（旧对端不回填 ⇒ 发起方存空串 ⇒
    /// 解除配对会**明确失败并说明需要升级**，而不是静默只改一边）。
    #[serde(default)]
    pub pairing_epoch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponse {
    pub success: bool,
    pub device_id: String,
    pub device_name: String,
    pub public_key_hex: String,
    pub error_msg: Option<String>,
    /// **本次配对实际生效的世代标识**。接收方若收到空的会自己生成一个并回填。
    #[serde(default)]
    pub pairing_epoch: String,
}

impl UnpairRequest {
    /// 签名覆盖的内容。
    ///
    /// `peer_epoch` **必须在**签名范围内 —— 否则攻击者可以把一个合法的
    /// 签名改成配合另一个世代使用，重放就又成了可能。
    pub fn signing_payload(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}",
            self.version, self.initiator_id, self.target_id, self.peer_epoch, self.timestamp, self.nonce
        )
    }
}

/// 解除配对请求（发起方 → 被解除的一方）。
///
/// 四条性质必须同时满足，设计理由见 `DESIGN_TRUST_SHUTTLE.md` §14.5.1：
/// 不能伪造（用已绑定公钥验签）、不能重放（`peer_epoch` 必须与本地
/// 当前绑定世代一致）、幂等（重复收到回 success）、不依赖在线（出站队列）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnpairRequest {
    pub version: u32,
    /// 发起解除的设备 = 本机的对端
    pub initiator_id: String,
    /// 被解除关系的本机
    pub target_id: String,
    /// 发起方所知道的"这次绑定的世代"（见 [`PairRequest::pairing_epoch`]）。
    /// 接收方只在它与本地当前值**一致**时才生效。
    pub peer_epoch: String,
    pub timestamp: i64,
    pub nonce: String,
    /// Ed25519(`initiator_id` 在本机库中已绑定的公钥, over `signing_payload`)
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnpairResponse {
    pub success: bool,
    pub target_id: String,
    pub receiver_name: String,
    pub message: String,
    /// 本机当前的配对世代（回显, 便于发起方对账 / 排障）。
    #[serde(default)]
    pub pairing_epoch: String,
    /// 机器可读原因：`already_unpaired` / `epoch_mismatch` / `not_paired`。
    /// **不要**用它做控制流 —— 那是 §14.4 修掉的反模式。
    #[serde(default)]
    pub reason_code: String,
}

/// 目录浏览请求（双栏穿梭右栏，§7.2）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseRequest {
    pub version: u32,
    pub requester_id: String,
    pub requester_name: String,
    /// 相对落盘根目录的子路径, 空串表示根目录。
    /// 旧实现没有这个字段, 界面只能看到落盘目录的第一层, 子文件夹完全无法进入
    /// —— 对一个文件传输工具来说这等于半残。
    #[serde(default)]
    pub sub_path: String,
    pub timestamp: i64,
    pub nonce: String,
    pub signature: String,

    // ---- v2：真实文件系统路径模型 ----
    /// 请求的卷。Windows 为 `"C:"`；Android 为虚拟卷
    /// （`internal` / `download` / `dcim` / `movies` / `documents`）。
    /// 空串 = 走旧语义（收件目录），用于与 1.x 对端互通。
    #[serde(default)]
    pub volume: String,
    /// 卷内相对路径（`/` 分隔）。与 `sub_path` 二选一，`volume` 非空时优先。
    #[serde(default)]
    pub rel_path: String,
    /// 分页偏移（§7.3 第 1 点）
    #[serde(default)]
    pub offset: u32,
    /// 分页长度，0 = 用服务端默认
    #[serde(default)]
    pub limit: u32,
    /// 请求方能力位（用于服务端判断能否返回 `volumes`）
    #[serde(default)]
    pub caps: u32,
    /// **本次匹配码**（对端处于「每次匹配码」等级时必填，§2.3）
    ///
    /// 与 `HandshakeRequest::grant_code` 完全同义，方向也一样：
    /// **对端（接收方）出码并显示在自己的窗口上**，请求方把看到的码
    /// 敲进来。理由见 `HandshakeRequest::grant_code` 的长注释。
    #[serde(default)]
    pub grant_code: String,
}

impl BrowseRequest {
    /// 实际要解析的卷内相对路径。
    pub fn effective_rel(&self) -> &str {
        if self.volume.is_empty() {
            &self.sub_path
        } else {
            &self.rel_path
        }
    }

    /// 本次是否走"真实卷"路径（而非旧的收件目录镜像）。
    pub fn is_volume_mode(&self) -> bool {
        !self.volume.is_empty()
    }
}

/// 可浏览的卷（§7.2）。界面的地址栏下拉直接用它渲染。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// `"C:"` / `"internal"`
    pub id: String,
    /// 展示名（`本地磁盘 (C:)` / `内部存储`）
    pub label: String,
    /// 总容量（0 = 未知）
    #[serde(default)]
    pub total_bytes: u64,
    /// 剩余空间
    #[serde(default)]
    pub free_bytes: u64,
    /// 是否在当前 access_scope 的可浏览范围内
    #[serde(default)]
    pub readable: bool,
}

/// 对端落盘目录中的单个条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteFileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub size_formatted: String,
    pub modified: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseResponse {
    pub success: bool,
    pub receiver_id: String,
    pub receiver_name: String,
    pub message: String,
    /// **本次失败是否只是"要码"这一协商回合**（`success = false` 时有意义）。
    ///
    /// 为什么必须有它：旧实现只有 `message`，于是客户端只能
    /// `message.contains("传输码")` 去猜 —— 那是把安全行为挂在中文
    /// 措辞上，对端一改文案就静默失效（`error.rs` 里
    /// `GrantCodeRequired` 注释批评的正是这个模式）。
    /// 传输路径早就有同名的结构化字段（`HandshakeResponse`），
    /// 浏览与取回却漏了，于是这两条路径只能退回匹配中文。
    ///
    /// `#[serde(default)]` 保证**旧对端不发这个字段时也能解析**
    /// （回落 `false`），所以这是纯增量改动，两端可混跑新旧。
    #[serde(default)]
    pub requires_grant_code: bool,
    pub files: Vec<RemoteFileEntry>,
    /// 本次列举的目录 (相对根, 空串 = 根), 供界面显示面包屑
    #[serde(default)]
    pub current_path: String,
    /// 上一级路径; 已在根目录时为 None
    #[serde(default)]
    pub parent_path: Option<String>,
    /// 条目数被 MAX_BROWSE_ENTRIES 截断。
    /// 不给这个标记的话界面会把"只显示前 N 条"当成"目录就这么多",
    /// 用户会以为文件丢了。
    #[serde(default)]
    pub truncated: bool,
    // ---- v2：真实文件系统 ----
    /// 对端开放了哪些可浏览卷（地址栏下拉的数据源）
    #[serde(default)]
    pub volumes: Vec<VolumeInfo>,
    /// 对端的**常用位置**（桌面 / 下载 / 文档…），已按该对端的访问范围过滤。
    ///
    /// 为什么必须过滤而不能原样回：受限时如果照发，对方就能看到
    /// "这台机器有 `C:\\Users\\<用户名>\\Desktop`" —— 既点了必然失败，
    /// 又**泄露了对方的系统用户名**（目录名本身就是信息）。
    /// 与 §8.2「列表里必须直接不出现」同一条道理。
    ///
    /// 空数组 = 对端没有可用常用位置（受限，或非 Windows 平台）。
    #[serde(default)]
    pub places: Vec<crate::storage::KnownPlace>,
    /// 本目录**总条目数**（分页用）。真实磁盘一个目录几千上万个条目很常见，
    /// 没有总数就没法做"加载更多"。
    #[serde(default)]
    pub total: u32,
    /// 本次返回的偏移（回显，便于客户端校验）
    #[serde(default)]
    pub offset: u32,
    /// 本次是否走真实卷模式
    #[serde(default)]
    pub volume_mode: bool,
    /// **实际使用的**卷 id（§7.2）。
    ///
    /// 客户端可能发的是通配 `*`（它不知道对方有 C: 还是 D:），
    /// 所以必须回显服务端挑中的那个，否则前端地址栏会一直显示 `*`。
    /// 1.x 语义下为空串。
    #[serde(default)]
    pub volume: String,
    /// 对端能力集（诊断用）
    #[serde(default)]
    pub caps: u32,
}

/// 请求对端把落盘目录中的指定文件推送回本机
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub version: u32,
    pub requester_id: String,
    pub requester_name: String,
    /// 本机传输端口: 对端据此反向发起传输
    pub requester_port: u16,
    /// 相对落盘根目录的路径列表, **可以含子目录层级** (如 `2024/报表/1月.xlsx`),
    /// 用于穿梭取回。服务端逐段做路径穿越校验。
    #[serde(default)]
    pub sub_paths: Vec<String>,
    pub timestamp: i64,
    pub nonce: String,
    pub signature: String,
    /// v2：请求落点子目录（相对接收端收件根）。
    /// 空串 = 落收件根（= 旧行为）。有了它才能实现
    /// "把 A 的文件取到我的 `D:\工作\`"（§7.7）。
    #[serde(default)]
    pub dest_sub_path: String,
    /// v2：目标所在卷（`"C:"` / `"D:"`）。
    ///
    /// 空串 = 1.x 语义，只在**对端收件目录**里找。
    ///
    /// ## 为什么必须有它
    ///
    /// 早先 `PullRequest` 没有卷字段，服务端一律
    /// `receive_dir.join(sub_path)`。于是右栏虽然能浏览**整个真实卷**
    /// （v2 卷模式），点「取回」却永远失败 —— 它拿着
    /// `项目/2026/a.csv` 去收件目录里找那个相对路径。
    /// 表现为**"能看见、点不动"**：浏览走真实文件系统，取回只认收件目录。
    /// 需求 ④「双栏穿梭改为真实文件系统」在取回这一半是**假的**。
    #[serde(default)]
    pub volume: String,
    /// v2：请求方能力位
    #[serde(default)]
    pub caps: u32,
    /// **本次匹配码**（对端处于「每次匹配码」等级时必填，§2.3）
    ///
    /// 与传输/浏览完全同规则，方向也一样：**文件持有方（接收方）出码**
    /// 并显示在自己的窗口上，请求方把看到的码敲进来。
    /// 理由见 `HandshakeRequest::grant_code` 的长注释。
    #[serde(default)]
    pub grant_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullResponse {
    pub success: bool,
    pub receiver_id: String,
    pub receiver_name: String,
    pub message: String,
    /// **本次失败是否只是"要码"这一协商回合**。语义同
    /// [`BrowseResponse::requires_grant_code`]（那里有完整论证）。
    #[serde(default)]
    pub requires_grant_code: bool,
}

/// 取消传输：发送方在真正开传前反悔（直发撤销，§5.2）。
///
/// 发送方 → 接收方的一条短控制帧。接收端收到后：
/// - 尚未开始落盘 → `success: true`，不产生任何文件
/// - 已经开始落盘 → `success: false`，UI 必须如实告知"撤不掉"
///
/// 刻意**不承诺"一定成功"**：链路随时可能断，UI 不能给用户一个假的
/// 安全感——那比没有撤销按钮更危险。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRequest {
    pub version: u32,
    /// 发起方的设备 id（接收端用它确认"确实是它要撤"）
    pub sender_id: String,
    pub transfer_id: String,
    pub timestamp: i64,
    pub nonce: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResponse {
    pub success: bool,
    pub receiver_id: String,
    pub receiver_name: String,
    pub message: String,
}
