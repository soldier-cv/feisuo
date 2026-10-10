use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use crate::error::Result;

pub const DEFAULT_TRANSFER_PORT: u16 = 42100;
pub const DEFAULT_DISCOVERY_PORT: u16 = 42101;
pub const DISCOVERY_MULTICAST_ADDR: &str = "239.255.42.100";

/// 关闭主窗口时的默认策略
pub const CLOSE_ACTION_ASK: &str = "ask";
pub const CLOSE_ACTION_TRAY: &str = "tray";
pub const CLOSE_ACTION_EXIT: &str = "exit";

fn default_log_level() -> String { "INFO".to_string() }
fn default_max_log_size_mb() -> u32 { 5 }
fn default_max_records() -> u32 { 500 }
fn default_retention_days() -> u32 { 30 }
fn default_close_action() -> String { CLOSE_ACTION_ASK.to_string() }
fn default_theme() -> String { "dark".to_string() }
fn default_discovery_bind() -> String { "0.0.0.0".to_string() }
fn default_transfer_bind() -> String { "0.0.0.0".to_string() }
fn default_approval_timeout() -> u64 { 60 }
fn default_auto_check_update() -> bool { true }
fn default_max_concurrent_transfers() -> u32 { 3 }

/// 「同类操作短期授权」的默认秒数（§2.3.1）：5 分钟。
fn default_session_grant_ttl() -> u64 { SESSION_GRANT_DEFAULT_SECS }

/// 「同类操作短期授权」的**硬上限**（§2.3.1）。
///
/// 刻意是常量而非配置项：它守的是安全属性而不是偏好。
/// 配成 1 小时的话，这张授权在用户心智里就等于 `permanent`，
/// 而 `session` 这一档的全部价值就是"每次都要确认"。
pub const SESSION_GRANT_MAX_SECS: u64 = 600;
pub const SESSION_GRANT_DEFAULT_SECS: u64 = 300;

/// 把配置里的秒数收敛成实际生效的秒数（§2.3.1）。
///
/// 返回 `None` = 功能关闭（配 0）。**上界在这里强制**，
/// 不依赖调用方记得检查 —— 安全边界应该由一个函数说了算。
pub fn effective_session_grant_ttl(configured: u64) -> Option<u64> {
    if configured == 0 {
        None
    } else {
        Some(configured.min(SESSION_GRANT_MAX_SECS))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub device_name: String,
    pub transfer_port: u16,
    pub discovery_port: u16,
    pub auto_receive: bool,
    pub autostart: bool,
    pub receive_dir: PathBuf,

    /// **来源网段白名单**：只接受来自这些网段的连接（CIDR 列表）。
    ///
    /// 空列表 = **不限制**（保持既有行为）。设了之后，配对与传输都必须来自
    /// 白名单里的网段，见 `auth_policy::subnet_verdict`。
    ///
    /// ## 为什么需要它
    ///
    /// 实测：传输端口绑在 `0.0.0.0`（所有网卡），而 Windows 防火墙的放行
    /// 规则作用域是 `Private, Public` —— **包含公用网络**。用户在首次运行的
    /// 弹窗上点一次「允许」，公网那道门就也开了。此后只要路由器做了端口转发、
    /// 或机器拿到公网 IPv4、或有全局可路由 IPv6，应用层**不会有任何东西
    /// 拒绝**。
    ///
    /// 防火墙是第一道，它挡不住"用户点了允许"这件事；这道是第二道。
    ///
    /// 常用写法：`192.168.31.0/24`（家里 Wi-Fi）、`100.64.0.0/10`（ZeroTier）。
    /// 无 `/` 时按 `/24` 处理。
    #[serde(default)]
    pub allowed_peer_subnets: Vec<String>,

    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_max_log_size_mb")]
    pub max_log_size_mb: u32,
    #[serde(default = "default_max_records")]
    pub max_history_records: u32,
    #[serde(default = "default_retention_days")]
    pub record_retention_days: u32,
    /// 发现服务绑定的地址。默认 0.0.0.0 (监听所有网卡)。
    /// 单机多实例 / 集成测试时可以改绑到指定回环地址, 避免互相抢占端口。
    #[serde(default = "default_discovery_bind")]
    pub discovery_bind: String,
    /// 传输服务绑定的地址。默认 0.0.0.0 (监听所有网卡)。
    /// 单机测试时可以改绑到 127.0.0.1 回环地址, 避免触发系统防火墙弹窗。
    #[serde(default = "default_transfer_bind")]
    pub transfer_bind: String,
    /// ask = 每次询问(最小化到托盘 / 退出), tray = 直接最小化, exit = 直接退出
    #[serde(default = "default_close_action")]
    pub close_action: String,
    /// 界面主题: dark | light
    #[serde(default = "default_theme")]
    pub theme: String,
    /// 人工审批等待秒数（§9.8.3）。
    ///
    /// 旧实现硬编码 60s。跨地域场景（§9.8）下对方可能在路上 / 无网，
    /// 60s 大概率不够，而**超时即直接拒绝**（fail-closed）⇒ 用户会频繁
    /// 遇到"明明想收却被拒"。改为可配，默认仍为 60s 以保持既有行为。
    #[serde(default = "default_approval_timeout")]
    pub approval_timeout_secs: u64,
    /// 「同类操作短期授权」的有效秒数（§2.3.1）。
    ///
    /// 用户在审批弹窗里点「允许本次，并在 X 分钟内免重复确认」时，
    /// 写入 `session_grants` 的那张授权按这个秒数过期。
    ///
    /// ## 为什么必须有上界
    ///
    /// 这张授权的安全前提是"短"。一旦允许配成几小时，
    /// 它在用户心智里就等于 `permanent` —— 而那正是
    /// `session` 这一档**存在的理由**（每次都要确认）。
    /// 所以：
    ///
    /// - **默认 300s（5 分钟）** —— 够"连着发几个文件"用完；
    /// - **硬上限 [`SESSION_GRANT_MAX_SECS`]（10 分钟）** ——
    ///   读取时 `min(配置, 上限)`，配大了也只生效 10 分钟。
    ///   这条上界是**代码里的常量**而不是配置项，
    ///   因为它守的是安全属性，不是偏好。
    ///
    /// 配 0 = 关闭这个功能（弹窗里不出现第三个按钮）。
    #[serde(default = "default_session_grant_ttl")]
    pub session_grant_ttl_secs: u64,
    /// 是否后台静默检查更新 (桌面端)。
    ///
    /// 默认开启: 飞梭是托盘常驻应用, 用户可能几周不开一次主界面,
    /// 关掉这个开关意味着新版本要靠用户自己想起来才拿得到。
    /// 该字段由桌面端宿主读写, core 本身不消费它。
    #[serde(default = "default_auto_check_update")]
    pub auto_check_update: bool,
    /// 最大并发传输任务数，默认 3，取值范围 1..=10
    #[serde(default = "default_max_concurrent_transfers")]
    pub max_concurrent_transfers: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        let default_name = device_display_name();

        Self {
            device_name: default_name,
            transfer_port: DEFAULT_TRANSFER_PORT,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            auto_receive: true,
            autostart: true,
            receive_dir: default_receive_dir(),
            log_level: default_log_level(),
            max_log_size_mb: default_max_log_size_mb(),
            max_history_records: default_max_records(),
            record_retention_days: default_retention_days(),
            discovery_bind: default_discovery_bind(),
            transfer_bind: default_transfer_bind(),
            close_action: default_close_action(),
            theme: "dark".to_string(),
            auto_check_update: default_auto_check_update(),
            max_concurrent_transfers: default_max_concurrent_transfers(),
            approval_timeout_secs: default_approval_timeout(),
            session_grant_ttl_secs: default_session_grant_ttl(),
            // 空 = 不限制来源网段。见 `allowed_peer_subnets` 的注释：
            // 不默认收紧是为了不改变既有行为，但界面上必须能看出"当前未设置"。
            allowed_peer_subnets: Vec::new(),
        }
    }
}

/// 应用私有数据目录 (存放配置 / 信任库 / 私钥)。
///
/// 显式设置 `FEISUO_APP_DIR` 可覆盖 —— Android 宿主需要在启动时把它
/// 指向 app 私有目录, 而不是让 core 去猜平台约定。
pub fn resolve_app_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FEISUO_APP_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    #[cfg(target_os = "android")]
    {
        // Android 的 XDG 变量基本都不存在, dirs::data_local_dir() 会返回 None,
        // 旧代码因此退化成相对路径 "./.feisuo" —— CWD 不可持久化, 每次启动都是新目录。
        PathBuf::from("/data/data/net.findfine.feisuo/files")
    }

    #[cfg(not(target_os = "android"))]
    {
        dirs::data_local_dir()
            .map(|d| d.join("feisuo"))
            .unwrap_or_else(|| PathBuf::from(".feisuo"))
    }
}

impl AppConfig {
    pub fn get_app_dir() -> PathBuf {
        resolve_app_dir()
    }

    pub fn get_log_dir() -> PathBuf {
        Self::get_app_dir().join("logs")
    }

    pub fn get_log_file_path() -> PathBuf {
        Self::get_log_dir().join("feisuo.log")
    }

    pub fn config_path() -> PathBuf {
        Self::get_app_dir().join("config.json")
    }

    /// 读取配置。文件缺失或内容损坏时回落到默认值并重写配置文件。
    /// 平台约定目录下的入口 (桌面端用)。
    pub fn load_or_default() -> Self {
        Self::load_or_default_in(&Self::get_app_dir())
    }

    /// 在指定目录下读取配置。
    ///
    /// 沙箱平台 (Android / iOS) 必须走这个入口: 平台目录只能由宿主
    /// 通过 `context.filesDir` 得知, core 猜不到, 猜错会导致配置与
    /// 设备指纹每次冷启动都落到不同位置。
    pub fn load_or_default_in(app_dir: &Path) -> Self {
        let path = app_dir.join("config.json");
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                match serde_json::from_str::<AppConfig>(&content) {
                    Ok(mut cfg) => {
                        // 枚举字段越界时归位, 避免脏数据把 UI 卡死
                        if !matches!(
                            cfg.close_action.as_str(),
                            CLOSE_ACTION_ASK | CLOSE_ACTION_TRAY | CLOSE_ACTION_EXIT
                        ) {
                            cfg.close_action = default_close_action();
                        }
                        if !matches!(cfg.theme.as_str(), "dark" | "light") {
                            cfg.theme = default_theme();
                        }
                        if !matches!(cfg.log_level.as_str(), "INFO" | "DEBUG" | "WARN" | "ERROR") {
                            cfg.log_level = default_log_level();
                        }
                        // 设备名无条件消毒, 不只是判空:
                        // 配置文件是可以被手工改的, 里面塞一个带 `\n` 的名字
                        // 会让广播出去的信标污染对端 UI 与日志。存回消毒后的值,
                        // 保证"本机看到的"与"对端看到的"始终是同一个名字。
                        cfg.device_name = sanitize_device_name(&cfg.device_name);
                        if cfg.discovery_bind.trim().is_empty()
                            || cfg.discovery_bind.parse::<std::net::IpAddr>().is_err()
                        {
                            tracing::warn!(
                                "发现服务绑定地址 {:?} 非法, 已重置为 0.0.0.0",
                                cfg.discovery_bind
                            );
                            cfg.discovery_bind = default_discovery_bind();
                        }
                        if cfg.transfer_bind.trim().is_empty()
                            || cfg.transfer_bind.parse::<std::net::IpAddr>().is_err()
                        {
                            tracing::warn!(
                                "传输服务绑定地址 {:?} 非法, 已重置为 0.0.0.0",
                                cfg.transfer_bind
                            );
                            cfg.transfer_bind = default_transfer_bind();
                        }
                        return cfg;
                    }
                    Err(e) => {
                        // 配置损坏时不能让整个应用起不来: 备份后重建默认配置
                        tracing::warn!("配置文件解析失败({}), 已备份并重建默认配置: {}", e, path.display());
                        let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    }
                }
            }
        }
        let default_cfg = Self::default();
        let _ = default_cfg.save_in(app_dir);
        default_cfg
    }

    /// 原子保存: 先写临时文件再 rename, 避免写一半掉电导致配置损坏。
    pub fn save(&self) -> Result<()> {
        self.save_in(&Self::get_app_dir())
    }

    /// 保存到指定目录 (沙箱平台入口)。
    pub fn save_in(&self, app_dir: &Path) -> Result<()> {
        if !app_dir.exists() {
            std::fs::create_dir_all(app_dir)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        let target = app_dir.join("config.json");
        let tmp = target.with_extension("json.tmp");
        std::fs::write(&tmp, content)?;
        // 某些文件系统上 rename 覆盖已存在文件会失败, 先移除旧文件再重命名
        if target.exists() {
            let _ = std::fs::remove_file(&target);
        }
        std::fs::rename(&tmp, &target)?;
        Ok(())
    }
}

/// 设备在局域网上显示的名字。
///
/// 旧实现只读 `COMPUTERNAME` / `HOSTNAME` 两个 Windows 环境变量,
/// 在 Linux / macOS / Android 上恒定拿不到, 全部设备都会显示成同一个
/// "我的设备" —— 局域网上出现多个同名设备, 用户根本分不清谁是谁。
fn device_display_name() -> String {
    // 1. 显式配置过就用配置
    if let Ok(name) = std::env::var("FEISUO_DEVICE_NAME") {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return sanitize_device_name(trimmed);
        }
    }
    // 2. 平台原生方式
    #[cfg(target_os = "windows")]
    {
        if let Ok(host) = std::env::var("COMPUTERNAME") {
            if !host.trim().is_empty() {
                return sanitize_device_name(&host);
            }
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Android 没有 hostname 环境变量, 读系统属性 android.os.Build.MODEL
        let model = android_build_model();
        if let Some(m) = model {
            if !m.trim().is_empty() {
                return sanitize_device_name(&m);
            }
        }
    }
    // 3. 通用兜底: 主机名
    for key in ["HOSTNAME", "HOST"] {
        if let Ok(host) = std::env::var(key) {
            if !host.trim().is_empty() {
                return sanitize_device_name(&host);
            }
        }
    }
    // 4. 与身份指纹关联, 保证同一台设备每次启动名字稳定
    if let Some(id) = crate::security::DeviceIdentity::current_device_id_hint() {
        // 取指纹尾部 6 位: 头部对所有设备都是 "feisuo-", 没有区分度
        let tail: String = id.chars().skip(id.len().saturating_sub(6)).collect();
        return format!("飞梭-{}", tail);
    }
    "飞梭设备".to_string()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn android_build_model() -> Option<String> {
    // Android 上没有 getprop 命令, 直接读系统属性文件
    let props = [
        "/system/build.prop",
        "/vendor/build.prop",
        "/default.prop",
    ];
    for p in props {
        if let Ok(content) = std::fs::read_to_string(p) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("ro.product.model=") {
                    let v = rest.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// 设备名会直接广播给局域网内其他设备, 必须消毒:
/// 空串、控制字符、超长、纯空白都会让对端 UI 排版错乱。
pub fn sanitize_device_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{0}')
        .collect::<String>()
        .trim()
        .to_string();
    let trimmed: String = cleaned.chars().take(32).collect();
    if trimmed.is_empty() {
        "飞梭设备".to_string()
    } else {
        trimmed
    }
}

/// 各平台默认的收件落盘目录。
///
/// 旧实现无条件用 `dirs::home_dir()/feisuo`:
/// 在 Android 上 home_dir 往往是 `None`(拿到就退化成相对路径 `./feisuo`),
/// 而 Android 应用的 CWD 不是可持久化位置, 配置重启后即失效。
fn default_receive_dir() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        // Android 公共下载目录: 用户能在文件管理器里直接找到
        let dir = PathBuf::from("/sdcard/Download/feisuo");
        if std::fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
        // 无外部存储权限时退回应用私有目录(一定能建)
        return PathBuf::from("/data/data/net.findfine.feisuo/files/feisuo");
    }

    #[cfg(not(target_os = "android"))]
    {
        dirs::home_dir()
            .map(|h| h.join("feisuo"))
            .unwrap_or_else(|| PathBuf::from("./feisuo"))
    }
}
