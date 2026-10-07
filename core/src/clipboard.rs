//! 剪贴板直发的**平台无关**部分（§6.2 / §6.3 / §6.5）。
//!
//! ## 为什么这半个模块在 core 而不在桌面端
//!
//! 早先整个剪贴板模块都住在 `desktop/src-tauri/src/clipboard.rs`，
//! 于是两个问题：
//!
//! 1. **测不到**。桌面端是 bin-only crate（没有 `lib.rs`），
//!    `cargo run --example` 拿不到它的模块。而这段逻辑全是纯函数 +
//!    文件操作，正是最该有断言的地方 —— 结果它一行都没被执行过。
//! 2. **Android 端用不了**。移动端同样要支持"剪贴板发送"（§6.3），
//!    但命名规则、TTL 清理、路径校验全在桌面端，只能复制一遍。
//!
//! 现在拆成两半：
//! - **本模块（core）**：内容分类、命名、预览、暂存、TTL 清理。
//!   无平台依赖，桌面端与 Android 端共用。
//! - **桌面端**：`mod imp`（Win32 的 `CF_HDROP` / `CF_DIB` /
//!   `CF_UNICODETEXT` 读取）与四个 Tauri 命令。Android 端换成
//!   `ClipboardManager` 即可，core 这一半不用动。
//!
//! ## 一条贯穿全模块的原则：预览不落盘
//!
//! 剪贴板里常混着密码、验证码、私人截图。所以：
//! - 预览阶段（D5，默认开启）**只读不写**，「看了没发」同样会在
//!   磁盘上留下明文；
//! - 暂存目录放在 **app 私有目录**而不是收件目录 —— 旧实现写在
//!   `receive_dir/.feisuo-staging`（用户可见），而清理只在"发送成功"
//!   后触发，用户抓了剪贴板却没发，明文就**永久**留在磁盘上；
//! - TTL 默认 24 小时，由引擎启动与每次发送前各清一次。

use serde::{Deserialize, Serialize};

use crate::storage::PathManager;

/// 文本预览的最大行数
pub const PREVIEW_MAX_LINES: usize = 200;
/// 单个剪贴板载荷上限（防止把几百 MB 的东西塞进内存）
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// 从剪贴板识别出的内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipboardContent {
    /// 资源管理器里复制的文件（**原路径，无需落盘**）
    Files { paths: Vec<String> },
    /// 位图
    Image {
        file_name: String,
        size: usize,
        /// 缩略图用的 base64（长边限制在 `preview_max_edge`）
        preview_base64: String,
        /// 原始 PNG 字节。`skip` 是因为界面只需要 base64 预览,
        /// 原文由宿主命令直接落盘, 不必把整张图再过一次 IPC。
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    /// 纯文本
    Text {
        file_name: String,
        size: usize,
        /// 前 N 行预览（给界面看，可截断）
        preview: String,
        /// 完整原文。**必须携带全文** —— 只留预览会写到暂存文件里就截断了。
        #[serde(skip)]
        full_text: String,
    },
    /// 明确拒绝的类型（界面要把原因说清楚，不能静默丢弃）
    Rejected { reason: String },
    /// 剪贴板里没有可发送的内容
    Empty,
}

impl ClipboardContent {
    /// 该内容是否需要先落盘才能进入发送链路。
    pub fn needs_staging(&self) -> bool {
        matches!(self, ClipboardContent::Image { .. } | ClipboardContent::Text { .. })
    }
}

/// 时间戳文件名后缀：`20260930-143025`（本机时区）。
pub fn timestamp_suffix() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// 给纯文本挑一个安全的扩展名（§6.2）。
///
/// 刻意**不生成 `.url`**：那是可执行的 INI 快捷方式，双击会跳转。
pub fn text_extension(text: &str) -> &'static str {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "txt";
    }
    // 能当 JSON 解析就用 .json, 对方双击有语法高亮
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() && trimmed.len() > 1 {
        return "json";
    }
    // Markdown 启发式: 首行是标题/列表/代码围栏
    let first = trimmed.lines().next().unwrap_or("");
    if first.starts_with('#')
        || first.starts_with("- ")
        || first.starts_with("* ")
        || first.starts_with("```")
    {
        return "md";
    }
    "txt"
}

/// 截取文本预览（限制行数与总长度）。
pub fn text_preview(text: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i >= PREVIEW_MAX_LINES {
            out.push_str("\n…（预览已截断）");
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        if out.len() > 8192 {
            out.push_str("\n…（预览已截断）");
            break;
        }
    }
    out
}

/// 按内容决定文件名与落盘行为（`read_clipboard` 的收尾，抽出以便共用）。
///
/// 把"分类 + 命名 + 拒绝规则"从平台读取里拆出来，是为了**同一套规则
/// 在 Windows 与 Android 上完全一致**。早先这些规则长在
/// `#[cfg(windows)]` 的读取函数里，Android 端接上的时候必然会走样
/// （比如忘了 SVG 拒绝 —— 而 SVG 是可执行 XML，恰好是最危险的一类）。
///
/// `make_preview` 由平台侧提供（Windows 是 base64 缩略图，
/// Android 可以给个更简单的实现或空串）。
pub fn classify_text(text: String) -> ClipboardContent {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return ClipboardContent::Empty;
    }
    // SVG 拒绝：这是可执行 XML, 落盘后双击/缩略图都会渲染
    if trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && trimmed.contains("<svg")) {
        return ClipboardContent::Rejected {
            reason: "剪贴板内容是 SVG（可执行 XML），为安全起见不落盘发送。请先转成 PNG。".into(),
        };
    }
    // 整段 HTML 同样拒绝落 .html；但只在"看起来就是整份 HTML 文档"时提示
    if trimmed.starts_with("<!DOCTYPE html") || trimmed.starts_with("<html") {
        return ClipboardContent::Rejected {
            reason: "剪贴板内容是一整份 HTML（打开会加载远程资源并可能执行脚本），为安全起见不落盘发送。"
                .into(),
        };
    }
    if text.len() > MAX_PAYLOAD_BYTES {
        return ClipboardContent::Rejected {
            reason: format!(
                "剪贴板文本有 {:.1} MB，超过 {} MB 上限，不落盘发送",
                text.len() as f64 / 1e6,
                MAX_PAYLOAD_BYTES / (1024 * 1024)
            ),
        };
    }
    let stamp = timestamp_suffix();
    let ext = text_extension(&text);
    let file_name = format!("剪贴板文本_{}.{}", stamp, ext);
    let size = text.len();
    let preview = text_preview(&text);
    ClipboardContent::Text { file_name, size, preview, full_text: text }
}

/// 暂存目录: 放在 **app 私有目录**而不是收件目录。
///
/// 旧实现写在 `receive_dir/.feisuo-staging`（用户可见目录），
/// 而清理只在"发送成功"后触发 —— 用户抓了剪贴板却没发，
/// 明文截图/密码就永久留在磁盘上（§6.5 缺陷 1）。
pub fn staging_dir(app_dir: &std::path::Path) -> std::path::PathBuf {
    app_dir.join("clipboard-staging")
}

/// 清理超过 `ttl_hours` 的暂存文件（§6.5）。
///
/// 剪贴板里常混着密码与验证码，无限期保留是**隐私漏洞**而不是便利。
/// 引擎启动时与每次发送前都应调用一次。
pub fn purge_stale_staging(app_dir: &std::path::Path, ttl_hours: i64) -> Result<usize, String> {
    let dir = staging_dir(app_dir);
    if !dir.exists() {
        return Ok(0);
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs((ttl_hours.max(1) * 3600) as u64))
        .ok_or_else(|| "时间计算溢出".to_string())?;
    let mut removed = 0usize;
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if modified < cutoff {
            if std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

pub fn stage_content(
    content: &ClipboardContent,
    staging_dir: &std::path::Path,
) -> Result<Vec<String>, String> {
    match content {
        ClipboardContent::Files { paths } => Ok(paths.clone()),
        ClipboardContent::Text { file_name, full_text, .. } => {
            write_staged(staging_dir, file_name, full_text.as_bytes())
        }
        ClipboardContent::Image { file_name, bytes, .. } => {
            write_staged(staging_dir, file_name, bytes)
        }
        ClipboardContent::Rejected { reason } => Err(reason.clone()),
        ClipboardContent::Empty => Err("剪贴板里没有可发送的内容".into()),
    }
}

/// 写入暂存目录并返回绝对路径。
///
/// 文件名来自 `classify_*` 内部生成（`剪贴板图片_<时间戳>.png` 等），
/// 仍然**再做一次落盘校验**：暂存目录是磁盘可写路径，
/// 任何来自外部的字符串都不该被直接 `join`。
fn write_staged(
    staging_dir: &std::path::Path,
    file_name: &str,
    bytes: &[u8],
) -> Result<Vec<String>, String> {
    PathManager::validate_relative_path(file_name)
        .map_err(|e| format!("暂存文件名非法: {}", e))?;
    std::fs::create_dir_all(staging_dir).map_err(|e| format!("创建暂存目录失败: {}", e))?;
    let target = staging_dir.join(file_name);
    std::fs::write(&target, bytes).map_err(|e| format!("写入暂存文件失败: {}", e))?;
    Ok(vec![target.to_string_lossy().to_string()])
}

/// 精确删除若干暂存文件（发送成功后调用）。
///
/// 必须按路径精确删：暂存目录里可能还躺着用户排队等待发送的内容
/// （旧实现的全量清空会把它们一起删掉，表现为静默丢数据）。
pub fn cleanup_staged(app_dir: &std::path::Path, paths: &[String]) -> Result<usize, String> {
    let dir = staging_dir(app_dir);
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for p in paths {
        let path = std::path::Path::new(p);
        // 二次确认在暂存目录内。走 `storage::is_within` 而不是手写
        // canonicalize + 等值比较：Windows 上 `\\?\` 前缀会让等值恒为 false
        // （表现为"发送成功了但暂存文件永远删不掉"），
        // 且暂存目录不存在时手写版会直接放弃删除。
        let inside = match path.parent() {
            Some(parent) => crate::storage::is_within(parent, &dir),
            None => false,
        };
        if !inside {
            tracing::warn!("拒绝删除暂存目录外的路径: {}", p);
            continue;
        }
        if std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}
