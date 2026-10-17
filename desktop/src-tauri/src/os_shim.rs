use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

/// Windows: 隐藏子进程控制台窗口的创建标志 (CREATE_NO_WINDOW)
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 为命令附加"不要弹出控制台窗口"的标志。
///
/// 这是"点击设置页会弹出一个黑窗口"的根因修复:
/// 本应用以 `windows_subsystem = "windows"` 编译, 自身没有控制台;
/// 而 `Command::new("reg")` 启动的是控制台子系统程序,
/// Windows 会为它分配一个新的控制台, 于是屏幕上闪过一个黑框。
fn hidden(mut cmd: Command) -> Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = &mut cmd;
    }
    cmd
}

/// 在系统文件管理器中打开目录 / 定位文件。
///
/// 不使用 `open` crate: 它在 Windows 上走 `cmd /c start`,
/// 会先创建一个控制台, 同样表现为"闪一个黑窗口"。
pub fn open_in_explorer(path: &Path) -> Result<(), String> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("创建目录失败: {}", e))?;
    }

    #[cfg(target_os = "windows")]
    {
        // 选中文件时 explorer 需要 `/select,` 前缀, 否则直接打开所在目录
        let arg = if path.is_file() {
            format!("/select,{}", path.to_string_lossy())
        } else {
            path.to_string_lossy().to_string()
        };
        let child = hidden(Command::new("explorer.exe"))
            .arg(arg)
            .spawn()
            .map_err(|e| format!("打开文件管理器失败: {}", e))?;
        drop(child);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("打开目录失败: {}", e))?;
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("打开目录失败: {}", e))?;
        Ok(())
    }
}

/// 隐藏执行一条命令并等待其结束 (不弹控制台窗口)
pub fn run_hidden(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    let output = hidden(Command::new(program))
        .args(args)
        .output()
        .map_err(|e| format!("执行 {} 失败: {}", program, e))?;
    Ok(output)
}

/// 以独立进程启动一个程序并**立即返回**，不等它结束。
///
/// 与 [`run_hidden`] 的区别是这里不 `.output()`：调用方（如应用内更新）
/// 启动新版本进程后自己就要退出，若在此处等待会把主流程卡住。
///
/// 返回的 `Child` 被刻意丢弃 —— 丢弃只关闭 Rust 侧的句柄，
/// 不会终止子进程，它会继续在后台运行。
pub fn spawn_detached(program: &Path, args: &[OsString]) -> Result<(), String> {
    let child = hidden(Command::new(program))
        .args(args)
        .spawn()
        .map_err(|e| format!("启动 {} 失败: {}", program.display(), e))?;
    drop(child);
    Ok(())
}

/// 以**提权**方式启动一个程序（必要时弹 UAC）。
///
/// ## 为什么不能直接 `spawn_detached`
///
/// `Command::spawn` 走的是 `CreateProcess`，它**不会**触发提权：目标
/// 清单要求 `requireAdministrator` 时，`CreateProcess` 直接失败
/// （`ERROR_ELEVATION_REQUIRED = 740`）；而 Inno Setup 的 `PrivilegesRequired=admin`
/// 判定发生在安装器内部 —— 它发现自己不是管理员时会**静默放弃**
/// （实测：非管理员下静默安装返回后 Program Files 里的文件时间戳纹丝不动，
/// 连一条安装日志都不产生）。用户看到的是"更新成功"，程序还是旧版。
///
/// 走 `ShellExecuteW` + `runas` 才会触发 UAC。这是 Windows 上唯一
/// 正确的"以管理员身份运行"入口。
///
/// 返回值：成功拿到 Shell 句柄即 `Ok(())`。用户在 UAC 上点"取消"时
/// 返回 `Err`（错误码 1223 = `ERROR_CANCELLED`），调用方应当把它当成
/// "用户拒绝提权"而不是"更新失败"。
pub fn spawn_elevated(program: &Path, args: &[OsString]) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let wide = |s: &std::ffi::OsStr| -> Vec<u16> {
            s.encode_wide().chain(std::iter::once(0)).collect()
        };

        // 参数按 Windows 命令行规则拼成一个字符串。
        // 每个参数都用双引号包起来，内部的双引号转义成 \"。
        let params = args
            .iter()
            .map(|a| {
                let s = a.to_string_lossy();
                format!("\"{}\"", s.replace('"', "\\\""))
            })
            .collect::<Vec<_>>()
            .join(" ");

        let verb = wide(std::ffi::OsStr::new("runas"));
        let file = wide(program.as_os_str());
        let params_w = wide(std::ffi::OsStr::new(&params));

        // ShellExecuteW 的返回值是"大于 32 表示成功"的伪句柄，不是错误码。
        let ret = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(file.as_ptr()),
                PCWSTR(params_w.as_ptr()),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        let code = ret.0 as isize;
        if code > 32 {
            return Ok(());
        }
        // 1223 = 用户在 UAC 对话框上点了"否"。
        let msg = if code == 1223 {
            "用户取消了管理员授权".to_string()
        } else {
            format!("请求管理员权限失败 (ShellExecute 返回 {code})")
        };
        Err(msg)
    }

    #[cfg(not(target_os = "windows"))]
    {
        // 非 Windows 没有 UAC 这一套，直接启动。
        spawn_detached(program, args)
    }
}

/// 在系统默认浏览器中打开一个 URL (主要用于更新失败时的手动兜底)。
///
/// 刻意**不用** `cmd /c start`：那会先创建控制台再启动浏览器，
/// 表现为"点一下闪一个黑框"。
pub fn open_url_in_browser(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        // 只允许 http(s)：URL 会来自远端 Release 响应，
        // 放行 `file://` 或 `javascript:` 等协议等于给远端一个本地执行入口。
        return Err(format!("拒绝打开非 http(s) 链接: {url}"));
    }

    #[cfg(target_os = "windows")]
    let result = hidden(Command::new("explorer.exe")).arg(url).spawn();

    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).spawn();

    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open").arg(url).spawn();

    result
        .map(|child| drop(child))
        .map_err(|e| format!("打开浏览器失败: {e}"))
}

#[cfg(test)]
mod tests {
    /// 防回归: **所有** `Command::new` 必须集中在 `os_shim.rs`。
    ///
    /// 为什么要用测试盯住这件事: 本文件以 `windows_subsystem = "windows"`
    /// 编译, 自身没有控制台。任何在别处用裸 `Command::new` 启动
    /// **控制台子系统**程序 (reg.exe / cmd.exe / powershell.exe …),
    /// Windows 都会给它新分配一个控制台, 屏幕上就闪出一个黑框。
    /// 这个 bug 当年就是这样进来的, 而且肉眼只在"点击设置页"时出现,
    /// 极难定位 —— 所以在编译期之外再加一条结构性约束。
    ///
    /// 判定方式: 扫描 `src/` 下所有 .rs 文件, 出现 `Command::new(` 的文件
    /// 只能有 `os_shim.rs` 一个。注释里的示例代码会被剔除后再判定。
    #[test]
    fn no_bare_command_spawn_outside_os_shim() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();

        let mut stack = vec![src_dir.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                // 本文件自己当然允许
                if name == "os_shim.rs" {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                // 只检查**产品代码**。`#[cfg(test)]` 里的临时诊断代码
                // (例如 jni_guard 调 llvm-nm) 不会进产物, 也就不会
                // 弹出黑窗口, 不该被这条守卫波及。
                let mut in_test_mod = false;
                let mut test_mod_indent = 0usize;
                for (idx, line) in content.lines().enumerate() {
                    let t = line.trim_start();
                    let indent = line.len() - t.len();
                    if in_test_mod {
                        // 缩进回退到声明处即离开该 mod
                        if !t.is_empty() && indent <= test_mod_indent && t.starts_with('}') {
                            in_test_mod = false;
                        }
                        continue;
                    }
                    if t.starts_with("#[cfg(test)]") {
                        // 下一行通常是 `mod tests {`
                        in_test_mod = true;
                        test_mod_indent = indent;
                        continue;
                    }
                    // 剔除注释行(本文件里就有 `Command::new("reg")` 的说明性注释)
                    if t.starts_with("//") || t.starts_with("*") || t.starts_with("/*") {
                        continue;
                    }
                    if line.contains("Command::new(") {
                        offenders.push(format!(
                            "{}:{}: {}",
                            path.strip_prefix(&src_dir)
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|_| name.to_string()),
                            idx + 1,
                            t
                        ));
                    }
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "以下位置绕过了 os_shim 的隐藏控制台封装, 会导致'点击设置页弹出黑窗口':\n  {}",
            offenders.join("\n  ")
        );
    }

    /// 防回归: `os_shim` 必须真的带了 `CREATE_NO_WINDOW`。
    /// 常量值写错(例如被改成 DETACHED_PROCESS)同样会漏出控制台。
    #[cfg(target_os = "windows")]
    #[test]
    fn create_no_window_flag_value_is_correct() {
        // CREATE_NO_WINDOW = 0x0800_0000, 来自 WinBase.h。
        // 常见误用是写成 0x08000000(= DETACHED_PROCESS) —— 数值只差一位,
        // 但 DETACHED_PROCESS 会让子进程彻底脱离控制台, 对 explorer 这类
        // GUI 程序反而不安全。
        assert_eq!(super::CREATE_NO_WINDOW, 0x0800_0000);
    }
}
