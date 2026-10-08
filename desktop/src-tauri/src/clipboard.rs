//! Windows 原生剪贴板读取（设计文档 §6）。
//!
//! # 为什么不用浏览器 API
//!
//! `navigator.clipboard.read()` 在 WebView2 下有两个硬伤：
//!
//! 1. 需要**用户手势 + 窗口焦点 + 显式授权**，在托盘常驻（窗口常隐藏）的
//!    场景下经常直接返回空 —— 而这恰恰是飞梭的主要使用形态。
//! 2. **完全拿不到 `CF_HDROP`**，也就是"在资源管理器里 `Ctrl+C` 复制若干文件"。
//!    这是剪贴板发送里最有价值的一条（§6.3）：用户复制的就是原路径，
//!    不需要先粘成临时文件。
//!
//! # 本模块只管"读"，其余都在 core
//!
//! 分类、命名、预览、暂存、TTL 清理全在 `feisuo_core::clipboard`。
//! 早先它们全住在这里，带来两个问题：
//!
//! 1. **测不到**：桌面端是 bin-only crate，`cargo run --example` 拿不到
//!    它的模块，于是这段纯逻辑一行都没被执行过（本轮实测：它藏了
//!    一处"分类规则只在 `#[cfg(windows)]` 里"的隐患，Android 端接上
//!    必然走样）。
//! 2. **Android 端用不了**：移动端同样要支持剪贴板发送（§6.3），
//!    但规则在桌面端，只能复制一遍 —— 而安全规则复制一遍就等于
//!    漏一遍（比如忘了拒绝 SVG）。
//!
//! 现在桌面端只剩两件事：Win32 的 `CF_HDROP` / `CF_DIB` /
//! `CF_UNICODETEXT` 读取，以及把结果交给 core 分类。
//! Android 端只需换掉 `read_clipboard` 的实现。
//!
//! # 类型识别的安全底线
//!
//! | 内容 | 处理 | 原因 |
//! |:---|:---|:---|
//! | `CF_HDROP` 文件列表 | **直接用原路径** | 无需落盘 |
//! | `CF_DIB` / PNG 位图 | 存 `.png` | 直落 |
//! | `image/svg+xml` | **拒绝** | SVG 是可执行 XML，缩略图与浏览器都会渲染 |
//! | `text/html` 整份文档 | **拒绝** | 双击会带远程资源与脚本 |
//! | `text/plain` | 智能后缀 `.txt` / `.json` / `.md` | 不生成 `.url`（可执行 INI 快捷方式） |
//!
//! 拒绝规则本身在 `feisuo_core::clipboard::classify_text`，两侧共用。

// 平台无关的那一半整体转出，桌面端原样再导出，
// 这样所有既有调用点（`crate::clipboard::stage_content` 等）不用改。
pub use feisuo_core::clipboard::*;

use std::sync::Mutex;

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
    use windows::core::HSTRING;

    /// 打开剪贴板并对 `body` 持锁。
    ///
    /// 剪贴板是**全局单例资源**：必须 Open/Close 配对，
    /// 且中间不要做长耗时操作（会阻塞其它应用粘贴）。
    fn with_clipboard<R>(mut body: impl FnMut() -> Option<R>) -> Option<R> {
        // 失败重试：剪贴板可能被别的进程短暂独占
        for _ in 0..8 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                let out = body();
                let _ = unsafe { CloseClipboard() };
                return out;
            }
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        None
    }

    /// PNG 编码的预览 base64。
    ///
    /// 刻意**不做真正缩放**：解码自己的 PNG 再缩放需要完整 zlib 解码器,
    /// 为一个 4000px 的预览不值得。base64 体积可接受, 而界面侧
    /// 用 CSS `object-fit` 限高即可。
    pub fn make_png_preview(png: &[u8]) -> String {
        base64_encode(png)
    }

    /// 读 `CF_HDROP`：资源管理器里复制的文件列表。
    pub fn read_file_list() -> Option<Vec<String>> {
        with_clipboard(|| {
            let h = unsafe { GetClipboardData(CF_HDROP.0 as u32) }.ok()?;
            let hglobal = HGLOBAL(h.0);
            let size = unsafe { GlobalSize(hglobal) } as usize;
            if size < 20 {
                return None;
            }
            let ptr = unsafe { GlobalLock(hglobal) } as *const u8;
            if ptr.is_null() {
                return None;
            }
            // DROPFILES: pFiles(4) + pt(8) + fNC(1) + fWide(1) + 填充 -> 偏移 20
            let header_len =
                i32::from_le_bytes(unsafe { std::slice::from_raw_parts(ptr, 4) }.try_into().ok()?)
                    as usize;
            if header_len == 0 || header_len >= size {
                let _ = unsafe { GlobalUnlock(hglobal) };
                return None;
            }
            // DROPFILES 之后紧跟以 double-NUL 结尾的 UTF-16 路径列表
            let base = unsafe { ptr.add(header_len) };
            let mut out: Vec<String> = Vec::new();
            let mut cursor = base;
            let limit = unsafe { ptr.add(size) };
            loop {
                if cursor >= limit {
                    break;
                }
                // UTF-16 路径列表以 double-NUL 结尾。
                // 注意: 必须从**本条路径的起点**读取 —— 扫描指针会前移到
                // 下一条, 直接用它读会读到空串。
                let start = cursor;
                let mut len = 0usize;
                let mut prev_zero = false;
                let mut p = start;
                while (p as usize) < limit as usize {
                    let unit = unsafe { *(p as *const u16) };
                    p = unsafe { p.add(2) };
                    if unit == 0 {
                        if prev_zero {
                            // double-NUL: 列表结束, `p` 已在循环末尾赋给 cursor
                            break;
                        }
                        prev_zero = true;
                    } else {
                        prev_zero = false;
                        len += 1;
                    }
                }
                if len == 0 {
                    break;
                }
                if out.len() >= 4096 {
                    // 防御: 剪贴板里塞几万个路径
                    break;
                }
                let units = unsafe { std::slice::from_raw_parts(start as *const u16, len) };
                let s = String::from_utf16_lossy(units);
                if !s.is_empty() {
                    out.push(s);
                }
                if (p as usize) >= limit as usize {
                    break;
                }
                cursor = p;
            }
            let _ = unsafe { GlobalUnlock(hglobal) };
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        })
    }

    /// 读 `CF_DIB`（位图）并编码为 PNG。
    pub fn read_dib_png() -> Option<(Vec<u8>, u32, u32)> {
        with_clipboard(|| {
            let h = unsafe { GetClipboardData(CF_DIB.0 as u32) }.ok()?;
            let hglobal = HGLOBAL(h.0);
            let size = unsafe { GlobalSize(hglobal) } as usize;
            if size == 0 || size > MAX_PAYLOAD_BYTES {
                return None;
            }
            let ptr = unsafe { GlobalLock(hglobal) } as *const u8;
            if ptr.is_null() {
                return None;
            }
            let data = unsafe { std::slice::from_raw_parts(ptr, size) };
            let out = dib_to_png(data);
            let _ = unsafe { GlobalUnlock(hglobal) };
            out
        })
    }

    /// 读纯文本（`CF_UNICODETEXT`）。
    pub fn read_text() -> Option<String> {
        with_clipboard(|| {
            let h = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }.ok()?;
            let hglobal = HGLOBAL(h.0);
            let size = unsafe { GlobalSize(hglobal) } as usize;
            if size < 2 || size > MAX_PAYLOAD_BYTES {
                return None;
            }
            let ptr = unsafe { GlobalLock(hglobal) } as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut units = size / 2;
            // 末尾一定有 double-NUL, 去掉终止符
            let slice = unsafe { std::slice::from_raw_parts(ptr, units) };
            if let Some(first_zero) = slice.iter().position(|&c| c == 0) {
                units = first_zero;
            }
            let s = String::from_utf16_lossy(&slice[..units]);
            let _ = unsafe { GlobalUnlock(hglobal) };
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
    }

    /// 剪贴板里是否有"HTML Format"（浏览器复制富文本时的标准格式）。
    ///
    /// 判定方式: 读 `CF_TEXT` 之外还要探测自定义格式。这里用最轻量的
    /// 方式 —— 尝试 `GetClipboardData` 拿到该格式的句柄即可，
    /// 不需要真的解析 HTML。
    #[allow(dead_code)]
    pub fn has_html() -> bool {
        with_clipboard(|| {
            let id = unsafe { RegisterClipboardFormatW(&HSTRING::from("HTML Format")) };
            if id == 0 {
                return None;
            }
            unsafe { GetClipboardData(id) }.ok().map(|_| true)
        })
        .unwrap_or(false)
    }

    /// 把 `CF_DIB`（BITMAPINFOHEADER + 像素，底部朝上 BGR）转成 PNG。
    fn dib_to_png(dib: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        if dib.len() < 40 {
            return None;
        }
        let header_size = i32::from_le_bytes(dib[0..4].try_into().ok()?) as usize;
        if header_size < 40 || header_size > dib.len() {
            return None;
        }
        let width = i32::from_le_bytes(dib[4..8].try_into().ok()?) as i32;
        let height_raw = i32::from_le_bytes(dib[8..12].try_into().ok()?) as i32;
        let planes = i16::from_le_bytes(dib[12..14].try_into().ok()?) as i32;
        let bpp = i16::from_le_bytes(dib[14..16].try_into().ok()?) as i32;
        let compression = u32::from_le_bytes(dib[16..20].try_into().ok()?);
        if planes != 1 || (bpp != 24 && bpp != 32) || compression != 0 {
            // 只处理未压缩的 24/32 位真彩色; 其余格式拒绝而不是猜
            return None;
        }
        let top_down = height_raw < 0;
        let height = height_raw.unsigned_abs();
        if width <= 0 || height == 0 || width > 20000 || height > 20000 {
            return None;
        }
        let width_u = width as u32;
        let height_u = height as u32;
        let bytes_per_row = ((width as usize * bpp as usize + 31) / 32) * 4;
        let pixel_area = bytes_per_row * height as usize;
        if header_size + pixel_area > dib.len() {
            return None;
        }
        let pixels = &dib[header_size..header_size + pixel_area];

        // BGRA -> RGBA, 并把底部朝上翻转为顶部朝下
        let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
        let src_step = bpp as usize / 8;
        for y in 0..height as usize {
            let src_row = if top_down { y } else { height as usize - 1 - y };
            let row = &pixels[src_row * bytes_per_row..src_row * bytes_per_row + width as usize * src_step];
            for px in row.chunks_exact(src_step) {
                rgba.push(px[2]); // R
                rgba.push(px[1]); // G
                rgba.push(px[0]); // B
                rgba.push(if src_step == 4 { px[3] } else { 255 });
            }
        }
        encode_png(&rgba, width_u, height_u)
    }

    /// 最小 PNG 编码器（RGBA / 8-bit）。
    ///
    /// 刻意**不引入 `image` 依赖**：这里只需要"把一块 RGBA 变成合法 PNG"，
    /// 自己写 60 行比拉一个几 MB 的依赖更划算，也避免 Android 交叉编译负担。
    fn encode_png(rgba: &[u8], width: u32, height: u32) -> Option<(Vec<u8>, u32, u32)> {
        /// CRC32（PNG 用）
        fn crc32(data: &[u8]) -> u32 {
            let mut table = [0u32; 256];
            for (i, item) in table.iter_mut().enumerate() {
                let mut c = i as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
                *item = c;
            }
            let mut c = 0xFFFF_FFFFu32;
            for &b in data {
                c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
            }
            c ^ 0xFFFF_FFFF
        }

        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            let mut buf = Vec::with_capacity(4 + body.len());
            buf.extend_from_slice(kind);
            buf.extend_from_slice(body);
            out.extend_from_slice(&buf);
            out.extend_from_slice(&crc32(&buf).to_be_bytes());
        }

        // 原始扫描线: 每行前置 filter byte 0
        let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
        for y in 0..height as usize {
            raw.push(0u8);
            let s = y * width as usize * 4;
            raw.extend_from_slice(&rgba[s..s + width as usize * 4]);
        }

        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, no filter, no interlace
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &zlib_store(&raw));
        chunk(&mut png, b"IEND", &[]);
        Some((png, width, height))
    }

    /// zlib 流（存储块，无压缩）—— 对剪贴板截图这种小图足够，
    /// 且完全避免引入 flate2。
    fn zlib_store(data: &[u8]) -> Vec<u8> {
        fn adler32(data: &[u8]) -> u32 {
            let (mut a, mut b) = (1u32, 0u32);
            for &byte in data {
                a = (a + byte as u32) % 65521;
                b = (b + a) % 65521;
            }
            (b << 16) | a
        }
        let mut out = vec![0x78, 0x01]; // CM=8, CINFO=7, FCHECK 使 (0x78<<8|0x01) % 31 == 0
        let mut i = 0usize;
        while i < data.len() {
            let n = usize::min(65535, data.len() - i);
            let last = if i + n >= data.len() { 1u8 } else { 0u8 };
            out.push(last);
            out.extend_from_slice(&(n as u16).to_le_bytes());
            out.extend_from_slice(&(!(n as u16)).to_le_bytes());
            out.extend_from_slice(&data[i..i + n]);
            i += n;
        }
        out.extend_from_slice(&adler32(data).to_be_bytes());
        out
    }

    /// 读 `CF_DIB` 得到的原始像素尺寸（仅用于日志/诊断）。
    #[allow(dead_code)]
    pub fn dib_dimensions() -> Option<(u32, u32)> {
        read_dib_png().map(|(_, w, h)| (w, h))
    }
}

#[cfg(windows)]
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// 读取剪贴板并分类（只读，不落盘）。
///
/// 界面拿它做**预览**（D5：预览默认开启），确认后才调
/// [`stage_clipboard_into`] 真正落盘。预览阶段不落盘是有意的：
/// 「看了没发」同样会在磁盘上留下明文。
#[cfg(windows)]
pub fn read_clipboard() -> ClipboardContent {
    // 1) CF_HDROP 优先 —— "复制文件直接发"是价值最高的一条（§6.3）
    if let Some(paths) = imp::read_file_list() {
        return ClipboardContent::Files { paths };
    }
    // 2) 位图
    if let Some((png, _w, _h)) = imp::read_dib_png() {
        if png.len() > feisuo_core::clipboard::MAX_PAYLOAD_BYTES {
            return ClipboardContent::Rejected {
                reason: format!(
                    "剪贴板图片有 {:.1} MB，超过 {} MB 上限，不落盘发送",
                    png.len() as f64 / 1e6,
                    feisuo_core::clipboard::MAX_PAYLOAD_BYTES / (1024 * 1024)
                ),
            };
        }
        let stamp = timestamp_suffix();
        let file_name = format!("剪贴板图片_{}.png", stamp);
        let size = png.len();
        let preview_base64 = imp::make_png_preview(&png);
        return ClipboardContent::Image { file_name, size, preview_base64, bytes: png };
    }
    // 3) 文本 —— 分类与拒绝规则在 core，Windows 与 Android 共用同一套
    if let Some(text) = imp::read_text() {
        return feisuo_core::clipboard::classify_text(text);
    }
    ClipboardContent::Empty
}

/// 最近一次预览读到的图片 / 文本。
///
/// 预览返回给界面时，图片字节和全文被 `serde(skip)` 丢掉了。
/// 确认落盘如果再读一次剪贴板，用户在看预览的这几秒里复制了别的东西，
/// 发出去的就不是刚看过的那份。
/// 这里留下预览那一次的内容，确认时只用它。
static PREVIEWED: Mutex<Option<ClipboardContent>> = Mutex::new(None);

/// 非 Windows 平台：Android / 其它宿主统一走空实现。
#[cfg(not(windows))]
pub fn read_clipboard() -> ClipboardContent {
    ClipboardContent::Empty
}

// ===========================================================================
// Tauri 命令层
// ===========================================================================

/// 读剪贴板并返回预览（**不落盘**）。
///
/// D5 决策：预览默认开启。界面拿到预览后由用户点「发送」，
/// 再调 [`stage_clipboard_payload`] 真正落盘。
#[tauri::command]
pub fn read_clipboard_preview(
    state: tauri::State<'_, crate::AppState>,
) -> Result<ClipboardContent, String> {
    let _ = &state;
    let content = crate::clipboard::read_clipboard();
    if content.needs_staging() {
        if let Ok(mut slot) = PREVIEWED.lock() {
            *slot = Some(content.clone());
        }
    }
    Ok(content)
}

/// 把**刚才预览过**的剪贴板内容落盘，返回可直接发送的路径列表。
///
/// 不用此刻剪贴板上的内容：预览和确认之间用户可能又复制了别的。
/// 没有待确认的预览就返回错误，界面会提示失败，而不是悄悄发出另一份。
#[tauri::command]
pub fn stage_clipboard_payload(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<String>, String> {
    let content = {
        let mut slot = PREVIEWED.lock().map_err(|_| "剪贴板预览状态不可用".to_string())?;
        slot.take()
    };
    let Some(content) = content else {
        return Err("没有待确认的剪贴板预览，请重新抓取".into());
    };
    let app_dir = state.engine.app_dir.clone();
    feisuo_core::clipboard::stage_content(
        &content,
        &feisuo_core::clipboard::staging_dir(&app_dir),
    )
}

/// 清理超期暂存文件（剪贴板暂存 TTL，默认 24h）。
#[tauri::command]
pub fn purge_clipboard_staging(
    ttl_hours: Option<i64>,
    state: tauri::State<'_, crate::AppState>,
) -> Result<usize, String> {
    let app_dir = state.engine.app_dir.clone();
    crate::clipboard::purge_stale_staging(&app_dir, ttl_hours.unwrap_or(24))
}

/// 发送成功后精确删除暂存文件。
#[tauri::command]
pub fn cleanup_clipboard_staging(
    paths: Vec<String>,
    state: tauri::State<'_, crate::AppState>,
) -> Result<usize, String> {
    let app_dir = state.engine.app_dir.clone();
    crate::clipboard::cleanup_staged(&app_dir, &paths)
}
