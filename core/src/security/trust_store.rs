use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::warn;
use serde::{Deserialize, Serialize};
use crate::config::AppConfig;
use crate::error::Result;
use crate::protocol::{MAX_PIN_ATTEMPTS, PIN_LOCKOUT_SECS};
use crate::security::trust_model::{AccessMode, AccessScope, TrustLevel};

/// schema 版本号。用于将来做更细粒度的增量迁移。
const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub device_id: String,
    pub device_name: String,
    pub public_key_hex: String,
    pub last_ip: String,
    pub bound_at: String,
    /// 旧字段, 保留一个版本供前端过渡; 内部一律以 [`Self::trust_level`] 为准
    pub is_trusted: bool,
    /// 信任等级（§2.2）。`is_trusted` 的超集。
    pub trust_level: TrustLevel,
    /// 是否在设备列表中可见（§3）
    pub visible: bool,
    /// 最后一次收到该设备信标的 Unix 秒（§3.6）。
    /// 旧库迁移时为 0，UI 必须显示"未知"，**不得伪造成"刚刚在线"**。
    pub last_seen_at: i64,
    /// 本次配对的世代标识（§14.5.1 的防重放锚点）。
    ///
    /// 空串 = 与对端**没有共同世代**（旧对端）。此时双向解除配对会
    /// **明确失败并说明需要升级**，而不是静默只改本地一边。
    #[serde(default)]
    pub pairing_epoch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRecord {
    pub id: i64,
    pub file_name: String,
    pub file_size: u64,
    pub file_size_formatted: String,
    pub direction: String,
    pub peer_name: String,
    pub peer_ip: String,
    pub status: String,
    pub created_at: i64,
    pub time_formatted: String,
    // ---- v2 新增：速度度量的原始量（§9.6.6：速度是派生值，不落库）----
    /// 清单声明的总字节数。失败时可能大于实际传输量。
    #[serde(default)]
    pub declared_size: u64,
    /// 纯数据流耗时（毫秒）—— **速度的唯一分母**
    #[serde(default)]
    pub duration_active_ms: u64,
    /// 墙钟耗时（毫秒），含审批等待
    #[serde(default)]
    pub duration_wall_ms: u64,
    /// 整文件 BLAKE3 复核耗时（毫秒）
    #[serde(default)]
    pub duration_verify_ms: u64,
    /// 是否走覆盖网（ZeroTier / Tailscale 等 100.64/10）
    #[serde(default)]
    pub over_overlay: bool,
    /// 两端 TCP 建连耗时（毫秒），链路延迟的粗略下界
    #[serde(default)]
    pub connect_ms: u64,
    /// 速度的人话文本，**读取时计算、不落库**（§9.6.6）。
    /// 样本不足（< 100ms）或零字节时为 `"—"`。
    #[serde(default)]
    pub speed_display: String,
    /// 传输包含的全部文件完整绝对路径列表
    #[serde(default)]
    pub file_paths: Vec<String>,
}

impl TransferRecord {
    /// 聚合吞吐（字节/秒）。
    ///
    /// **只由 `duration_active_ms` 推导**，并施加 100ms 最小样本门槛
    /// （`transport::diagnostics::MIN_SPEED_SAMPLE_MS`）。
    /// 门槛不满足时返回 `None`，调用方应显示"—"。
    pub fn avg_speed_bps(&self) -> Option<u64> {
        if self.duration_active_ms < crate::transport::diagnostics::MIN_SPEED_SAMPLE_MS {
            return None;
        }
        if self.file_size == 0 {
            return None;
        }
        Some(self.file_size * 1000 / self.duration_active_ms)
    }

    /// 速度的人话格式，复用 `format_bytes` 的 1024 进制。
    pub fn speed_display(&self) -> String {
        match self.avg_speed_bps() {
            Some(bps) => format!("{}/s", format_bytes(bps)),
            None => "—".to_string(),
        }
    }
}

/// 安全事件类型（§3.9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityEventKind {
    /// 设备提交了与既有绑定**不同**的公钥 —— 可能是身份顶替尝试
    KeyConflict,
    /// 用户解除了对某设备的信任
    TrustRevoked,
    /// 设备在短时间内反复上下线
    Flapping,
    /// 可访问范围被放宽
    ScopeWidened,
    /// **入站请求被硬拒绝**（未配对 / 自动接收关闭 / 无写入权限 / 网段不允许）。
    ///
    /// 记的是"**每一次**被拒的入站请求"。反复敲门本身就是要让用户知道的：
    /// 一台被解除配对的设备如果还在一次次尝试，界面上如果什么都不显示，
    /// 用户只会以为"解除配对生效了"——而实际上那台设备还在敲门。
    RejectedAttempt,
    /// **「每次匹配码」等级的传输码核对失败**（§2.3）
    ///
    /// 单列一种而不是塞进 `RejectedAttempt`：用户输错码是**正常操作**
    /// （手滑、看错、对方屏幕没看到）。混进"被拒绝的请求"会让安全
    /// 事件列表变成噪音，用户从此不再认真看它 ——
    /// **一个总是误报的告警等于没有告警**。
    GrantCodeMismatch,
}

impl SecurityEventKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            SecurityEventKind::KeyConflict => "key_conflict",
            SecurityEventKind::TrustRevoked => "trust_revoked",
            SecurityEventKind::Flapping => "flapping",
            SecurityEventKind::ScopeWidened => "scope_widened",
            SecurityEventKind::RejectedAttempt => "rejected_attempt",
            SecurityEventKind::GrantCodeMismatch => "grant_code_mismatch",
        }
    }
}

/// 安全事件记录。**必须让用户知情，不能只写日志**（§3.9）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityEvent {
    pub id: i64,
    pub kind: String,
    pub peer_device_id: String,
    pub peer_name: String,
    pub detail: String,
    pub created_at: i64,
    pub time_formatted: String,
}

/// 一台设备的一个可达地址（P1 ⑩ / P4 ⑧）。
///
/// 一台设备可能有多个：ZeroTier 覆盖网地址 + 物理网卡地址 + 换过的
/// 历史地址。只记一个（旧设计）时，DHCP 一换就彻底失联。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceEndpoint {
    pub device_id: String,
    pub ip: String,
    /// 传输端口。`0` = 未知（老库只有 `last_ip`），发现层会走主动探测补齐。
    pub port: u16,
    /// `"overlay"` / `"lan"` / `"public"` / `"unknown"`
    pub kind: String,
    pub first_seen: i64,
    pub last_seen: i64,
    /// 是否真的在这个地址上收到过签名信标
    pub verified: bool,
}

/// 端点类型在"选路"时的优先级。**覆盖网优先**（D6：主场景是跨地域
/// ZeroTier，且 ZeroTier 自身已加密、绕开家庭 NAT）。
///
/// 为什么不是"延迟优先"：那需要先探测，而探测本身要走网络 ——
/// 用"地址性质"做静态判断是零成本的，且在用户的主要场景下方向正确。
pub fn endpoint_priority(kind: &str) -> u8 {
    match kind {
        "overlay" => 3,
        "lan" => 2,
        // 回环优先于其它真实地址：它一定**通**（本机就在这儿），
        // 而 lan/public 只是"可能通"。单机联调时这决定了能否连上。
        "loopback" => 2,
        "public" => 1,
        _ => 0,
    }
}

/// 端点性质的人话标签。
pub fn endpoint_kind_label(kind: &str) -> &'static str {
    match kind {
        "overlay" => "虚拟网卡（ZeroTier 等）",
        "lan" => "局域网",
        "loopback" => "本机回环",
        "public" => "公网",
        _ => "未知",
    }
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// 配对绑定结果
#[derive(Debug, Clone, PartialEq)]
pub enum BindOutcome {
    /// 首次绑定成功
    Added,
    /// 同一设备重复配对, 仅刷新了设备名 / IP
    Refreshed,
    /// 该 device_id 已绑定到另一把公钥, 拒绝顶替
    KeyConflict,
}

/// 解除配对的**结果**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpairOutcome {
    /// 已生效（本次真的把信任降到了未信任）
    Applied,
    /// 本来就未信任 —— **幂等**：重复解除必须回成功而不是报错，
    /// 否则"两边都点一次解除"就会有一边看到失败，而那个失败毫无信息量
    AlreadyUnpaired,
    /// 世代不符：对方所引用的绑定世代不是本地当前的绑定。
    ///
    /// 这**不是**错误，而是"这条解除帧已经过期" —— 它多半是一帧
    /// **重放**（攻击者录下旧的、双方重新配对后又发来）。
    /// 此时**绝不能**动本地状态，否则等于替攻击者拆掉新绑定。
    EpochMismatch,
    /// 双方没有共同世代（旧对端），无法安全地双向解除。
    NoSharedEpoch,
    /// 本机信任库里根本没有这台设备
    NotPaired,
}

impl UnpairOutcome {
    pub fn is_applied(self) -> bool {
        matches!(self, UnpairOutcome::Applied)
    }
}

/// 单个文件分块断点续传数据库记录
#[derive(Debug, Clone)]
pub struct ChunkResumeRecord {
    pub transfer_id: String,
    pub file_index: u32,
    pub relative_path: String,
    pub blake3_hash: String,
    pub file_size: u64,
    pub staged_path: String,
    pub next_chunk_index: u32,
    pub bytes_resumed: u64,
    pub updated_at: i64,
}

pub struct TrustStore {
    conn: Mutex<Connection>,
    active_pair_pin: Mutex<Option<(String, i64)>>, // PIN and expire timestamp (secs)
    /// IP -> (连续失败次数, 解锁时间戳)
    pin_failures: Mutex<HashMap<String, (u32, i64)>>,
}

impl TrustStore {
    /// 加锁辅助函数: 任一次持锁 panic 都不应让整个信任库永久中毒。
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn open() -> Result<Self> {
        Self::open_at(AppConfig::get_app_dir().join("trust_store.db"))
    }

    /// 在指定路径打开信任库。
    /// 抽出来是为了让"同一进程内跑两个节点"的集成测试可以各自持有独立状态,
    /// 否则两个节点会共用同一个全局 trust_store.db, 根本无法验证配对流程。
    pub fn open_at(db_path: std::path::PathBuf) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(&db_path)?;

        // WAL + busy_timeout: 默认的 DELETE journal / 0 超时在并发访问或
        // 崩溃残留 journal 文件时会立刻返回 SQLITE_BUSY。
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));

        Self::migrate(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
            active_pair_pin: Mutex::new(None),
            pin_failures: Mutex::new(HashMap::new()),
        })
    }

    /// 建表 + 版本化迁移。
    ///
    /// ## 迁移的铁律
    ///
    /// 1. **绝不丢用户已有的信任列表**。`is_trusted` 会被映射成
    ///    `trust_level`，而不是被丢弃后要求用户重新配对。
    /// 2. **幂等**。`ensure_column` 先查 `PRAGMA table_info` 再决定是否
    ///    `ALTER`，所以重复打开同一个库不会报错。
    /// 3. 迁移失败**必须让应用起不来**，而不是带着半个 schema 继续跑 ——
    ///    半个 schema 会让 `authorize` 的 fail-closed 行为变得不可预测。
    fn migrate(conn: &Connection) -> Result<()> {
        // ---- 基础表（v1，已存在的用户库里已有，这里 CREATE IF NOT EXISTS 兜底）----
        conn.execute(
            "CREATE TABLE IF NOT EXISTS trusted_devices (
                device_id TEXT PRIMARY KEY,
                device_name TEXT NOT NULL,
                public_key_hex TEXT NOT NULL,
                last_ip TEXT NOT NULL,
                bound_at TEXT NOT NULL,
                is_trusted INTEGER NOT NULL DEFAULT 1
            )",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS transfer_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_name TEXT NOT NULL,
                file_size INTEGER NOT NULL,
                direction TEXT NOT NULL,
                peer_name TEXT NOT NULL,
                peer_ip TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_meta (k TEXT PRIMARY KEY, v INTEGER NOT NULL)",
            [],
        )?;

        // ---- v2：信任等级 / 可见性 / 最后在线时间 ----
        Self::ensure_column(conn, "trusted_devices", "trust_level", "TEXT NOT NULL DEFAULT 'pending'")?;
        // 配对世代（§14.5.1 的防重放锚点）。
        // 空串 = 与对端没有共同世代（旧对端）⇒ 双向解除配对会**明确失败**，
        // 而不是静默只改一边。存量库补齐时统一给空串, 重新配对即自动获得。
        Self::ensure_column(
            conn,
            "trusted_devices",
            "pairing_epoch",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        Self::ensure_column(conn, "trusted_devices", "visible", "INTEGER NOT NULL DEFAULT 1")?;
        Self::ensure_column(conn, "trusted_devices", "last_seen_at", "INTEGER NOT NULL DEFAULT 0")?;

        // 一次性数据迁移: is_trusted -> trust_level
        // 只在 trust_level 仍是默认值 'pending' 且 is_trusted=1 时才提升,
        // 这样重复执行不会把用户后来改成的 'session' 覆盖回 'permanent'。
        conn.execute(
            "UPDATE trusted_devices
             SET trust_level = 'permanent'
             WHERE is_trusted = 1 AND (trust_level IS NULL OR trust_level = 'pending')",
            [],
        )?;

        // ---- v2：访问范围 ----
        conn.execute(
            "CREATE TABLE IF NOT EXISTS access_scopes (
                peer_device_id TEXT PRIMARY KEY,
                mode           TEXT NOT NULL DEFAULT 'all',
                allow_volumes  TEXT NOT NULL DEFAULT '[]',
                allow_paths    TEXT NOT NULL DEFAULT '[]',
                deny_paths     TEXT NOT NULL DEFAULT '[]',
                can_pull       INTEGER NOT NULL DEFAULT 1,
                can_push       INTEGER NOT NULL DEFAULT 0,
                updated_at     INTEGER NOT NULL DEFAULT 0
            )",
            [],
        )?;

        // ---- v2：会话授权（"每次匹配码"模式）----
        conn.execute(
            "CREATE TABLE IF NOT EXISTS session_grants (
                peer_device_id TEXT NOT NULL,
                scope          TEXT NOT NULL,
                granted_at     INTEGER NOT NULL,
                expires_at     INTEGER NOT NULL,
                PRIMARY KEY (peer_device_id, scope)
            )",
            [],
        )?;

        // ---- v2：安全事件（§3.9）----
        conn.execute(
            "CREATE TABLE IF NOT EXISTS security_events (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                kind       TEXT NOT NULL,
                peer_device_id TEXT NOT NULL DEFAULT '',
                peer_name  TEXT NOT NULL DEFAULT '',
                detail     TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_security_events_created ON security_events(created_at DESC)",
            [],
        )?;

        // ---- v2：访问审计（mode=all 时启用，§8.2 第 2 层）----
        conn.execute(
            "CREATE TABLE IF NOT EXISTS access_audit (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                peer_id    TEXT NOT NULL,
                peer_name  TEXT NOT NULL DEFAULT '',
                volume     TEXT NOT NULL DEFAULT '',
                path       TEXT NOT NULL DEFAULT '',
                op         TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_access_audit_created ON access_audit(created_at DESC)",
            [],
        )?;

        // ---- v2：传输记录的速度度量原始量（§9.6.6）----
        Self::ensure_column(conn, "transfer_history", "declared_size", "INTEGER NOT NULL DEFAULT 0")?;
        Self::ensure_column(conn, "transfer_history", "duration_active_ms", "INTEGER NOT NULL DEFAULT 0")?;
        Self::ensure_column(conn, "transfer_history", "duration_wall_ms", "INTEGER NOT NULL DEFAULT 0")?;
        Self::ensure_column(conn, "transfer_history", "duration_verify_ms", "INTEGER NOT NULL DEFAULT 0")?;
        Self::ensure_column(conn, "transfer_history", "over_overlay", "INTEGER NOT NULL DEFAULT 0")?;
        Self::ensure_column(conn, "transfer_history", "connect_ms", "INTEGER NOT NULL DEFAULT 0")?;
        Self::ensure_column(conn, "transfer_history", "file_paths", "TEXT NOT NULL DEFAULT '[]'")?;

        // ---- v2：完整诊断记录（整条 JSON，供事后离线分析）----
        conn.execute(
            "CREATE TABLE IF NOT EXISTS transfer_diagnostics (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                transfer_id  TEXT NOT NULL,
                direction    TEXT NOT NULL,
                peer_name    TEXT NOT NULL DEFAULT '',
                peer_ip      TEXT NOT NULL DEFAULT '',
                outcome      TEXT NOT NULL,
                bytes        INTEGER NOT NULL DEFAULT 0,
                data_ms      INTEGER NOT NULL DEFAULT 0,
                speed_bps    INTEGER NOT NULL DEFAULT 0,
                payload      TEXT NOT NULL,
                created_at   INTEGER NOT NULL
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_diag_created ON transfer_diagnostics(created_at DESC)",
            [],
        )?;

        // ---- v2：断点续传状态（P1 ⑪）----
        // 记录"哪些文件已经完整落盘"。断点的粒度是**文件**而不是分块:
        // 文件粒度能覆盖 95% 的实际场景（传输被中断 / 用户点取消 /
        // 链路闪断, 通常发生在文件边界附近或整文件传输中途),
        // 而块级 bitmap 要额外维护"哪些块已校验"的状态, 复杂度与
        // 崩溃一致性风险高得多。
        //
        // ⚠️ 只在**整文件 BLAKE3 复核通过并提交之后**才写入 ——
        // 提前记就等于把截断文件当成完整的, 那会静默损坏数据。
        conn.execute(
            "CREATE TABLE IF NOT EXISTS transfer_parts (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                transfer_id    TEXT NOT NULL,
                sender_id      TEXT NOT NULL,
                relative_path  TEXT NOT NULL,
                blake3_hash    TEXT NOT NULL,
                file_size      INTEGER NOT NULL,
                committed_path TEXT NOT NULL,
                completed_at   INTEGER NOT NULL,
                UNIQUE(transfer_id, relative_path)
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_parts_transfer ON transfer_parts(transfer_id)",
            [],
        )?;
        // 同名文件被删/改名后, 旧记录会指向不存在的路径。
        // 清理靠这个索引 + 存在性检查, 不做级联删除（跨表无外键）。
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_parts_completed ON transfer_parts(completed_at DESC)",
            [],
        )?;

        // ---- v2.1：分块级断点续传进度表 ----
        conn.execute(
            "CREATE TABLE IF NOT EXISTS transfer_chunk_progress (
                transfer_id      TEXT NOT NULL,
                file_index       INTEGER NOT NULL,
                relative_path    TEXT NOT NULL,
                blake3_hash      TEXT NOT NULL,
                file_size        INTEGER NOT NULL,
                staged_path      TEXT NOT NULL,
                next_chunk_index INTEGER NOT NULL,
                bytes_resumed    INTEGER NOT NULL,
                updated_at       INTEGER NOT NULL,
                PRIMARY KEY(transfer_id, file_index)
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_chunk_prog_transfer ON transfer_chunk_progress(transfer_id)",
            [],
        )?;

        // ---- v2：每台设备记**多个**可达端点（P1 ⑩ / P4 ⑧）----
        //
        // 旧设计只记 `trusted_devices.last_ip` 一个地址。那在
        // "设备同时有 ZeroTier 与物理网卡" 的场景下是**根本性的缺陷**：
        // 存下的是哪个取决于最后一次是谁先回包，DHCP 一换就彻底失联，
        // 而用户既没重启也没改配置 —— 表现为"设备明明开着却搜不到"。
        //
        // 记多个端点后：单播可以同时探全部地址；偏好规则（覆盖网优先，
        // D6 决策）也才有数据依据。
        // ⚠️ SQL 里**不能写 `--` 注释**（那是 Rust 的），SQLite 会直接
        // 把它当成语法错误。注释只能放在字符串外面。
        conn.execute(
            "CREATE TABLE IF NOT EXISTS device_endpoints (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                device_id   TEXT NOT NULL,
                ip          TEXT NOT NULL,
                port        INTEGER NOT NULL,
                kind        TEXT NOT NULL DEFAULT 'unknown',
                first_seen  INTEGER NOT NULL,
                last_seen   INTEGER NOT NULL,
                verified    INTEGER NOT NULL DEFAULT 0,
                UNIQUE(device_id, ip, port)
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_endpoints_device ON device_endpoints(device_id)",
            [],
        )?;
        // 清理时按 (device_id, last_seen) 找最久没响应的
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_endpoints_seen ON device_endpoints(last_seen DESC)",
            [],
        )?;

        // ---- v3：`can_push` 默认值纠正 ----
        //
        // 早先 `AccessScope::can_push` 的**结构体默认**是 `false`，
        // 而入站传输根本没查它（只查了从不使用的 `Op::Push`）——
        // 所以这个开关是死的，而它自己的文档写的是
        // "默认 false（默认只允许写入收件目录）"，语义自相矛盾。
        //
        // 现在两处都改了：默认 `true`，并且 `Op::Receive` 真的查它。
        // 于是库里**存量**的 `can_push = 0` 必须一起翻正，否则老设备
        // 升级后会突然变成"一个文件都发不进来"。
        //
        // ## 为什么可以直接把存量 `0` 改成 `1`，不用区分"用户设的"
        //
        // 因为**从来没有任何界面能设置它**：`can_push` 在整个前端里
        // 只出现在 `feisuoBridge.ts` 的类型定义和"非 Tauri 环境"的
        // 兜底返回值里，没有任何组件调用 `setAccessScope`。
        // 也就是说库里每一个 `0` 都是"默认值的化石"，不可能是用户决定。
        // 换成有 UI 之后这里必须改成"只翻 `updated_at = 0` 的行"。
        let flipped = conn.execute(
            "UPDATE access_scopes SET can_push = 1 WHERE can_push = 0 AND updated_at = 0",
            [],
        )?;
        if flipped > 0 {
            tracing::info!(
                "已把 {} 条历史访问范围的 can_push 纠正为 1（该开关此前无界面可设置，存量 0 均为默认值化石）",
                flipped
            );
        }

        let _ = conn.execute(
            "INSERT INTO schema_meta (k, v) VALUES ('version', ?1)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![SCHEMA_VERSION],
        )?;

        Ok(())
    }

    /// 若 `table` 中不存在 `column`，则以 `decl` 声明新增。
    fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name.eq_ignore_ascii_case(column) {
                return Ok(());
            }
        }
        drop(rows);
        drop(stmt);
        conn.execute(&format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, decl), [])?;
        Ok(())
    }

    /// 旧接口保留：等价于"是否已配对"（`permanent` 或 `session`）。
    ///
    /// **新代码请用 [`TrustStore::trust_level`] + [`crate::security::authorize`]**，
    /// 不要用这个布尔值做准入判断 —— 它表达不了信任等级与访问范围。
    pub fn is_device_trusted(&self, device_id: &str) -> Result<bool> {
        Ok(self
            .trust_level(device_id)?
            .map(|l| l.is_paired())
            .unwrap_or(false))
    }

    /// 读取对端信任等级。未在信任库中返回 `Ok(None)`。
    pub fn trust_level(&self, device_id: &str) -> Result<Option<TrustLevel>> {
        if device_id.is_empty() {
            return Ok(None);
        }
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT trust_level FROM trusted_devices WHERE device_id = ?1")?;
        let mut rows = stmt.query(params![device_id])?;
        if let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            Ok(Some(TrustLevel::from_db_str(&raw)))
        } else {
            Ok(None)
        }
    }

    /// 设置对端信任等级，并同步维护 `is_trusted` 兼容列。
    pub fn set_trust_level(&self, device_id: &str, level: TrustLevel) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trusted_devices
             SET trust_level = ?1, is_trusted = ?2
             WHERE device_id = ?3",
            params![level.as_db_str(), if level.is_paired() { 1 } else { 0 }, device_id],
        )?;
        Ok(())
    }

    /// 设置设备在列表中的可见性（§3）。
    pub fn set_visible(&self, device_id: &str, visible: bool) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trusted_devices SET visible = ?1 WHERE device_id = ?2",
            params![if visible { 1 } else { 0 }, device_id],
        )?;
        Ok(())
    }

    /// 记录"最后一次收到该设备信标"的时间（§3.6）。
    ///
    /// **必须落库**：否则重启本机后所有离线设备的"上次在线"都会变成未知。
    pub fn mark_seen(&self, device_id: &str, ip: &str) -> Result<()> {
        let conn = self.conn();
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "UPDATE trusted_devices SET last_seen_at = ?1, last_ip = ?2 WHERE device_id = ?3",
            params![now, ip, device_id],
        )?;
        Ok(())
    }

    pub fn get_device_pubkey(&self, device_id: &str) -> Result<Option<String>> {
        let conn = self.conn();
        Self::read_trusted_pubkey(&conn, device_id)
    }

    /// 内部实现: 复用已有连接, 避免 std::sync::Mutex 不可重入导致自死锁。
    ///
    /// 只返回**已配对**设备（`permanent` / `session`）的公钥。
    /// `blocked` / `pending` 不返回 —— 准入由 [`crate::security::authorize`] 负责。
    fn read_trusted_pubkey(conn: &Connection, device_id: &str) -> Result<Option<String>> {
        let mut stmt = conn.prepare(
            "SELECT public_key_hex FROM trusted_devices
             WHERE device_id = ?1 AND trust_level IN ('permanent','session')",
        )?;
        let mut rows = stmt.query(params![device_id])?;
        if let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            Ok(Some(key))
        } else {
            Ok(None)
        }
    }

    /// 读取**任意**已绑定设备的公钥，**不**按信任等级过滤。
    ///
    /// 专门供 [`TrustStore::bind_device`] 做身份顶替检测：即便一条记录
    /// 处于 `pending`（旧库里 `is_trusted=0`），只要 `device_id` 已存在，
    /// 就必须走"公钥比对"而不是 `INSERT` —— 否则会撞 PRIMARY KEY 报错。
    fn read_any_pubkey(conn: &Connection, device_id: &str) -> Result<Option<String>> {
        let mut stmt =
            conn.prepare("SELECT public_key_hex FROM trusted_devices WHERE device_id = ?1")?;
        let mut rows = stmt.query(params![device_id])?;
        if let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            Ok(Some(key))
        } else {
            Ok(None)
        }
    }

    /// 读取某台设备**已存**的信任档位（`bind_device` 内部用）。
    ///
    /// 刻意**不**复用 `TrustStore::trust_level`：那个是 `&self`
    /// 且会在自己里 `conn()`，而 `bind_device` 的这一段**已经持着锁**
    /// （`std::sync::Mutex` 不可重入，再锁一次就是永久死锁 ——
    /// 这正是 [`TrustStore::bind_device`] 顶部那段注释记录的旧事故）。
    fn read_trust_level(conn: &Connection, device_id: &str) -> Result<Option<TrustLevel>> {
        let mut stmt =
            conn.prepare("SELECT trust_level FROM trusted_devices WHERE device_id = ?1")?;
        let mut rows = stmt.query(params![device_id])?;
        match rows.next()? {
            Some(row) => {
                let raw: String = row.get(0)?;
                Ok(Some(TrustLevel::from_db_str(&raw)))
            }
            None => Ok(None),
        }
    }

    /// 绑定受信设备。
    /// 与旧实现的关键差异: **绝不覆盖已绑定设备的公钥**。
    /// 旧实现是 upsert 覆盖公钥, 局域网攻击者只要拿到 6 位 PIN,
    /// 就能拿一个"已信任的 device_id + 自己的公钥"把既有身份静默顶替。
    pub fn bind_device(&self, dev: &TrustedDevice) -> Result<BindOutcome> {
        let now = chrono::Utc::now().timestamp();

        // 持锁的**只有纯 SQL 那部分**。
        //
        // 冲突分支要调 `self.add_security_event()`，而它内部又 `self.conn()`。
        // `std::sync::Mutex` 不可重入 ⇒ 同一个线程再锁自己 = **永久死锁**。
        // 早先这里是 `let conn = self.conn();` 写在函数开头、冲突分支在
        // 守卫存活期间调 add_security_event —— 于是"设备换了公钥后重新配对"
        // 这条路径会**永久挂死**（不是报错，是再也不返回）。
        // 而它恰恰是最该给用户看清楚的场景。
        let (outcome, existing_pubkey) = {
            let conn = self.conn();

            // 身份顶替检测必须查**任意**已绑定记录, 不能只查"已信任"的:
            // 旧实现只查 is_trusted=1, 于是"存在一条 is_trusted=0 的同 id 记录"
            // 会跳过比对直接走 INSERT, 撞 PRIMARY KEY 而报错。
            if let Some(existing) = Self::read_any_pubkey(&conn, &dev.device_id)? {
                if existing.eq_ignore_ascii_case(&dev.public_key_hex) {
                    // ---- 现存档位是什么，决定要不要写回配对时选定的档位 ----
                    //
                    // 早先这里**无条件**保留旧 `trust_level`，理由是
                    // "用户可能已把它降级为 session"。那个理由对**没被解除过**
                    // 的设备是对的，对**被解除过**的设备是错的：
                    //
                    //   配对(permanent) → A 解除配对 → 双方 pending
                    //     → B 重新走完整配对仪式 → 服务端给出默认档 permanent
                    //     → 本函数走 Refreshed，把 permanent **丢掉**
                    //     ⇒ 用户看到"配对成功"，设备却还是「未信任」，
                    //       之后每次传输都要重新弹审批，
                    //       而**界面上没有任何入口能改这个状态**。
                    //
                    // 判据不是"无脑覆盖"，而是**区分 `pending` 的两个来源**：
                    //
                    // - `pending` **不是**用户可选的档位 ——
                    //   `set_trust_level` 只接受 `permanent` / `session`。
                    //   它只由两处产生：建行时的默认值、以及解除配对。
                    //   两种都不是"用户的选择"。
                    // - `session` **是**用户的选择，降级后重新配对必须保留。
                    //
                    // 守卫 `repair_after_unpair_restores_trust_but_keeps_user_downgrade`
                    // 两侧都钉住了。
                    let stored_level = Self::read_trust_level(&conn, &dev.device_id)?;
                    let restore_trust = matches!(stored_level, None | Some(TrustLevel::Pending));
                    let level = if restore_trust {
                        dev.trust_level
                    } else {
                        stored_level.unwrap_or(TrustLevel::Pending)
                    };
                    // Refreshed: 只刷新非安全字段 + 最后在线时间。
                    conn.execute(
                        "UPDATE trusted_devices
                         SET device_name = ?1, last_ip = ?2, is_trusted = ?3, last_seen_at = ?4,
                             pairing_epoch = ?5, trust_level = ?6
                         WHERE device_id = ?7",
                        params![
                            dev.device_name,
                            dev.last_ip,
                            if level.is_paired() { 1 } else { 0 },
                            now,
                            // **世代必须在这里换新**（§14.5.1）。
                            //
                            // 漏掉这一步, 防重放就整个失效: A↔B 以世代 E1 配对,
                            // A 录下一帧带 E1 的解除帧; 双方重新配对后世代**仍是 E1**
                            // （如果 Refreshed 不碰它）; 攻击者重放那一帧 ——
                            // 世代对得上, **新绑定被拆掉**, 而这正是世代机制
                            // 唯一要防的事。
                            //
                            // 与 `trust_level` 不同: 世代是**一次性 nonce**,
                            // 不是用户偏好, 所以刷新它不会覆盖任何用户选择。
                            dev.pairing_epoch,
                            level.as_db_str(),
                            dev.device_id
                        ],
                    )?;
                    (BindOutcome::Refreshed, None)
                } else {
                    // 公钥不一致 = 有人拿着同一个 device_id 换了身份。
                    // 绝不能覆盖。锁已释放后再记安全事件。
                    (BindOutcome::KeyConflict, Some(existing))
                }
            } else {
                conn.execute(
                    "INSERT INTO trusted_devices
                        (device_id, device_name, public_key_hex, last_ip, bound_at,
                         is_trusted, trust_level, visible, last_seen_at, pairing_epoch)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9)",
                    params![
                        dev.device_id,
                        dev.device_name,
                        dev.public_key_hex,
                        dev.last_ip,
                        dev.bound_at,
                        if dev.trust_level.is_paired() { 1 } else { 0 },
                        dev.trust_level.as_db_str(),
                        now,
                        dev.pairing_epoch
                    ],
                )?;
                (BindOutcome::Added, None)
            }
        };

        if let Some(existing) = existing_pubkey {
            self.add_security_event(
                SecurityEventKind::KeyConflict,
                &dev.device_id,
                &dev.device_name,
                &format!(
                    "设备提交了与既有绑定不同的公钥, 已拒绝绑定 (已绑定指纹前12位={})",
                    &existing.chars().take(12).collect::<String>()
                ),
            )?;
        }
        Ok(outcome)
    }

    /// 仅更新设备名 / 最近 IP 等非安全字段
    pub fn update_device_meta(&self, device_id: &str, device_name: &str, ip: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trusted_devices SET device_name = ?1, last_ip = ?2 WHERE device_id = ?3",
            params![device_name, ip, device_id],
        )?;
        Ok(())
    }

    pub fn update_last_ip(&self, device_id: &str, ip: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trusted_devices SET last_ip = ?1 WHERE device_id = ?2",
            params![ip, device_id],
        )?;
        Ok(())
    }

    /// 把一台设备降回**未信任**，但**保留这一行**（§14.3.1 的 I9）。
    ///
    /// ## 为什么不是 `DELETE`
    ///
    /// 曾经有一个 `remove_device` 走 `DELETE`，于是连 `visible` 一起删了：
    /// 用户「隐藏 + 解除配对」之后，那台设备下次重新配对会**重新出现在
    /// 主列表**，而他明确说过不想看见它。信任关系没了，
    /// 但「我不想看见它」这条偏好与信任无关，不该被一起清掉。
    ///
    /// 同时清掉 `access_scopes` / `session_grants` —— 那两者是**授权**，
    /// 解除信任后留着它们等于「没解除」。
    ///
    /// ❌ `remove_device` **已删除**（不是弃用）。它唯一的调用方是
    /// `unpair_device` 命令里「找不到对方地址」的兜底分支，于是
    /// **有没有 IP 决定了行是被 UPDATE 还是被 DELETE** ——
    /// 同一个按钮，两种结果，取决于一个用户看不见也控制不了的变量。
    /// 保留它就等于保留这条分叉；删掉它，分叉在编译期就没了。
    /// 守卫 `no_local_only_unpair_path_survives`。
    ///
    /// **代价不存在**：行数上界 = 用户这辈子配对过的设备数
    /// （唯一 INSERT 点是 `bind_device`，且只在真实握手成功后），
    /// 所以**不需要清理策略**。我曾以为需要，那是错的 —— §14.11 决策 3。
    pub fn downgrade_to_untrusted(&self, device_id: &str) -> Result<bool> {
        let conn = self.conn();
        let affected = conn.execute(
            "UPDATE trusted_devices
             SET trust_level = 'pending', is_trusted = 0, pairing_epoch = ''
             WHERE device_id = ?1",
            params![device_id],
        )?;
        let _ = conn.execute(
            "DELETE FROM access_scopes WHERE peer_device_id = ?1",
            params![device_id],
        )?;
        let _ = conn.execute(
            "DELETE FROM session_grants WHERE peer_device_id = ?1",
            params![device_id],
        )?;
        Ok(affected > 0)
    }

    /// 读取某台设备当前的配对世代（空串 = 无共同世代）。
    pub fn pairing_epoch(&self, device_id: &str) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT pairing_epoch FROM trusted_devices WHERE device_id = ?1")?;
        let mut rows = stmt.query(params![device_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(r.get::<_, String>(0)?)),
            None => Ok(None),
        }
    }

    /// 记下某台设备的配对世代（配对 / 重新配对时写）。
    pub fn set_pairing_epoch(&self, device_id: &str, epoch: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trusted_devices SET pairing_epoch = ?1 WHERE device_id = ?2",
            params![epoch, device_id],
        )?;
        Ok(())
    }

    /// 处理对端发来的 `MSG_UNPAIR`（§14.5.1）。
    ///
    /// 四条性质里这个函数负责三条：**不能重放**（世代比对）、
    /// **幂等**（已未信任回 `AlreadyUnpaired`）、**不改错对象**（世代不符
    /// 时绝不动状态）。"不能伪造"由调用方的验签负责 —— 签名用的是
    /// 发起方在本机库中**已绑定**的公钥，所以攻击者无法冒充他人解除关系。
    pub fn apply_peer_unpair(&self, device_id: &str, peer_epoch: &str) -> Result<UnpairOutcome> {
        let Some(current) = self.pairing_epoch(device_id)? else {
            return Ok(UnpairOutcome::NotPaired);
        };
        // ---- 顺序要紧：`AlreadyUnpaired` 必须**先**于世代判断 ----
        //
        // `downgrade_to_untrusted` 会把 `pairing_epoch` 清空（这是对的：
        // 留着就等于重放旧帧仍能命中）。但如果先查世代，一次成功的解除
        // 之后再来第二帧就会落到 `NoSharedEpoch` —— 而那在界面上的文案是
        // "对方版本过旧，请先升级"，对**一个连点两次解除**的用户来说
        // 完全误导。
        //
        // 先判"已经是未信任"是安全的：那种情况下**没有任何东西需要保护**，
        // 本地状态一个字都不会动。而重新配对后等级变回 permanent，
        // 这条分支自然不再命中，世代校验照常生效。
        if self.trust_level(device_id)? == Some(TrustLevel::Pending) {
            return Ok(UnpairOutcome::AlreadyUnpaired);
        }
        // 无共同世代 = 对端是旧版本。此时**不能**盲目降级 —— 那样等于
        // 承诺了一个我们无法验证来源的"双向"解除。明确失败并让上层说明。
        if current.is_empty() || peer_epoch.is_empty() {
            return Ok(UnpairOutcome::NoSharedEpoch);
        }
        if current != peer_epoch {
            // 极可能就是重放: 对方引用的是一次**已经作废**的绑定。
            warn!(
                "拒绝过期的解除配对帧: 设备 {} 的来帧世代与本地不符",
                device_id
            );
            return Ok(UnpairOutcome::EpochMismatch);
        }
        self.downgrade_to_untrusted(device_id)?;
        Ok(UnpairOutcome::Applied)
    }

    pub fn list_devices(&self) -> Result<Vec<TrustedDevice>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT device_id, device_name, public_key_hex, last_ip, bound_at,
                    is_trusted, trust_level, visible, last_seen_at, pairing_epoch
             FROM trusted_devices
             ORDER BY visible ASC, bound_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let trust_raw: String = row.get(6)?;
            let visible_int: i32 = row.get(7)?;
            let level = TrustLevel::from_db_str(&trust_raw);
            Ok(TrustedDevice {
                device_id: row.get(0)?,
                device_name: row.get(1)?,
                public_key_hex: row.get(2)?,
                last_ip: row.get(3)?,
                bound_at: row.get(4)?,
                is_trusted: row.get::<_, i32>(5)? == 1,
                trust_level: level,
                visible: visible_int == 1,
                last_seen_at: row.get(8)?,
                pairing_epoch: row.get(9)?,
            })
        })?;

        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    /// 只列出在列表中可见的设备（§3.7 的「我的设备」分组用）。
    pub fn list_visible_devices(&self) -> Result<Vec<TrustedDevice>> {
        Ok(self
            .list_devices()?
            .into_iter()
            .filter(|d| d.visible)
            .collect())
    }

    /// 只列出已隐藏的设备（「已隐藏」抽屉用，§3.1）。
    pub fn list_hidden_devices(&self) -> Result<Vec<TrustedDevice>> {
        Ok(self
            .list_devices()?
            .into_iter()
            .filter(|d| !d.visible)
            .collect())
    }

    pub fn generate_pair_pin(&self) -> (String, i64) {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let num: u32 = rng.gen_range(100_000..999_999);
        let pin = format!("{:06}", num);
        let expire = chrono::Utc::now().timestamp() + 30; // 30秒严格即时有效，用完即失效
        let mut active = self.active_pair_pin.lock().unwrap_or_else(|e| e.into_inner());
        *active = Some((pin.clone(), expire));
        (pin, expire)
    }

    /// 配对码暴力破解限速: 同一 IP 连续失败 N 次后锁定一段时间。
    pub fn is_pin_locked(&self, ip: &str) -> bool {
        let now = chrono::Utc::now().timestamp();
        let map = self.pin_failures.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(ip) {
            Some((count, unlock_at)) if *count >= MAX_PIN_ATTEMPTS => now < *unlock_at,
            _ => false,
        }
    }

    pub fn register_pin_failure(&self, ip: &str) -> u32 {
        let now = chrono::Utc::now().timestamp();
        let mut map = self.pin_failures.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(ip.to_string()).or_insert((0, 0));
        entry.0 += 1;
        if entry.0 >= MAX_PIN_ATTEMPTS {
            entry.1 = now + PIN_LOCKOUT_SECS;
        }
        entry.0
    }

    pub fn clear_pin_failures(&self, ip: &str) {
        let mut map = self.pin_failures.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(ip);
    }

    /// 校验并消费配对码 (单次有效)
    pub fn verify_pair_pin(&self, pin: &str) -> bool {
        let now = chrono::Utc::now().timestamp();
        let mut active = self.active_pair_pin.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((stored_pin, expire)) = &*active {
            if now <= *expire && constant_time_eq(stored_pin.as_bytes(), pin.as_bytes()) {
                *active = None; // 验证成功单次即时销毁
                return true;
            }
        }
        false
    }

    /// 写一条传输历史。
    ///
    /// ## 为什么多了一堆时间参数（设计文档 §9.6）
    ///
    /// 需求是"传输记录要显示速度"。速度是**派生值**，不落库；
    /// 落库的必须是**原始量**：
    ///
    /// - `declared_size` —— 清单声明的总字节（失败时可能大于实际传输量）
    /// - `bytes_transferred` —— 实际传输字节（作为 `file_size` 写入）
    /// - `duration_active_ms` —— **纯数据流耗时，速度的唯一分母**
    /// - `duration_wall_ms` —— 墙钟耗时（含审批等待，只用于展示"含等待 X"）
    /// - `duration_verify_ms` —— 整文件 BLAKE3 复核耗时（不计入速度）
    ///
    /// 早期实现把 `elapsed` 算出来后用 `let _ = elapsed;` 直接丢弃
    /// （见 `client.rs` 发送循环），本方法就是为了接上这条被切断的管道。
    #[allow(clippy::too_many_arguments)]
    pub fn add_transfer_record(
        &self,
        file_name: &str,
        bytes_transferred: u64,
        direction: &str,
        peer_name: &str,
        peer_ip: &str,
        status: &str,
        max_records: u32,
        retention_days: u32,
        metrics: TransferMetrics,
        file_paths: &[String],
    ) -> Result<()> {
        let conn = self.conn();
        let now = chrono::Utc::now().timestamp();
        let paths_json = serde_json::to_string(file_paths).unwrap_or_else(|_| "[]".to_string());
        conn.execute(
            "INSERT INTO transfer_history
                (file_name, file_size, direction, peer_name, peer_ip, status, created_at,
                 declared_size, duration_active_ms, duration_wall_ms, duration_verify_ms,
                 over_overlay, connect_ms, file_paths)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                file_name,
                bytes_transferred as i64,
                direction,
                peer_name,
                peer_ip,
                status,
                now,
                metrics.declared_size as i64,
                metrics.duration_active_ms as i64,
                metrics.duration_wall_ms as i64,
                metrics.duration_verify_ms as i64,
                if metrics.over_overlay { 1 } else { 0 },
                metrics.connect_ms as i64,
                paths_json,
            ],
        )?;

        // 按保留天数清理过期记录
        if retention_days > 0 {
            let expire_threshold = now - (retention_days as i64 * 86400);
            let _ = conn.execute("DELETE FROM transfer_history WHERE created_at < ?1", params![expire_threshold]);
        }

        // 按最大记录条数上限清理，超出自动删除最老记录
        if max_records > 0 {
            let _ = conn.execute(
                "DELETE FROM transfer_history WHERE id NOT IN (
                    SELECT id FROM transfer_history ORDER BY id DESC LIMIT ?1
                )",
                params![max_records],
            );
        }

        Ok(())
    }

    pub fn list_transfer_records(&self, limit: u32) -> Result<Vec<TransferRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, file_name, file_size, direction, peer_name, peer_ip, status, created_at,
                    declared_size, duration_active_ms, duration_wall_ms, duration_verify_ms,
                    over_overlay, connect_ms, file_paths
             FROM transfer_history ORDER BY id DESC LIMIT ?1"
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            let id: i64 = row.get(0)?;
            let file_name: String = row.get(1)?;
            let file_size_i64: i64 = row.get(2)?;
            let file_size = file_size_i64.max(0) as u64;
            let direction: String = row.get(3)?;
            let peer_name: String = row.get(4)?;
            let peer_ip: String = row.get(5)?;
            let status: String = row.get(6)?;
            let created_at: i64 = row.get(7)?;
            let declared_size = row.get::<_, i64>(8)?.max(0) as u64;
            let duration_active_ms = row.get::<_, i64>(9)?.max(0) as u64;
            let duration_wall_ms = row.get::<_, i64>(10)?.max(0) as u64;
            let duration_verify_ms = row.get::<_, i64>(11)?.max(0) as u64;
            let over_overlay = row.get::<_, i32>(12)? == 1;
            let connect_ms = row.get::<_, i64>(13)?.max(0) as u64;
            let paths_json = row.get::<_, Option<String>>(14)?.unwrap_or_else(|| "[]".to_string());
            let file_paths: Vec<String> = serde_json::from_str(&paths_json).unwrap_or_default();

            let size_formatted = format_bytes(file_size);
            let dt = chrono::DateTime::from_timestamp(created_at, 0)
                .unwrap_or_else(|| chrono::Utc::now());
            let local_dt: chrono::DateTime<chrono::Local> = chrono::DateTime::from(dt);
            let time_formatted = local_dt.format("%m-%d %H:%M").to_string();

            // 速度是派生值, 读取时算而不是落库（口径可能随实现演进, §9.6.6）
            let rec = TransferRecord {
                id,
                file_name,
                file_size,
                file_size_formatted: size_formatted,
                direction,
                peer_name,
                peer_ip,
                status,
                created_at,
                time_formatted,
                declared_size,
                duration_active_ms,
                duration_wall_ms,
                duration_verify_ms,
                over_overlay,
                connect_ms,
                speed_display: String::new(),
                file_paths,
            };
            let speed_display = rec.speed_display();
            Ok(TransferRecord { speed_display, ..rec })
        })?;

        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    pub fn clear_transfer_records(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM transfer_history", [])?;
        Ok(())
    }

    // =======================================================================
    // 会话授权（"每次匹配码"模式，§2.3）
    // =======================================================================

    /// 授予一次会话授权。
    pub fn grant_session(&self, peer_id: &str, scope: &str, ttl_secs: i64) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO session_grants (peer_device_id, scope, granted_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(peer_device_id, scope) DO UPDATE SET
                granted_at = excluded.granted_at,
                expires_at = excluded.expires_at",
            params![peer_id, scope, now, now + ttl_secs],
        )?;
        Ok(())
    }

    /// 是否持有该操作的有效授权（`all` 视为通配）。
    pub fn has_valid_grant(&self, peer_id: &str, scope: &str) -> Result<bool> {
        if peer_id.is_empty() {
            return Ok(false);
        }
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT expires_at FROM session_grants WHERE peer_device_id = ?1 AND scope IN (?2, 'all')",
        )?;
        let mut rows = stmt.query(params![peer_id, scope])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, i64>(0)? > now {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 消费授权（"单次有效"语义）。
    pub fn consume_grant(&self, peer_id: &str, scope: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM session_grants WHERE peer_device_id = ?1 AND scope IN (?2, 'all')",
            params![peer_id, scope],
        )?;
        Ok(())
    }

    /// **撤销**某台设备某类（或全部）短期授权（§2.3.1）。
    ///
    /// ## 为什么这是安全要求而不是便利功能
    ///
    /// `session_grants` 里的每一行都意味着"接下来这段时间这台设备
    /// 不用再确认"。**没有撤销手段的授权不是"可撤销的信任"，
    /// 只是"用户不知道它还在"** —— 而那正是 §14.11.3 反对
    /// "N 分钟免码"的理由：时间窗本身不危险，
    /// **用户看不见、关不掉**才危险。
    ///
    /// 所以「可撤销」与「短期」是**同一件安全性质的两半**：
    /// 短期保证"最坏情况有限"，可撤销保证"用户随时能把最坏情况归零"。
    /// 只做前者，用户在 5 分钟内无能为力。
    ///
    /// `scope` 传空串 = 撤销该设备**全部**类别的授权。
    /// 返回被删的行数（0 = 本来就没有）。
    pub fn revoke_session_grant(&self, peer_id: &str, scope: &str) -> Result<u32> {
        if peer_id.is_empty() {
            return Ok(0);
        }
        let conn = self.conn();
        let n = if scope.trim().is_empty() {
            conn.execute(
                "DELETE FROM session_grants WHERE peer_device_id = ?1",
                params![peer_id],
            )?
        } else {
            conn.execute(
                "DELETE FROM session_grants WHERE peer_device_id = ?1 AND scope IN (?2, 'all')",
                params![peer_id, scope],
            )?
        };
        Ok(n as u32)
    }

    /// 列出某台设备当前**还有效**的短期授权（§2.3.1）。
    ///
    /// 界面上要能看到"这台设备现在免确认到什么时候" ——
    /// 看不见的状态不该存在（见 [`TrustStore::revoke_session_grant`]
    /// 上面那段论证）。
    pub fn list_active_grants(&self, peer_id: &str) -> Result<Vec<(String, i64)>> {
        let conn = self.conn();
        let now = chrono::Utc::now().timestamp();
        let mut stmt = conn.prepare(
            "SELECT scope, expires_at FROM session_grants
             WHERE peer_device_id = ?1 AND expires_at >= ?2
             ORDER BY expires_at DESC",
        )?;
        let rows = stmt.query_map(params![peer_id, now], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 清理过期授权。由引擎定时调用。
    pub fn purge_expired_grants(&self) -> Result<()> {
        let conn = self.conn();
        let now = chrono::Utc::now().timestamp();
        conn.execute("DELETE FROM session_grants WHERE expires_at < ?1", params![now])?;
        Ok(())
    }

    // =======================================================================
    // 安全事件（§3.9）
    // =======================================================================

    pub fn add_security_event(
        &self,
        kind: SecurityEventKind,
        peer_id: &str,
        peer_name: &str,
        detail: &str,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO security_events (kind, peer_device_id, peer_name, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind.as_db_str(), peer_id, peer_name, detail, chrono::Utc::now().timestamp()],
        )?;
        // 只保留最近 200 条, 避免长期运行后无限增长
        let _ = conn.execute(
            "DELETE FROM security_events WHERE id NOT IN (
                SELECT id FROM security_events ORDER BY id DESC LIMIT 200
            )",
            [],
        );
        Ok(())
    }

    pub fn list_security_events(&self, limit: u32) -> Result<Vec<SecurityEvent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, kind, peer_device_id, peer_name, detail, created_at
             FROM security_events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            let created_at: i64 = row.get(5)?;
            Ok(SecurityEvent {
                id: row.get(0)?,
                kind: row.get(1)?,
                peer_device_id: row.get(2)?,
                peer_name: row.get(3)?,
                detail: row.get(4)?,
                created_at,
                time_formatted: format_local_time(created_at),
            })
        })?;
        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    pub fn clear_security_events(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM security_events", [])?;
        Ok(())
    }

    // =======================================================================
    // 访问审计（§8.2 第 2 层）
    // =======================================================================

    pub fn add_access_audit(
        &self,
        peer_id: &str,
        peer_name: &str,
        volume: &str,
        path: &str,
        op: &str,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO access_audit (peer_id, peer_name, volume, path, op, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![peer_id, peer_name, volume, path, op, chrono::Utc::now().timestamp()],
        )?;
        // 审计表增长快, 只留最近 1000 条
        let _ = conn.execute(
            "DELETE FROM access_audit WHERE id NOT IN (
                SELECT id FROM access_audit ORDER BY id DESC LIMIT 1000
            )",
            [],
        );
        Ok(())
    }

    pub fn list_access_audit(&self, limit: u32) -> Result<Vec<AccessAuditRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, peer_id, peer_name, volume, path, op, created_at
             FROM access_audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(AccessAuditRow {
                id: row.get(0)?,
                peer_id: row.get(1)?,
                peer_name: row.get(2)?,
                volume: row.get(3)?,
                path: row.get(4)?,
                op: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;
        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    // =======================================================================
    // 访问范围（§8）
    // =======================================================================

    /// 读取对端的可访问范围；未配置时返回 [`AccessScope::default`]（D1: 默认全部）。
    pub fn get_access_scope(&self, peer_id: &str) -> Result<AccessScope> {
        if peer_id.is_empty() {
            return Ok(AccessScope::default());
        }
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT mode, allow_volumes, allow_paths, deny_paths, can_pull, can_push, updated_at
             FROM access_scopes WHERE peer_device_id = ?1",
        )?;
        let mut rows = stmt.query(params![peer_id])?;
        match rows.next()? {
            None => Ok(AccessScope::default()),
            Some(row) => Ok(AccessScope {
                mode: AccessMode::from_db_str(&row.get::<_, String>(0)?),
                allow_volumes: json_list(&row.get::<_, String>(1)?),
                allow_paths: json_list(&row.get::<_, String>(2)?),
                deny_paths: json_list(&row.get::<_, String>(3)?),
                can_pull: row.get::<_, i32>(4)? == 1,
                can_push: row.get::<_, i32>(5)? == 1,
                updated_at: row.get(6)?,
            }),
        }
    }

    /// 写入对端的可访问范围。
    ///
    /// 范围**放宽**时记一条安全事件 —— §8.2 第 2 层：
    /// 可观测性是安全模型的必要组成，只做限制不做记录是不完整的。
    pub fn set_access_scope(&self, peer_id: &str, scope: &AccessScope) -> Result<()> {
        // 先取旧值（在持锁之前读, 避免不可重入自死锁）
        let previous = self.get_access_scope(peer_id).unwrap_or_default();
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO access_scopes
                (peer_device_id, mode, allow_volumes, allow_paths, deny_paths, can_pull, can_push, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(peer_device_id) DO UPDATE SET
                mode = excluded.mode,
                allow_volumes = excluded.allow_volumes,
                allow_paths = excluded.allow_paths,
                deny_paths = excluded.deny_paths,
                can_pull = excluded.can_pull,
                can_push = excluded.can_push,
                updated_at = excluded.updated_at",
            params![
                peer_id,
                scope.mode.as_db_str(),
                serde_json::to_string(&scope.allow_volumes).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&scope.allow_paths).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&scope.deny_paths).unwrap_or_else(|_| "[]".into()),
                if scope.can_pull { 1 } else { 0 },
                if scope.can_push { 1 } else { 0 },
                now,
            ],
        )?;
        drop(conn);

        let widened = previous.mode != scope.mode
            || previous.allow_volumes.len() < scope.allow_volumes.len()
            || (!previous.can_pull && scope.can_pull)
            || (!previous.can_push && scope.can_push);
        if widened {
            self.add_security_event(
                SecurityEventKind::ScopeWidened,
                peer_id,
                "",
                &format!(
                    "可访问范围由 {} 放宽为 {} (卷 {} -> {}, can_pull={}, can_push={})",
                    previous.mode.as_db_str(),
                    scope.mode.as_db_str(),
                    previous.allow_volumes.len(),
                    scope.allow_volumes.len(),
                    scope.can_pull,
                    scope.can_push
                ),
            )?;
        }
        Ok(())
    }

    // =======================================================================
    // 传输诊断（供事后离线分析）
    // =======================================================================

    /// 落一条完整诊断记录。
    pub fn insert_diagnostics(&self, diag: &crate::transport::TransferDiagnostics) -> Result<()> {
        let payload =
            serde_json::to_string(diag).map_err(crate::error::FeisuoError::Serialization)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO transfer_diagnostics
                (transfer_id, direction, peer_name, peer_ip, outcome, bytes,
                 data_ms, speed_bps, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                diag.transfer_id,
                diag.direction,
                diag.peer_device_name,
                diag.peer_ip,
                diag.outcome,
                diag.bytes_transferred as i64,
                diag.timings.data_ms as i64,
                diag.avg_speed_bps().unwrap_or(0) as i64,
                payload,
                diag.created_at,
            ],
        )?;
        // 只留最近 200 条完整诊断
        let _ = conn.execute(
            "DELETE FROM transfer_diagnostics WHERE id NOT IN (
                SELECT id FROM transfer_diagnostics ORDER BY id DESC LIMIT 200
            )",
            [],
        );
        Ok(())
    }

    /// 记录一个**已完整落盘并提交**的文件（断点续传用）。
    ///
    /// 只允许在整文件 BLAKE3 复核通过、`rename` 提交成功之后调用。
    /// 提前记录等于把截断文件当成完整的 —— 断点续传会把损坏传播下去，
    /// 而且**用户永远查不出来**（文件大小对、名字对、就是内容错）。
    pub fn record_completed_part(
        &self,
        transfer_id: &str,
        sender_id: &str,
        relative_path: &str,
        blake3_hash: &str,
        file_size: u64,
        committed_path: &str,
    ) -> Result<()> {
        let conn = self.conn();
        // 幂等: 同一 (transfer_id, relative_path) 重复提交时覆盖而非报错,
        // 因为用户重传同名文件会走到这里（UNIQUE 冲突是预期路径）。
        conn.execute(
            "INSERT INTO transfer_parts
                (transfer_id, sender_id, relative_path, blake3_hash, file_size, committed_path, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(transfer_id, relative_path) DO UPDATE SET
                blake3_hash    = excluded.blake3_hash,
                file_size      = excluded.file_size,
                committed_path = excluded.committed_path,
                completed_at   = excluded.completed_at",
            params![
                transfer_id,
                sender_id,
                relative_path,
                blake3_hash,
                file_size as i64,
                committed_path,
                chrono::Utc::now().timestamp()
            ],
        )?;
        Ok(())
    }

    /// 查某次传输里**已完整落盘且文件仍在**的条目。
    ///
    /// ## 必须校验文件仍然存在
    ///
    /// 用户在两次传输之间删掉 / 改名了已收文件是常事。只看数据库记录
    /// 会让续传"跳过"一个用户已经主动删掉的文件 —— 结果是用户以为
    /// 重传了，其实什么都没发生，而收件目录里那个文件已经不在了。
    /// 存在性检查很便宜（每个文件一次 `metadata`），换来的是
    /// 续传结果与用户意图一致。
    pub fn completed_parts(
        &self,
        transfer_id: &str,
    ) -> Result<Vec<crate::protocol::CompletedPart>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT relative_path, blake3_hash, file_size, committed_path
             FROM transfer_parts WHERE transfer_id = ?1",
        )?;
        let rows = stmt.query_map(params![transfer_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (rel, hash, size, path) = r?;
            if !std::path::Path::new(&path).is_file() {
                // 文件已被删除/移动 —— 当作没收到过
                continue;
            }
            out.push(crate::protocol::CompletedPart {
                relative_path: rel,
                blake3_hash: hash,
                file_size: size.max(0) as u64,
                committed_path: path,
            });
        }
        Ok(out)
    }

    /// 清理超过 `keep_days` 天的续传记录。
    ///
    /// 主要动机是**隐私**：`committed_path` 记录了用户收件文件的完整路径。
    pub fn purge_completed_parts(&self, keep_days: i64) -> Result<u32> {
        let conn = self.conn();
        let cutoff = chrono::Utc::now().timestamp() - keep_days * 86400;
        let n = conn.execute(
            "DELETE FROM transfer_parts WHERE completed_at < ?1",
            params![cutoff],
        )?;
        Ok(n as u32)
    }

    // =======================================================================
    // 分块级断点续传（transfer_chunk_progress）
    // =======================================================================

    /// 记录或更新某个未完成文件的分块断点续传进度。
    pub fn record_chunk_progress(
        &self,
        transfer_id: &str,
        file_index: u32,
        relative_path: &str,
        blake3_hash: &str,
        file_size: u64,
        staged_path: &str,
        next_chunk_index: u32,
        bytes_resumed: u64,
    ) -> Result<()> {
        let conn = self.conn();
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO transfer_chunk_progress (
                transfer_id, file_index, relative_path, blake3_hash, file_size,
                staged_path, next_chunk_index, bytes_resumed, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(transfer_id, file_index) DO UPDATE SET
                relative_path = excluded.relative_path,
                blake3_hash = excluded.blake3_hash,
                file_size = excluded.file_size,
                staged_path = excluded.staged_path,
                next_chunk_index = excluded.next_chunk_index,
                bytes_resumed = excluded.bytes_resumed,
                updated_at = excluded.updated_at",
            params![
                transfer_id,
                file_index as i64,
                relative_path,
                blake3_hash,
                file_size as i64,
                staged_path,
                next_chunk_index as i64,
                bytes_resumed as i64,
                now,
            ],
        )?;
        Ok(())
    }

    /// 查询某次传输中某个文件的分块续传进度（若暂存文件不存在则返回 None）。
    pub fn get_chunk_progress(
        &self,
        transfer_id: &str,
        file_index: u32,
    ) -> Result<Option<ChunkResumeRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT transfer_id, file_index, relative_path, blake3_hash, file_size,
                    staged_path, next_chunk_index, bytes_resumed, updated_at
             FROM transfer_chunk_progress
             WHERE transfer_id = ?1 AND file_index = ?2",
        )?;
        let mut rows = stmt.query(params![transfer_id, file_index as i64])?;
        if let Some(row) = rows.next()? {
            let staged_path: String = row.get(5)?;
            if !std::path::Path::new(&staged_path).is_file() {
                return Ok(None);
            }
            Ok(Some(ChunkResumeRecord {
                transfer_id: row.get(0)?,
                file_index: row.get::<_, i64>(1)? as u32,
                relative_path: row.get(2)?,
                blake3_hash: row.get(3)?,
                file_size: row.get::<_, i64>(4)? as u64,
                staged_path,
                next_chunk_index: row.get::<_, i64>(6)? as u32,
                bytes_resumed: row.get::<_, i64>(7)? as u64,
                updated_at: row.get(8)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// 单个文件提交成功或重试失败后，清理该文件的分块进度。
    pub fn remove_chunk_progress(&self, transfer_id: &str, file_index: u32) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM transfer_chunk_progress WHERE transfer_id = ?1 AND file_index = ?2",
            params![transfer_id, file_index as i64],
        )?;
        Ok(())
    }

    /// 清除某次传输的所有分块进度（如用户手动撤销本次传输时）。
    pub fn remove_all_chunk_progress(&self, transfer_id: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM transfer_chunk_progress WHERE transfer_id = ?1",
            params![transfer_id],
        )?;
        Ok(())
    }

    /// 清理过期的暂存分块进度（超过 max_age_secs，例如 24 小时）。
    pub fn purge_expired_chunk_progress(&self, max_age_secs: i64) -> Result<u32> {
        let conn = self.conn();
        let cutoff = chrono::Utc::now().timestamp() - max_age_secs;
        let n = conn.execute(
            "DELETE FROM transfer_chunk_progress WHERE updated_at < ?1",
            params![cutoff],
        )?;
        Ok(n as u32)
    }

    // =======================================================================
    // 设备端点（多地址，P1 ⑩ / P4 ⑧）
    // =======================================================================

    /// 记住一个设备端点。
    ///
    /// ## 只对**已在信任库里**的设备生效
    ///
    /// 否则任何人都能往这张表里塞自己的 IP，让所有飞梭实例持续
    /// 向它发包 —— 变成一个放大器。信任库里的设备至少通过了配对。
    ///
    /// `verified = 1` 表示"真的在这个地址上收到过它的签名信标"，
    /// `0` 表示"从别处听说的地址"（例如历史 `last_ip` 迁移过来的）。
    /// 两者都要探，但诊断上要能区分。
    pub fn remember_endpoint(
        &self,
        device_id: &str,
        ip: &str,
        port: u16,
        kind: &str,
        verified: bool,
    ) -> Result<()> {
        if device_id.trim().is_empty() || ip.trim().is_empty() {
            return Ok(());
        }
        if !self.is_device_trusted(device_id)? {
            // 没配对过的设备不记端点 —— 理由见文档注释
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO device_endpoints
                (device_id, ip, port, kind, first_seen, last_seen, verified)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
             ON CONFLICT(device_id, ip, port) DO UPDATE SET
                kind      = excluded.kind,
                last_seen = excluded.last_seen,
                verified  = MAX(device_endpoints.verified, excluded.verified)",
            params![
                device_id,
                ip.trim(),
                port as i64,
                kind,
                now,
                if verified { 1i64 } else { 0i64 }
            ],
        )?;
        Ok(())
    }

    /// 列出全部已知端点（发现层用来单播）。
    ///
    /// 排序刻意是"覆盖网优先 + 最近活跃优先"（D6：主场景是跨地域
    /// ZeroTier）。`kind` 由 `classify_ip` 判定：
    /// `100.64/10` = overlay，`10/8`·`172.16/12`·`192.168/16` = lan，其余 public。
    pub fn all_endpoints(&self) -> Result<Vec<DeviceEndpoint>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT device_id, ip, port, kind, first_seen, last_seen, verified
             FROM device_endpoints",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DeviceEndpoint {
                device_id: row.get(0)?,
                ip: row.get(1)?,
                port: row.get::<_, i64>(2)?.max(0) as u16,
                kind: row.get(3)?,
                first_seen: row.get(4)?,
                last_seen: row.get(5)?,
                verified: row.get::<_, i64>(6)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        // 覆盖网优先；同类里最近活跃的先试（它最可能真的在线）
        out.sort_by(|a, b| {
            let pa = endpoint_priority(&a.kind);
            let pb = endpoint_priority(&b.kind);
            pb.cmp(&pa).then_with(|| b.last_seen.cmp(&a.last_seen))
        });
        Ok(out)
    }

    /// 某台设备当前的**首选**端点（D6：覆盖网优先）。
    ///
    /// 取不到时回落到 `trusted_devices.last_ip`（老库的兼容路径）——
    /// 迁移后那张表还在，不回落会让所有老设备第一次启动时"消失"。
    pub fn preferred_endpoint(&self, device_id: &str) -> Result<Option<DeviceEndpoint>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT device_id, ip, port, kind, first_seen, last_seen, verified
             FROM device_endpoints WHERE device_id = ?1
             ORDER BY last_seen DESC",
        )?;
        let mut best: Option<DeviceEndpoint> = None;
        let rows = stmt.query_map(params![device_id], |row| {
            Ok(DeviceEndpoint {
                device_id: row.get(0)?,
                ip: row.get(1)?,
                port: row.get::<_, i64>(2)?.max(0) as u16,
                kind: row.get(3)?,
                first_seen: row.get(4)?,
                last_seen: row.get(5)?,
                verified: row.get::<_, i64>(6)? != 0,
            })
        })?;
        for r in rows {
            let e = r?;
            match &best {
                None => best = Some(e),
                Some(b) => {
                    if endpoint_priority(&e.kind) > endpoint_priority(&b.kind) {
                        best = Some(e);
                    }
                }
            }
        }
        if best.is_some() {
            return Ok(best);
        }
        // 回落: 老库只有 last_ip
        Ok(self
            .last_ip_of(device_id)?
            .filter(|ip| !ip.trim().is_empty())
            .map(|ip| DeviceEndpoint {
                device_id: device_id.to_string(),
                ip,
                // 端口未知 = 0；发现层会跳过端口为 0 的端点并走主动探测
                port: 0,
                kind: "unknown".to_string(),
                first_seen: 0,
                last_seen: 0,
                verified: false,
            }))
    }

    /// 端点上限清理：每台设备最多留 `keep_per_device` 个，
    /// 且总表不超过 `max_total` 行。
    ///
    /// 不做上限的话，一张用了三年的机器能攒出几百个历史地址，
    /// 每 3 秒全探一遍纯属浪费。
    pub fn prune_endpoints(&self, keep_per_device: usize, max_total: usize) -> Result<u32> {
        let conn = self.conn();
        let now = chrono::Utc::now().timestamp();
        // 1. 长期（30 天）没有任何响应的直接删
        let stale = now - 30 * 86400;
        let mut removed: u32 =
            conn.execute("DELETE FROM device_endpoints WHERE last_seen < ?1", params![stale])?
                as u32;
        // 2. 每台设备只留最近活跃的 N 个
        conn.execute(
            "DELETE FROM device_endpoints WHERE id NOT IN (
                 SELECT id FROM (
                     SELECT id, ROW_NUMBER() OVER (
                         PARTITION BY device_id ORDER BY last_seen DESC
                     ) AS rn
                     FROM device_endpoints
                 ) WHERE rn <= ?1
             )",
            params![keep_per_device as i64],
        )?;
        // 3. 总行数兜底
        let total: i64 =
            conn.query_row("SELECT COUNT(*) FROM device_endpoints", [], |r| r.get(0))?;
        if total as usize > max_total {
            let cut = total - max_total as i64;
            conn.execute(
                "DELETE FROM device_endpoints WHERE id IN (
                     SELECT id FROM device_endpoints ORDER BY last_seen ASC LIMIT ?1
                 )",
                params![cut],
            )?;
            removed += cut as u32;
        }
        Ok(removed)
    }

    fn last_ip_of(&self, device_id: &str) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT last_ip FROM trusted_devices WHERE device_id = ?1")?;
        let mut rows = stmt.query(params![device_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// 导出诊断报告（§9.7）—— **用户测完把这份文件发回来，我据此分析**。
    ///
    /// ## 为什么报告要"能直接回答问题"，而不是只倒一堆原始行
    ///
    /// 这份报告的**唯一用途**是别人（我）拿到它之后不必再追问。所以：
    ///
    /// - **必须有构建与时间上下文**。同一台机器上"昨天 3 MB/s、今天 30 MB/s"
    ///   如果不知道中间换过版本、没有意义 —— 数字会被归到错的因果上。
    /// - **必须先给摘要**。50 行原始日志要人（或我）逐行扫，而摘要能一眼
    ///   回答"有没有失败""是不是被 buffer 限住了""速度波动大不大"。
    /// - **必须显式报告解析失败的条数**。按 D12，新增字段若漏了
    ///   `#[serde(default)]`，历史记录会**整条无法反序列化**；而这份报告
    ///   原本只是把 payload 原样打出来，于是"全部解析失败"和"没有数据"
    ///   在报告里**长得一模一样**。
    /// - **必须带上安全事件**。被拒 / 被拉黑 / 码输错这些在传输诊断里
    ///   完全看不到，但它们常常正是"为什么没传成"的答案。
    ///
    /// 发现侧数据（收包数、采纳数、回包数）**不在这里** —— 它是进程内的
    /// 计数器，没有落库；报告末尾会指明去哪看。
    pub fn export_diagnostics_report(&self, limit: u32, self_device_id: &str) -> Result<String> {
        // ---- 锁的作用域必须包住"读完就放"，不能覆盖整个函数 ----
        //
        // `self.conn()` 返回的是 `MutexGuard`，而 `std::sync::Mutex`
        // **不可重入**。本函数后面还要调 `self.list_security_events()`，
        // 它内部又去 `self.conn()` 拿**同一把**锁 —— 于是当前线程拿着锁
        // 再去锁它自己，**永久死锁**。
        //
        // 症状极难定位：不是报错，是**整个函数再也不返回**。
        // 用户侧表现是点「导出诊断报告」之后窗口永久无响应（这条路径
        // 跑在 UI 线程上），而 CPU 占用为 0，看起来像"程序没反应了"。
        // 本函数早先把 `let conn = self.conn();` 写在函数开头，编译器
        // 完全满意 —— 借用检查器管不到互斥量的运行时语义。
        //
        // 所以：把持锁的部分收进一个块，块结束即释放。
        let (diags, unparsed) = {
            let conn = self.conn();
            let mut stmt = conn
                .prepare("SELECT payload FROM transfer_diagnostics ORDER BY id DESC LIMIT ?1")?;
            let rows = stmt.query_map(params![limit], |row| row.get::<_, String>(0))?;
            let mut diags: Vec<crate::transport::TransferDiagnostics> = Vec::new();
            let mut unparsed: Vec<String> = Vec::new();
            for r in rows {
                let payload: String = r?;
                match serde_json::from_str::<crate::transport::TransferDiagnostics>(&payload) {
                    Ok(d) => diags.push(d),
                    Err(_) => unparsed.push(payload),
                }
            }
            (diags, unparsed)
        };

        let mut out = String::new();
        // ---- 抬头：没有这些，数字无法归因 ----
        out.push_str("# 飞梭传输诊断报告\n\n");
        out.push_str(&format!("- 生成时间: {}\n", chrono::Local::now().to_rfc3339()));
        out.push_str(&format!("- 飞梭版本: {}\n", env!("CARGO_PKG_VERSION")));
        out.push_str(&format!("- 操作系统: {}\n", std::env::consts::OS));
        out.push_str(&format!("- 本机设备: {}\n", self_device_id));
        out.push_str(&format!("- 请求条数: {}\n", limit));

        out.push_str(&format!(
            "- 成功解析: {} 条 · **解析失败: {} 条**\n",
            diags.len(),
            unparsed.len()
        ));
        if !unparsed.is_empty() {
            out.push_str(
                "\n> ⚠️ 有记录无法解析。最常见原因是**新版本加了字段但漏了 \
                 `#[serde(default)]`**（D12 铁律），或这些记录来自更老的版本。\n\
                 > 若这里条数很多，说明诊断数据不可信，请连同日志一起反馈。\n",
            );
        }
        out.push('\n');

        // ---- 摘要 ----
        out.push_str("## 摘要\n\n");
        if diags.is_empty() {
            out.push_str("（无可用诊断记录）\n\n");
        } else {
            use std::collections::BTreeMap;
            let mut by_outcome: BTreeMap<&str, u32> = BTreeMap::new();
            let mut by_dir: BTreeMap<&str, u32> = BTreeMap::new();
            let mut speeds: Vec<u64> = Vec::new();
            let mut under_bdp = 0u32;
            let mut regrow_tried = 0u32;
            let mut regrown = 0u32;
            let mut integrity_bad = 0u32;
            let mut links: BTreeMap<String, u32> = BTreeMap::new();
            let mut failed_reasons: BTreeMap<String, u32> = BTreeMap::new();

            for d in &diags {
                *by_outcome.entry(d.outcome.as_str()).or_default() += 1;
                *by_dir.entry(d.direction.as_str()).or_default() += 1;
                if let Some(b) = d.avg_speed_bps() {
                    speeds.push(b);
                }
                if d.bdp.under_bdp {
                    under_bdp += 1;
                }
                if d.bdp.regrow_tried {
                    regrow_tried += 1;
                }
                if d.bdp.regrown {
                    regrown += 1;
                }
                if !d.integrity_ok {
                    integrity_bad += 1;
                }
                *links.entry(d.link.describe()).or_default() += 1;
                if d.outcome == "failed" {
                    let r = d
                        .error
                        .as_deref()
                        .unwrap_or("未说明原因")
                        .chars()
                        .take(60)
                        .collect::<String>();
                    *failed_reasons.entry(r).or_default() += 1;
                }
            }

            out.push_str(&format!(
                "- 方向: {}\n",
                by_dir
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" · ")
            ));
            out.push_str(&format!(
                "- 结果: {}\n",
                by_outcome
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" · ")
            ));
            if !speeds.is_empty() {
                speeds.sort_unstable();
                let mid = speeds.len() / 2;
                out.push_str(&format!(
                    "- 纯数据流速度: 最低 {} · **中位 {}** · 最高 {}\n",
                    format_bytes(speeds[0]),
                    format_bytes(speeds[mid]),
                    format_bytes(speeds[speeds.len() - 1]),
                ));
            }
            out.push_str(&format!(
                "- 缓冲区: 被 BDP 卡住 {under_bdp} 条 · 尝试过二次调优 {regrow_tried} 条 · 真的调大了 {regrown} 条\n"
            ));
            out.push_str(&format!(
                "- BLAKE3 校验失败: {integrity_bad} 条{}\n",
                if integrity_bad == 0 {
                    "（0 = 数据完整）"
                } else {
                    " ← 有损坏，必须查"
                }
            ));
            out.push_str(&format!(
                "- 链路画像: {}\n",
                links
                    .iter()
                    .map(|(k, v)| format!("{k} ×{v}"))
                    .collect::<Vec<_>>()
                    .join(" · ")
            ));
            if !failed_reasons.is_empty() {
                out.push_str("- 失败原因:\n");
                for (r, c) in &failed_reasons {
                    out.push_str(&format!("  ×{c}  {r}\n"));
                }
            }
            out.push('\n');
        }

        // ---- 原始记录 ----
        out.push_str(&format!("## 逐条记录（最多 {limit} 条，最新在前）\n\n"));
        for d in &diags {
            out.push_str(&d.to_log_line());
            out.push_str("\n  归因: ");
            out.push_str(&d.attribution());
            out.push_str("\n\n");
        }
        if !unparsed.is_empty() {
            out.push_str("## 解析失败的原始记录\n\n");
            for p in &unparsed {
                out.push_str(&format!("{p}\n\n"));
            }
        }

        // ---- 安全事件：常是"为什么没传成"的答案 ----
        //
        // 标题**无条件**输出，为空时写"（无）"。
        //
        // 理由和上面的"解析失败条数"是同一条：这份报告的唯一用途是
        // "别人拿到后不必再追问"，所以"这一节我没看"和"这一节是空的"
        // 必须能区分。早先只在 `!events.is_empty()` 时输出标题，于是
        // 干净库的报告里**整节消失** —— 读报告的人无法判断是"没有事件"
        // 还是"生成报告时没查这一项"，而后者恰好是更该担心的一种。
        let events = self.list_security_events(20).unwrap_or_default();
        out.push_str("## 安全事件（最近 20 条）\n\n");
        if events.is_empty() {
            out.push_str("（无 —— 本次查询范围内没有被拒 / 被拉黑 / 码不匹配等记录）\n\n");
        } else {
            for e in &events {
                out.push_str(&format!(
                    "- [{}] {} · {}{}\n",
                    e.time_formatted,
                    e.kind,
                    if e.peer_name.is_empty() {
                        "(未知名)".to_string()
                    } else {
                        e.peer_name.clone()
                    },
                    if e.detail.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", e.detail)
                    }
                ));
            }
            out.push('\n');
        }

        // ---- 去哪找发现侧数据 ----
        out.push_str(
            "## 发现侧数据不在本报告里\n\n\
             收包 / 采纳 / 回包 的计数是**进程内**计数器，没有落库。\n\
             它在日志文件里，每 30 秒一条 `[发现] ...`：\n\
             - Windows: `%LOCALAPPDATA%\\feisuo\\feisuo.log`\n\
             - 「搜不到设备」时请把该文件一并反馈 —— 它能区分\n  \
               `一个包都没收到`（网络层不通）/ `收到了但全被拒`（信任或版本）/\n  \
               `采纳了但没回包`（回包路径），这三者界面表现完全一样。\n",
        );
        Ok(out)
    }

    /// 读回诊断记录（UI 用）。
    pub fn list_diagnostics(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::transport::TransferDiagnostics>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT payload FROM transfer_diagnostics ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |row| row.get::<_, String>(0))?;
        let mut list = Vec::new();
        for r in rows {
            if let Ok(d) = serde_json::from_str::<crate::transport::TransferDiagnostics>(&r?) {
                list.push(d);
            }
        }
        Ok(list)
    }
}

/// 写入传输历史时附带的度量数据。
///
/// 全 0 默认值让旧调用点可以先传 [`TransferMetrics::default`] 逐步接入；
/// **样本不足时速度显示"—"，绝不会算出假数字**。
#[derive(Debug, Clone, Copy, Default)]
pub struct TransferMetrics {
    pub declared_size: u64,
    pub duration_active_ms: u64,
    pub duration_wall_ms: u64,
    pub duration_verify_ms: u64,
    pub over_overlay: bool,
    pub connect_ms: u64,
}

impl TransferMetrics {
    /// 从 [`crate::transport::TransferDiagnostics`] 提取。
    pub fn from_diagnostics(d: &crate::transport::TransferDiagnostics) -> Self {
        Self {
            declared_size: d.bytes_declared,
            duration_active_ms: d.timings.data_ms,
            duration_wall_ms: d.timings.total_ms,
            duration_verify_ms: d.timings.verify_ms,
            over_overlay: d.link.over_overlay,
            connect_ms: d.link.tcp_connect_ms,
        }
    }
}

/// 访问审计的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessAuditRow {
    pub id: i64,
    pub peer_id: String,
    pub peer_name: String,
    pub volume: String,
    pub path: String,
    pub op: String,
    pub created_at: i64,
}

/// 解析数据库里的 JSON 字符串数组；损坏时回落为空列表。
///
/// **刻意不返回 Err**：范围配置损坏时应当"退化成默认"而不是让整个
/// 浏览/取回功能不可用 —— 默认值是"全部可读"，配合强制排除清单仍然安全。
fn json_list(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

/// 统一的时间格式化（本地时区 `MM-DD HH:MM`）。
fn format_local_time(ts: i64) -> String {
    let dt = chrono::DateTime::from_timestamp(ts, 0).unwrap_or_else(|| chrono::Utc::now());
    chrono::DateTime::<chrono::Local>::from(dt)
        .format("%m-%d %H:%M")
        .to_string()
}

/// 固定时长的常量时间比较, 避免通过响应耗时旁路爆破 6 位配对码。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
