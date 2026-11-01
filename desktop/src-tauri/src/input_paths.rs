//! 把用户**粘贴进来的一段文本**解析成"要发送的路径清单"。
//!
//! ## 为什么这件事必须由 Rust 做，而不是前端拼字符串
//!
//! 1. **"这个路径存在吗"只有 Rust 能答**。前端要是把不存在的路径塞进清单，
//!    用户会看到一条假条目，直到点「发送」才被协议层或 `preview_send_paths`
//!    拒绝 —— 而且那条错误会把**整批**路径一起否掉（`preview_send_paths`
//!    遇到一个不存在的路径就返回 Err），一行笔误害得其余几十行都发不出去。
//! 2. **解析规则全是操作系统语义**：盘符、大小写不敏感、UNC、`\\?\` 扩展长度
//!    前缀、`%USERPROFILE%`、`~`。前端各写一套，迟早与 Rust 侧的判断分叉，
//!    而分叉的表现是"清单里显示得挺好，一发就报路径不存在"。
//!
//! 所以这里只做**纯字符串 + 元数据**的处理，逐行给出结论：
//! 能用的进 [`Resolved::paths`]，不能用的进 [`Resolved::problems`]（带行号），
//! **一行出错不影响其他行** —— 用户粘的是从资源管理器/终端里复制来的一片路径，
//! 里面混着一两条失效的很正常。
//!
//! ## 支持的写法（都是为了对上真实的粘贴来源）
//!
//! | 粘贴来源 | 形态 | 处理 |
//! |:---|:---|:---|
//! | 资源管理器「复制为路径」 | `"C:\a b\c.txt"`（可多行） | 去掉引号；一行多段也能拆开 |
//! | 浏览器 / 编辑器「复制文件地址」 | `file:///C:/a%20b/c.txt` | 去 scheme + 百分号解码 |
//! | 终端 / 脚本 | `%USERPROFILE%\Desktop\a.txt` | 展开环境变量 |
//! | PowerShell / Git Bash 习惯 | `~/Desktop/a.txt` | 展开为用户主目录 |
//! | 从别处拷来的长路径 | `\\?\C:\a\b.txt` | 去掉扩展长度前缀 |

use std::path::PathBuf;

/// 一条没能解析成可用路径的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// 1-based 行号 —— 用户要在输入框里找到那一行，所以必须给行号而不是"某个路径"
    pub line: usize,
    /// 归一化之后的文本（用户能看到自己到底粘贴了什么）
    pub text: String,
    /// 给用户看的中文原因
    pub reason: String,
}

/// 解析结果：能用的路径 + 逐行的失败原因。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub paths: Vec<String>,
    pub problems: Vec<Problem>,
}

/// 把一段文本解析成路径清单。
pub fn resolve(raw: &str) -> Resolved {
    let mut out = Resolved::default();

    for (idx, line) in raw.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let (segments, leftover) = split_line(trimmed);
        if let Some(text) = leftover {
            out.problems.push(Problem {
                line: line_no,
                text,
                reason: "引号外还有内容，无法确定这是一个路径".to_string(),
            });
        }

        for segment in segments {
            let path = normalize(&segment);
            if path.is_empty() {
                continue;
            }
            if let Some(reason) = reject_reason(&path) {
                out.problems.push(Problem {
                    line: line_no,
                    text: path,
                    reason,
                });
                continue;
            }
            // 同一批里重复粘贴同一个路径没有意义：清单本来也只收一份
            // （`pushStaged` 会去重），先在这里静默收掉，免得用户看到
            // "已添加 5 个"却发现清单里只有 3 行。
            if out.paths.iter().any(|existing| same_path(existing, &path)) {
                continue;
            }
            out.paths.push(path);
        }
    }

    out
}

/// 把一行拆成一个或多个候选路径。
///
/// 规则只有两条，都是为了对上资源管理器的「复制为路径」：
/// 1. 整行**不含引号** → 整行就是一个路径（路径里可以有空格，所以不按空格切）；
/// 2. 含引号 → 只取引号内的内容，一段一个路径。Windows 文件名里不允许 `"`，
///    所以"引号外还有非空内容"一定是粘贴错了，明确报出来而不是猜。
///
/// 返回 `(候选, 引号外的残留内容)`。
fn split_line(line: &str) -> (Vec<String>, Option<String>) {
    if !line.contains('"') {
        return (vec![line.to_string()], None);
    }

    let mut segments = Vec::new();
    let mut outside = String::new();
    let mut current: Option<String> = None;

    for ch in line.chars() {
        match ch {
            '"' => match current.take() {
                Some(s) => segments.push(s),
                None => current = Some(String::new()),
            },
            _ => match current.as_mut() {
                Some(s) => s.push(ch),
                None => {
                    if !ch.is_whitespace() {
                        outside.push(ch);
                    }
                }
            },
        }
    }
    // 引号没闭合：把已经读到的当一段。用户少打了一个引号也该能用，
    // 直接丢掉会让人以为粘贴没生效。
    if let Some(s) = current.take() {
        segments.push(s);
    }

    let leftover = if outside.trim().is_empty() {
        None
    } else {
        Some(outside)
    };
    (segments, leftover)
}

/// 把一段候选文本归一化成路径字符串（不做存在性检查）。
fn normalize(raw: &str) -> String {
    let mut s = raw.trim().trim_matches('"').trim().to_string();
    if s.is_empty() {
        return s;
    }
    if let Some(url_path) = strip_file_url(&s) {
        s = percent_decode(&url_path);
    }
    s = strip_verbatim_prefix(&s);
    s = expand_env_vars(&s);
    s = expand_tilde(&s);
    s.trim().to_string()
}

/// `file:///C:/a%20b/c.txt` → `C:/a b/c.txt`；不是 file URL 时返回 None。
fn strip_file_url(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    // 只按**前缀字面量**的长度切片。不要反过来用 `s.len() - rest.len()`：
    // 那要求"小写化不改变字节长度"，而这条不变量并不由这里的代码保证。
    let rest = if lower.starts_with("file:///") {
        &s["file:///".len()..]
    } else if lower.starts_with("file://") {
        &s["file://".len()..]
    } else {
        return None;
    };
    // `file://localhost/C:/x` 里的主机名要去掉；别的写法（网络共享）原样保留
    let rest = rest
        .strip_prefix("localhost/")
        .or_else(|| rest.strip_prefix("localhost\\"))
        .unwrap_or(rest);
    // `file:////srv/share/x`（四道斜杠 = UNC 共享）：开头那道斜杠是 URL 的
    // 路径分隔符，而 UNC 需要两道反斜杠。不还原的话它会被判成相对路径，
    // 用户看到"不是完整路径"，而这条 URL 其实完全合法。
    if rest.starts_with('/') || rest.starts_with('\\') {
        let trimmed = rest.trim_start_matches(['/', '\\']);
        return Some(format!(r"\\{}", trimmed.replace('/', "\\")));
    }
    Some(rest.to_string())
}

/// `%20` → 空格。只解码合法的一对十六进制，其余原样保留（路径里真的可能有 `%`）。
///
/// 必须按**字节**解码再转成字符串，不能逐字节当字符拼：中文路径在 URL 里是
/// UTF-8 字节序列（`日` = `%E6%97%A5`），逐字节 `as char` 会拼出三个
/// 拉丁字符 `æ—¥`，得到一个看起来像乱码的**错误**路径 —— 然后存在性检查
/// 报"路径不存在"，用户怎么改都不对。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // 合法路径的第一段 `file:///` 里不会有半截转义，但粘贴来的文本什么都有：
    // 非法 UTF-8 用 lossy 兜住，剩下的交给存在性检查 —— 那里的报错比
    // 在这里 panic 有用。
    String::from_utf8_lossy(&out).into_owned()
}

/// `\\?\C:\a` → `C:\a`；`\\?\UNC\srv\share` → `\\srv\share`。
fn strip_verbatim_prefix(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{}", rest);
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    s.to_string()
}

/// 展开 `%NAME%` 形式的环境变量；变量不存在时**原样保留**。
///
/// 为什么不存在时要保留：`%20` 这种片段在文件名里是合法字符，
/// 而"猜不出就删掉"会把 `a%20b.txt` 变成 `a.txt` —— 一个更糟的错误路径。
fn expand_env_vars(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('%') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('%') {
            Some(close) => {
                let name = &after[..close];
                let value = if name.is_empty() {
                    None
                } else {
                    std::env::var(name).ok().filter(|v| !v.is_empty())
                };
                match value {
                    Some(v) => out.push_str(&v),
                    // 原样写回，连百分号一起
                    None => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                // 只有开头那个 %，后面没有配对的：剩下的都是字面量
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 展开开头的 `~`（`~`、`~/x`、`~\x`）。
fn expand_tilde(s: &str) -> String {
    if s != "~" && !s.starts_with("~/") && !s.starts_with(r"~\") {
        return s.to_string();
    }
    let Some(home) = dirs::home_dir() else {
        return s.to_string();
    };
    let rest = if s == "~" { "" } else { &s[1..] };
    let rest = rest.trim_start_matches(['/', '\\']);
    if rest.is_empty() {
        return home.display().to_string();
    }
    home.join(rest).display().to_string()
}

/// 归一化之后的路径为什么不能用（`None` = 可以用）。
fn reject_reason(path: &str) -> Option<String> {
    let buf = PathBuf::from(path);
    if !buf.is_absolute() {
        // 相对路径在这里**没有可信的基准目录**：目标进程的工作目录是 exe 所在目录，
        // 与用户心里的"当前文件夹"（资源管理器 / 终端）完全不是一回事。
        // 与其猜一个大概率错的地方，不如直接说清楚要什么。
        return Some("不是完整路径，请粘贴带盘符的完整路径（例如 C:\\Users\\...）".to_string());
    }
    if !buf.exists() {
        return Some("路径不存在（文件可能已被移动或删除）".to_string());
    }
    None
}

/// Windows 语义下的"同一个路径"：大小写不敏感、忽略结尾分隔符。
fn same_path(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.trim_end_matches(['\\', '/']).to_lowercase()
    }
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个一定存在的目录（本仓的 crate 目录），用来测"存在即通过"。
    fn existing_dir() -> String {
        env!("CARGO_MANIFEST_DIR").to_string()
    }

    #[test]
    fn plain_path_with_spaces_is_one_candidate() {
        let raw = r"C:\Program Files\Some App\a.txt";
        let (segs, leftover) = split_line(raw);
        assert_eq!(segs, vec![raw.to_string()]);
        assert!(leftover.is_none());
    }

    #[test]
    fn explorer_copy_as_path_strips_quotes_and_splits_lines() {
        // 资源管理器多选「复制为路径」就是这么给的：每行一个带引号的路径
        let dir = existing_dir();
        let raw = format!("\"{dir}\"\n\"{dir}\"");
        let out = resolve(&raw);
        // 同一个路径粘两遍只收一次，且不报错
        assert_eq!(out.paths, vec![dir.clone()]);
        assert!(out.problems.is_empty(), "{:?}", out.problems);
    }

    #[test]
    fn multiple_quoted_paths_on_one_line_are_split() {
        let dir = existing_dir();
        let raw = format!("\"{dir}\" \"{dir}\\src\"");
        let out = resolve(&raw);
        assert_eq!(out.paths.len(), 2, "{:?}", out.paths);
    }

    #[test]
    fn text_outside_quotes_is_reported_not_guessed() {
        let dir = existing_dir();
        let raw = format!("我复制错了 \"{dir}\"");
        let out = resolve(&raw);
        assert_eq!(out.paths.len(), 1);
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].line, 1);
    }

    #[test]
    fn file_url_is_decoded() {
        let dir = existing_dir().replace('\\', "/");
        // 把路径里的反斜杠换成 %5C，顺带验证百分号解码
        let encoded = format!("file:///{}", dir.replace('/', "%5C"));
        assert_eq!(percent_decode(&encoded[8..]), dir.replace('/', "\\"));
        // 归一化后应当仍是一个存在的绝对路径
        assert!(PathBuf::from(normalize(&encoded)).exists());
    }

    #[test]
    fn percent_decode_handles_non_ascii_utf8() {
        // 中文路径在 URL 里是 UTF-8 字节序列：`日` = E6 97 A5。
        // 逐字节当字符拼会得到 "æ—¥"，那是个**错误**路径。
        assert_eq!(percent_decode("%E6%97%A5.txt"), "日.txt");
        assert_eq!(percent_decode("a%20b.txt"), "a b.txt");
        // 不成对的 % 原样保留（文件名里真的可能有 %）
        assert_eq!(percent_decode("100%.txt"), "100%.txt");
        assert_eq!(percent_decode("a%2"), "a%2");
    }

    #[test]
    fn file_url_unc_share_becomes_unc_path() {
        // 四道斜杠 = UNC 共享；还原成 `\\srv\share\x` 才是绝对路径
        assert_eq!(
            strip_file_url("file:////srv/share/x.txt").as_deref(),
            Some(r"\\srv\share\x.txt")
        );
        // 盘符写法不能被误伤
        assert_eq!(
            strip_file_url("file:///C:/a/b.txt").as_deref(),
            Some("C:/a/b.txt")
        );
        // 不是 file URL 时不处理
        assert!(strip_file_url(r"C:\a\b.txt").is_none());
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(strip_verbatim_prefix(r"\\?\C:\a\b.txt"), r"C:\a\b.txt");
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\srv\share\a.txt"),
            r"\\srv\share\a.txt"
        );
        assert_eq!(strip_verbatim_prefix(r"C:\a"), r"C:\a");
    }

    #[test]
    fn unknown_env_var_stays_literal() {
        assert_eq!(
            expand_env_vars(r"%NO_SUCH_VAR_9f3%\a.txt"),
            "%NO_SUCH_VAR_9f3%\\a.txt"
        );
        // 只有开头的 % 时不能把后面的内容吞掉
        assert_eq!(expand_env_vars("%broken"), "%broken");
    }

    #[test]
    fn home_tilde_expands_to_something_absolute() {
        let expanded = expand_tilde(r"~\Desktop\a.txt");
        if dirs::home_dir().is_some() {
            assert!(PathBuf::from(&expanded).is_absolute(), "{expanded}");
            assert!(!expanded.starts_with('~'), "{expanded}");
        }
    }

    #[test]
    fn relative_path_is_rejected_with_reason() {
        let out = resolve(r"Desktop\a.txt");
        assert!(out.paths.is_empty());
        assert_eq!(out.problems.len(), 1);
        assert!(out.problems[0].reason.contains("完整路径"));
    }

    #[test]
    fn missing_path_reports_its_line_number() {
        let dir = existing_dir();
        let raw = format!("{dir}\r\nC:\\definitely\\not\\here\\x.txt\r\n");
        let out = resolve(&raw);
        assert_eq!(out.paths, vec![dir]);
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].line, 2);
        assert!(out.problems[0].reason.contains("不存在"));
    }

    #[test]
    fn blank_lines_are_ignored() {
        let out = resolve("\n\n   \n");
        assert!(out.paths.is_empty());
        assert!(out.problems.is_empty());
    }

    #[test]
    fn same_path_ignores_case_and_trailing_separator() {
        assert!(same_path(r"C:\Users\Me\a.txt", r"c:\users\me\A.TXT"));
        assert!(same_path(r"C:\Users\Me\", r"C:\Users\Me"));
        assert!(!same_path(r"C:\a", r"C:\b"));
    }
}
