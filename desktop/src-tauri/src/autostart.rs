use tracing::info;

use crate::os_shim::run_hidden;

const RUN_KEY: &str = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "Feisuo";

/// 缓存上次读取结果。读取注册表要拉起 `reg.exe` 进程,
/// 旧实现每次打开"系统设置"页都会重新拉一次, 既慢又会闪控制台。
///
/// **时间戳与结果必须放在同一把锁里。**
/// 旧实现用两个独立 `AtomicI64` / `AtomicBool`, 读的时候先读时间戳、
/// 命中缓存再读结果 —— 两个原子量之间没有任何一致性保证:
/// 写侧是先 `CACHED_AT.store(now)` 再 `CACHED_VALUE.store(result)`,
/// 于是并发的另一个线程可能读到**新鲜的时间戳 + 过期的结果**,
/// 表现为"刚打开自启开关, 界面却显示关闭"并持续 5 秒。
static CACHE: std::sync::Mutex<Option<(i64, bool)>> = std::sync::Mutex::new(None);
const CACHE_TTL_SECS: i64 = 5;

pub struct AutostartManager;

impl AutostartManager {
    /// Configures Windows registry Run key for current user
    pub fn set_autostart(enabled: bool) -> Result<(), String> {
        #[cfg(target_os = "windows")]
        {
            // 关闭时先确认是否真的存在。
            // `reg delete` 在值不存在时返回退出码 1 并在 stderr 打印
            // "The system was unable to find the specified registry key or value",
            // 旧实现直接把它当失败抛给前端 —— 于是"本来就是关的, 再点一次关闭"
            // 会弹出一个莫名其妙的红色报错。判据要基于 reg query 的退出码,
            // 不能去匹配 stderr 文案(那随系统语言变化)。
            if !enabled && !Self::query_registry_value() {
                Self::invalidate_cache();
                return Ok(());
            }

            let current_exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let exe_path = current_exe.to_str().ok_or("Invalid exe path")?;
            let cmd_val = format!("\"{}\" --daemon", exe_path);

            let status = if enabled {
                run_hidden(
                    "reg",
                    &["add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &cmd_val, "/f"],
                )?
            } else {
                run_hidden("reg", &["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])?
            };

            if !status.status.success() {
                return Err(format!(
                    "写入开机自启注册表项失败: {}",
                    String::from_utf8_lossy(&status.stderr).trim()
                ));
            }

            info!(
                "Windows autostart {} with entry: {}",
                if enabled { "enabled" } else { "disabled" },
                if enabled { cmd_val.as_str() } else { "<removed>" }
            );
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = enabled;
        }

        Self::invalidate_cache();
        Ok(())
    }

    /// 直接查注册表, 不走缓存。值不存在时返回 false。
    #[cfg(target_os = "windows")]
    fn query_registry_value() -> bool {
        run_hidden("reg", &["query", RUN_KEY, "/v", VALUE_NAME])
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    pub fn is_autostart_enabled() -> bool {
        #[cfg(target_os = "windows")]
        {
            let now = chrono::Utc::now().timestamp();
            {
                let guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((at, value)) = *guard {
                    if now - at < CACHE_TTL_SECS {
                        return value;
                    }
                }
            }

            let result = Self::query_registry_value();

            let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some((now, result));
            return result;
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    fn invalidate_cache() {
        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}
