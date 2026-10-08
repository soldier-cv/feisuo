//! 信任模型的数据类型定义。
//!
//! 设计依据见 `DESIGN_TRUST_SHUTTLE.md` §2 / §8。
//!
//! # 为什么要拆成"两个正交维度 + 一张能力表"
//!
//! 旧模型只有一个 `is_trusted: bool`，只能表达"能 / 不能"。
//! 但用户实际需要表达三件**正交**的事：
//!
//! 1. **信任强度** —— 永久免密 / 每次要码 / 尚未认证 / 彻底拒绝
//!    → [`TrustLevel`]
//! 2. **是否可见** —— 列表噪音与隐私
//!    → `trusted_devices.visible`（见 [`TrustLevel`] 的使用方 [`Presence`] 所在的名册）
//! 3. **能做什么** —— 能浏览？能取走？能写入？
//!    → [`AccessScope`]
//!
//! 三者任意组合，`bool` 撑不住。

use serde::{Deserialize, Serialize};

// ===========================================================================
// 信任强度
// ===========================================================================

/// 设备的信任等级。
///
/// 与旧 `is_trusted: bool` 的映射（迁移时执行）：
/// - `is_trusted = 1` → [`TrustLevel::Permanent`]
/// - `is_trusted = 0` → [`TrustLevel::Pending`]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// 待认证：局域网见过但没配对。**可出现在设备列表**，但任何操作都要先配对。
    Pending,
    /// 永久信任：配对过且用户选择"永久"。后续操作静默完成。
    Permanent,
    /// 每次匹配码：配对过，但每次操作都要扫码 / 输 6 位码。
    ///
    /// 设计依据（§2.3）：**不能退化成手输码** —— 摩擦与永久信任差距过大，
    /// 等于不可用。因此主推扫码，会话授权码内容携带操作范围。
    Session,
}

impl TrustLevel {
    /// 数据库存储形式。
    pub fn as_db_str(self) -> &'static str {
        match self {
            TrustLevel::Pending => "pending",
            TrustLevel::Permanent => "permanent",
            TrustLevel::Session => "session",
        }
    }

    /// 从数据库字符串解析；未知值一律回落为最严的 [`TrustLevel::Pending`]。
    ///
    /// **fail-closed 是硬要求**：配置文件与数据库都可以被手工改，
    /// 一个拼错的 `trust_level` 绝不能被解释成"更信任"。
    pub fn from_db_str(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "permanent" => TrustLevel::Permanent,
            "session" => TrustLevel::Session,
            _ => TrustLevel::Pending,
        }
    }

    /// 是否已建立长期身份绑定（即"配对过"）。
    pub fn is_paired(self) -> bool {
        matches!(self, TrustLevel::Permanent | TrustLevel::Session)
    }

    /// 是否需要每次操作都出示授权码。
    pub fn requires_grant(self) -> bool {
        matches!(self, TrustLevel::Session)
    }
}

// ===========================================================================
// 可访问范围
// ===========================================================================

/// 可访问范围的模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    /// 只暴露飞梭收件目录 —— 最严格
    ReceiveOnly,
    /// 白名单：仅允许指定的盘符或目录
    Allowlist,
    /// 黑名单：允许所有盘符，除了排除的目录
    Denylist,
    /// 全部：所有内容均可访问，不区分系统目录
    All,
}

impl AccessMode {
    pub fn as_db_str(self) -> &'static str {
        match self {
            AccessMode::ReceiveOnly => "receive_only",
            AccessMode::Allowlist => "allowlist",
            AccessMode::Denylist => "denylist",
            AccessMode::All => "all",
        }
    }

    /// 未知值回落为 `all` —— 与 D1 决策一致（默认全部）。
    pub fn from_db_str(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "receive_only" | "receive-only" => AccessMode::ReceiveOnly,
            "allowlist" | "allow_list" => AccessMode::Allowlist,
            "denylist" | "deny_list" => AccessMode::Denylist,
            _ => AccessMode::All,
        }
    }
}

/// 单台对端的可访问范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessScope {
    pub mode: AccessMode,
    /// 允许的卷标识（Windows `"C:"` / Android `"internal"`）
    pub allow_volumes: Vec<String>,
    /// 卷内再收窄的目录（可空）
    pub allow_paths: Vec<String>,
    /// 用户自定义排除目录
    pub deny_paths: Vec<String>,
    /// 是否允许对方**取走**本机文件
    pub can_pull: bool,
    /// 是否允许对端**往本机写**文件。
    ///
    /// **默认 true**（允许写，但落点被强制在收件目录）—— D2 决策。
    ///
    /// ## 为什么默认 true 而不是 false
    ///
    /// 早先这里是 `false`，注释写"默认只允许写入收件目录"。但 `false`
    /// 的实际语义是"**完全不允许写**"，两句话说的不是一回事。照字面
    /// 实现的结果是：刚配对完的设备一个文件都发不进来，而界面上没有
    /// 任何东西提示原因。D2 的本意是"**收窄落点**"，不是"关掉接收"。
    ///
    /// 收窄落点这件事**已经由结构保证**了：`server.rs` 在握手应答**之前**
    /// 就把 `dest_sub_path` 过一遍 `normalize_sub_path`（挡 `..` / 绝对路径 /
    /// 盘符 / 隐藏目录 / 超深）+ 逐段创建 + `is_within(receive_dir)`，
    /// 任何一项不过就**明确拒绝握手**。落点因此恒在收件目录之内，
    /// 这个开关只需表达"允不允许写"，默认就该允许。
    ///
    /// ⚠️ 上面的 `AccessMode::ReceiveOnly` / `allow_paths` 是**读**的范围，
    /// 与写入无关，也**不该**拿来管写入：`can_read` 在 `ReceiveOnly` 下
    /// 对任何路径都返回 false，若把它接进 `Op::Receive` 的判定，
    /// "只允许写收件目录"的设备会一个文件都收不到。
    ///
    /// 真需要关的场景是"我不想让这台设备给我发东西"——那有更直接的
    /// 开关（信任等级降级 / 解除配对），不必借这个含义模糊的旋钮。
    pub can_push: bool,
    pub updated_at: i64,
}

impl Default for AccessScope {
    fn default() -> Self {
        Self {
            mode: AccessMode::All,
            allow_volumes: Vec::new(),
            allow_paths: Vec::new(),
            deny_paths: Vec::new(),
            can_pull: true,
            can_push: true,
            updated_at: 0,
        }
    }
}

impl AccessScope {
    /// **不可关闭**的系统敏感路径排除清单（§8.2 第 1 层）。
    ///
    /// 这些是"系统与凭据"目录，不属于"用户自己的数据"，
    /// 所以与 D1「默认全部」不冲突 —— 全部指的是数据卷全部可浏览。
    pub const MANDATORY_DENY: &'static [&'static str] = &[
        "C:\\Windows",
        "C:\\Program Files",
        "C:\\Program Files (x86)",
        "C:\\ProgramData",
        "C:\\$Recycle.Bin",
        "C:\\System Volume Information",
        "C:\\Users\\*\\AppData",
        "C:\\Users\\*\\.ssh",
        "C:\\Users\\*\\.gnupg",
        "C:\\Users\\*\\.aws",
        "C:\\Users\\*\\NTUSER.DAT",
        "/sdcard/Android/data",
        "/sdcard/Android/obb",
    ];

    /// 命中强制排除清单时返回 true。
    ///
    /// 支持 `*` 单段通配（仅第一层，如 `C:\Users\*\AppData`），
    /// 比较时统一分隔符并忽略大小写。
    ///
    /// ## 必须是**前缀**匹配，不能是全等匹配
    ///
    /// 规则项写的是 `C:\Windows`，但没人会去浏览 `C:\Windows` 本身 ——
    /// 用户（和恶意对端）会直接请求 `C:\Windows\System32\config\SAM`。
    /// 全等匹配下那条请求**不命中**，整个排除清单形同虚设。
    /// 所以这里枚举待判路径的**每一级祖先**逐个比对。
    pub fn is_mandatory_denied(path: &str) -> bool {
        matches_any_pattern_with_ancestors(&Self::MANDATORY_DENY, path)
    }

    /// 综合判断某个路径是否可读。
    ///
    /// 判定顺序：用户排除黑名单 → 模式判断（全部/黑名单/白名单/仅收件）。
    pub fn can_read(&self, volume: &str, abs_path: &str) -> bool {
        let norm = normalize_for_compare(abs_path);
        // 命中自定义黑名单排除目录，一律拒绝访问
        if matches_any_pattern_with_ancestors(&self.deny_paths, &norm) {
            return false;
        }
        match self.mode {
            // 收件目录之外的任何路径都不可读
            AccessMode::ReceiveOnly => false,
            // 全部模式：所有盘符与目录均可访问，不区分系统目录
            AccessMode::All => true,
            // 黑名单模式：排除项已在上方拦截，未被排除的均允许访问
            AccessMode::Denylist => true,
            // 白名单模式：命中指定目录或指定卷即放行
            AccessMode::Allowlist => {
                if !self.allow_paths.is_empty() && is_under_any_pattern(&self.allow_paths, &norm) {
                    return true;
                }
                if !self.allow_volumes.is_empty()
                    && self.allow_volumes.iter().any(|v| v.eq_ignore_ascii_case(volume))
                {
                    return true;
                }
                false
            }
        }
    }
}

/// 判断 `path` 是否落在 `patterns` 任一条目之下（**放行侧**，严格通配）。
///
/// 枚举 `path` 的每一级祖先（含自身），用**单段** `*` 通配匹配。
///
/// 旧实现只有 `norm.starts_with(&format!("{}/", p))`，两个问题：
/// 1. 不支持 `*` —— 用户填 `C:\Users\*\项目` 会被**静默忽略**，
///    白名单形同虚设（看起来配了，实际什么都看不到）；
/// 2. 不匹配自身 —— 白名单里写了 `D:\项目`，请求 `D:\项目` 本身会被拒。
///    祖先枚举同时修掉这两点。
fn is_under_any_pattern<S: AsRef<str>>(patterns: &[S], path: &str) -> bool {
    if patterns.is_empty() {
        return false;
    }
    ancestors_with_self(path)
        .iter()
        .any(|cand| {
            patterns
                .iter()
                .any(|pat| glob_match_one_segment(pat.as_ref(), cand))
        })
}

/// 归一化 + ASCII 小写，用于比较。
///
/// **必须用 `to_ascii_lowercase` 而不是 `to_lowercase`**：
/// - `to_ascii_lowercase` 只映射 `A-Z`，**字节长度与字符边界完全不变**，
///   所以后面可以安全地按字节切片比较；
/// - `to_lowercase`（Unicode）会把 `'İ'` 展开成两个字符，字节长度变化，
///   按字节切片就会切在字符中间 → panic。
///   而 ASCII 大小写折叠正是这里唯一需要的（路径里只有 ASCII 字母会歧义）。
fn normalize_for_compare(path: &str) -> String {
    let mut s = path.trim().replace('\\', "/").to_ascii_lowercase();
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// 待判路径的**各级祖先**（含自身），从短到长。
///
/// 顺序刻意是从短到长：调用方要拿第一个命中当原因，短的更接近用户的
/// 真实意图（"这是 AppData 下的东西"，而不是 "这是 Local 下的东西"）。
/// 盘符段（`C:`）单独跳过 —— 没有盘符的裸路径（Android 的 `/sdcard/...`）
/// 要保留，否则 `/sdcard` 本身会变成一个无意义的祖先。
fn ancestors_with_self(path: &str) -> Vec<String> {
    let norm = normalize_for_compare(path);
    let segs: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    let mut out = Vec::with_capacity(segs.len());
    for i in 1..=segs.len() {
        // 末段是盘符（如 `C:`）时不能作为独立祖先, 但拼前缀时要保留
        let cand = segs[..i].join("/");
        let is_bare_drive = i == 1 && cand.ends_with(':');
        if !is_bare_drive {
            out.push(cand);
        }
    }
    out
}

/// 判断 `path` 是否等于、或位于 `patterns` 任一条目之下（逐级祖先比对）。
///
/// 见 [`AccessScope::is_mandatory_denied`] 的说明：前缀语义是安全要求，
/// 不是优化。
fn matches_any_pattern_with_ancestors<S: AsRef<str>>(patterns: &[S], path: &str) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let cands = ancestors_with_self(path);
    // 短路顺序：先试最长（自身），命中率高且最快返回。
    for cand in cands.iter().rev() {
        if patterns
            .iter()
            .any(|pat| glob_match_deny(pat.as_ref(), cand))
        {
            return true;
        }
    }
    false
}

/// **拒绝列表**用的通配匹配：`*` 可跨任意多段。
///
/// ## 为什么拒绝与放行要用不同的通配语义
///
/// 旧实现只有一个 `glob_match_one_segment`，两边共用。对**放行**规则
/// （`allow_paths`）这是对的 —— `C:\Users\*\AppData` 不该意外匹配
/// `C:\Users\a\b\AppData`，多开放一层就是多开一个洞。
///
/// 但对**拒绝**规则方向正好相反：多拒一条只是让用户多点一次，
/// 少拒一条可能直接泄露 `AppData\Local\...\Cookies`。
/// 所以拒绝侧必须用"宽松"匹配，放行侧继续用"严格"匹配。
/// **两个方向共用一套语义，就必然有一边是错的。**
fn glob_match_deny(pattern: &str, value: &str) -> bool {
    let pat = normalize_for_compare(pattern);
    let val = normalize_for_compare(value);
    match pat.split_once('*') {
        None => pat == val,
        Some((pre, post)) => {
            // `*` 吸收中段任意长度（含 0），但前后两段不得重叠。
            // 用 `>=` 而不是 `==`：`c:/users/13262/appdata` 与
            // `c:/users/*/appdata` 的中间是 "13262"（4 字节），
            // 写成 `==` 会把它判成不匹配 —— 那就等于清单没生效。
            if val.len() < pre.len() + post.len() {
                return false;
            }
            // `normalize_for_compare` 已保证字节长度与字符边界不变（ASCII 小写），
            // 所以这里的切片是安全的；用 `starts_with` / `ends_with` 而非
            // 直接 `&val[..n]`，让 panic 不可能发生。
            val.starts_with(pre) && val.ends_with(post)
        }
    }
}

/// 只支持**单段** `*` 通配的匹配（放行侧用；够用且不会误伤）。
///
/// 刻意不做跨段 glob：完整 glob 会让 `C:\Users\*\AppData` 意外匹配
/// `C:\Users\a\b\AppData`，反而开出本该封住的路径。
///
/// 大小写折叠已在 [`normalize_for_compare`] 内完成（ASCII only），
/// 所以这里用 `starts_with` / `ends_with` 而不是字节切片 ——
/// 后者遇到中文路径会切在字符中间直接 panic。
fn glob_match_one_segment(pattern: &str, value: &str) -> bool {
    let pat = normalize_for_compare(pattern);
    let val = normalize_for_compare(value);
    match pat.split_once('*') {
        None => pat == val,
        Some((pre, post)) => {
            if val.len() < pre.len() + post.len() {
                return false;
            }
            if !val.starts_with(pre) || !val.ends_with(post) {
                return false;
            }
            // 中间不得再含分隔符 —— 保证 `*` 只吃一段
            !val[pre.len()..val.len() - post.len()].contains('/')
        }
    }
}

// ===========================================================================
// 操作与决策
// ===========================================================================

/// 一次受控操作。鉴权决策的输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// 配对（交换指纹）
    Pair,
    /// 对方向我推送文件（入站传输）
    Receive,
    /// 对方浏览我的目录
    Browse,
    /// 对方取走我的文件
    Pull,
    /// 对方往我的磁盘写文件
    Push,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Pair => "pair",
            Op::Receive => "receive",
            Op::Browse => "browse",
            Op::Pull => "pull",
            Op::Push => "push",
        }
    }

    /// 人类可读的操作名，用于审批弹窗与安全事件。
    pub fn label_cn(self) -> &'static str {
        match self {
            Op::Pair => "配对",
            Op::Receive => "投递文件",
            Op::Browse => "浏览本机文件",
            Op::Pull => "取走本机文件",
            Op::Push => "写入本机磁盘",
        }
    }

    /// 是否属于"读/取走"类 —— 这类受 `can_pull` 与 `AccessScope` 约束。
    pub fn is_read_like(self) -> bool {
        matches!(self, Op::Browse | Op::Pull)
    }
}

/// 拒绝的**机器可读**原因。
///
/// ## 为什么需要它（而不是只带一句文案）
///
/// 旧版 `Decision::Deny(String)` 只有一个字段，于是调用方无法区分
/// "这个拒绝能被人工审批救回"和"这个拒绝就是最终判决"，
/// 只能去 `contains()` 中文文案。代码里真的这么干了
/// （`server.rs` 的 `r.contains("未配对") || r.contains("自动接收")`），
/// 于是**安全行为依赖于措辞**：把「已关闭自动接收」改成
/// 「已关闭自动接收文件」，一台永久信任设备就会从"弹审批"
/// 变成"硬拒绝"，且无任何编译期错误。
///
/// 这与 [`crate::error::FeisuoError::GrantCodeRequired`] 拒绝靠字符串
/// 分支是同一类问题，而那个问题已经在这个仓库里被判过一次刑。
///
/// 有了 `code` 之后，`approvable` 由**产出它的那一处**决定，
/// 下游只读 [`DenyCode::is_approvable`]，没有任何地方需要看文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyCode {
    /// 网段白名单里有无法解析的条目（配置错误）
    SubnetConfigInvalid,
    /// 来源不在允许的网段内
    SubnetNotAllowed,
    /// 未配对 —— **可由人工审批救回**
    NotPaired,
    /// 已关闭自动接收 —— **可由人工审批救回**
    AutoReceiveOff,
    /// 会话授权无效/无法校验
    GrantInvalid,
    /// 无法读取访问范围配置（fail-closed）
    ScopeCheckFailed,
    /// 访问范围不允许这个操作
    ScopeDenied,
}

impl DenyCode {
    /// 是否属于"问一下用户就能继续"的那一类。
    ///
    /// **这是唯一该被下游读的属性**，而它是"一个 code 对一个答案"的
    /// 纯函数 —— 所以"哪些拒绝可以被审批救回"永远只有一个真相源。
    /// 单元测试 `deny_approvable_set_is_exactly_the_two_expected_codes`
    /// 把它钉住。
    pub fn is_approvable(self) -> bool {
        matches!(self, DenyCode::NotPaired | DenyCode::AutoReceiveOff)
    }
}

/// 鉴权结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 放行
    Allow,
    /// 需要一次会话授权码（`session` 等级专用）
    RequireGrant,
    /// 拒绝。
    ///
    /// - `code`：机器可读原因，**控制流只准读它**；
    /// - `user_message`：面向用户的中文原因，**只准显示**。
    Deny {
        code: DenyCode,
        user_message: String,
    },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }

    /// 拒绝原因（仅供显示 / 日志 / 安全事件）。
    ///
    /// **不要**用它做控制流 —— 那是本类型被改造出来的原因（见 [`DenyCode`]）。
    pub fn denied_reason(&self) -> Option<&str> {
        match self {
            Decision::Deny { user_message, .. } => Some(user_message.as_str()),
            _ => None,
        }
    }

    /// 是否可以由人工审批救回 —— 供审批分支分流。
    ///
    /// **刻意不在 `Deny` 里再存一份 `approvable`**：那会造出第二个真相源，
    /// 而本仓库这一整轮修的全是"同一个事实存在两处"的问题
    /// （`blocked` 存两处、发现计数读两次）。要问"能不能救回"就去问
    /// [`DenyCode::is_approvable`]，它只有一份。
    pub fn is_approvable_deny(&self) -> bool {
        match self {
            Decision::Deny { code, .. } => code.is_approvable(),
            _ => false,
        }
    }

    /// 构造一条拒绝。`approvable` 由 `code` 推导，不单独传参。
    pub fn deny(code: DenyCode, user_message: impl Into<String>) -> Self {
        Decision::Deny {
            code,
            user_message: user_message.into(),
        }
    }
}

/// 会话授权的临时凭证。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGrant {
    pub peer_device_id: String,
    pub scope: String,
    pub granted_at: i64,
    pub expires_at: i64,
}

impl SessionGrant {
    pub fn is_valid_at(&self, now: i64) -> bool {
        self.expires_at > now
    }
}

// ===========================================================================
// 在线状态
// ===========================================================================

/// 设备名册里的在线状态（§3.5）。
///
/// **关键设计：展示层 TTL 与传输层 TTL 解耦。**
/// `DEVICE_TTL_SECS = 20` 是传输前的快速判定阈值（保持不变），
/// 但**列表不再以它为生死线** —— 离线设备仍留在列表里（§3.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// 在线（20s 内有信标）
    Online,
    /// 重连中（20s ~ 5min）
    Reconnecting,
    /// 离线（> 5min）
    Offline,
    /// 已隐藏
    Hidden,
}

impl Presence {
    /// 判定阈值（秒）。
    pub const ONLINE_SECS: i64 = 20;
    /// 超过该秒数判定为离线。
    pub const OFFLINE_SECS: i64 = 300;

    /// 依据 `last_seen_at` 判定在线态。
    pub fn from_last_seen(last_seen_at: i64, now: i64) -> Self {
        if last_seen_at <= 0 {
            return Presence::Offline;
        }
        let age = now - last_seen_at;
        if age < Self::ONLINE_SECS {
            Presence::Online
        } else if age < Self::OFFLINE_SECS {
            Presence::Reconnecting
        } else {
            Presence::Offline
        }
    }

    pub fn label_cn(self) -> &'static str {
        match self {
            Presence::Online => "在线",
            Presence::Reconnecting => "重连中",
            Presence::Offline => "离线",
            Presence::Hidden => "已隐藏",
        }
    }

    /// 是否可以接受拖放发送（仍需在松手瞬间做一次实际探测）。
    pub fn accepts_drop(self) -> bool {
        matches!(self, Presence::Online | Presence::Reconnecting)
    }
}
