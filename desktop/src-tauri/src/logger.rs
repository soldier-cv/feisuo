use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload::Handle;
use tracing_subscriber::EnvFilter;
use feisuo_core::AppConfig;

/// 日志写入器的全部可变状态。
///
/// **必须用同一把锁保护文件句柄与大小计数。**
/// 旧实现拆成 `current_file` / `current_size` 两把锁, 且加锁顺序相反:
/// - `rotate_if_needed`: 先 size 再 file
/// - `write_data`:        先 file 再 size
///
/// 这构成经典的锁序反转。两个锁都是**阻塞式** `std::sync::Mutex`,
/// 一旦 A 线程持 size 等 file、B 线程持 file 等 size, 就是永久死锁,
/// 没有任何超时能救。tracing 是多线程的(accept 循环 / 进度事件 / 发现广播
/// 都会打日志), 长跑 + 日志超过上限触发轮转时必然撞上。
struct WriterState {
    file: Option<File>,
    size: u64,
}

struct RollingFileWriterInner {
    log_dir: PathBuf,
    file_name: String,
    max_bytes: u64,
    max_backups: usize,
    state: Mutex<WriterState>,
}

impl RollingFileWriterInner {
    /// 在已持有 state 锁的前提下做归档递移。
    fn rotate_locked(&self, state: &mut WriterState) {
        state.file = None; // 关闭当前句柄

        let base = self.log_dir.join(&self.file_name);
        // 归档递移: .2 -> .3, .1 -> .2
        for i in (1..self.max_backups).rev() {
            let from = self.log_dir.join(format!("{}.{}", self.file_name, i));
            let to = self.log_dir.join(format!("{}.{}", self.file_name, i + 1));
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }

        let first_backup = self.log_dir.join(format!("{}.1", self.file_name));
        let _ = fs::rename(&base, &first_backup);

        // 重新创建主日志文件
        state.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&base)
            .ok();
        state.size = 0;
    }

    fn write_data(&self, buf: &[u8]) -> std::io::Result<usize> {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.size + buf.len() as u64 > self.max_bytes {
            self.rotate_locked(&mut state);
        }
        let written = match state.file.as_mut() {
            Some(f) => f.write(buf)?,
            // 日志文件不可写时不能让 tracing 的整个调用链 panic
            None => buf.len(),
        };
        state.size += written as u64;
        Ok(written)
    }

    fn flush_data(&self) -> std::io::Result<()> {
        if let Ok(mut state) = self.state.lock() {
            if let Some(ref mut f) = state.file {
                let _ = f.flush();
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct RollingFileWriter {
    inner: Arc<RollingFileWriterInner>,
}

impl RollingFileWriter {
    pub fn new(log_dir: PathBuf, file_name: &str, max_bytes: u64, max_backups: usize) -> Self {
        let _ = fs::create_dir_all(&log_dir);
        let log_path = log_dir.join(file_name);
        let existing_size = fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();

        Self {
            inner: Arc::new(RollingFileWriterInner {
                log_dir,
                file_name: file_name.to_string(),
                max_bytes,
                max_backups,
                state: Mutex::new(WriterState {
                    file,
                    size: existing_size,
                }),
            }),
        }
    }
}

impl Write for RollingFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write_data(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush_data()
    }
}

/// 用于在运行期动态调整日志级别的 reload 句柄。
/// 旧实现只在启动时读取一次 log_level, 用户在设置里改成 DEBUG
/// 必须重启进程才生效, 排障体验很差。
static ENV_FILTER: OnceLock<Option<Handle<EnvFilter, tracing_subscriber::Registry>>> = OnceLock::new();

/// 依据日志级别构造全局过滤器 (level 之外全部关闭)
pub fn build_env_filter(level_name: &str) -> EnvFilter {
    let level = match level_name.to_uppercase().as_str() {
        "DEBUG" | "TRACE" => "debug",
        "WARN" => "warn",
        "ERROR" => "error",
        _ => "info",
    };
    EnvFilter::new(format!(
        "warn,feisuo_core={0},feisuo_desktop={0},feisuo-desktop={0}",
        level
    ))
}

pub fn init_logger(config: &AppConfig) {
    init_logger_in(&AppConfig::get_app_dir(), config)
}

/// 在指定数据目录下初始化日志。
/// 日志目录必须跟着引擎的实际数据目录走, 否则沙箱平台上会出现
/// "日志写到了 A 目录, 配置在 B 目录" 这种没法排查的分裂状态。
pub fn init_logger_in(app_dir: &std::path::Path, config: &AppConfig) {
    let log_dir = app_dir.join("logs");
    let max_bytes = (config.max_log_size_mb as u64).max(1) * 1024 * 1024;
    let writer = RollingFileWriter::new(log_dir, "feisuo.log", max_bytes, 3);

    // 可热更新的级别过滤器
    let (filter_layer, handle) =
        tracing_subscriber::reload::Layer::new(build_env_filter(&config.log_level));
    let _ = ENV_FILTER.set(Some(handle));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(move || writer.clone());

    let subscriber = tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer);

    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// 运行期切换日志级别 (立即生效, 无需重启)
pub fn set_level(level_name: &str) {
    if let Some(Some(handle)) = ENV_FILTER.get() {
        let new_filter = build_env_filter(level_name);
        if let Err(e) = handle.modify(|filter| *filter = new_filter) {
            tracing::debug!("热更新日志级别失败: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试: 并发写入 + 高频轮转不得死锁。
    ///
    /// 旧实现把文件句柄与大小计数拆成两把锁, 且加锁顺序相反
    /// (`rotate_if_needed` 是 size→file, `write_data` 是 file→size),
    /// 构成锁序反转。两个都是阻塞式 `Mutex`, 撞上就是永久死锁,
    /// 没有任何超时能救。tracing 是多线程的, 日志超过上限后必然撞上。
    ///
    /// 这里把上限压到极小(每次写都触发轮转)并用多线程猛打,
    /// 旧实现在这个用例下会稳定卡死。
    #[test]
    fn concurrent_writes_with_aggressive_rotation_do_not_deadlock() {
        let dir = std::env::temp_dir().join(format!(
            "feisuo-log-race-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        let _ = fs::remove_dir_all(&dir);

        // max_bytes = 1: 任何一次写入都会触发轮转, 把死锁窗口拉到最大
        let writer = RollingFileWriter::new(dir.clone(), "t.log", 1, 3);

        let mut handles = Vec::new();
        for t in 0..8 {
            // RollingFileWriter 是 Clone(内部 Arc), 每个线程拿一份句柄,
            // 但共享同一份 WriterState 锁 —— 这正是要压测的并发路径。
            let mut w = writer.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..200 {
                    let line = format!("thread {} line {}\n", t, i);
                    w.write_all(line.as_bytes()).expect("写入不得失败");
                }
            }));
        }

        // 给死锁留出明确的时间窗: 全部 join 若超过 20s 即视为死锁。
        // (std::thread 无 join_timeout, 所以用通道 + recv_timeout 判定)
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for h in handles {
                let _ = h.join();
            }
            let _ = tx.send(());
        });

        let finished = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .is_ok();
        assert!(
            finished,
            "并发写入 + 高频轮转发生死锁: 20 秒内未全部完成, 说明存在锁序反转"
        );

        // 轮转后主日志文件必须仍然存在且非空(句柄被正确重建)
        let main = dir.join("t.log");
        assert!(main.exists(), "轮转后主日志文件必须存在");
        assert!(
            fs::metadata(&main).unwrap().len() > 0,
            "轮转后主日志文件不应为空"
        );
        // 归档数量不得超过上限
        let backups = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with("t.log.") && n != "t.log.1"
            })
            .count();
        assert!(backups <= 2, "归档数量应受 max_backups 限制, 实际: {}", backups);

        let _ = fs::remove_dir_all(&dir);
    }

    fn uuid_like() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
