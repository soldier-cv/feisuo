//! 可浏览卷枚举（§7.2 / §7.4）。
//!
//! # 为什么需要"卷"这个抽象
//!
//! 旧协议只有"相对收件根目录的子路径"，于是穿梭右栏只能看见
//! `~/feisuo` 的镜像 —— 那不是文件系统浏览器。要做真实地址栏
//! （`D:\项目\2026`）就必须有一个"绝对路径"模型，而绝对路径的根就是卷。
//!
//! # Windows 与 Android 的卷语义完全不同
//!
//! | 平台 | 卷 id | 说明 |
////! |:---|:---|:---|
//! | Windows | `"C:"` `"D:"` | 盘符，来自 `GetLogicalDrives` |
//! | Android | `internal` / `download` / `dcim` / `movies` / `documents` | 虚拟卷，映射到 SAF / 公共目录 |
//!
//! Android **没有盘符**。强行让 UI 画盘符复选框是行不通的，
//! 必须用"存储分类"多选。`Android/data` 与 `Android/obb` 是
//! **其他 App 的私有空间**，SAF 也拿不到，直接不在可选列表里。

use crate::protocol::VolumeInfo;

/// 单次分页的默认条数。
///
/// 1000 是旧协议 `MAX_BROWSE_ENTRIES` 的值。真实磁盘一个目录
/// 上万个条目很常见，所以改成"分页 + 总数"，这个数只是单页大小。
pub const DEFAULT_PAGE_SIZE: u32 = 500;
/// 单页上限：防止对端（或恶意对端）一次返回 10 万条撑爆界面。
pub const MAX_PAGE_SIZE: u32 = 2000;

// ===========================================================================
// Windows
// ===========================================================================
#[cfg(windows)]
pub fn list_volumes() -> Vec<VolumeInfo> {
    use windows::Win32::Storage::FileSystem::GetLogicalDrives;
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let id = format!("{}:", letter);
        // ⚠️ Win32 根路径**必须有冒号 + 尾部反斜杠**，即 `"C:\"`。
        //
        // 早先这里写的是 `format!("{}\\", letter)` —— **冒号根本没写进去**，
        // 得到的是 `"C:\"`… 不，得到的是 `"C:\"` 里那个盘符加一个反斜杠，
        // 也就是 `"C:\"` 少了冒号的 `"C:\"`。
        // 后果是 `GetDiskFreeSpaceExW` / `GetVolumeInformationW` /
        // `GetDriveTypeW` 全部返回 `ERROR_PATH_NOT_FOUND`：
        //   · 容量恒为 0 → 地址栏显示"剩 0 B"，用户以为盘满了；
        //   · 卷标签取不到 → 永远显示"本地磁盘 (C:)"；
        //   · 盘类型判不出 → 软盘/光驱会混进卷列表。
        //
        // 之所以值得写这么多：这个 bug **不产生任何错误日志**
        // （我们主动把失败吞成 0），只表现为"数据不对"，
        // 而 `cargo check` 与类型系统完全看不见它。
        let root = format!("{}:\\", letter);
        // 软盘/CD 没有可浏览价值，且插着会让用户困惑
        match drive_kind(&root) {
            Some(DriveKind::Removable) | Some(DriveKind::Cdrom) => continue,
            _ => {}
        }
        let (total, free) = volume_space(&root);
        let label = match volume_label(&root) {
            Some(l) if !l.trim().is_empty() => format!("{} ({})", l.trim(), id),
            _ => format!("本地磁盘 ({})", id),
        };
        out.push(VolumeInfo {
            id,
            label,
            total_bytes: total,
            free_bytes: free,
            readable: true,
        });
    }
    out
}

#[cfg(windows)]
#[derive(PartialEq, Eq, Clone, Copy)]
enum DriveKind {
    Fixed,
    Removable,
    Cdrom,
    Network,
    Unknown,
}

/// `GetDriveTypeW` 返回值（Win32 API 是裸 u32，windows-rs 没生成枚举）。
#[cfg(windows)]
mod drive_type {
    pub const FIXED: u32 = 3;
    pub const REMOTE: u32 = 4;
    pub const CDROM: u32 = 5;
    pub const REMOVABLE: u32 = 2;
}

#[cfg(windows)]
fn drive_kind(root: &str) -> Option<DriveKind> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    // 必须用 `HSTRING` 构造再取 `PCWSTR`。
    //
    // 早先的写法是 `GetDriveTypeW(&HSTRING::from(root))` —— 靠 `Deref`
    // 隐式转换能编译，于是这个类型错误一直没被看见。但语义是错的：
    // `PCWSTR` 期待的是**以 NUL 结尾的 UTF-16 路径**，
    // 而传过去的指针指向 HSTRING 内部的 `Vec<u16>` ——
    // 那块缓冲区的终止符位置不保证，函数会读到越界或垃圾，
    // 返回 `DRIVE_NO_ROOT_DIR`(1)。所有盘都被判成 `Unknown`，
    // 而 `Unknown` 不在过滤名单里 ⇒ 软盘/光驱会混进卷列表。
    let path = HSTRING::from(root);
    let t = unsafe { GetDriveTypeW(windows::core::PCWSTR(path.as_ptr())) };
    Some(match t {
        drive_type::FIXED => DriveKind::Fixed,
        drive_type::REMOVABLE => DriveKind::Removable,
        drive_type::CDROM => DriveKind::Cdrom,
        drive_type::REMOTE => DriveKind::Network,
        _ => DriveKind::Unknown,
    })
}

#[cfg(windows)]
fn volume_label(root: &str) -> Option<String> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    let mut label_buf = [0u16; 260];
    let mut fs_buf = [0u16; 260];
    let mut serial = 0u32;
    let mut max_comp = 0u32;
    let mut flags = 0u32;
    let path = HSTRING::from(root);
    let ok = unsafe {
        GetVolumeInformationW(
            windows::core::PCWSTR(path.as_ptr()),
            Some(&mut label_buf[..]),
            Some(&mut serial),
            Some(&mut max_comp),
            Some(&mut flags),
            Some(&mut fs_buf[..]),
        )
    };
    if ok.is_ok() {
        let len = label_buf.iter().position(|&c| c == 0).unwrap_or(0);
        Some(String::from_utf16_lossy(&label_buf[..len]))
    } else {
        // 取不到卷标签不是错误（FAT32/无标签卷/权限不足都会这样），
        // 调用方会退回"本地磁盘 (C:)"。记 debug 便于事后排障。
        tracing::debug!("读取卷标签失败 ({}): {:?}", root, ok.err());
        None
    }
}

#[cfg(windows)]
fn volume_space(root: &str) -> (u64, u64) {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut free_to_caller = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    let path = HSTRING::from(root);
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            windows::core::PCWSTR(path.as_ptr()),
            Some(&mut free_to_caller),
            Some(&mut total),
            Some(&mut total_free),
        )
    };
    // 拿不到容量不是错误（网络驱动器、部分虚拟盘会失败），但**必须可见**：
    // 静默返回 0 会让地址栏显示"剩 0 B"，用户以为盘满了。
    if let Err(e) = &ok {
        tracing::debug!("读取卷容量失败 ({}): {}", root, e);
        return (0, 0);
    }
    (total, free_to_caller)
}

// ===========================================================================
// Android
// ===========================================================================
#[cfg(target_os = "android")]
pub fn list_volumes() -> Vec<VolumeInfo> {
    // 虚拟卷 → 公共目录映射。
    // 不含 data / obb: 那是其他 App 的私有空间, SAF 也拿不到。
    vec![
        VolumeInfo {
            id: "internal".into(),
            label: "内部存储".into(),
            total_bytes: 0,
            free_bytes: 0,
            readable: true,
        },
        VolumeInfo {
            id: "download".into(),
            label: "下载".into(),
            total_bytes: 0,
            free_bytes: 0,
            readable: true,
        },
        VolumeInfo {
            id: "dcim".into(),
            label: "相机照片".into(),
            total_bytes: 0,
            free_bytes: 0,
            readable: true,
        },
        VolumeInfo {
            id: "movies".into(),
            label: "影片".into(),
            total_bytes: 0,
            free_bytes: 0,
            readable: true,
        },
        VolumeInfo {
            id: "documents".into(),
            label: "文档".into(),
            total_bytes: 0,
            free_bytes: 0,
            readable: true,
        },
    ]
}

#[cfg(target_os = "android")]
pub fn resolve_volume(id: &str) -> Option<String> {
    Some(
        match id {
            "internal" => "/storage/emulated/0",
            "download" => "/storage/emulated/0/Download",
            "dcim" => "/storage/emulated/0/DCIM",
            "movies" => "/storage/emulated/0/Movies",
            "documents" => "/storage/emulated/0/Documents",
            _ => return None,
        }
        .to_string(),
    )
}

// ===========================================================================
// 其它平台（开发机 / Linux 桌面）
// ===========================================================================
#[cfg(all(not(windows), not(target_os = "android")))]
pub fn list_volumes() -> Vec<VolumeInfo> {
    // Unix 没有盘符, 用 "/" 作为唯一根
    vec![VolumeInfo {
        id: "local".into(),
        label: "根目录 /".into(),
        total_bytes: 0,
        free_bytes: 0,
        readable: true,
    }]
}

#[cfg(all(not(windows), not(target_os = "android")))]
pub fn resolve_volume(_id: &str) -> Option<String> {
    Some("/".to_string())
}

/// 把 `(volume, rel_path)` 解析成真实文件系统路径。
///
/// **只做拼接，不做安全校验** —— 逐段穿越校验由
/// [`crate::storage::PathManager`] 负责，混在一起做反而容易漏。
/// 返回 `None` 表示卷 id 未知。
///
/// 分平台用 `#[cfg]` 拆开而不是 `cfg!()` 运行时分支：后者会让
/// 三个分支都被类型检查，Windows 构建时 `resolve_volume` 根本不存在，
/// 直接编译失败。
#[cfg(windows)]
pub fn resolve_browse_path(volume: &str, rel_path: &str) -> Option<std::path::PathBuf> {
    // 卷 id 必须是单字母盘符。`\\?\` / UNC / `C` 无冒号一律拒 ——
    // 不校验的话 `volume = ".."` 就能拼出 `..\` 开头的路径。
    let letter = volume.strip_suffix(':')?;
    if letter.len() != 1 || !letter.chars().next()?.is_ascii_alphabetic() {
        return None;
    }
    // 盘根必须是 `"X:\"` —— **冒号不能少**。
    //
    // 这里的坑和 `list_volumes` 里那个一模一样：写 `format!("{}\\", letter)`
    // 得到的是 `"X:\"` 里少一个冒号的 `"X:\"`…
    // 也就是 `"X"` + `\` = `"X\"`，一个**不存在的路径**。
    // 它不会 panic、不会报错，只是让后续每次 `read_dir` 都返回
    // "路径不存在"，表现为"这个盘打不开"且完全看不出原因。
    let mut p = std::path::PathBuf::from(format!(
        "{}:\\",
        letter.to_ascii_uppercase()
    ));
    let unified = rel_path.replace('/', "\\");
    for seg in unified.split('\\').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    Some(p)
}

#[cfg(target_os = "android")]
pub fn resolve_browse_path(volume: &str, rel_path: &str) -> Option<std::path::PathBuf> {
    let root = resolve_volume(volume)?;
    let unified = rel_path.replace('\\', "/");
    let mut p = std::path::PathBuf::from(root);
    for seg in unified.split('/').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    Some(p)
}

#[cfg(all(not(windows), not(target_os = "android")))]
pub fn resolve_browse_path(_volume: &str, rel_path: &str) -> Option<std::path::PathBuf> {
    let mut p = std::path::PathBuf::from("/");
    for seg in rel_path.split('/').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    Some(p)
}

/// [`resolve_browse_path`] 的**逆运算**：把绝对路径拆成 `(卷, 卷内相对路径)`。
///
/// 存在的理由：常用目录（桌面 / 下载 / 文档…）是**绝对路径**，而浏览协议
/// 收发的是 `(volume, rel_path)`。转换必须发生在**后端** ——
/// 拆分的规则是平台相关的（盘符 / SAF 卷 / 根目录），放到前端就等于
/// 用 TypeScript 把同一套规则再写一遍，早晚会和这里漂移成
/// "点桌面跳进了 C:\ 根目录"。
///
/// 与 [`resolve_browse_path`] 严格互逆：有测试
/// `known_places_round_trip_through_volume_and_rel` 钉住往返一致。
///
/// 相对路径的分隔符固定用 `/` —— 与 `normalize_sub_path` 的约定一致
/// （它按 `/` 切段，再在 Windows 分支里换成 `\`）。
///
/// 返回 `None` 表示这个绝对路径不落在任何可识别的卷根上
/// （盘符不合法、UNC、Verbatim 前缀没剥干净等）。
#[cfg(windows)]
pub fn split_browse_path(abs: &std::path::Path) -> Option<(String, String)> {
    // 必须先剥 `\?\` 前缀，否则 `\?\C:\Users` 会被当成 UNC 路径，
    // 首字符判断直接失配 —— 表现是"桌面这类常用位置全都不见了"。
    let abs = crate::storage::paths::strip_verbatim_prefix(abs);
    let s = abs.to_string_lossy().to_string();
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    let volume = format!("{}:", (bytes[0] as char).to_ascii_uppercase());
    let rest = s[2..].trim_start_matches(['\\', '/']);
    let rel_path = rest.replace('\\', "/");
    Some((volume, rel_path))
}

/// 非 Windows：卷根即 `/`，整条路径都是"相对路径"。
///
/// Android 走的是另一套（SAF 虚拟卷，见上面那个 `cfg` 分支）——
/// 那里拿不到真实绝对路径，所以调用方不该对这个平台调本函数。
#[cfg(not(windows))]
pub fn split_browse_path(abs: &std::path::Path) -> Option<(String, String)> {
    let s = abs.to_string_lossy().replace('\\', "/");
    let rel_path = s.trim_start_matches('/').to_string();
    Some((String::new(), rel_path))
}

/// `volume` 是否是本机真实存在的卷。
pub fn is_known_volume(volume: &str) -> bool {
    list_volumes().iter().any(|v| v.id == volume)
}

/// 客户端"随便给个可读卷"的通配符（§7.2）。
///
/// 存在的理由：客户端**不该猜盘符**。它既不知道对方有 C: 还是只有 D:，
/// 也不知道哪个盘在对方的 `access_scope` 白名单里。让客户端先发一次
/// "C:" 探测，被拒了再试 "D:"，会表现为列表闪一下 + 弹一条
/// 「未知的卷: C:」—— 用户看到的是"这台设备坏了"。
///
/// 服务端收到 `*` 时：取 scope 内第一个可读卷并真的用上它，
/// 同时在 `volumes` 里回完整列表供地址栏下拉。
pub const VOLUME_ANY: &str = "*";

/// 按 `access_scope` 过滤卷列表，并给每个卷打上 `readable` 标记。
///
/// §8.2：命中强制排除清单的路径对用户**直接不出现**（不是灰掉 ——
/// 灰掉会泄露"这里有东西"）。
pub fn filter_volumes_by_scope(volumes: &mut Vec<VolumeInfo>, scope: &crate::security::AccessScope) {
    match scope.mode {
        crate::security::AccessMode::All | crate::security::AccessMode::Denylist => {
            for v in volumes.iter_mut() {
                v.readable = true;
            }
        }
        crate::security::AccessMode::Allowlist => {
            for v in volumes.iter_mut() {
                // 如果卷在 allow_volumes 中，或者 allow_paths 中有路径属于该卷
                let in_vol = scope
                    .allow_volumes
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&v.id));
                let in_path = scope.allow_paths.iter().any(|p| {
                    p.to_ascii_lowercase().starts_with(&v.id.to_ascii_lowercase())
                });
                v.readable = in_vol || in_path;
            }
            volumes.retain(|v| v.readable);
        }
        // ReceiveOnly: 只暴露收件目录, 真实卷一个都不给
        crate::security::AccessMode::ReceiveOnly => {
            volumes.clear();
        }
    }
}

/// scope 内的第一个可读卷（配合 [`VOLUME_ANY`]）。
pub fn first_readable_volume(scope: &crate::security::AccessScope) -> Option<String> {
    let mut vols = list_volumes();
    filter_volumes_by_scope(&mut vols, scope);
    vols.into_iter().find(|v| v.readable).map(|v| v.id)
}

/// 规范化客户端传来的分页参数。
pub fn normalize_page(offset: u32, limit: u32) -> (usize, usize) {
    let off = offset as usize;
    let lim = if limit == 0 {
        DEFAULT_PAGE_SIZE as usize
    } else {
        (limit as usize).min(MAX_PAGE_SIZE as usize)
    };
    (off, lim)
}
