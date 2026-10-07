//! 路径边界判定（`is_within`）的唯一实现。
//!
//! # 为什么需要单独一个模块
//!
//! 判定"拼接出来的路径有没有逃出根目录"这件事，本项目里出现了 7 次
//! （目录浏览 ×2、传输落盘、剪贴板暂存、取回落点……）。每次都写成
//!
//! ```ignore
//! let root  = root.canonicalize().unwrap_or_else(|_| root.clone());
//! let probe = dir.canonicalize().unwrap_or_else(|_| dir.clone());
//! if !probe.starts_with(&root) { /* 拒绝 */ }
//! ```
//!
//! 这段代码在 Linux/macOS 上是对的，在 Windows 上有**两个**问题，
//! 而且第二个问题让"纵深防御"变成了"纵深失效"：
//!
//! ## 问题 1：逐字前缀导致误拒（Windows 特有）
//!
//! `Path::canonicalize()` 在 Windows 上返回**逐字路径**（verbatim path），
//! 也就是带 `\\?\` 前缀的绝对路径：
//!
//! ```text
//! root  -> \\?\C:\Users\me\feisuo
//! probe -> C:\Users\me\feisuo\sub      ← 目标不存在时 canonicalize 失败，回落成普通写法
//! ```
//!
//! 于是 `probe.starts_with(&root)` 拿 `C:\...` 去比 `\\?\C:\...`，
//! **永远为 false** —— 只要浏览一个磁盘上还不存在的子目录，就会被
//! 误判成"逃逸出根目录"而拒绝。表现是 Security error 而不是 404，
//! 排查时很难想到是前缀问题。
//!
//! ## 问题 2：目标不存在时，检查等于没做
//!
//! `unwrap_or_else(|_| dir.clone())` 在 `canonicalize` 失败时**直接用原始
//! 字符串**参与比较。`C:\Users\me\feisuo\a\b\c`（尚不存在）会原样通过
//! 与 `\\?\C:\Users\me\feisuo` 的比较——这一次是巧合般地正确，
//! 但只要根目录那一侧也不存在，两边都是普通写法，检查就退化成
//! 纯字符串前缀比较，**中间任何一段符号链接都不会被解析**。
//!
//! ## 本模块的做法
//!
//! `canonicalize_lenient` 逐级向上找到**最深的、真实存在的祖先**，
//! 对它做 `canonicalize`（这一步会解析全部符号链接与 `..`），
//! 再把剩下的路径段原样接回去。这样：
//!
//! - 剥掉 `\\?\` 前缀 → 问题 1 消失；
//! - 目标不存在也能得到**规范化过**的绝对路径 → 问题 2 消失；
//! - 中间存在符号链接时，祖先的 `canonicalize` 会把它解析成真实位置，
//!   于是"链接指向根目录之外"这种逃逸**反而能被抓到**（比原来更严）。
//!
//! 代价是每层一次 `canonicalize` 系统调用。目录浏览/取回这类操作
//! 一次只有几层，且本来就是同步磁盘 I/O 主导的，不构成瓶颈。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// 去掉 Windows 逐字路径的 `\\?\` 前缀，使两条路径可比。
///
/// `\\?\C:\a` 与 `C:\a` 指向同一个位置，但 `Path` 认为它们不同。
/// 只在前缀确实是逐字前缀时才动手，其他前缀（UNC 的 `\\server\share`）
/// 原样保留 —— 那些不是逐字路径，剥了反而会坏。
///
/// 非 Windows 平台原样返回（分配一个 `PathBuf` 而已，调用点都在
/// 目录浏览/取回这类磁盘 I/O 路径上，不在热循环里）。
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy();
        // 两种逐字形式：`\\?\C:\...` 与 `\\?\UNC\server\share\...`
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// 规范化到"绝对、无 `..`、无符号链接"的形式。
///
/// 与 `Path::canonicalize` 的区别：**目标不存在时不报错**，而是规范化
/// 最深的已存在祖先后接回剩余路径段。返回的路径一定可以直接和别的
/// 规范化路径做 `starts_with` 比较。
pub fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return strip_verbatim_prefix(&real);
    }
    // 目标不存在：逐级上溯，找到最深的能规范化的祖先。
    // `tail` 记录从祖先往下到目标的各段，最后要逆序接回。
    let mut tail: Vec<OsString> = Vec::new();
    let mut cursor = path;
    loop {
        let Some(parent) = cursor.parent() else {
            break;
        };
        if let Some(name) = cursor.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(real) = parent.canonicalize() {
            let mut out = strip_verbatim_prefix(&real);
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return out;
        }
        cursor = parent;
    }
    // 连根都规范化不了（网络盘掉了、路径非法）：原样返回但剥掉前缀。
    strip_verbatim_prefix(path).to_path_buf()
}

/// `child` 是否位于 `root` 之内（含 `root` 自身）。
///
/// 两个参数都会先经 [`canonicalize_lenient`] 规范化，所以调用方**不需要**
/// 自己 `canonicalize`，也**不需要**担心 Windows 前缀与目标不存在的问题。
///
/// `false` 表示应当拒绝。注意这是纯判定，不做 I/O 之外的任何决策，
/// 调用方仍需自行决定拒绝时回什么错误。
pub fn is_within(child: &Path, root: &Path) -> bool {
    let root_real = canonicalize_lenient(root);
    let child_real = canonicalize_lenient(child);
    // Windows 上比较时统一用反斜杠语义：Path::starts_with 是按组件比的，
    // 组件在 Windows 上是按 `\` 和 `/` 两种分隔符解析的，所以这里无需额外处理。
    child_real.starts_with(&root_real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 守卫：`is_within` 必须对**尚不存在**的子目录返回 true。
    ///
    /// 这条在 Windows 上是有历史的：老实现用
    /// `canonicalize().unwrap_or_else(|_| 原路径)` 再 `starts_with`，
    /// 而 `canonicalize` 在 Windows 返回 `\\?\C:\...`、目标不存在时又
    /// 回落成 `C:\...`，前缀不同 → 恒为 false → 浏览未创建的子目录
    /// 被误报成"路径逃逸"。这个测试在 Linux 上也会通过（那里没有
    /// 前缀问题），所以它的价值是**锁住语义**：将来谁把 `is_within`
    /// 换成手写 canonicalize，Linux CI 上也能发现行为退化。
    #[test]
    fn within_true_for_nonexistent_child() {
        let base = std::env::temp_dir().join(format!("feisuo-paths-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let ghost = base.join("not").join("created").join("yet");
        assert!(!ghost.exists(), "前置条件: 该子目录必须不存在");
        assert!(is_within(&ghost, &base), "不存在的子目录应判定为在内");
        let _ = fs::remove_dir_all(&base);
    }

    /// 守卫：真的逃出去必须被拒（防止"为了修 false positive 把检查放水"）。
    #[test]
    fn within_false_for_sibling_with_shared_prefix() {
        let base = std::env::temp_dir().join(format!("feisuo-paths-sib-a-{}", std::process::id()));
        let evil = std::env::temp_dir().join(format!("feisuo-paths-sib-b-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&evil);
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&evil).unwrap();
        // 字符串前缀相同（`...sib-a` vs `...sib-b` 共享 `...sib-`），
        // 但组件级比较必须判 false —— 这正是 starts_with 语义的意义。
        assert!(!is_within(&evil, &base));
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&evil);
    }

    /// 守卫：`..` 逃逸（虽然上游 `normalize_sub_path` 已挡，这里是纵深防御）。
    #[test]
    fn within_rejects_dotdot_escape() {
        let base = std::env::temp_dir().join(format!("feisuo-paths-dd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("inner")).unwrap();
        let escape = base.join("inner").join("..").join("..");
        assert!(!is_within(&escape, &base), ".. 逃逸必须被拒");
        let _ = fs::remove_dir_all(&base);
    }
}