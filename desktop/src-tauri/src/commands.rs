use std::path::{Path, PathBuf};
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use feisuo_core::storage::PathManager;
use feisuo_core::{
    DiscoveredDevice, FeisuoEngine, FeisuoError, RemoteFileEntry, TrustedDevice,
    CLOSE_ACTION_ASK, CLOSE_ACTION_EXIT, CLOSE_ACTION_TRAY,
};

use crate::autostart::AutostartManager;
use crate::os_shim::open_in_explorer;
use crate::updater::{UpdateService, UpdateStatus};

pub struct AppState {
    pub engine: Arc<FeisuoEngine>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LocalDeviceInfo {
    pub device_id: String,
    pub device_name: String,
    pub local_ip: String,
    pub transfer_port: u16,
    pub discovery_port: u16,
    pub receive_dir: String,
    pub autostart: bool,
    pub auto_receive: bool,
    pub log_level: String,
    pub max_log_size_mb: u32,
    pub max_history_records: u32,
    pub record_retention_days: u32,
    pub close_action: String,
    pub theme: String,
    pub device_count: usize,
    pub trusted_count: usize,
    /// 桌面端当前版本 (来自 CARGO_PKG_VERSION, 与发布 tag 同源)
    pub app_version: String,
    /// 是否后台静默检查更新
    pub auto_check_update: bool,
    /// 来源网段白名单（CIDR）。**空 = 接受任何来源**。
    ///
    /// 设置页要能**读回已保存的值**，否则用户存完刷新就看到空的，
    /// 以为没保存成功。
    pub allowed_peer_subnets: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiskFileInfo {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub size_formatted: String,
    pub modified: String,
}

fn format_bytes(bytes: u64) -> String {
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

fn format_time(secs: std::time::SystemTime) -> String {
    secs.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| chrono::DateTime::from_timestamp(d.as_secs() as i64, 0))
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

/// 分页列举本机目录。返回 `(本页条目, 总条目数, 还有更多)`。
///
/// ## 为什么先全读再排序再切片
///
/// 和对端浏览同一个理由（见 `server.rs::list_dir_entries_paged`）：
/// 目录项的返回顺序由文件系统决定（NTFS 的 B-tree、ext4 的 hash 都不一样），
/// 不排序的话"第 2 页"会和"第 1 页"重复或漏文件 —— 用户表现为"往下滚有重复"。
/// 而 `skip/take` 作用在未排序的原始顺序上，分界点每次请求都在漂移。
fn read_dir_entries_paged(
    path: &Path,
    offset: usize,
    limit: usize,
) -> (Vec<DiskFileInfo>, u32, bool) {
    let (mut items, total) = read_dir_sorted(path);
    let total_u32 = total.min(u32::MAX as usize) as u32;
    if offset >= items.len() {
        return (Vec::new(), total_u32, false);
    }
    items = items.split_off(offset);
    let shown = items.len();
    items.truncate(limit);
    (items, total_u32, offset + shown < total)
}

/// 读全目录 + 排序（目录在前, 同类按名）。返回 `(全部条目, 过滤后总数)`。
fn read_dir_sorted(path: &Path) -> (Vec<DiskFileInfo>, usize) {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                let name = entry.file_name().to_string_lossy().to_string();
                // 隐藏项一律不显示：内部的 `.feisuo-incoming` 暂存目录
                // 露出来只会让用户以为"有个文件卡住了"。
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = meta.is_dir();
                let size = if is_dir { 0 } else { meta.len() };
                result.push(DiskFileInfo {
                    name,
                    is_dir,
                    size,
                    size_formatted: if is_dir {
                        "文件夹".to_string()
                    } else {
                        format_bytes(size)
                    },
                    modified: meta
                        .modified()
                        .map(format_time)
                        .unwrap_or_else(|_| "-".to_string()),
                });
            }
        }
    }
    result.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    let total = result.len();
    // 旧路径（无分页）仍受 MAX_LOCAL_DIR_ENTRIES 保护
    if total > MAX_LOCAL_DIR_ENTRIES {
        result.truncate(MAX_LOCAL_DIR_ENTRIES);
    }
    (result, total)
}

/// 本机目录列举的条目上限。取 1000 与对端 `MAX_BROWSE_ENTRIES` 对齐,
/// 两侧行为一致才不至于让人以为"本机能看更多 / 对方不能看全"。
const MAX_LOCAL_DIR_ENTRIES: usize = 1000;

#[tauri::command]
pub async fn get_local_info(state: State<'_, AppState>) -> Result<LocalDeviceInfo, String> {
    let local_ip = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "127.0.0.1".into());

    let device_count = state.engine.discovery.get_online_devices().await.len();
    let trusted_count = state.engine.list_trusted_devices().map(|d| d.len()).unwrap_or(0);

    let cfg = state.engine.config.read().await;
    Ok(LocalDeviceInfo {
        device_id: state.engine.identity.device_id.clone(),
        device_name: cfg.device_name.clone(),
        local_ip,
        transfer_port: cfg.transfer_port,
        discovery_port: cfg.discovery_port,
        receive_dir: cfg.receive_dir.to_string_lossy().to_string(),
        autostart: AutostartManager::is_autostart_enabled(),
        auto_receive: cfg.auto_receive,
        log_level: cfg.log_level.clone(),
        max_log_size_mb: cfg.max_log_size_mb,
        max_history_records: cfg.max_history_records,
        record_retention_days: cfg.record_retention_days,
        close_action: cfg.close_action.clone(),
        theme: cfg.theme.clone(),
        device_count,
        trusted_count,
        app_version: crate::updater::current_version(),
        auto_check_update: cfg.auto_check_update,
        allowed_peer_subnets: cfg.allowed_peer_subnets.clone(),
    })
}

#[tauri::command]
pub async fn get_online_devices(state: State<'_, AppState>) -> Result<Vec<DiscoveredDevice>, String> {
    Ok(state.engine.discovery.get_online_devices().await)
}

/// 设备名册：**在线表 ∪ 信任库**（§3.6）。
///
/// 侧栏**必须**消费这个命令而不是 `get_online_devices()`：
/// 后者只有 20 秒 TTL，配对过的设备离线就会从列表消失，
/// 用户无法区分"它关机了"与"发现服务被防火墙挡了"（后者才需要报警）。
#[tauri::command]
pub async fn get_device_roster(
    state: State<'_, AppState>,
) -> Result<Vec<feisuo_core::DeviceRosterEntry>, String> {
    state.engine.get_roster().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_trusted_devices(state: State<'_, AppState>) -> Result<Vec<TrustedDevice>, String> {
    state.engine.list_trusted_devices().map_err(|e| e.to_string())
}

/// ❌ `remove_trusted_device` **已删除**（不是弃用）。
///
/// 它是一个**已注册但无调用方**的命令 —— 界面走的是下面的 `unpair_device`，
/// Android 侧还没接这个动作。而它能做的事正是 §14.5 要消灭的：
/// **单方面断开**（纯本地 `DELETE`，不通知对端，于是关系不对称，
/// 且不对称的方向是危险的那一侧）。只要它还在注册表里，
/// WebView 里任何 JS 都能调到它。
///
/// 更糟的是它当时还是「找不到对方地址」那条兜底分支的实现 ——
/// 于是**有没有 IP 决定了行是被 UPDATE 还是被 DELETE**。
/// 两条冗余通路，比让它们"保持一致"更彻底的做法是删掉一条。
/// 守卫 `scripts/guard_no_local_only_unpair.py`。

/// 撤销某台设备的全部短期授权（§2.3.1 的「可撤销」）。
///
/// **这是安全能力，不是便利功能。** 没有撤销手段的短期授权
/// 只是「用户不知道它还在」—— 而那正是 §14.11.3 反对
/// 「N 分钟免码」的理由：时间窗本身不危险，
/// **看不见、关不掉**才危险。
#[tauri::command]
pub fn revoke_device_grants(
    device_id: String,
    state: State<'_, AppState>,
) -> Result<u32, String> {
    state
        .engine
        .revoke_device_grants(&device_id)
        .map_err(|e| e.to_string())
}

/// 列出**全部设备**当前生效中的短期授权（§2.3.1）。
#[tauri::command]
pub fn list_active_grants(
    state: State<'_, AppState>,
) -> Result<std::collections::HashMap<String, Vec<feisuo_core::ActiveGrant>>, String> {
    state
        .engine
        .list_active_grants()
        .map_err(|e| e.to_string())
}

/// 实际生效的授权窗口秒数（0 = 功能关闭）。
///
/// 前端**不许**自己硬编码这个数字：core 那边有 10 分钟的硬上界
/// （`SESSION_GRANT_MAX_SECS`，它是安全属性不是偏好），
/// 前端写死就会在用户调大配置时**说谎**。
#[tauri::command]
pub async fn get_session_grant_window(state: State<'_, AppState>) -> Result<u64, String> {
    Ok(state.engine.session_grant_window().await)
}

/// 解除配对（§14.5）：**双向**。界面上的「解除配对」应当走这里。
///
/// ## 为什么不能只按 device_id 就把命令丢给 core
///
/// `MSG_UNPAIR` 需要对端的 `ip:port`，而界面上点按钮时手里只有一个
/// `device_id`。让前端去查端点等于把"哪个地址能用"这条判断复制到
/// 每个调用点 —— 而正确的答案在信任库里（`preferred_endpoint`：
/// 覆盖网优先，老库回落到 `last_ip`）。
#[tauri::command]
pub async fn unpair_device(
    device_id: String,
    state: State<'_, AppState>,
) -> Result<UnpairReportDto, String> {
    // 端点解析：优先用已验证的端点；端口为 0（老库只有 last_ip）时
    // 回落到本机配置的传输端口 —— 那正是发现层对这类端点的处理方式。
    let (ip, port) = {
        let store = &state.engine.trust_store;
        match store.preferred_endpoint(&device_id) {
            Ok(Some(ep)) => (ep.ip.clone(), if ep.port == 0 { 0 } else { ep.port }),
            Ok(None) => ("".to_string(), 0),
            Err(e) => return Err(format!("读取设备端点失败: {}", e)),
        }
    };

    let cfg_port = state.engine.config.read().await.transfer_port;
    let port = if port == 0 { cfg_port } else { port };

    if ip.trim().is_empty() {
        // 没有任何可用地址 ⇒ 只能本地降级, 且**必须如实告知**。
        // 这里不走 core 的 `unpair_device`：它需要 ip:port 才能尝试通知，
        // 而这里连 ip 都没有。
        //
        // ⚠️ 这个分支的**实现在 core 里**（`unpair_device_local_only`），
        // 不是就地写几行。理由：就地写就意味着"解除配对会不会删掉这一行"
        // 只能靠 code review 保证 —— 集成测试够不到 `State<'_, AppState>`。
        // 挪进 core 之后它成了一条**可断言的行为**。
        // 早先这里调的是 `remove_trusted_device`（`DELETE`），于是
        // **有没有 IP 决定了行是被 UPDATE 还是被 DELETE** ——
        // 用户刚隐藏的设备会仅仅因为"恰好没记住 IP"而重新出现在主列表。
        let report = state
            .engine
            .unpair_device_local_only(&device_id)
            .map_err(|e| e.to_string())?;
        return Ok(UnpairReportDto {
            local_applied: report.local_applied,
            peer_notified: report.peer_notified,
            reason_code: report.reason_code,
            user_message: report.user_message,
        });
    }

    let report = state
        .engine
        .unpair_device(&ip, port, &device_id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(UnpairReportDto {
        local_applied: report.local_applied,
        peer_notified: report.peer_notified,
        reason_code: report.reason_code,
        user_message: report.user_message,
    })
}

/// 解除配对结果（宿主只透出界面需要的四样，多余字段不进 IPC 契约）。
#[derive(serde::Serialize)]
pub struct UnpairReportDto {
    /// 本机是否已降级为未信任
    pub local_applied: bool,
    /// 对端是否**也**解除了
    pub peer_notified: bool,
    /// 机器可读原因
    pub reason_code: String,
    /// 可直接展示的一句话
    pub user_message: String,
}

#[tauri::command]
pub async fn pair_with_device(
    target_ip: String,
    target_port: u16,
    pin: String,
    state: State<'_, AppState>,
) -> Result<TrustedDevice, String> {
    state
        .engine
        .pair_with_device(&target_ip, target_port, &pin)
        .await
        .map_err(|e| e.to_string())
}

/// 发送文件给对端。
///
/// 返回值把三件事区分开，而不是一个 bool：
/// - `0` = 全部发完（`skipped > 0` 时表示有几个被断点续传跳过）；
/// - `grant_code_required` = 对端处于「每次匹配码」等级，需要用户抄码后重试；
/// - 抛错 = 真失败。
///
/// 之所以不把"需要码"塞进错误：它是**一次正常的协商回合**，
/// 当成错误的话待发队列会被清空、用户只看到"对方拒绝传输"。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOutcome {
    /// 因断点续传而跳过的文件数
    pub skipped: u32,
    /// 对端需要本次传输码；`message` 是给用户看的原因
    pub grant_code_required: bool,
    pub message: String,
    /// **实际发出的文件数**（目录已递归展开，§7.6）。
    ///
    /// 必须与 `skipped` 分开报：选 1 个含 3000 个文件的文件夹时，
    /// "已发送 1 个文件"是彻头彻尾的谎报。
    pub expanded_files: u32,
    /// 展开时跳过的条目数（符号链接 / 隐藏 / 内部目录 / 超限）
    pub expand_skipped: u32,
    pub expand_symlinks_skipped: u32,
    /// 展开时触发的限制说明
    pub expand_limit_hit: Option<String>,
}

/// 发送文件到目标设备。
///
/// `dest_sub_path` 指定**对方收件目录下**的落点子目录（§7.7），空串 = 收件根。
///
/// ## ⚠️ 这个参数曾经缺失，而 Tauri 不会因此报错
///
/// 早先本命令没有 `dest_sub_path`，而 `feisuoBridge.sendFiles` 一直在传它。
/// Tauri v2 的 `invoke` 按**已声明的参数名**反序列化，多余的 key 被**静默丢弃**
/// —— 不报错、不警告。于是穿梭发送"落到对方地址栏当前目录"这个功能
/// 在 Windows 桌面端**从未生效过**，文件一律落在收件根，
/// 而界面上地址栏停在哪一层、按钮提示写去哪一层，全都看不出来。
///
/// 这类"传了但没人接"的断链比缺功能更难查：没有错误、没有日志，
/// 只有行为与文案不符。所以凡是 `feisuoBridge` 里 `invoke` 的参数，
/// 都必须在本命令签名里出现。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn send_files(
    target_device_id: String,
    target_ip: String,
    target_port: u16,
    target_name: String,
    files: Vec<String>,
    grant_code: Option<String>,
    dest_sub_path: Option<String>,
    state: State<'_, AppState>,
) -> Result<SendOutcome, String> {
    if files.is_empty() {
        return Err("未选择任何文件".into());
    }
    let path_bufs: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
    for p in &path_bufs {
        if !p.exists() {
            return Err(format!("文件不存在或已被移动: {}", p.display()));
        }
    }
    let code = grant_code.as_deref().unwrap_or("").trim().to_string();
    // 展开发生在发送内部，发送成功后再取 —— 保证"展开详情"与
    // "这次发送"一定对应，不会显示上一次的残留。
    let _ = state.engine.take_last_scan();
    match state
        .engine
        .send_files_to_dest_with_code(
            &target_ip,
            target_port,
            &target_device_id,
            &target_name,
            path_bufs,
            dest_sub_path.as_deref().unwrap_or(""),
            &code,
        )
        .await
    {
        // 本机在 5 秒窗口里撤销。不是失败 —— 当成失败的话前端会弹红色
        // 「发送失败」，用户会以为没撤成，然后把同一批文件再发一遍。
        Err(FeisuoError::LocallyAborted(_)) | Err(FeisuoError::Cancelled) => {
            Ok(SendOutcome {
                skipped: 0,
                grant_code_required: false,
                message: "已撤销，文件未发出".into(),
                expanded_files: 0,
                expand_skipped: 0,
                expand_symlinks_skipped: 0,
                expand_limit_hit: None,
            })
        }
        Ok(skipped) => {
            let scan = state.engine.take_last_scan();
            Ok(SendOutcome {
                skipped,
                grant_code_required: false,
                message: String::new(),
                expanded_files: scan.as_ref().map(|s| s.items.len() as u32).unwrap_or(0),
                expand_skipped: scan.as_ref().map(|s| s.skipped).unwrap_or(0),
                expand_symlinks_skipped: scan
                    .as_ref()
                    .map(|s| s.symlinks_skipped)
                    .unwrap_or(0),
                expand_limit_hit: scan.and_then(|s| s.limit_hit),
            })
        }
        Err(FeisuoError::GrantCodeRequired(msg)) => Ok(SendOutcome {
            skipped: 0,
            grant_code_required: true,
            message: if msg.trim().is_empty() {
                format!("「{}」被设置为「每次匹配码」", target_name)
            } else {
                msg
            },
            expanded_files: 0,
            expand_skipped: 0,
            expand_symlinks_skipped: 0,
            expand_limit_hit: None,
        }),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn generate_pair_pin(state: State<'_, AppState>) -> Result<String, String> {
    let (pin, _) = state.engine.trust_store.generate_pair_pin();
    Ok(pin)
}

#[tauri::command]
pub async fn open_receive_folder(state: State<'_, AppState>) -> Result<(), String> {
    let recv_dir = state.engine.config.read().await.receive_dir.clone();
    open_in_explorer(&recv_dir)
}

#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    AutostartManager::set_autostart(enabled)
}

// 说明: 这里原本还有 `is_autostart_enabled` 与 `get_device_id` 两个命令,
// 现已删除 —— 它们的数据本来就由 `get_local_info` 完整带出:
//   * `autostart` 字段直接来自 `AutostartManager::is_autostart_enabled()`
//     (系统实况, 不是配置里的意图值), 设置页的开关就绑定在它上面;
//   * `device_id` 字段同理。
// 前端从未调用过这两个命令(wiring_guard 查出来的), 属于纯死代码。
// 保留它们只会让人误以为存在两条取同一份数据的路径。

/// 列举**本机**目录（穿梭左栏，§7.1）。
///
/// ## 为什么从"收件目录镜像"改成真实文件系统
///
/// 旧实现把左栏钉在 `receive_dir`（`~/feisuo`）上，而右栏已经走真实卷
/// （`C:` / `D:`）。左右两侧能力不一致时，穿梭这个功能是半残的：
/// 用户在右边能进 `D:\项目\2026\报表`，回到左边却只能看到自己收到过的东西。
///
/// ## 安全边界：这里**不套用** access_scope
///
/// scope 是"我允许**别人**看什么"（§8），与"我看**自己**的盘"无关。
/// 套上去的后果很荒谬：用户设了白名单 `D:`，然后发现自己在左栏
/// 连 C 盘都看不到，而那本机文件他本来就有完整权限。
///
/// 但仍然要挡两件事：
/// 1. 路径穿越（前端传来的 `rel_path` 不可信 —— 本地 IPC 入口同样不可信）；
/// 2. 内部暂存目录 `.feisuo-incoming`（那是对端的收件中转，用户不需要看见，
///    看见还会误以为"有个文件卡住了"）。
#[tauri::command]
pub async fn list_directory_files(
    volume: Option<String>,
    rel_path: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> Result<LocalDirListingDto, String> {
    let vol = volume.unwrap_or_default();
    let raw_rel = rel_path.unwrap_or_default();
    // 空卷 = 1.x 语义（收件目录镜像），与对端浏览保持同一套回退规则
    let legacy_base = state.engine.config.read().await.receive_dir.clone();
    let volume_mode = !vol.is_empty();
    // 提前复制一份给闭包用：下面的 DTO 还要用 `vol` 本身。
    let vol_for_task = vol.clone();
    let (base, rel) = if volume_mode {
        let r = normalize_local_subpath(&raw_rel)?;
        let root = feisuo_core::storage::volumes::resolve_browse_path(&vol, "")
            .ok_or_else(|| format!("未知的卷: {}", vol))?;
        (root, r)
    } else {
        let r = normalize_local_subpath(&raw_rel)?;
        (legacy_base, r)
    };

    let mut target = base.clone();
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        target.push(seg);
    }
    // 纵深防御: 拼接结果必须仍在 base 之内（挡符号链接）
    // 必须走 `is_within`：手写 canonicalize+starts_with 在 Windows 上会因为
    // `\\?\` 前缀把合法路径误判成逃逸，详见 core/src/storage/paths.rs。
    {
        if !feisuo_core::storage::is_within(&target, &base) {
            return Err("路径逃逸出基准目录, 已拒绝".to_string());
        }
        if target.is_dir() {
            // 存在且是目录 = 合法
        } else if target.exists() {
            return Err("目标不是目录".to_string());
        }
    }

    let (offset, limit) = feisuo_core::storage::volumes::normalize_page(
        offset.unwrap_or(0),
        limit.unwrap_or(0),
    );
    // 同步 read_dir 不能占住 async worker: 目标可能在网络盘/移动硬盘上,
    // 一次 read_dir 卡住几秒是常见情况, 会连带拖慢同一 runtime 上的其它命令。
    let listed = tauri::async_runtime::spawn_blocking(move || {
        if !target.exists() {
            // 1.x 语义下收件目录可能还没建；卷模式下路径不存在要报错
            if vol_for_task.is_empty() {
                let _ = std::fs::create_dir_all(&target);
            } else {
                return Err(format!("路径不存在: {}", target.to_string_lossy()));
            }
        }
        Ok(read_dir_entries_paged(&target, offset, limit))
    })
    .await
    .map_err(|e| format!("目录列举任务异常: {}", e))??;

    let (files, total, truncated) = listed;
    Ok(LocalDirListingDto {
        files,
        current_path: rel.clone(),
        parent_path: if rel.is_empty() {
            None
        } else {
            Some(match rel.rfind('/') {
                Some(i) => rel[..i].to_string(),
                None => String::new(),
            })
        },
        truncated,
        total,
        offset: offset as u32,
        volume_mode: volume_mode,
        volume: vol,
        // 卷列表**两种模式都要给**。
        //
        // 早先只在 `volume_mode` 下返回，于是「收件目录」镜像模式（本机左栏
        // 的初始状态）拿到空数组，界面上的 `v-if="localVolumes.length > 0"`
        // 把整个卷下拉藏掉 —— 用户看到的是"左栏没有地址栏"，而且**没有任何
        // 办法切到真实磁盘**，因为唯一能填上这个数组的入口就是那个下拉。
        // 这是一个自己把自己锁死的循环依赖。
        //
        // 本机左栏是"这台设备自己的文件"，不涉及对端，因此不按
        // `access_scope` 过滤（那是限制**别人**能看什么的，D10）。
        volumes: feisuo_core::storage::volumes::list_volumes(),
    })
}

/// 归一化本机目录浏览的子路径: 拒绝 `..`、绝对路径、盘符、隐藏目录与超深路径。
fn normalize_local_subpath(raw: &str) -> Result<String, String> {
    let unified = raw.trim().replace('\\', "/");
    if unified.is_empty() || unified == "." || unified == "/" {
        return Ok(String::new());
    }
    if unified.starts_with('/') {
        return Err("浏览路径必须是相对路径".to_string());
    }
    if unified.contains(':') {
        return Err("浏览路径不得包含盘符".to_string());
    }
    if unified.contains('\0') {
        return Err("浏览路径包含非法字符".to_string());
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in unified.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err("浏览路径不得包含上级目录 (..)".to_string()),
            s if s.starts_with('.') => return Err("浏览路径不得进入隐藏目录".to_string()),
            s if s.ends_with(' ') || s.ends_with('.') => {
                // Windows 会静默吞掉结尾的空格与点, 导致"显示一个文件、
                // 磁盘上却是另一个"的错位
                return Err("目录名不得以空格或点结尾".to_string());
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return Ok(String::new());
    }
    if parts.len() > 16 {
        return Err("浏览路径层级超过 16 层上限".to_string());
    }
    Ok(parts.join("/"))
}

/// 本机目录列举结果 (serde 默认 snake_case, 前端桥接层做 camelCase 转换)。
#[derive(serde::Serialize)]
pub struct LocalDirListingDto {
    pub files: Vec<DiskFileInfo>,
    pub current_path: String,
    pub parent_path: Option<String>,
    pub truncated: bool,
    /// 本目录总条目数（分页用）
    pub total: u32,
    /// 本次返回的偏移
    pub offset: u32,
    /// 是否走真实卷模式（false = 收件目录镜像）
    pub volume_mode: bool,
    /// 本次使用的卷 id（1.x 语义下为空串）
    pub volume: String,
    /// 本机可浏览的卷（地址栏下拉的数据源）
    pub volumes: Vec<feisuo_core::VolumeInfo>,
}

#[tauri::command]
pub fn minimize_window(window: tauri::WebviewWindow) {
    let _ = window.minimize();
}

#[tauri::command]
pub fn toggle_maximize_window(window: tauri::WebviewWindow) {
    if window.is_maximized().unwrap_or(false) {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
}

/// 前端已自行决定"最小化到托盘", 这里只做一次隐藏 (不再二次弹确认框)
#[tauri::command]
pub fn hide_window(window: tauri::WebviewWindow) {
    let _ = window.hide();
}

/// 彻底退出应用 (来自前端"退出程序"确认框与托盘菜单)
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    tracing::info!("Feisuo exiting by user request");
    app.exit(0);
}

/// 引擎的**真实**健康状态。
///
/// 为什么必须有这个命令: 引擎启动失败时(端口被占用是最常见原因),
/// `get_online_devices` 之类的调用**不会**报错 —— 发现服务的设备表本来就是空的,
/// 返回 `Ok([])`。前端若靠"调用没抛异常"推断在线, 就会显示绿灯"在线",
/// 还会顺手把错误横幅清空。
///
/// 加上开机自启 + 托盘常驻, 窗口根本不显示, 用户只看到托盘图标,
/// 以为一切正常, 直到某天需要传文件才发现永远传不了。
#[derive(Debug, Serialize, Deserialize)]
pub struct EngineStatus {
    /// 引擎是否已成功启动 (发现 + 传输服务都起来了)
    pub running: bool,
    /// 最近一次启动失败的原因; 正常为 null
    pub error: Option<String>,
}

#[tauri::command]
pub fn get_engine_status(state: State<'_, AppState>) -> EngineStatus {
    EngineStatus {
        running: state.engine.is_started(),
        error: state.engine.start_error(),
    }
}

/// 确保窗口可见 (已可见时为空操作)。
///
/// 飞梭是**托盘常驻**应用, 关窗后窗口处于隐藏状态, 而下列事件在隐藏时
/// 照样会发生, 且都需要用户知情或决定:
/// - 原生拖放: 文件进了发送清单, 但用户看不到, 表现为"拖了没反应";
/// - 传输完成 / 失败: 无人值守场景下用户永远不知道文件已到;
/// - 人工审批: 弹窗不可见 → core 只等 60 秒 → 静默等满后被拒;
/// - 引擎启动失败: 托盘图标在、功能全无, 是最难自查的一类故障。
///
/// 只把窗口唤出来并聚焦, 不代替用户做任何决定。
#[tauri::command]
pub fn ensure_window_visible(window: tauri::WebviewWindow) {
    let visible = window.is_visible().unwrap_or(false);
    if visible {
        return;
    }
    if let Err(e) = window.show() {
        tracing::warn!("拖放后显示窗口失败: {}", e);
        return;
    }
    let _ = window.unminimize();
    let _ = window.set_focus();
    tracing::info!("窗口处于隐藏状态, 已主动唤出主界面");
}

/// 主动探测对端 IP: 真正等待应答, 失败时给出可读原因
#[tauri::command]
pub async fn probe_device(
    target_ip: String,
    state: State<'_, AppState>,
) -> Result<DiscoveredDevice, String> {
    state
        .engine
        .probe_device(&target_ip)
        .await
        .map_err(|e| e.to_string())
}

/// 浏览对端目录 (双栏穿梭右栏，§7.2)。
///
/// `volume` + `relPath` = 真实文件系统地址（`C:` / `项目/2026`）。
/// `volume` 省略或空串 = 1.x 语义（只看对端收件目录镜像），用于与旧版本对端互通。
/// `offset` / `limit` = 分页游标（§7.3）。
///
/// 返回值带 `volumes`（地址栏下拉的数据源）/ `total`（分页总数）/
/// `volumeMode`（对端是否支持真实卷），前端据此决定画盘符下拉还是退回面包屑。
#[tauri::command]
pub async fn list_remote_files(
    target_ip: String,
    target_port: u16,
    volume: Option<String>,
    rel_path: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
    grant_code: Option<String>,
    state: State<'_, AppState>,
) -> Result<RemoteBrowseListingDto, String> {
    let target = feisuo_core::BrowseTarget {
        volume: volume.unwrap_or_default(),
        rel_path: rel_path.unwrap_or_default(),
        offset: offset.unwrap_or(0),
        limit: limit.unwrap_or(0),
        grant_code: grant_code.unwrap_or_default(),
    };
    let listing = state
        .engine
        .list_remote_files(&target_ip, target_port, &target)
        .await
        .map_err(|e| e.to_string())?;
    Ok(RemoteBrowseListingDto {
        files: listing.files,
        current_path: listing.current_path,
        parent_path: listing.parent_path,
        truncated: listing.truncated,
        message: listing.message,
        volumes: listing.volumes,
        total: listing.total,
        offset: listing.offset,
        volume_mode: listing.volume_mode,
        volume: listing.volume,
        peer_caps: listing.peer_caps,
    })
}

/// 目录浏览结果的前端 DTO (serde 默认按 snake_case, 前端桥接层做 camelCase 转换)。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBrowseListingDto {
    pub files: Vec<RemoteFileEntry>,
    pub current_path: String,
    pub parent_path: Option<String>,
    pub truncated: bool,
    pub message: String,
    /// 对端开放的可浏览卷。1.x 对端返回空数组 —— 前端据此隐藏盘符下拉，
    /// 而不是画一个点了就报错的空下拉框。
    pub volumes: Vec<feisuo_core::VolumeInfo>,
    /// 本目录总条目数（分页用）
    pub total: u32,
    /// 本次返回的偏移
    pub offset: u32,
    /// 对端是否走真实卷模式
    pub volume_mode: bool,
    /// 实际使用的卷 id（请求发的是通配 `*` 时由服务端回显）
    pub volume: String,
    /// 对端能力位（诊断用）
    pub peer_caps: u32,
}

/// 回应审批请求。
///
/// **没有 `grant_code` 参数。** 「每次匹配码」等级下，匹配码是
/// **本机生成、显示在审批窗口上**的（`ApprovalRequest.grant_challenge`），
/// 由**发起方**从它自己的界面敲回来。审批人只决定
/// "允许一次 / 允许并永久信任 / 拒绝"。
///
/// 方向为什么是这样，见 `core/src/transport/server.rs` 里
/// `ApprovalManager::issue_challenge` 的长注释 —— 简言之：
/// 不信任发起方的是接收方，所以该由接收方出题。
#[tauri::command]
pub fn respond_approval(
    approval_id: String,
    action: String,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    // 用 `from_ui_str` 做归一, 旧前端仍发 "allow" / "block_permanent" 也能用
    let act = feisuo_core::ApprovalAction::from_ui_str(&action)
        .ok_or_else(|| format!("未知的审批动作: {}", action))?;
    if !state.engine.respond_approval(&approval_id, act) {
        return Err("该请求已失效或已被处理".into());
    }
    Ok(true)
}

#[tauri::command]
pub async fn open_log_folder(state: State<'_, AppState>) -> Result<(), String> {
    open_in_explorer(&state.engine.app_dir.join("logs"))
}

#[tauri::command]
pub fn get_transfer_history(
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> Result<Vec<feisuo_core::TransferRecord>, String> {
    let limit = limit.unwrap_or(200);
    state.engine.list_transfer_records(limit).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn clear_transfer_history(state: State<'_, AppState>) -> Result<bool, String> {
    state
        .engine
        .clear_transfer_records()
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// 导出传输诊断报告到磁盘，返回文件路径。
///
/// **这是"用户复现问题后把数据交给开发者"的标准通路**（设计文档 §9.7）。
/// 报告里每一次传输都有：分段耗时（建连/握手/审批/清单/数据流/校验/提交/回执）、
/// 链路画像（同子网？覆盖网？建连耗时）、吞吐采样（min/max/停顿次数）与一段
/// 人话归因。拿到这个文件就能直接判断"慢在宽带、慢在审批、还是慢在磁盘"。
#[tauri::command]
pub fn export_transfer_diagnostics(
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let limit = limit.unwrap_or(200);
    let report = state
        .engine
        .export_diagnostics_report(limit)
        .map_err(|e| e.to_string())?;
    if report.trim().is_empty() {
        return Err("暂无诊断记录：请先完成至少一次传输".to_string());
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = state
        .engine
        .app_dir
        .join("diagnostics")
        .join(format!("feisuo-diagnostics-{}.txt", stamp));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建导出目录失败: {}", e))?;
    }
    std::fs::write(&path, report).map_err(|e| format!("写入诊断报告失败: {}", e))?;
    Ok(path.to_string_lossy().to_string())
}

/// 读取最近 N 条诊断记录（界面「传输记录 → 最近诊断」用）。
///
/// ## 为什么返回 DTO 而不是裸 `TransferDiagnostics`
///
/// `attribution()` 是 core 里的一个**方法**，不是序列化字段 ——
/// 它每次读取时按本次采样重新算，不落库（落库的是原始量，见
/// §9.6.6 "速度是派生值"）。而它恰恰是排障时最有价值的一行：
/// 「缓冲区不足，瓶颈在飞梭」这种话，是用户能直接拿去改配置的东西。
///
/// 直接返回裸结构体的话，前端拿到的只有一堆 `samples` / `min_bps`，
/// 还得自己把阶段耗时拼成句子 —— 而拼出来的句子必然与
/// `export_diagnostics_report` 里那行不一致。用户看到两个地方说法不同，
/// 就两个都不信。所以归因必须由 core 算、在这里附带返回。
#[tauri::command]
pub fn get_transfer_diagnostics(
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> Result<Vec<TransferDiagnosticDto>, String> {
    let records = state
        .engine
        .list_diagnostics(limit.unwrap_or(50))
        .map_err(|e| e.to_string())?;
    Ok(records
        .into_iter()
        .map(|d| {
            // 两个都是**借用**调用, 必须在移动任何字段**之前**算完:
            // `d.error` 是 `Option<String>`, 一旦移动就走借用,
            // 而 `attribution()` / `speed_display()` 还要读 `&self`。
            let attribution = d.attribution();
            let speed_display = d.speed_display();
            TransferDiagnosticDto {
                direction: d.direction,
                peer_name: d.peer_device_name,
                peer_ip: d.peer_ip,
                outcome: d.outcome,
                error: d.error,
                file_count: d.file_count,
                bytes_transferred: d.bytes_transferred,
                attribution,
                speed_display,
                data_ms: d.timings.data_ms,
                connect_ms: d.timings.connect_ms,
                approval_wait_ms: d.timings.approval_wait_ms,
                verify_ms: d.timings.verify_ms,
                over_overlay: d.link.over_overlay,
                created_at: d.created_at,
            }
        })
        .collect())
}

/// `get_transfer_diagnostics` 的返回形状。
///
/// 只带界面真正要显示的字段：`TransferDiagnostics` 里还有 nonce、
/// 采样点、分块数等内部量，全塞给前端只会让人不知道该看哪个。
#[derive(serde::Serialize)]
pub struct TransferDiagnosticDto {
    pub direction: String,
    pub peer_name: String,
    pub peer_ip: String,
    /// `completed` / `failed` / `cancelled`
    pub outcome: String,
    pub error: Option<String>,
    pub file_count: u32,
    pub bytes_transferred: u64,
    /// 归因：core 算好的人话，界面原样显示。
    pub attribution: String,
    pub speed_display: String,
    pub data_ms: u64,
    pub connect_ms: u64,
    /// 人工审批等待。**单独给出**是因为它是唯一"不是程序慢"的原因，
    /// 混进总耗时会让人以为程序有问题。
    pub approval_wait_ms: u64,
    pub verify_ms: u64,
    pub over_overlay: bool,
    pub created_at: i64,
}

/// 读取安全事件（§3.9：信任变更必须让用户知情）。
#[tauri::command]
pub fn get_security_events(
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> Result<Vec<feisuo_core::SecurityEvent>, String> {
    state
        .engine
        .list_security_events(limit.unwrap_or(50))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cancel_incoming_transfer(
    peer_device_id: String,
    transfer_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    // 撤销需要实时地址：清单里离线设备只有 last_ip。
    let roster = state.engine.get_roster().await.map_err(|e| e.to_string())?;
    let peer = roster
        .iter()
        .find(|d| d.device_id == peer_device_id)
        .ok_or_else(|| "找不到该设备，请刷新后重试".to_string())?;
    let ip = peer
        .ip
        .clone()
        .or_else(|| {
            if peer.last_ip.is_empty() {
                None
            } else {
                Some(peer.last_ip.clone())
            }
        })
        .ok_or_else(|| "该设备没有已知的可达地址，无法发起撤销".to_string())?;
    let port = peer.transfer_port.unwrap_or(0);
    if port == 0 {
        return Err("该设备当前不在线（没有传输端口），撤销请求无法送达".to_string());
    }
    // ⚠️ 这里**不再**拿 `peer_device_id` 当 transfer_id 占位。
    //
    // 早先那么写，注释说是"靠对端扫描暂存目录来匹配"。但 staging 目录
    // 恰恰是按 `transfer_id` 命名的，而 `transfer_id` 现在是**内容哈希**
    // （`compute_resume_key`）。设备 id 永远匹配不上 ⇒ 撤销请求"成功"
    // 返回、界面显示"已撤销"，而文件照样落进收件目录。
    // 要求 ② 的 5 秒撤销整个是假的。
    //
    // 现在：优先用调用方（UI 从进度事件里拿到的）真实 id；拿不到就由
    // 引擎回落到"最近一次发起"；两个都没有则**明确报错** ——
    // 宁可说"无法撤销"，也绝不谎报成功。
    state
        .engine
        .cancel_transfer(
            &ip,
            port,
            &peer_device_id,
            transfer_id.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| e.to_string())
}

/// 设置对端信任等级（§2.3：每设备可选永久 / 每次匹配码）。
#[tauri::command]
pub fn set_device_trust_level(
    device_id: String,
    level: String,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    // **不能**直接 `TrustLevel::from_db_str(&level)`。
    //
    // `from_db_str` 把未知值回落成 `Pending`（对**数据库**是对的：配置被
    // 手改坏时必须 fail-closed）。但它在这里是**命令入参**：界面传一个拼错的
    // 字符串（"perm" / "blockd"）就会把一台**永久信任**设备静默改成
    // **未信任**，无报错、界面上看不出、直到对方发文件开始弹审批。
    // 用户会以为是对方出问题。
    //
    // 所以入参只接受界面真的会发的两个值，其余**报错**。
    //
    // `pending` 也不接受，尽管它是 `TrustLevel` 的一个变体：它是**派生**的
    // （不在信任库里 / 刚被解除配对），没有写入方的界面会凭空造出这个值。
    // 早先这里用 `TrustLevel::from_db_str` 解析入参，而那个函数为未知
    // 字符串返回 `Pending` —— 于是一个拼错的等级会被**静默降级成最严的那档**，
    // 界面上看不出任何异常。
    let parsed = match level.trim().to_ascii_lowercase().as_str() {
        "permanent" => feisuo_core::security::TrustLevel::Permanent,
        "session" => feisuo_core::security::TrustLevel::Session,
        other => {
            return Err(format!(
                "未知的信任等级 {:?} —— 只接受 permanent(永久信任) / session(每次认证)",
                other
            ))
        }
    };
    state
        .engine
        .set_device_trust_level(&device_id, parsed)
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// 设置设备可见性（§3：隐藏 / 取消隐藏）。
#[tauri::command]
pub fn set_device_visible(
    device_id: String,
    visible: bool,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    state
        .engine
        .set_device_visible(&device_id, visible)
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// 列出已隐藏设备（「已隐藏」抽屉，§3.1）。
#[tauri::command]
pub fn get_hidden_devices(
    state: State<'_, AppState>,
) -> Result<Vec<feisuo_core::TrustedDevice>, String> {
    state.engine.list_hidden_devices().map_err(|e| e.to_string())
}

/// 读取对端的可访问范围（§8）。
#[tauri::command]
pub fn get_device_access_scope(
    device_id: String,
    state: State<'_, AppState>,
) -> Result<feisuo_core::security::AccessScope, String> {
    state
        .engine
        .get_access_scope(&device_id)
        .map_err(|e| e.to_string())
}

/// 写入对端的可访问范围（§8）。范围放宽会自动记安全事件。
#[tauri::command]
pub fn set_device_access_scope(
    device_id: String,
    scope: feisuo_core::security::AccessScope,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    state
        .engine
        .set_access_scope(&device_id, &scope)
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// 预览一批路径（文件或文件夹）会被展开成什么（§7.6）。
///
/// ## 为什么必须"先预览再发"
///
/// 目录递归会跳过符号链接、隐藏文件、`.git`/`node_modules` 等内部目录。
/// 用户拖一个含 `.git` 的项目进去 expecting 完整同步，
/// 结果只收到 3000 个文件里的 20 个 —— 而界面上一个字的解释都没有。
/// 预览让用户在**按发送之前**就知道会发什么、跳过什么。
///
/// 同步磁盘遍历必须离开 async worker（见 `list_directory_files` 的同类注释）。
#[tauri::command]
pub async fn preview_send_paths(
    paths: Vec<String>,
) -> Result<SendPreviewDto, String> {
    let bufs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    for p in &bufs {
        if !p.exists() {
            return Err(format!("路径不存在或已被移动: {}", p.display()));
        }
    }
    let (items, scan, roots) = tauri::async_runtime::spawn_blocking(move || {
        feisuo_core::storage::folder_scan::expand_all_detailed(&bufs)
    })
    .await
    .map_err(|e| format!("目录展开任务异常: {}", e))?
    .map_err(|e| e.to_string())?;
    Ok(SendPreviewDto {
        file_count: items.len(),
        total_bytes: scan.total_bytes,
        skipped: scan.skipped,
        symlinks_skipped: scan.symlinks_skipped,
        limit_hit: scan.limit_hit,
        // 拖入的路径里**有没有目录**。
        //
        // 为什么必须精确回答、而不能靠"拖入数 vs 展开后文件数"推断：
        // 一个**只含 1 个文件**的文件夹会让两者相等，于是被误判成"普通文件"
        // 而跳过预览 —— 而"文件夹要确认"正是产品要求（拖一个项目目录可能
        // 就是几千个文件，用户有权先看一眼）。这个漏洞只在最常见的
        // 小目录场景出现，恰恰是最容易被用户撞上的那种。
        has_directory: paths.iter().any(|p| PathBuf::from(p).is_dir()),
        // 只回前若干条：一次拖 2 万个文件不该把 2 万个路径塞进 IPC
        sample: items
            .iter()
            .take(20)
            .map(|(_, rel)| rel.clone())
            .collect(),
        // **按根**的展开统计。待发清单是按路径列的，没有这个的话
        // 清单只能显示目录项自身的大小（NTFS 上是 4.0 KB），
        // 表现为"提示说 7 个文件 17.8 MB，清单写 1 项 4.0 MB" ——
        // 同一件事两个数字，而用户只能看到错的那个。
        expanded: roots,
    })
}

/// 目录展开预览的结果。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendPreviewDto {
    pub file_count: usize,
    pub total_bytes: u64,
    pub skipped: u32,
    pub symlinks_skipped: u32,
    /// 触发的限制说明（`None` = 全部正常）
    pub limit_hit: Option<String>,
    /// 拖入的路径里**有没有目录**。UI 用它决定"直发"还是"先展开预览"。
    pub has_directory: bool,
    /// 前 20 个目标相对路径的样例
    pub sample: Vec<String>,
    /// 每个被拖入路径各自的展开统计（文件数 / 字节数 / 是否目录）
    pub expanded: Vec<feisuo_core::storage::folder_scan::RootExpansion>,
}

/// 某台设备的全部已知端点（按"覆盖网优先"排序，§3.9 / P4 ⑧）。
#[tauri::command]
pub fn list_device_endpoints(
    device_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<feisuo_core::DeviceEndpoint>, String> {
    let all = state
        .engine
        .trust_store
        .all_endpoints()
        .map_err(|e| e.to_string())?;
    Ok(all.into_iter().filter(|e| e.device_id == device_id).collect())
}

/// 某台设备的首选端点（覆盖网优先，D6 决策）。
#[tauri::command]
pub fn get_preferred_endpoint(
    device_id: String,
    state: State<'_, AppState>,
) -> Result<Option<feisuo_core::DeviceEndpoint>, String> {
    state
        .engine
        .trust_store
        .preferred_endpoint(&device_id)
        .map_err(|e| e.to_string())
}

/// 批量取本地文件真实大小 (供待发清单展示)
#[tauri::command]
pub fn probe_file_sizes(paths: Vec<String>) -> Result<std::collections::HashMap<String, u64>, String> {
    let mut map = std::collections::HashMap::with_capacity(paths.len());
    for p in paths {
        if let Ok(meta) = std::fs::metadata(&p) {
            map.insert(p, meta.len());
        }
    }
    Ok(map)
}

/// 请求对端把指定文件推送回本机 (双栏穿梭"取回")。
/// `sub_paths` 每项是**卷内**相对路径, 可以含子目录层级, 也可以是目录
/// （目录会递归展开并保留层级）。
/// `volume` 指定目标所在卷（`"C:"`）；空串 = 1.x 语义，只在对端收件目录里找。
/// `dest_sub_path` 指定本机落点子目录（§7.7），空串 = 落收件根。
/// `grant_code` 是本次传输码（对端处于「每次匹配码」等级时必填，§2.3）。
///
/// 返回 `grant_code_required = true` 表示这是一次**协商回合**（不是失败）：
/// 穿梭右栏据此弹码、让用户抄给对方、再带码重试。选中项必须保留。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn request_pull(
    target_ip: String,
    target_port: u16,
    sub_paths: Vec<String>,
    dest_sub_path: Option<String>,
    grant_code: Option<String>,
    volume: Option<String>,
    state: State<'_, AppState>,
) -> Result<SendOutcome, String> {
    let code = grant_code.as_deref().unwrap_or("").trim().to_string();
    match state
        .engine
        .request_pull_in_volume(
            &target_ip,
            target_port,
            sub_paths,
            dest_sub_path.as_deref().unwrap_or(""),
            &code,
            volume.as_deref().unwrap_or(""),
        )
        .await
    {
        Ok(()) => Ok(SendOutcome {
            skipped: 0,
            grant_code_required: false,
            message: String::new(),
            expanded_files: 0,
            expand_skipped: 0,
            expand_symlinks_skipped: 0,
            expand_limit_hit: None,
        }),
        Err(FeisuoError::GrantCodeRequired(msg)) => Ok(SendOutcome {
            skipped: 0,
            grant_code_required: true,
            message: msg,
            expanded_files: 0,
            expand_skipped: 0,
            expand_symlinks_skipped: 0,
            expand_limit_hit: None,
        }),
        Err(e) => Err(e.to_string()),
    }
}

/// 把剪贴板 / 临时生成的内容落盘成一个真实文件, 返回其绝对路径。
/// 前端拿不到本地真实路径, 只有经过这一步才能走通用的"发送文件"链路。
#[tauri::command]
pub async fn stage_temp_payload(
    file_name: String,
    bytes: Vec<u8>,
    state: State<'_, AppState>,
) -> Result<String, String> {
    // 文件名安全校验: 绝不允许调用方借此写入任意路径
    PathManager::validate_relative_path(&file_name).map_err(|e| e.to_string())?;

    let dir = state.engine.config.read().await.receive_dir.clone();
    let dir = dir.join(".feisuo-staging");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建暂存目录失败: {}", e))?;

    let target = dir.join(&file_name);
    std::fs::write(&target, &bytes).map_err(|e| format!("写入暂存文件失败: {}", e))?;
    Ok(target.to_string_lossy().to_string())
}

/// 清理发送完成后残留的暂存文件。
///
/// `paths` 传入本次**实际提交**的暂存文件时只删这些; 省略时才清空整个目录。
///
/// 之所以必须按路径精确删除: 暂存目录里还可能躺着用户排队等待发送的剪贴板 /
/// 临时文件。旧实现在**任意**传输结束 (包括对端推过来的接收) 时都清空整个目录,
/// 于是"复制一张截图排队 → 同时另一个传输刚好结束"会把排队中的文件从磁盘上
/// 删掉, 用户点发送时才发现文件不见了 —— 静默丢数据。
#[tauri::command]
pub fn cleanup_temp_payloads(
    paths: Option<Vec<String>>,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let dir = state
        .engine
        .config
        .try_read()
        .map(|c| c.receive_dir.join(".feisuo-staging"))
        .unwrap_or_default();

    match paths {
        Some(list) => {
            for p in list {
                let path = PathBuf::from(&p);
                // 只允许删除暂存目录内的文件, 绝不允许借此删任意路径。
                //
                // 判据是"父目录在暂存目录之内"而不是"父目录 == 暂存目录"：
                // 剪贴板暂存目前直接落在暂存根下，但一旦加了子目录分层，
                // 等值比较会把合法文件全判成非法，用户看到的是"删不掉"。
                //
                // 必须走 `is_within`：手写 canonicalize 在 Windows 上会因为
                // `\\?\` 前缀不同而恒为 false，详见 core/src/storage/paths.rs。
                let inside = match path.parent() {
                    Some(parent) => feisuo_core::storage::is_within(parent, &dir),
                    // 没有父目录 = 裸文件名（如 "C:file"），不可能在暂存目录内
                    None => false,
                };
                if !inside {
                    tracing::warn!("拒绝删除暂存目录外的路径: {}", p);
                    continue;
                }
                if let Err(e) = std::fs::remove_file(&path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!("删除暂存文件 {} 失败: {}", p, e);
                    }
                }
            }
        }
        None => {
            if dir.exists() {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
    Ok(true)
}

/// 本机的常用位置（桌面 / 下载 / 文档…），供穿梭框左栏的"快捷位置"用。
///
/// 为什么要单独一条命令，而不能让前端自己拼路径：
/// 桌面/文档/图片常被重定向到 OneDrive（真路径是
/// `C:\\Users\\<u>\\OneDrive\\桌面`），且目录名**本地化**
/// （中文系统叫 `文档`）。前端拼 `USERPROFILE + "\\Desktop"`
/// 对这两类用户全是错的 —— 表现为"点了没反应"，而界面毫无线索。
///
/// 右栏（对端）的常用位置走浏览响应的 `places` 字段，不走这里：
/// 那是**对端**的路径，且已按该对端的访问范围过滤过。
#[tauri::command]
pub async fn get_known_places() -> Result<Vec<feisuo_core::storage::KnownPlace>, String> {
    Ok(feisuo_core::storage::known_places())
}

/// 把前端读到的一份配置持久化。
/// 每次保存都会把完整配置落盘, 并即时应用可热更新的项 (日志级别 / 主题)。
#[tauri::command]
pub async fn update_app_config(
    payload: serde_json::Value,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let patch: serde_json::Value = payload;
    let mut cfg = state.engine.config.write().await;

    if let Some(name) = patch.get("device_name").and_then(|v| v.as_str()) {
        // 必须在这里消毒, 而不是等到广播时再消毒。
        // 设备名会原样写进 config、广播给局域网内所有对端、并进入传输记录与日志。
        // 带 `\n` 的名字会让对端 UI 排版错乱, 还能在对端日志里伪造记录
        // (log injection)。只做 trim + 长度检查是不够的。
        let cleaned = feisuo_core::sanitize_device_name(name);
        if name.trim().is_empty() {
            return Err("设备名称不能为空".into());
        }
        if cleaned == "飞梭设备" && name.trim() != "飞梭设备" {
            return Err("设备名称含有无效字符 (控制字符 / 纯空白)".into());
        }
        cfg.device_name = cleaned;
    }
    if let Some(ar) = patch.get("auto_receive").and_then(|v| v.as_bool()) {
        cfg.auto_receive = ar;
    }
    if let Some(ll) = patch.get("log_level").and_then(|v| v.as_str()) {
        let normalized = ll.to_uppercase();
        if !matches!(normalized.as_str(), "INFO" | "DEBUG" | "WARN" | "ERROR") {
            return Err("日志级别取值非法".into());
        }
        cfg.log_level = normalized;
    }
    if let Some(ca) = patch.get("close_action").and_then(|v| v.as_str()) {
        if !matches!(ca, CLOSE_ACTION_ASK | CLOSE_ACTION_TRAY | CLOSE_ACTION_EXIT) {
            return Err("关闭方式取值非法".into());
        }
        cfg.close_action = ca.to_string();
    }
    if let Some(theme) = patch.get("theme").and_then(|v| v.as_str()) {
        if matches!(theme, "dark" | "light") {
            cfg.theme = theme.to_string();
        }
    }
    // 来源网段白名单。**必须逐条校验后再落盘** ——
    // 前端已经做了格式检查，但那是"提示"，这里才是"把关"：
    // 存下一条解析不了的值，用户会看到"配了却谁都连不上"，
    // 而界面说的是"格式非法"—— 真因与提示对不上，最难查的一类问题。
    if let Some(list) = patch.get("allowed_peer_subnets") {
        let arr = list
            .as_array()
            .ok_or_else(|| "来源网段必须是字符串数组".to_string())?;
        let mut cleaned: Vec<String> = Vec::with_capacity(arr.len());
        for item in arr {
            let raw = item
                .as_str()
                .ok_or_else(|| "来源网段的每一项都必须是字符串".to_string())?;
            let t = raw.trim();
            if t.is_empty() {
                continue;
            }
            // 上限：防手滑粘贴一大坨，也防 map 无限增长
            if cleaned.len() >= 32 {
                return Err("来源网段最多 32 条".into());
            }
            if !feisuo_core::security::is_valid_subnet_entry(t) {
                return Err(format!(
                    "网段 {:?} 无法识别。格式如 192.168.31.0/24（IPv4 网段；\
                     飞梭只监听 IPv4，IPv6 暂不支持）",
                    t
                ));
            }
            cleaned.push(t.to_string());
        }
        cfg.allowed_peer_subnets = cleaned;
    }
    if let Some(size) = patch.get("max_log_size_mb").and_then(|v| v.as_u64()) {
        // 0 会让日志写入每一条都触发轮转, 必须夹到下限
        cfg.max_log_size_mb = size.clamp(1, 100) as u32;
    }
    if let Some(records) = patch.get("max_history_records").and_then(|v| v.as_u64()) {
        cfg.max_history_records = records.clamp(50, 100_000) as u32;
    }
    if let Some(days) = patch.get("record_retention_days").and_then(|v| v.as_u64()) {
        if days > 3650 {
            return Err("保留天数过大 (上限 3650 天)".into());
        }
        cfg.record_retention_days = days as u32;
    }
    if let Some(dir) = patch.get("receive_dir").and_then(|v| v.as_str()) {
        let candidate = PathBuf::from(dir.trim());
        if candidate.as_os_str().is_empty() {
            return Err("保存目录不能为空".into());
        }
        std::fs::create_dir_all(&candidate).map_err(|e| format!("创建目录失败: {}", e))?;
        cfg.receive_dir = candidate;
    }
    if let Some(v) = patch.get("auto_check_update").and_then(|v| v.as_bool()) {
        cfg.auto_check_update = v;
    }

    // 必须保存到引擎实际使用的数据目录, 不能用 resolve_app_dir() 再猜一次:
    // Android 宿主是显式注入目录的, 这里猜会写回错误位置。
    let app_dir = state.engine.app_dir.clone();
    cfg.save_in(&app_dir).map_err(|e| e.to_string())?;
    let new_level = cfg.log_level.clone();
    drop(cfg);

    // 日志级别立即生效, 无需重启
    crate::logger::set_level(&new_level);

    // 设备名/落盘目录变更后广播一条事件, 前端可立即刷新显示
    let _ = app.emit("feisuo://config-updated", ());
    Ok(true)
}

// =======================================================================
// 应用内更新
//
// `UpdateService` 需要 AppHandle 才能往前端推事件, 而 AppHandle 只有
// 在 `Builder::build()` 之后才拿得到 —— 所以它在 `setup` 里创建并
// `manage`, 命令通过独立的 State 取用。
// =======================================================================

/// 取当前更新状态快照 (界面挂载与切回设置页时调用)。
#[tauri::command]
pub async fn get_update_status(
    updater: State<'_, Arc<UpdateService>>,
) -> Result<UpdateStatus, String> {
    Ok(updater.status().await)
}

/// 检查更新并自动下载。
///
/// `interactive` 传 true 时失败会额外弹提示 —— 手动点击才需要,
/// 后台静默调度必须安静失败, 否则拔网线时一天弹 7 次错误。
#[tauri::command]
pub async fn check_for_update(
    interactive: Option<bool>,
    updater: State<'_, Arc<UpdateService>>,
) -> Result<UpdateStatus, String> {
    let interactive = interactive.unwrap_or(true);
    // 后台调度同样会走到这里, 靠 gate 串行化避免并发下载同一份更新包
    Ok(updater.check_and_download(interactive).await)
}

/// 应用已下载的更新: 换名接力 → 拉起新版本 → 退出当前实例。
///
/// 成功时这个函数**不会返回** (进程已退出), 因此前端拿到响应后
/// 不应再刷新界面 —— 实际上它通常也收不到响应。
#[tauri::command]
pub async fn apply_update(
    app: tauri::AppHandle,
    updater: State<'_, Arc<UpdateService>>,
) -> Result<bool, String> {
    updater.apply_pending(&app).await?;
    Ok(true)
}

/// 在系统浏览器打开下载页。
///
/// 自动下载失败(代理拦截 / CDN 限流)时的兜底: 让用户自己下载,
/// 总比卡在"检查更新失败"无处可去要好。
#[tauri::command]
pub fn open_update_page(url: Option<String>) -> Result<bool, String> {
    crate::updater::open_release_page(url.as_deref().unwrap_or(""))?;
    Ok(true)
}
