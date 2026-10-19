//! 应用内自动更新。
//!
//! 对标 QuickClip 的 `UpdateService`, 但**产物形态不同**:
//! QuickClip 发的是 Inno Setup 安装包, 靠 `/SILENT /CLOSEAPPLICATIONS` 覆盖;
//! 飞梭是绿色版单文件 exe, 所以更新是"下载新 exe → 退出 → 换掉旧 exe → 重启"。
//!
//! ## 为什么坚持自己写而不用 `tauri-plugin-updater`
//!
//! 官方插件要求 Release 里放 `latest.json` + `.sig` 签名文件,
//! 且校验依赖打包期注入的 `pubkey`。而飞梭当前是**无签名绿色版**
//! (`bundle.active = false`), 强行接插件会引入一套签名密钥管理体系,
//! 与"零配置、免安装"的定位相悖。
//!
//! ## 渠道优先级: Gitee 优先, GitHub 降级
//!
//! 与 QuickClip 一致 —— 国内直连 Gitee 毫秒级, GitHub 常常超时。
//! 三级降级: Gitee API → GitHub API → GitHub Releases 页面(跟随 302 读 tag)。
//!
//! ## 「自动检查更新」= 检查 **并下载**
//!
//! 后台调度与手动点击走的是同一条 [`UpdateService::check_and_download`]:
//! 发现新版本就**静默下载到本地**, 下载完成后界面进入 `ready`, 用户只需
//! 点一下「立即更新」。设置页的开关文案因此写成「自动检查并下载更新」——
//! 否则用户会以为关掉下载能省流量, 实际上并没有那个选项。
//!
//! ## 永远不卡死的两条硬约束
//!
//! 1. **每一次检查都有总预算** ([`CHECK_BUDGET`]) —— 单通道超时只是逐段兜底,
//!    三级通道串起来最坏仍有几十秒, 再叠上 DNS/代理的异常停顿就成了
//!    "按钮永远显示进行中"。总预算保证无论如何都会落到一个终态。
//! 2. **下载请求绝不能带总超时** —— 见 [`DOWNLOAD_HEADERS_TIMEOUT`] 的注释。
//!    这一条曾经是错的, 表现为"新版本明明存在, 自动下载却永远失败"。
//!
//! ## 安全约束
//!
//! 下载地址来自远端 JSON 响应, 因此 [`is_trusted_download_url`] 是硬闸门:
//! 只放行 `*.gitee.com` / `*.github.com` / `*.githubusercontent.com`。
//! 少了这一条, 一个被劫持的 API 响应就能让客户端执行任意 exe。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};

use crate::os_shim::open_url_in_browser;

const REPO_OWNER: &str = "soldier-cv";
const REPO_NAME: &str = "feisuo";

const GITEE_LATEST_API: &str = "https://gitee.com/api/v5/repos/huaxudong/feisuo/releases/latest";
const GITHUB_LATEST_API: &str = "https://api.github.com/repos/soldier-cv/feisuo/releases/latest";
const GITHUB_LATEST_PAGE: &str = "https://github.com/soldier-cv/feisuo/releases/latest";
const GITEE_RELEASES_PAGE: &str = "https://gitee.com/huaxudong/feisuo/releases";

/// 绿色版 exe 的产物名。release.yml 正是按这个名字上传的。
const ASSET_NAME: &str = "Feisuo-win-x64.exe";
/// 安装版 Setup 的产物名。release.yml 正是按这个名字上传的。
pub const SETUP_ASSET_NAME: &str = "Feisuo-Setup-x64.exe";

/// 安装版标记文件名（由 Inno Setup 安装包释放到程序所在目录）。
pub const INSTALLED_MARKER: &str = "Feisuo.installed";

/// 判断当前是否为 Inno Setup 规范安装版。
pub fn is_installed_copy() -> bool {
    #[cfg(target_os = "windows")]
    {
        if let Ok(current_exe) = std::env::current_exe() {
            if let Some(parent) = current_exe.parent() {
                return parent.join(INSTALLED_MARKER).exists();
            }
        }
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// 换版时旧 exe 的临时文件名。Windows 上正在运行的 exe 无法删除,
/// 只能先改名 —— 改完名旧映像仍被占用, 于是新文件用**另一个名字**落地,
/// 由下一次启动做"换名接力"(见 [`UpdateService::apply_pending`])。
const ROLLOVER_SUFFIX: &str = ".new";

/// 更新包体积上限。飞梭绿色版约 22 MB, 给到 200 MB 足够宽松,
/// 同时能挡住"Content-Length 谎报超大体积把磁盘写满"。
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;

const API_TIMEOUT: Duration = Duration::from_secs(6);
/// 镜像落后复核用的 GitHub 探测超时（见 `check_for_update`）。
///
/// 比 [`API_TIMEOUT`] 短，因为这次请求**只用来证伪**"已经最新"这个结论，
/// 拿不到结果就维持原结论。它每天最多发生一次（用户已是最新时），
/// 3 秒不会让人等不下去；反过来，若这里用 6 秒，国内用户每次点
/// 「检查更新」都要先白等 6 秒才知道自己没更新。
const STALE_PROBE_TIMEOUT: Duration = Duration::from_millis(3000);
/// **整轮检查**的总预算（三级通道全部奏效前的硬上界）。
///
/// 逐段超时（6s + 3s + 6s + 6s）已经覆盖了正常网络，但 DNS 挂起、
/// 系统代理黑洞、TLS 握手卡死这类停顿没有上层兜底时，界面会永远停在
/// "正在检查更新…" —— 用户看到的不是"网络慢"，而是"按钮卡住了"。
/// 超预算后按检查失败处理：**必然有一个终态**，用户可以再点一次或
/// 走「前往下载页」。
const CHECK_BUDGET: Duration = Duration::from_secs(25);

/// 下载请求**等待响应头**的上限（TCP 已连上、服务器迟迟不给响应头）。
///
/// ## 这里曾经有一个把自动下载整体废掉的 bug
///
/// 早先这一行的位置挂的是 `.timeout(API_TIMEOUT)`（6~8 秒）。reqwest 的
/// 请求超时是**总超时**，覆盖"从连接开始到响应体读完"（见 `TotalTimeoutBody`），
/// 于是 22 MB 的绿色版 exe 需要 **3.7 MB/s** 的持续速度才能下完——
/// 慢速网络下换算出来就是"每次都在半途被掐断"（安装版 6.9 MB 也需 1.1 MB/s）。
/// 用户看到的是"发现新版本，但下载失败"，重启再试还是一样。
///
/// 正确的分工:
/// - **响应头**用本常量（超时说明对面不可用，早失败早降级）;
/// - **响应体**用 [`DOWNLOAD_IDLE_TIMEOUT`] 逐块兜底 —— 只看"有没有进展"，
///   纵容慢速连接，但不放过真正的停滞。
const DOWNLOAD_HEADERS_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(250);

const INITIAL_DELAY: Duration = Duration::from_secs(8);
const NORMAL_INTERVAL: Duration = Duration::from_secs(12 * 3600);
const RETRY_INTERVAL: Duration = Duration::from_secs(180);
const MAX_RETRY_ATTEMPTS: u32 = 7;

/// 前端订阅的更新状态事件。
const UPDATE_EVENT: &str = "feisuo://update-status";

/// 当前程序版本 (取自编译期 `CARGO_PKG_VERSION`, 与 workspace 版本号同源)。
pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 更新流程阶段。拆成"检查"与"下载"两段, 避免界面长时间停在"正在检查"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdatePhase {
    Idle,
    Checking,
    Downloading,
    /// 已下载就绪, 等待用户点「立即更新」
    Ready,
    /// 已是最新
    UpToDate,
    Failed,
}

impl UpdatePhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            UpdatePhase::Idle => "idle",
            UpdatePhase::Checking => "checking",
            UpdatePhase::Downloading => "downloading",
            UpdatePhase::Ready => "ready",
            UpdatePhase::UpToDate => "uptodate",
            UpdatePhase::Failed => "failed",
        }
    }
}

/// 更新状态快照, 直接作为 IPC 返回值与事件负载。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub phase: UpdatePhase,
    pub message: String,
    pub current_version: String,
    /// 最新可用版本; 未知时为空串
    pub latest_version: String,
    pub tag_name: String,
    /// 已下载包的本地路径; 未就绪时为空串
    pub pending_path: String,
    pub bytes_received: u64,
    pub bytes_total: u64,
    /// 手动下载页 (兜底用)
    pub release_page: String,
    /// 是否处于「检查或下载中」—— 前端据此禁用按钮
    pub busy: bool,
    /// 是否为标准安装版（true: 安装版; false: 绿色便携版）
    pub is_installed: bool,
    /// 单调递增的发布序号。
    ///
    /// ## 为什么必须有它
    ///
    /// 状态有**两条通道**送到前端: `feisuo://update-status` 事件, 以及
    /// `check_for_update` 命令的返回值。二者到达顺序**没有保证** ——
    /// 命令返回值是 click 那一刻的快照, 可能比先到的事件更旧。
    /// 早先前端无条件 `updateStatus.value = 返回值`, 于是一个迟到的
    /// "正在检查更新…（busy=true）"快照可以覆盖掉已经到达的终态,
    /// 界面就永久停在"进行中…" —— 后端早就没事了, 是界面卡住了。
    ///
    /// 前端只接受 `seq` 不小于当前值的快照, 这类覆盖被结构性排除。
    pub seq: u64,
    /// 最近一次**检查跑完**的时刻 (Unix 毫秒; 0 = 从未检查)。
    ///
    /// 与 `seq` 不同, 这是一个**粘性**字段: 下载进度、就绪状态等
    /// 后续快照都会带着它, 否则"上次检查时间"会在下载开始时被抹掉。
    pub last_checked_at_ms: i64,
}

impl UpdateStatus {
    fn idle() -> Self {
        Self {
            phase: UpdatePhase::Idle,
            message: String::new(),
            current_version: current_version(),
            latest_version: String::new(),
            tag_name: String::new(),
            pending_path: String::new(),
            bytes_received: 0,
            bytes_total: 0,
            release_page: GITEE_RELEASES_PAGE.to_string(),
            busy: false,
            is_installed: is_installed_copy(),
            seq: 0,
            last_checked_at_ms: 0,
        }
    }
}

/// 一次远端发布的信息。
#[derive(Debug, Clone)]
struct ReleaseInfo {
    version: String,
    tag_name: String,
    download_url: String,
    asset_size: u64,
}

/// 已下载、等待安装的更新。
#[derive(Debug, Clone)]
struct PendingUpdate {
    version: String,
    tag_name: String,
    local_path: PathBuf,
}

/// GitHub / Gitee 统一的 release 响应形状 (两者字段名一致)。
#[derive(Debug, Deserialize)]
struct ReleaseDto {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    assets: Vec<AssetDto>,
}

#[derive(Debug, Deserialize)]
struct AssetDto {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// 全局单例更新服务。
///
/// 用 `tokio::sync::Mutex` 而不是 `std::sync::Mutex`:
/// 整个检查+下载流程都在锁内 await, 用 std 锁会跨线程阻塞并可能死锁。
pub struct UpdateService {
    updates_dir: PathBuf,
    client: reqwest::Client,
    /// 持有 handle 是为了让 `publish` 在任何地方都能把状态推给前端,
    /// 而不是让每一处调用方都得把 `&AppHandle` 一路传下来 ——
    /// 漏传一次的结果是"状态变了但界面不动", 极难排查。
    app: AppHandle,
    /// 引擎里的同一份配置 (用来持久化"上次检查时间")。
    ///
    /// 与 `commands::update_app_config` 共用同一个 `RwLock`, 因此设置页
    /// 刚改完的配置不会在下次检查时被旧内存副本覆盖写回。
    config: Arc<RwLock<feisuo_core::AppConfig>>,
    /// 串行化"检查 + 下载", 防止用户连点与后台调度撞在一起
    gate: Mutex<()>,
    status: Mutex<UpdateStatus>,
    /// 已下载待安装的更新
    pending: Mutex<Option<PendingUpdate>>,
    /// 连续失败计数, 达到 MAX_RETRY_ATTEMPTS 后熔断到下一个常规周期
    failures: Mutex<u32>,
    /// 状态发布序号（见 [`UpdateStatus::seq`]）。`publish` 是唯一的
    /// 递增点, 用原子量而不是再抢一把锁。
    seq: AtomicU64,
    /// 叫醒后台调度循环（开关变化时用）。
    ///
    /// 没有它的话，用户关掉「自动检查并下载更新」之后，正在长睡的
    /// 调度循环最多还要睡满 12 小时才读到新配置 —— 而界面上的开关
    /// **已经翻过去了**，看起来像"关了没用"。
    wake: tokio::sync::Notify,
}

impl UpdateService {
    pub fn new(app: AppHandle, config: Arc<RwLock<feisuo_core::AppConfig>>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!("Feisuo/{} (+https://github.com/{REPO_OWNER}/{REPO_NAME})", current_version()))
            .connect_timeout(API_TIMEOUT)
            // 不设全局 timeout: 大 exe 下载可能很久, 由 DOWNLOAD_IDLE_TIMEOUT
            // 逐次读超时兜底 —— 全局超时会误杀正在正常下载的慢速连接。
            //
            // 重定向必须**跟着走但每一跳都查**: Gitee 的下载地址连跳两次
            // （gitee.com → gitee.com/attach_files → foruda.gitee.com），
            // 而 `is_trusted_download_url` 只查了**初始** URL ——
            // 一个被劫持的 API 响应完全可以返回 gitee.com 的地址、
            // 再用 302 把客户端引到任意主机去下 exe。白名单在这里等于形同虚设。
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    return attempt.error("重定向次数过多");
                }
                // 每一跳的**目标**都必须在白名单内
                if is_trusted_download_url(attempt.url().as_str()) {
                    attempt.follow()
                } else {
                    attempt.error("重定向目标不在受信任域名内")
                }
            }))
            .build()
            .unwrap_or_else(|e| {
                // 这里几乎不可能失败（策略是纯配置），但万一 TLS 后端初始化失败，
                // 兜底的 `Client::new()` 会**丢掉上面的逐跳白名单** —— 至少让它可见，
                // 而不是静默降级成一个不校验重定向的客户端。
                tracing::warn!("构建更新 HTTP 客户端失败 ({e})，已退回默认客户端：重定向不再受域名白名单约束");
                reqwest::Client::new()
            });

        let updates_dir = feisuo_core::AppConfig::get_app_dir().join("updates");

        // 恢复持久化的「上次检查时间」: 托盘常驻下用户可能几周不看设置页,
        // 启动后到首次检查之间显示"尚未检查"是错的。
        let mut initial = UpdateStatus::idle();
        initial.last_checked_at_ms = config
            .try_read()
            .ok()
            .and_then(|c| c.last_update_check_utc.clone())
            .and_then(|s| parse_rfc3339_millis(&s))
            .unwrap_or(0);

        Self {
            updates_dir,
            client,
            app,
            config,
            gate: Mutex::new(()),
            status: Mutex::new(initial),
            pending: Mutex::new(None),
            failures: Mutex::new(0),
            seq: AtomicU64::new(0),
            wake: tokio::sync::Notify::new(),
        }
    }

    pub async fn status(&self) -> UpdateStatus {
        self.status.lock().await.clone()
    }

    /// 上次检查的失败次数, 供界面显示"已重试 N 次"。
    pub async fn consecutive_failures(&self) -> u32 {
        *self.failures.lock().await
    }

    /// 扫描更新目录, 恢复上次已下载但未安装的更新。
    ///
    /// 必须在启动时调用: 用户点「立即更新」后如果安装失败(比如被占用),
    /// 更新包还在磁盘上, 不恢复的话界面上会退回"无更新可用"。
    pub async fn restore_pending(&self) {
        if let Some(found) = self.scan_pending_dir().await {
            // 不在 `pending` 锁内 await `publish`：那把锁会一直握到整个
            // if 块结束，而 publish 内部虽然不碰 pending，却会因为
            // 持锁跨 await 让并发的 `apply_pending` 白等。
            let mut p = self.pending.lock().await;
            let already = p.is_some();
            if !already {
                *p = Some(found.clone());
            }
            drop(p);
            if !already {
                tracing::info!("恢复已下载的更新: {}", found.local_path.display());
                self.publish(UpdateStatus {
                    phase: UpdatePhase::Ready,
                    message: format!("新版本 {} 已下载就绪", found.tag_name),
                    latest_version: found.version.clone(),
                    tag_name: found.tag_name.clone(),
                    pending_path: found.local_path.to_string_lossy().to_string(),
                    release_page: GITEE_RELEASES_PAGE.to_string(),
                    ..UpdateStatus::idle()
                })
                .await;
            }
        }
    }

    /// 手动检查(用户点了「检查更新」)。
    ///
    /// `interactive` 只影响**失败时是否主动弹窗**; 后台静默调度传 false,
    /// 免得一天弹 7 次错误。
    pub async fn check_and_download(&self, interactive: bool) -> UpdateStatus {
        let _guard = match self.gate.try_lock() {
            Ok(g) => g,
            Err(_) => {
                tracing::info!("更新检查或下载已在进行中，复用当前任务");
                return self.status().await;
            }
        };

        // 已经有就绪的更新包, 且确实比当前版本新 —— 不必重新走网络。
        //
        // ⚠️ 必须先把快照取到变量再判。写成 `if let Some(p) = self.pending.lock().await.clone()`
        // 时，那个 `MutexGuard` 是临时量，存活到**整个 if let 块**结束 ——
        // 块里还有 `publish(...).await`，于是持着 `pending` 锁跨 await，
        // 并发的 `apply_pending`（用户点「立即更新」）会白等这把锁。
        let ready_pending = self.pending.lock().await.clone();
        if let Some(p) = ready_pending {
            if p.local_path.exists() && is_newer(&p.version, &current_version()) {
                let st = self
                    .publish(UpdateStatus {
                        phase: UpdatePhase::Ready,
                        message: format!("新版本 {} 已下载就绪", p.tag_name),
                        latest_version: p.version.clone(),
                        tag_name: p.tag_name.clone(),
                        pending_path: p.local_path.to_string_lossy().to_string(),
                        busy: false,
                        ..UpdateStatus::idle()
                    })
                    .await;
                return st;
            }
        }

        self.publish_from(|cur| UpdateStatus {
            phase: UpdatePhase::Checking,
            message: "正在检查更新…".into(),
            busy: true,
            ..cur.clone()
        })
        .await;

        // 整轮检查必须在预算内结束 —— 见 `CHECK_BUDGET`。
        // 逐段超时管不了"某一段本身卡住"（DNS 挂起 / 代理黑洞 / TLS 假死），
        // 那种情况下按钮会永远显示"进行中…"，用户能做的只有重启程序。
        let check = match tokio::time::timeout(CHECK_BUDGET, self.check_for_update()).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!("检查更新超过 {} 秒预算，强制以失败收尾", CHECK_BUDGET.as_secs());
                Err(format!(
                    "检查更新超时（{} 秒内未完成）。可稍后重试，或点「前往下载页」手动下载",
                    CHECK_BUDGET.as_secs()
                ))
            }
        };

        // 无论成败都记下"检查跑完的时刻": 界面上的「上次检查」描述的是
        // **尝试过**这件事, 而不是"成功过"—— 后者会让断网用户看到的时间
        // 永远停在很久以前, 反而像是检查功能坏了。
        // 必须在下方的终态 publish 之前: 终态快照带着这个粘性字段一起推给前端。
        self.record_check_time().await;

        let check = match check {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("检查更新失败: {e}");
                self.bump_failure().await;
                let st = self
                    .publish_from(|cur| UpdateStatus {
                        phase: UpdatePhase::Failed,
                        message: e,
                        busy: false,
                        ..cur.clone()
                    })
                    .await;
                if interactive {
                    self.notify("检查更新失败");
                }
                return st;
            }
        };

        match check {
            None => {
                self.reset_failures().await;
                self.publish_from(|cur| UpdateStatus {
                    phase: UpdatePhase::UpToDate,
                    message: format!("当前已是最新版本 v{}", current_version()),
                    busy: false,
                    ..cur.clone()
                })
                .await
            }
            Some(release) => {
                let tag = release.tag_name.clone();
                let version = release.version.clone();
                self.publish_from(|cur| UpdateStatus {
                    phase: UpdatePhase::Downloading,
                    message: format!("发现新版本 {tag}，正在下载"),
                    latest_version: version,
                    tag_name: tag.clone(),
                    busy: true,
                    ..cur.clone()
                })
                .await;

                match self.download(&release).await {
                    Ok(path) => {
                        self.reset_failures().await;
                        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        let mut p = self.pending.lock().await;
                        *p = Some(PendingUpdate {
                            version: release.version.clone(),
                            tag_name: release.tag_name.clone(),
                            local_path: path.clone(),
                        });
                        drop(p);
                        self.cleanup_old_updates(&path).await;

                        self.publish_from(|cur| UpdateStatus {
                            phase: UpdatePhase::Ready,
                            message: format!("新版本 {} 已下载就绪", release.tag_name),
                            latest_version: release.version.clone(),
                            tag_name: release.tag_name.clone(),
                            pending_path: path.to_string_lossy().to_string(),
                            bytes_received: size,
                            bytes_total: size,
                            busy: false,
                            ..cur.clone()
                        })
                        .await
                    }
                    Err(e) => {
                        tracing::warn!("下载更新失败: {e}");
                        self.bump_failure().await;
                        self.publish_from(|cur| UpdateStatus {
                            phase: UpdatePhase::Failed,
                            message: format!("发现新版本 {tag}，但下载失败: {e}。可在设置页用浏览器手动下载。"),
                            latest_version: release.version,
                            tag_name: tag,
                            busy: false,
                            ..cur.clone()
                        })
                        .await
                    }
                }
            }
        }
    }

    /// 依次尝试 Gitee → GitHub API → GitHub 页面。
    ///
    /// 返回 `Ok(None)` 表示已是最新; `Err` 表示三条通道全失败。
    ///
    /// ## Gitee 说"已是最新"时**必须**再问一次 GitHub
    ///
    /// Gitee 是**主通道**（国内 GitHub API 常超时），而它可能**静默落后**：
    /// `release.yml` 的 Gitee 同步是 `continue-on-error`，附件上传连
    /// curl 退出码都没检查，失败也不阻断发版。所以"镜像停在旧版"是
    /// 一件会**长期存在**的状态，而不是应该假设不会发生的意外。
    ///
    /// 早先这里 `Ok(v) => return` 直接下结论：Gitee 返回一个旧 tag ⇒
    /// `is_newer` 为 false ⇒ `Ok(None)` ⇒ 告诉用户「当前已是最新版本」，
    /// 而且**再也不去问 GitHub**。用户就这样卡在旧版，界面上看不出任何异常。
    ///
    /// 这与 `finish()` 里"附件缺失不能当成没有新版本"是同一个道理，
    /// 同一个作者在同一份文件的相邻几十行里犯的、方向相反的两个错：
    /// 一个把"拿不到"当"没有"，另一个把"落后"当"最新"。
    ///
    /// 复核走 [`STALE_PROBE_TIMEOUT`] 的短超时，失败就**维持** Gitee 的结论 ——
    /// 拿不到证据不等于有更新，绝不能因为探测失败就凭空提示有新版。
    ///
    /// ## 每一段都要推进界面上的文案
    ///
    /// 三级通道串行跑满时是十几秒的等待。界面停在"正在检查更新…"一动不动，
    /// 用户读到的是"卡住了"；把当前正在问哪条通道说出来，等待就变成
    /// 可解释的过程（也顺便让用户知道为什么要等）。
    async fn check_for_update(&self) -> Result<Option<ReleaseInfo>, String> {
        tracing::info!("开始检查应用更新，当前本地版本: v{}", current_version());
        self.note_stage("正在检查更新…（Gitee 主通道）").await;
        let gitee = self.try_gitee_api().await;
        match &gitee {
            Ok(Some(v)) => {
                tracing::info!("Gitee 通道发现新版本: {} (下载地址: {})", v.tag_name, v.download_url);
                return Ok(Some(v.clone()));
            }
            // Gitee 说已是最新：**先别信**，见本函数文档。
            Ok(None) => {
                tracing::info!("Gitee 响应当前已是最新，启动 GitHub 探测以复核镜像状态...");
                self.note_stage("正在检查更新…（GitHub 复核镜像）").await;
                let probe = self.probe_github_api().await;
                if let Ok(Some(v)) = &probe {
                    tracing::info!("Gitee 镜像落后, 已改用 GitHub 的 v{}", v.tag_name);
                }
                return Self::prefer_probe_over_up_to_date(probe);
            }
            Err(e) => tracing::warn!("Gitee 通道不可用: {e}，降级 GitHub"),
        }

        self.note_stage("正在检查更新…（GitHub 兜底）").await;
        let github = self.try_github_api().await;
        match &github {
            Ok(v) => return Ok(v.clone()),
            Err(e) => tracing::warn!("GitHub API 通道不可用: {e}，降级 Releases 页面"),
        }

        self.note_stage("正在检查更新…（Releases 页面兜底）").await;
        let page = self.try_github_page().await;
        match &page {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(format!("检查更新失败（已尝试 Gitee / GitHub / 页面）: {e}")),
        }
    }

    /// 推送一条"检查进行中"的阶段文案（保持 `busy`, 只换 message）。
    async fn note_stage(&self, message: &str) {
        // 必须走 `publish_from`: 早先这里写成
        // `publish(UpdateStatus { ..self.status.lock().await.clone() })`,
        // 而 `note_stage` 是**检查更新的第一个动作** —— 于是整条流水线在
        // 打印第一行日志之后、发出第一次网络请求之前就永久挂起，
        // 日志里因此连一条 update 记录都没有。
        self.publish_from(|cur| UpdateStatus {
            phase: UpdatePhase::Checking,
            message: message.to_string(),
            busy: true,
            ..cur.clone()
        })
        .await;
    }

    /// "镜像说已是最新"之后，探测结果怎么用。**纯函数**，便于直接断言。
    ///
    /// 单独抽出来的理由不是可读性，而是这条规则**曾经是反的**，
    /// 而它原先埋在 async 网络代码里 —— 没有任何一处能对它下断言，
    /// 于是"镜像落后 ⇒ 永远提示已是最新"就这样发出去过。
    ///
    /// 规则只有一条，而且两个方向都不能搞错：
    /// - 探测到更新 ⇒ **用**它（镜像在骗人，用户必须能更新）
    /// - 探测失败 ⇒ **维持**已是最新（拿不到证据不等于有更新；
    ///   反过来处理会让断网时凭空提示有新版，比不检查更糟）
    fn prefer_probe_over_up_to_date(
        probe: Result<Option<ReleaseInfo>, String>,
    ) -> Result<Option<ReleaseInfo>, String> {
        match probe {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!("GitHub 落后复核失败, 维持镜像结论: {e}");
                Ok(None)
            }
        }
    }

    /// 三条通道共用的收尾: 比版本 + 挑附件。
    fn finish(&self, dto: &ReleaseDto, source: &str) -> Result<Option<ReleaseInfo>, String> {
        let tag = dto.tag_name.trim();
        if tag.is_empty() {
            return Err(format!("{source} 响应里没有 tag_name"));
        }
        let version = tag.trim_start_matches(['v', 'V']).to_string();

        if !is_newer(&version, &current_version()) {
            tracing::info!("检查更新({source}): 最新 {tag}，当前 v{}，无需更新", current_version());
            return Ok(None);
        }
        tracing::info!("检查更新({source}): 发现新版本 {tag}，开始挑选适合当前客户端形态的安装附件",);

        // 附件缺失**不能**当成"没有新版本": 否则用户永远停在旧版本却毫无提示。
        let Some(asset) = pick_asset(&dto.assets) else {
            let target_name = if is_installed_copy() { SETUP_ASSET_NAME } else { ASSET_NAME };
            return Err(format!(
                "发现新版本 {tag}，但没有 {target_name} ({ASSET_NAME}) 附件。可手动访问 {GITEE_RELEASES_PAGE}"
            ));
        };

        if !is_trusted_download_url(&asset.browser_download_url) {
            return Err(format!("新版本下载地址不受信任，已拒绝下载: {}", asset.browser_download_url));
        }

        Ok(Some(ReleaseInfo {
            version,
            tag_name: tag.to_string(),
            download_url: asset.browser_download_url.clone(),
            asset_size: asset.size,
        }))
    }

    async fn try_gitee_api(&self) -> Result<Option<ReleaseInfo>, String> {
        let resp = self
            .client
            .get(GITEE_LATEST_API)
            .header("Accept", "application/json")
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("Gitee HTTP {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Gitee HTTP {}", status.as_u16()));
        }
        let dto: ReleaseDto = resp.json().await.map_err(|e| format!("Gitee 响应解析失败: {e}"))?;
        self.finish(&dto, "Gitee API")
    }

    async fn try_github_api(&self) -> Result<Option<ReleaseInfo>, String> {
        self.github_api(API_TIMEOUT).await
    }

    /// 镜像落后复核用的 GitHub 探测（见 [`STALE_PROBE_TIMEOUT`]）。
    ///
    /// 与 `try_github_api` 的**唯一**区别是超时更短。刻意不抽成
    /// "返回 ReleaseInfo 或 None"的通用函数：两条通道对结果的处理方式
    /// 不同（一条失败即降级、一条失败即维持原结论），混在一起就得
    /// 用返回值去编码调用方的意图，可读性反而更差。
    async fn probe_github_api(&self) -> Result<Option<ReleaseInfo>, String> {
        self.github_api(STALE_PROBE_TIMEOUT).await
    }

    async fn github_api(&self, timeout: Duration) -> Result<Option<ReleaseInfo>, String> {
        let resp = self
            .client
            .get(GITHUB_LATEST_API)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("GitHub HTTP {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("GitHub HTTP {}", status.as_u16()));
        }
        let dto: ReleaseDto = resp.json().await.map_err(|e| format!("GitHub 响应解析失败: {e}"))?;
        self.finish(&dto, "GitHub API")
    }

    /// 不走 api.github.com: 跟随 `/releases/latest` 的 302, 从落地 URL 里取 tag。
    ///
    /// 这条兜底的价值在于它**只用 CDN 与 HTML**, 完全不碰 API 限流。
    async fn try_github_page(&self) -> Result<Option<ReleaseInfo>, String> {
        let resp = self
            .client
            .get(GITHUB_LATEST_PAGE)
            .header("Accept", "text/html,application/xhtml+xml")
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("GitHub 页面 HTTP {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("GitHub 页面 HTTP {}", status.as_u16()));
        }

        let final_url = resp.url().to_string();
        let (tag, _version) = parse_tag_from_url(&final_url)
            .ok_or_else(|| format!("无法从 {final_url} 解析版本号"))?;

        let dto = ReleaseDto {
            tag_name: tag.clone(),
            assets: vec![AssetDto {
                name: ASSET_NAME.to_string(),
                browser_download_url: format!(
                    "https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/{tag}/{ASSET_NAME}"
                ),
                size: 0,
            }],
        };
        self.finish(&dto, "GitHub 页面")
    }

    /// 流式下载到 `.part`, 完整落盘后再改名 —— 半截文件绝不能被当成可用更新。
    async fn download(&self, release: &ReleaseInfo) -> Result<PathBuf, String> {
        if !is_trusted_download_url(&release.download_url) {
            return Err(format!("下载地址不受信任: {}", release.download_url));
        }
        std::fs::create_dir_all(&self.updates_dir)
            .map_err(|e| format!("创建更新目录失败: {e}"))?;

        let is_setup = is_installed_copy()
            || release.download_url.to_ascii_lowercase().contains("setup");
        let pkg_prefix = if is_setup { "Feisuo-Setup-x64-" } else { "Feisuo-win-x64-" };
        let dest = self.updates_dir.join(format!("{pkg_prefix}{}.exe", release.version));
        let part = dest.with_extension("exe.part");

        if dest.exists() {
            if let Ok(m) = std::fs::metadata(&dest) {
                let local_len = m.len();
                if local_len > 1024 * 1024
                    && (release.asset_size == 0 || local_len == release.asset_size)
                {
                    tracing::info!("复用已下载就绪的更新包: {}", dest.display());
                    return Ok(dest);
                }
            }
        }

        tracing::info!("开始下载更新包: {} -> {}", release.download_url, dest.display());
        // 刻意**不设**总超时: 见 `DOWNLOAD_HEADERS_TIMEOUT` 的注释 ——
        // 请求级超时覆盖整个响应体, 22 MB 的包会被半途掐断。
        // 这里只兜住"TCP 连上但不给响应头"这一段, 响应体交给下面的
        // 逐块读超时（只看有没有进展）。
        let resp = tokio::time::timeout(
            DOWNLOAD_HEADERS_TIMEOUT,
            self.client.get(&release.download_url).send(),
        )
        .await
        .map_err(|_| {
            format!(
                "连接更新服务器超时（{} 秒内没有响应）",
                DOWNLOAD_HEADERS_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("连接失败: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }

        let expected = resp.content_length().unwrap_or(release.asset_size);
        if expected > MAX_DOWNLOAD_BYTES {
            return Err(format!("更新包体积异常 ({expected} 字节)，已拒绝"));
        }

        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(|e| format!("创建临时文件失败: {e}"))?;

        let mut received: u64 = 0;
        let mut resp = resp;
        let mut last_emit = std::time::Instant::now() - PROGRESS_EVENT_INTERVAL;

        loop {
            // 逐次读超时而非全局超时: 国内直连偶尔会停顿十几秒,
            // 45 秒无数据才算真卡死。
            let next = tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, resp.chunk()).await;
            let chunk = match next {
                Err(_) => {
                    let _ = std::fs::remove_file(&part);
                    return Err(format!("下载停滞超过 {} 秒", DOWNLOAD_IDLE_TIMEOUT.as_secs()));
                }
                Ok(Err(e)) => {
                    let _ = std::fs::remove_file(&part);
                    return Err(format!("读取失败: {e}"));
                }
                Ok(Ok(None)) => break,
                Ok(Ok(Some(c))) => c,
            };

            received += chunk.len() as u64;
            if received > MAX_DOWNLOAD_BYTES {
                let _ = std::fs::remove_file(&part);
                return Err("更新包超过体积上限".into());
            }

            if let Err(e) = file.write_all(&chunk).await {
                let _ = std::fs::remove_file(&part);
                return Err(format!("写入失败: {e}"));
            }

            // 进度事件按 250ms 节流: 每块都 emit 会把 webview 消息队列打满,
            // 反而拖慢下载本身。
            if last_emit.elapsed() >= PROGRESS_EVENT_INTERVAL {
                last_emit = std::time::Instant::now();
                self.publish_from(|cur| UpdateStatus {
                    phase: UpdatePhase::Downloading,
                    message: format!(
                        "发现新版本 {}，正在下载 {}",
                        release.tag_name,
                        format_download_progress(received, expected)
                    ),
                    latest_version: release.version.clone(),
                    tag_name: release.tag_name.clone(),
                    bytes_received: received,
                    bytes_total: expected,
                    busy: true,
                    ..cur.clone()
                })
                .await;
            }
        }

        file.flush().await.map_err(|e| format!("落盘失败: {e}"))?;
        drop(file);

        // 体积明显不符时不直接采用: 多半是代理截断或 CDN 返回了错误页。
        if expected > 0 && received < expected {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(format!("下载不完整 ({received}/{expected} 字节)"));
        }

        // Windows 的 rename 不会覆盖已存在文件, 必须先删掉旧的半成品。
        // 走到这里说明 dest 存在但体积不符(见上面的复用判断), 删掉是安全的。
        let _ = tokio::fs::remove_file(&dest).await;
        tokio::fs::rename(&part, &dest)
            .await
            .map_err(|e| format!("重命名更新包失败: {e}"))?;

        tracing::info!("更新已下载: {}", dest.display());
        Ok(dest)
    }

    /// 应用已下载的更新: 换名接力 → 退出 → 下次启动完成替换。
    ///
    /// ## 为什么不能直接覆盖
    ///
    /// Windows 上正在运行的 exe 被内核独占, 既不能删除也不能覆写。
    /// 常规解法是外部 updater 进程, 但飞梭是绿色版单文件 ——
    /// 用户下载下来目录里只有 `feisuo-desktop.exe`, 旁边没有 helper。
    ///
    /// 所以采用**换名接力**:
    /// 1. 把当前 exe 改名为 `xxx.exe.new`(改运行中的 exe 名是允许的);
    /// 2. 把新版本复制到当前 exe 的**原路径**(此刻该路径已空出来);
    /// 3. 拉起原路径的新 exe 后退出;
    /// 4. 新实例启动时发现同目录的 `.new`, 删掉它, 顺便把自己改名为
    ///    正式名 —— 如果第 2 步因权限失败, 第 4 步的旧实例就仍在运行,
    ///    用户不会落到"没有程序"的状态。
    ///
    /// 第 4 步由 [`apply_rollover_if_needed`] 在启动最早期完成。
    pub async fn apply_pending(&self, app: &AppHandle) -> Result<(), String> {
        let pending = self
            .pending
            .lock()
            .await
            .clone()
            .ok_or_else(|| "还没有已下载的更新".to_string())?;

        if !pending.local_path.exists() {
            return Err("更新包已丢失，请重新检查更新".into());
        }

        let current_exe = std::env::current_exe().map_err(|e| format!("无法定位当前程序: {e}"))?;
        if !is_newer(&pending.version, &current_version()) {
            return Err(format!("当前 v{} 已是最新，无需更新", current_version()));
        }

        let is_setup_pkg = pending
            .local_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase().contains("setup"))
            .unwrap_or(false);

        // 安装包静默安装分支：如果更新包是 Setup 安装程序或当前处于安装目录
        if is_setup_pkg || is_installed_copy() {
            tracing::info!("以静默方式启动安装程序进行升级: {}", pending.local_path.display());
            let silent_args = [
                std::ffi::OsString::from("/SILENT"),
                std::ffi::OsString::from("/NORESTART"),
                std::ffi::OsString::from("/CLOSEAPPLICATIONS"),
                std::ffi::OsString::from("/FORCECLOSEAPPLICATIONS"),
                std::ffi::OsString::from("/SUPPRESSMSGBOXES"),
            ];
            // ⚠️ 必须用**提权**启动，不能用 `spawn_detached`。
            //
            // `spawn_detached` 走 `CreateProcess`，它不会触发 UAC：
            // - 目标要求 requireAdministrator ⇒ 直接失败（740）；
            // - 而 Inno Setup（`PrivilegesRequired=admin`）在发现自己不是
            //   管理员时会**静默放弃** —— 实测：非管理员下静默安装跑完，
            //   Program Files 里的 exe 时间戳纹丝不动，连日志都不产生。
            //
            // 用户看到的是"更新成功"，可程序还是旧版，而且升级入口
            // 已经关掉了（exe 随即退出）—— 只能靠下次手动下载。
            //
            // 走 `runas` 才会弹 UAC。用户在 UAC 上点"取消"是**预期行为**，
            // 此时明确告诉他并保留更新包，让他随时能重试。
            let installer_pid =
                match crate::os_shim::spawn_elevated(&pending.local_path, &silent_args) {
                    Ok(pid) => pid,
                    Err(e) => {
                        // 不要退出：退出会让用户既没升级、也没了界面。
                        return Err(format!(
                            "{e}。已保留安装包，可在准备好后点「立即更新」重试，\
                             或到「前往下载页」手动安装：{}",
                            pending.local_path.display()
                        ));
                    }
                };

            // 装完之后必须有人把程序拉起来 —— 否则就是"更新成功，然后屏幕上
            // 什么都没有"。
            //
            // ## 为什么这件事不能交给安装包自己的 [Run] 段
            //
            // Inno 的 `runasoriginaluser` 只有在它能拿到"提权前那个用户令牌"
            // 时才成立（`Setup.SpawnClient.InstExecEx`：拿不到 spawn server 就
            // 直接 `InstExec`）。而这里正是拿不到的情况 —— 飞梭本身是**非提权**
            // 进程，用 `runas` 启动安装包意味着 SetupLdr 一出生就是管理员，
            // 于是 `runasoriginaluser` 静默退化成"以管理员身份启动"。
            //
            // 那样装完的程序会带着管理员令牌常驻：资源管理器（中完整性）往
            // 高完整性窗口拖文件会被 UIPI 拦掉，而拖放正是飞梭的主要入口 ——
            // 用户看到的是"拖上去没反应"，且完全无从判断原因。
            //
            // 所以启动动作留在**本进程这一侧**：中继由非提权进程创建，拉起的
            // 新版本也是非提权，与绿色版换名接力完全同构。
            let relaunch_args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
            match spawn_installer_relay(installer_pid, &current_exe, &relaunch_args) {
                Ok(()) => {
                    tracing::info!("已安排安装完成后拉起新版本: {}", current_exe.display());
                }
                Err(e) => {
                    // 中继起不来时**不能**照样退出：安装包已经把程序杀掉了，
                    // 这一退就是"更新完没有程序"。宁可留在界面上把原因说清楚，
                    // 让用户自己点开始菜单里的飞梭。
                    tracing::error!("安排安装后拉起失败: {e}");
                    return Err(format!(
                        "安装程序已启动，但无法安排安装完成后自动打开飞梭（{e}）。\
                         安装完成后请从开始菜单打开飞梭。"
                    ));
                }
            }
            app.exit(0);
            std::future::pending::<()>().await;
            return Ok(());
        }

        let dir = current_exe
            .parent()
            .ok_or_else(|| "无法确定程序所在目录".to_string())?;
        let staged = current_exe.with_extension(format!("exe{ROLLOVER_SUFFIX}"));
        let backup = current_exe.with_extension(format!("exe{ROLLOVER_SUFFIX}.old"));

        // 上一轮接力残留, 先清干净再开始
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_file(&backup);

        // 1) 复制(不是改名)新版本到 .new —— 更新包在 %LOCALAPPDATA%,
        //    必须复制过去, 否则跨目录移动会失败
        std::fs::copy(&pending.local_path, &staged)
            .map_err(|e| format!("写入新版本失败: {e}（请确认程序目录可写）"))?;

        // 2) 接力: .new -> 正式名
        apply_rollover(&current_exe, &staged, &backup)
            .map_err(|e| format!("替换程序文件失败: {e}（请关闭其他占用该文件的程序后重试）"))?;

        // 3) 拉起新版本 —— 但**绝不能在旧进程还活着时拉**。
        //
        // ## 这里曾经是一个"更新完成后程序打不开"的致命竞态
        //
        // `tauri-plugin-single-instance` 在 Windows 上用一把**按 identifier
        // 命名**的内核互斥体做单实例判定（identifier 是 `net.findfine.feisuo`，
        // 与版本号无关）。`apply_pending` 早先是「先 spawn 新 exe，再
        // `app.exit(0)`」—— 新进程这时候会在插件 setup 里发现互斥体
        // 已被占用，于是走"第二实例"分支：`SendMessage` 给旧窗口、
        // `std::process::exit(0)` —— **新进程立刻自杀**。
        // 紧接着旧进程也退出，屏幕上什么都不剩。
        //
        // 实测（旧顺序）：12 次全部失败，`survived=0 died=12`；
        // 改成"先退出、等它真的消失、再拉起"：10 次全部成功。
        //
        // 但 `app.exit(0)` 之后本函数就不能再执行任何代码了 —— 所以真正
        // 的拉起动作必须交给**本进程之外**的人。这里用 `cmd.exe` 起一条
        // 极小的等待中继：等当前 PID 消失后再启动新 exe。
        //
        // 用 `ping` 而不是 `timeout` 做延时：`timeout` 在没有控制台的
        // 会话里会直接报 "输入重定向不受支持" 而不等待，`ping -n` 恒可用。
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let relaunch_dir = dir.to_path_buf();
        let pid = std::process::id();
        spawn_relaunch_after_exit(&current_exe, &args, pid)?;
        tracing::info!(
            "已安排退出后拉起新版本 (pid={}), 目录: {}",
            pid,
            relaunch_dir.display()
        );

        // 4) 退出旧实例 —— 互斥体随进程退出释放，中继随后拉起新版本。
        app.exit(0);
        // exit() 之后本函数不会返回, 但 tokio 任务调度上仍需给一个值
        std::future::pending::<()>().await;
        Ok(())
    }

    async fn scan_pending_dir(&self) -> Option<PendingUpdate> {
        let mut entries = tokio::fs::read_dir(&self.updates_dir).await.ok()?;
        let mut best: Option<PendingUpdate> = None;

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy().to_string();
            // 命名规则: Feisuo-Setup-x64-<version>.exe 或 Feisuo-win-x64-<version>.exe
            let rest = name
                .strip_prefix("Feisuo-Setup-x64-")
                .or_else(|| name.strip_prefix("Feisuo-win-x64-"))
                .and_then(|s| s.strip_suffix(".exe"));
            let Some(rest) = rest else {
                continue;
            };
            if rest.is_empty() || !is_newer(rest, &current_version()) {
                continue;
            }
            // 1MB 以下不可能是真实安装包或绿色版 exe, 多半是网络截断或错误页
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > 1024 * 1024 => {}
                _ => continue,
            }
            if best
                .as_ref()
                .map(|b| is_newer(rest, &b.version))
                .unwrap_or(true)
            {
                best = Some(PendingUpdate {
                    version: rest.to_string(),
                    tag_name: format!("v{rest}"),
                    local_path: path,
                });
            }
        }
        best
    }

    /// 只保留刚下载好的那一份, 避免更新目录无限膨胀。
    async fn cleanup_old_updates(&self, keep: &Path) {
        if let Ok(mut entries) = tokio::fs::read_dir(&self.updates_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let p = entry.path();
                if p == keep {
                    continue;
                }
                let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                if name.starts_with("Feisuo-win-x64-") || name.starts_with("Feisuo-Setup-x64-") {
                    let _ = tokio::fs::remove_file(p).await;
                }
            }
        }
    }

    /// 把状态写进内存、推给前端, 并**保证两条通道的顺序可分辨**。
    ///
    /// - `seq` 每发布一次 +1: 前端凭它丢弃迟到的旧快照
    ///   （命令返回值与事件是两条通道, 到达顺序没有保证）。
    /// - `last_checked_at_ms` 是粘性字段: 谁都不许把它改小。
    ///   有些快照是以 `UpdateStatus::idle()` 为底建的（如恢复待安装更新）,
    ///   若不在这里兜住, "上次检查时间"会在这些路径上被抹成"从未检查"。
    ///
    /// ## ⚠️ 调用方**绝不可**在参数里取 `self.status` 的锁
    ///
    /// `publish(UpdateStatus { ..self.status.lock().await.clone() })` 会
    /// **永久死锁**，而且是从第一次调用就开始 —— 这一行曾经存在于本文件
    /// 的 8 个调用点上，把整个更新子系统锁死到"检查更新永远转圈、
    /// `get_update_status` 永不返回、连前端看门狗都一起挂住"。
    ///
    /// 原因不是逻辑错，是 Rust 的**临时量生命周期**：那个 `MutexGuard`
    /// 或写作 `..self.status.lock().await.clone()` 时，guard 存活到**整条语句**
    /// 结束，而这条语句包含 `publish(...).await`；`publish` 内部再取同一把
    /// 锁 ⇒ 自己等自己，永不返回。
    ///
    /// 所以：**派生快照一律走 [`Self::publish_from`]**，那里锁在 `publish`
    /// 之前就已经释放。守卫 `publish_must_not_nest_the_status_lock` 会拦住
    /// 这种写法重新出现 —— 它是静默的，编译通过、运行时只是"卡住"。
    async fn publish(&self, mut status: UpdateStatus) -> UpdateStatus {
        status.seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut cur = self.status.lock().await;
            if cur.last_checked_at_ms > status.last_checked_at_ms {
                status.last_checked_at_ms = cur.last_checked_at_ms;
            }
            *cur = status.clone();
        }
        // 事件推给前端: 后台调度触发的更新必须主动反映到界面上,
        // 托盘常驻下用户不会去点"检查更新"才发现新版本。
        if let Err(e) = self.app.emit(UPDATE_EVENT, status.clone()) {
            tracing::warn!("推送更新状态事件失败: {e}");
        }
        status
    }

    /// 以**当前快照**为底派生一份新状态并发布。
    ///
    /// 这是 [`Self::publish`] 的唯一安全入口，用于"只改几个字段、其余沿用"
    /// 的所有场景（阶段文案、进度、终态）。锁在调用 `publish` **之前**
    /// 就已经释放，因此不可能与 `publish` 内部的取锁相互嵌套。
    ///
    /// 直接写 `publish(UpdateStatus { ..cur, .. })` 也行，但必须先把
    /// `self.status.lock().await.clone()` 绑到一个变量上 —— 写成行内
    /// 展开就会死锁。这个函数把那一步变成不可能忘。
    async fn publish_from<F>(&self, build: F) -> UpdateStatus
    where
        F: FnOnce(&UpdateStatus) -> UpdateStatus,
    {
        // 这行的 guard 在分号处就释放了（语句结束），远早于 publish。
        let snapshot = self.status.lock().await.clone();
        self.publish(build(&snapshot)).await
    }

    /// 记下"这一轮检查跑完了", 并持久化到 `config.json`。
    ///
    /// 只更新内存里的快照 + 配置文件, **不 emit** —— 调用方紧接着就会发布
    /// 终态快照（已是最新 / 发现新版本 / 失败）, 那个快照带着这个粘性字段
    /// 一起到前端。这里额外推一条只会让界面闪一下。
    async fn record_check_time(&self) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.status.lock().await.last_checked_at_ms = now_ms;

        // 写配置失败不是致命错误: 内存里的时间已经生效, 下次检查成功
        // 写盘时自然补上。**不能**因此影响检查流程本身。
        let mut cfg = self.config.write().await;
        cfg.last_update_check_utc = Some(
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms)
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        );
        if let Err(e) = cfg.save() {
            tracing::warn!("持久化「上次检查时间」失败: {e}");
        }
    }

    async fn bump_failure(&self) {
        let mut f = self.failures.lock().await;
        *f += 1;
    }

    async fn reset_failures(&self) {
        *self.failures.lock().await = 0;
    }

    /// 配置里的「自动检查并下载更新」被改过之后调用。
    ///
    /// 立刻叫醒后台调度循环，让它按新值决定"继续跑"还是"停下等开关"。
    /// 不这么做的话，关掉开关后仍会按旧周期联网，最多要到 12 小时后
    /// 才真正停下 —— 而界面上开关**已经**是关的。
    pub fn notify_config_changed(&self) {
        self.wake.notify_waiters();
    }

    /// 启动静默检查调度: 8 秒后首次, 成功后每 12 小时, 失败则 3 分钟后重试,
    /// 连续失败 7 次熔断到下一个常规周期。
    ///
    /// 为什么要熔断: 国内直连 Gitee 时, 拔网线 / 公司防火墙会让每次检查都失败。
    /// 不熔断的话就是每 3 分钟一次无效网络请求, 笔记本指示灯常年亮。
    ///
    /// ## 开关必须**当场**生效，不能只在启动时读一次
    ///
    /// 早先 `auto_check` 是**值参数**，只在 `setup` 里读一次配置 —— 用户
    /// 在设置页关掉「自动检查并下载更新」后，后台仍然每 12 小时跑一次；
    /// 重新打开也一样，要等下次启动。而界面上的开关**立刻**翻过去了，
    /// 于是"关掉了却还在联网"这件事在界面上完全看不出来。
    ///
    /// 现在每轮都从共享配置里读一次，并用 `Notify` 让改配置的动作能
    /// **立刻叫醒**一个正在长睡的调度循环（否则关开关后最多还要等
    /// 12 小时才停）。
    pub fn spawn_scheduler(self: &std::sync::Arc<Self>) {
        let service = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(INITIAL_DELAY).await;
            loop {
                // 每轮现读，而不是启动时那份快照。
                let enabled = service
                    .config
                    .read()
                    .await
                    .auto_check_update;
                if !enabled {
                    tracing::info!("自动检查更新已在设置中关闭, 等待开关变化");
                    // 等到有人改配置；期间不发起任何网络请求。
                    service.wake.notified().await;
                    continue;
                }

                let status = service.check_and_download(false).await;
                let ok = matches!(status.phase, UpdatePhase::UpToDate | UpdatePhase::Ready);

                if ok {
                    service.reset_failures().await;
                    // 长睡期间若开关被关掉 / 改成开启，要能被立刻叫醒。
                    tokio::select! {
                        _ = tokio::time::sleep(NORMAL_INTERVAL) => {}
                        _ = service.wake.notified() => {}
                    }
                    continue;
                }

                let failures = service.consecutive_failures().await;
                if failures < MAX_RETRY_ATTEMPTS {
                    tracing::info!(
                        "自动检查更新失败 ({failures}/{MAX_RETRY_ATTEMPTS}), {} 秒后重试",
                        RETRY_INTERVAL.as_secs()
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(RETRY_INTERVAL) => {}
                        _ = service.wake.notified() => {}
                    }
                } else {
                    tracing::warn!("自动检查更新已连续失败 {MAX_RETRY_ATTEMPTS} 次, 熔断到下个常规周期");
                    service.reset_failures().await;
                    tokio::select! {
                        _ = tokio::time::sleep(NORMAL_INTERVAL) => {}
                        _ = service.wake.notified() => {}
                    }
                }
            }
        });
    }

    /// 失败时的窗口横幅。托盘常驻下不唤出窗口的话, 用户永远不知道结果。
    fn notify(&self, message: &str) {
        if let Err(e) = self.app.emit(
            "feisuo://update-notice",
            serde_json::json!({ "message": message }),
        ) {
            tracing::warn!("推送更新提示事件失败: {e}");
        }
    }
}

/// 启动最早期执行的接力收尾。
///
/// 见 [`UpdateService::apply_pending`] 的第 4 步。这里必须**同步**执行且
/// 不依赖任何异步运行时 —— Tauri 窗口此时还没建起来。
///
/// ## 为什么 `.old` 删不掉是**正常**的, 而"删掉了"才要警惕
///
/// `apply_rollover` 步骤 1 把当前 exe rename 成 `.old`, 那个映像
/// **此刻仍在运行**。Windows 上运行中的 exe 被内核独占, `remove_file`
/// 必然失败 —— 那个 `let _ =` 吞掉失败是**对的**, 它只是"顺手试一下"。
///
/// 真正的清理发生在**下一次**启动, 也就是本函数。但那时 `.old` 依然
/// 可能是**用户唯一的程序**:
/// - 上一次接力在"步骤 1 成功、步骤 2 失败"之间进程被杀;
/// - 或步骤 2 成功、新版本启动后**自己崩了**、用户手动重跑了 `.old`。
///
/// 这两种情况下删掉 `.old` = 删掉用户仅剩的可执行文件。所以本函数
/// 只在**确认正式路径上有一个能用的程序**时才清理 `.old`。
pub fn apply_rollover_if_needed() {
    let Ok(current_exe) = std::env::current_exe() else {
        return;
    };
    let staged = current_exe.with_extension(format!("exe{ROLLOVER_SUFFIX}"));
    if staged.exists() {
        // 当前这个实例就是从 .new 拉起来的(路径已由启动器解析为正式名,
        // 或用户手动重跑), 收尾只是把暂存件清掉。
        let _ = std::fs::remove_file(&staged);
    }

    // 上一轮接力失败的残留 —— 但**必须先确认正式路径上有东西**。
    //
    // 没有这条检查时: 接力在步骤 2 失败 + 进程被杀 ⇒ 用户目录里只剩
    // `xxx.exe.new.old`(旧版本), 正式路径空着。此时启动会走到这里,
    // 把 `.old` 删掉 —— 于是用户**既没有正式程序、备份也被删了**。
    let backup = current_exe.with_extension(format!("exe{ROLLOVER_SUFFIX}.old"));
    if backup.exists() && !current_exe.exists() {
        tracing::warn!(
            "正式路径 {} 上没有可执行文件, 保留 {} 作为回退 —— \
             删掉它会让用户彻底没有可运行的程序",
            current_exe.display(),
            backup.display()
        );
        return;
    }
    let _ = std::fs::remove_file(backup);
}

/// 安排"安装包静默装完之后，把新版本重新打开"。
///
/// ## 这一步为什么非做不可
///
/// 应用内「立即更新」走的是 `/SILENT`，而 `Feisuo.iss` 里唯一那条启动
/// 条目带 `skipifsilent` —— 静默安装**不会**启动程序。同时安装包会先
/// `taskkill /IM feisuo-desktop.exe /F`（否则覆盖不了 exe），本进程随后
/// 退出。两头都不拉，用户看到的就是"点完立即更新，安装界面闪一下，
/// 然后什么都没有" —— 0.1.12 的真实故障。
///
/// ## 为什么中继是"非提权"的 PowerShell，而不是安装包的 `[Run]` 段
///
/// 见 [`UpdateService::apply_pending`] 里那段长注释：`runas` 启动的安装包
/// 拿不到原始用户令牌，`runasoriginaluser` 会静默退化成"以管理员身份启动"，
/// 那样常驻的飞梭收不到资源管理器拖来的文件（UIPI）。所以中继必须由
/// **本进程**（中完整性）创建，拉起来的新版本也就回到中完整性。
///
/// ## 等待谁
///
/// 等**两个** PID：安装包（`installer_pid`，由
/// [`crate::os_shim::spawn_elevated`] 返回）和本进程。
///
/// - 等安装包：装完之前 `Program Files` 里的 exe 正被写，提前拉起会失败
///   或被单实例插件判成第二实例；
/// - 等本进程：`app.exit(0)` 到内核真正回收进程/互斥体之间还有一小段，
///   不等就会撞上 `tauri-plugin-single-instance` 那把按 identifier 命名的
///   互斥体，新进程刚启动就自杀（绿色版分支踩过这个坑，实测 12/12 失败）。
///
/// ## 为什么不用 `Wait-Process`
///
/// 实测：对**提权**进程，中完整性会话里 `Get-Process -Id` 能成功，但
/// `Wait-Process -Id` 直接抛 `ProcessCommandException`（它要的访问权比
/// 查询进程信息更高）。所以这里用 `Get-Process` 轮询存在性 —— 它只需要
/// 查询权限，恰好能看见管理员进程。
fn spawn_installer_relay(
    installer_pid: u32,
    target: &Path,
    args: &[std::ffi::OsString],
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let self_pid = std::process::id();
        let ps_quote = |s: &str| format!("'{}'", s.replace('\'', "''"));

        // 参数逐个引起来：`--daemon` 这种开机自启参数丢了，更新后就会
        // 凭空弹出一个窗口（用户以为程序自己打开了）。
        let mut launch_args = String::new();
        for a in args {
            launch_args.push(' ');
            launch_args.push_str(&ps_quote(&a.to_string_lossy()));
        }

        let target_ps = ps_quote(&target.to_string_lossy());
        let log_path = dirs::data_local_dir()
            .map(|d| d.join("feisuo").join("installer-relay.log"))
            .unwrap_or_else(|| std::env::temp_dir().join("feisuo-installer-relay.log"));
        let log_ps = ps_quote(&log_path.to_string_lossy());
        let log_dir_ps = ps_quote(
            &log_path
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
        );

        // 10 分钟硬上界：安装包卡住时中继不能永远挂着（那是用户机器上
        // 一个谁也不认识的常驻进程）。到点仍然尝试拉起。
        //
        // 拉起失败要重试 20 次 × 400ms：安装刚结束时 `Program Files` 里的
        // exe 可能还被卸载器/杀软扫描占用，一次 `Start-Process` 失败就放弃
        // 等于把用户扔在"装好了但打不开"。
        let script = format!(
            "$ErrorActionPreference = 'SilentlyContinue'; \
             $deadline = (Get-Date).AddMinutes(10); \
             while ((Get-Process -Id {installer_pid} -ErrorAction SilentlyContinue) -and \
                    ((Get-Date) -lt $deadline)) {{ Start-Sleep -Milliseconds 200 }}; \
             while ((Get-Process -Id {self_pid} -ErrorAction SilentlyContinue) -and \
                    ((Get-Date) -lt $deadline)) {{ Start-Sleep -Milliseconds 200 }}; \
             Start-Sleep -Milliseconds 600; \
             $ok = $false; \
             for ($i = 0; $i -lt 20 -and -not $ok; $i++) {{ \
               try {{ Start-Process -FilePath {target_ps}{launch_args} -ErrorAction Stop; $ok = $true }} \
               catch {{ Start-Sleep -Milliseconds 400 }} \
             }}; \
             New-Item -ItemType Directory -Force -Path {log_dir_ps} | Out-Null; \
             Add-Content -Path {log_ps} -Value ((Get-Date -Format 'o') + ' relaunch=' + $ok); \
             exit"
        );

        crate::os_shim::spawn_detached(
            Path::new("powershell.exe"),
            &[
                std::ffi::OsString::from("-NoProfile"),
                std::ffi::OsString::from("-NonInteractive"),
                std::ffi::OsString::from("-WindowStyle"),
                std::ffi::OsString::from("Hidden"),
                std::ffi::OsString::from("-Command"),
                std::ffi::OsString::from(script),
            ],
        )
    }

    #[cfg(not(target_os = "windows"))]
    {
        // 非 Windows 的安装包分支用不到（只有 Windows 有 Setup 安装版）。
        let _ = (installer_pid, target, args);
        Err("当前平台不支持安装包静默升级".into())
    }
}

/// 安排"当前进程退出之后再拉起新版本"。
///
/// ## 为什么不能直接 spawn
///
/// 单实例插件用的互斥体按 identifier 命名、与版本无关
/// （`net.findfine.feisuo-sim`）。旧进程还活着时 spawn 出来的新进程会在
/// 插件 setup 里发现互斥体已被占用 ⇒ 走"第二实例"分支 ⇒
/// `std::process::exit(0)` —— **新进程立刻自杀**；紧接着旧进程也退出，
/// 屏幕上什么都不剩，用户看到的是"更新成功但程序打不开"。
///
/// 实测（旧顺序，先 spawn 再 exit）：12 次全部失败，`survived=0 died=12`。
/// 改成"先退出、等它真的消失、再拉起"：10 次全部成功。
///
/// 所以必须：**先退出旧进程 → 互斥体释放 → 再拉起新 exe**。
/// 但 `app.exit(0)` 之后本进程无法继续执行，只能让外部的中继来做。
///
/// ## 中继为什么用 PowerShell 而不是 cmd
///
/// cmd 的 `goto` 只认**行首**的标签，`&:label&` 这种写法是无效的
/// （实测：中继静默地什么都不做，目标程序根本没被拉起）。
/// 要让它成立就得写一个临时 `.cmd` 文件 —— 那是留在磁盘上的可执行残留，
/// 还要跟杀软抢扫描时间窗。PowerShell 一行就能表达"等进程退出再启动"，
/// 且 `Wait-Process` 精确、不需要自己轮询。
///
/// 等到之后**再睡 300ms**：`Wait-Process` 返回时进程已终止，但内核
/// 句柄/互斥体未必已完成清理；留一点余量比赌"立刻就好"便宜得多。
fn spawn_relaunch_after_exit(
    exe: &Path,
    args: &[std::ffi::OsString],
    pid: u32,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // PowerShell 单引号字符串里，一个 ' 要写成 ''。
        let ps_quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let mut launch_args = String::new();
        for a in args {
            launch_args.push(' ');
            launch_args.push_str(&ps_quote(&a.to_string_lossy()));
        }
        let exe_ps = ps_quote(&exe.to_string_lossy());

        // -NoProfile: 不加载用户 profile（可能很慢或报错）。
        // -NonInteractive / -WindowStyle Hidden: 中继自己不留窗口、不等输入。
        // 末尾 exit: 中继拉起新版本后自己结束，不常驻。
        let script = format!(
            "Wait-Process -Id {pid} -Timeout 30 -ErrorAction SilentlyContinue; \
             Start-Sleep -Milliseconds 300; \
             Start-Process -FilePath {exe_ps}{launch_args}; \
             exit"
        );

        // `spawn_detached` 立即返回、不等待：中继进程会活在本次退出之后。
        crate::os_shim::spawn_detached(
            Path::new("powershell.exe"),
            &[
                std::ffi::OsString::from("-NoProfile"),
                std::ffi::OsString::from("-NonInteractive"),
                std::ffi::OsString::from("-WindowStyle"),
                std::ffi::OsString::from("Hidden"),
                std::ffi::OsString::from("-Command"),
                std::ffi::OsString::from(script),
            ],
        )
    }

    #[cfg(not(target_os = "windows"))]
    {
        // 非 Windows 没有单实例互斥体这个坑，直接拉起即可。
        let _ = pid;
        crate::os_shim::spawn_detached(exe, args)
    }
}

/// 把 `staged` 换到 `current_exe`, 失败时用 `backup` 兜底回滚。
fn apply_rollover(current_exe: &Path, staged: &Path, backup: &Path) -> Result<(), String> {
    // 1) 让出正式路径 —— 改运行中 exe 的名字是 Windows 允许的
    std::fs::rename(current_exe, backup).map_err(|e| format!("重命名当前程序失败: {e}"))?;

    // 2) 新版本占位
    if let Err(e) = std::fs::rename(staged, current_exe) {
        // 关键回滚: 换名失败必须把旧程序换回原位, 否则用户直接没程序可用了
        let _ = std::fs::rename(backup, current_exe);
        return Err(e.to_string());
    }

    // 3) 旧映像此刻**仍在运行**, Windows 独占它 ⇒ `remove_file` 必然失败。
    //    那个 `let _ =` 吞掉失败是对的: 它只是"顺手试一下", 真正该发生的是
    //    下次启动时由 `apply_rollover_if_needed` 清理。
    //    （原注释写"只能留给下次启动清理"却又放在这里删, 自相矛盾;
    //      实际发生的正是"删不掉", 所以行为一直是对的, 只是理由写错了。）
    let _ = std::fs::remove_file(backup);
    Ok(())
}

/// 挑选适合当前客户端形态的安装/更新附件。
/// 安装版优先匹配 Setup 安装程序，绿色版优先匹配单文件 exe。
///
/// 刻意**不接受任何其他附件**(源码包、校验和): 客户端只更新 exe,
/// 认错文件类型会导致下载到 tar.gz 然后当 exe 启动。
fn pick_asset(assets: &[AssetDto]) -> Option<&AssetDto> {
    let installed = is_installed_copy();
    let preferred = if installed { SETUP_ASSET_NAME } else { ASSET_NAME };
    let fallback = if installed { ASSET_NAME } else { SETUP_ASSET_NAME };

    // 1. 优先匹配当前形态对应的附件
    if let Some(a) = assets.iter().find(|a| a.name.eq_ignore_ascii_case(preferred)) {
        return Some(a);
    }
    // 2. 降级匹配备选附件
    if let Some(a) = assets.iter().find(|a| a.name.eq_ignore_ascii_case(fallback)) {
        return Some(a);
    }
    // 3. 通用兜底（必须以 .exe 结尾）
    assets
        .iter()
        .find(|a| a.name.to_ascii_lowercase().ends_with(".exe"))
}

/// 只放行官方域名。
///
/// 这是**安全边界**: 下载 URL 完全来自远端 JSON, 若不校验主机名,
/// 一个被劫持或伪造的 API 响应就能让客户端下载并执行任意程序。
pub fn is_trusted_download_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return false;
    }
    let host = lower
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");

    host == "gitee.com"
        || host.ends_with(".gitee.com")
        || host == "github.com"
        || host.ends_with(".github.com")
        || host.ends_with(".githubusercontent.com")
        || host == "release-assets.githubusercontent.com"
        || host == "objects.githubusercontent.com"
}

/// 从 `/releases/tag/vX.Y.Z` 里取出 tag 与版本号。
fn parse_tag_from_url(url: &str) -> Option<(String, String)> {
    let idx = url.find("/releases/tag/")?;
    let tag = url[idx + "/releases/tag/".len()..]
        .split(['?', '#', '/'])
        .next()?
        .trim();
    if tag.is_empty() {
        return None;
    }
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    Some((tag.to_string(), version))
}

/// 把 `config.json` 里的 RFC3339 时间戳解析成 Unix 毫秒（失败返回 `None`）。
///
/// 配置是**可以被手工编辑的**, 解析失败必须安静地退回"从未检查"，
/// 绝不能让一个手抖写坏的时间戳把设置页整段界面带崩。
fn parse_rfc3339_millis(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|t| t.timestamp_millis())
}

/// `x.y.z` 版本比较: `candidate` 比 `current` 新时为 true。
///
/// 刻意不用 `semver` crate: 版本号由本项目自己打 tag, 形式固定,
/// 而引入完整 semver 解析(含 prerelease/build 规则) 只会带来
/// 一堆"我们永远打不出来"的分支。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let Some(c) = parse_version(candidate) else {
        return false;
    };
    let Some(cur) = parse_version(current) else {
        return false;
    };
    for i in 0..3 {
        if c[i] != cur[i] {
            return c[i] > cur[i];
        }
    }
    false
}

fn parse_version(text: &str) -> Option<[u64; 3]> {
    let core = text
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()?;
    let mut out = [0u64; 3];
    for (i, part) in core.split('.').enumerate().take(3) {
        out[i] = part.trim().parse::<u64>().ok()?;
    }
    Some(out)
}

fn format_download_progress(received: u64, total: u64) -> String {
    if total > 0 {
        let pct = (received as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as u64;
        format!("{pct}%（{}/{}）", format_bytes(received), format_bytes(total))
    } else {
        format!("{}/未知大小", format_bytes(received))
    }
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// 在系统浏览器打开下载页(自动下载失败时的兜底)。
pub fn open_release_page(url: &str) -> Result<(), String> {
    let target = if url.is_empty() { GITEE_RELEASES_PAGE } else { url };
    open_url_in_browser(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_works() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        // 相等与更旧都必须为 false, 否则每次启动都会弹"发现新版本"
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // 缺段按 0 补齐: "0.1" 等价于 "0.1.0", 不得被当成新版本
        assert!(!is_newer("0.1.0", "0.1"));
        assert!(!is_newer("0.1", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1"));
        // 前缀 v 与预发布后缀
        assert!(is_newer("v1.2.3", "1.2.2"));
        assert!(!is_newer("1.2.3", "v1.2.3"));
        // 解析失败一律当作"不更新", 绝不能因为脏 tag 就触发下载
        assert!(!is_newer("not-a-version", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
    }

    /// 「上次检查时间」的解析: 合法值往返, 手改坏的配置安静退回"从未检查"。
    ///
    /// config.json 是可以被手工编辑的, 一个写坏的字符串不得让设置页
    /// 显示出一个荒唐的时间（或直接崩掉那段界面）。
    #[test]
    fn last_check_time_is_parsed_defensively() {
        let ms = parse_rfc3339_millis("2026-10-16T08:30:00Z").expect("合法 RFC3339 必须能解析");
        let back = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).unwrap();
        assert_eq!(back.to_rfc3339(), "2026-10-16T08:30:00+00:00");

        // 带时区偏移的写法同样接受
        assert!(parse_rfc3339_millis("2026-10-16T16:30:00+08:00").is_some());
        // 脏数据一律 None（界面显示"尚未检查"）, 绝不 panic
        assert!(parse_rfc3339_millis("").is_none());
        assert!(parse_rfc3339_millis("昨天下午").is_none());
        assert!(parse_rfc3339_millis("2026-13-45T99:99:99Z").is_none());
    }

    /// 「上次检查时间」是**粘性**字段, 任何快照都不许把它改小/清零。
    ///
    /// 恢复待安装更新、下载进度这些快照是以 `UpdateStatus::idle()` 为底建的,
    /// 清零之后设置页会显示"尚未检查" —— 而用户明明刚检查过。
    /// 这里钉住 `publish` 内部用的合并规则（取两者较大值）。
    #[test]
    fn last_check_time_is_sticky() {        let merge = |cur: i64, incoming: i64| cur.max(incoming);
        assert_eq!(merge(1_760_000_000_000, 0), 1_760_000_000_000, "idle() 为底的快照不得清零");
        assert_eq!(merge(0, 1_760_000_000_000), 1_760_000_000_000);
        // 新值更大时必须能前进, 否则时间会永远停在第一次检查
        assert_eq!(merge(1_760_000_000_000, 1_760_000_100_000), 1_760_000_100_000);
    }

    /// 镜像落后时的复核规则（`prefer_probe_over_up_to_date`）。
    ///
    /// 三个方向都必须钉住, 因为这个规则**曾经整体是反的**:
    /// 镜像说「已是最新」就直接下结论, 用户永远停在旧版本且毫无提示。
    #[test]
    fn stale_mirror_probe_rule() {        let mk = |v: &str| ReleaseInfo {
            version: v.to_string(),
            tag_name: format!("v{v}"),
            download_url: format!(
                "https://github.com/soldier-cv/feisuo/releases/download/v{v}/{ASSET_NAME}"
            ),
            asset_size: 1,
        };

        // 1) 探测到更新 ⇒ 必须用它。镜像在骗人, 用户必须能更新。
        let got = UpdateService::prefer_probe_over_up_to_date(Ok(Some(mk("9.9.9"))))
            .expect("复核成功不该是 Err");
        assert!(
            got.map(|r| r.version).as_deref() == Some("9.9.9"),
            "镜像说『已是最新』但探测到 9.9.9 时, 必须改用 9.9.9 —— \
             否则用户永远卡在旧版本, 界面上看不出任何异常"
        );

        // 2) 两条通道都认为没有更新 ⇒ 维持『已是最新』
        let got = UpdateService::prefer_probe_over_up_to_date(Ok(None)).expect("Ok(None) 不该是 Err");
        assert!(got.is_none(), "两条通道都没发现更新时不应凭空提示有新版");

        // 3) 探测失败 ⇒ **维持**『已是最新』, 绝不能反过来。
        //    反过来的后果是断网时每次都提示有新版, 比不检查更糟。
        let got = UpdateService::prefer_probe_over_up_to_date(Err("timeout".into()))
            .expect("探测失败必须回落到 Ok(None), 而不是把错误抛给用户");
        assert!(
            got.is_none(),
            "探测失败时不得提示有更新 —— 拿不到证据不等于有更新"
        );
    }

    #[test]
    fn trusted_url_allowlist_rejects_lookalikes() {
        // 合法
        assert!(is_trusted_download_url("https://gitee.com/huaxudong/feisuo/releases/download/v1/Feisuo-win-x64.exe"));
        assert!(is_trusted_download_url("https://github.com/soldier-cv/feisuo/releases/download/v1/Feisuo-win-x64.exe"));
        assert!(is_trusted_download_url("https://release-assets.githubusercontent.com/x/y/z.exe"));
        // 恶意仿冒: 主机名只是"看起来像"
        assert!(!is_trusted_download_url("https://gitee.com.evil.com/x.exe"));
        assert!(!is_trusted_download_url("https://evilgitee.com/x.exe"));
        assert!(!is_trusted_download_url("https://github.com.evil.io/x.exe"));
        assert!(!is_trusted_download_url("https://evil.com/?x=gitee.com"));
        // 非 http 协议一律拒绝(file:// 会变成任意本地执行入口)
        assert!(!is_trusted_download_url("file:///C:/Windows/System32/cmd.exe"));
        assert!(!is_trusted_download_url("javascript:alert(1)"));
    }

    #[test]
    fn tag_is_parsed_from_latest_redirect() {
        assert_eq!(
            parse_tag_from_url("https://github.com/soldier-cv/feisuo/releases/tag/v0.3.1"),
            Some(("v0.3.1".to_string(), "0.3.1".to_string()))
        );
        assert_eq!(parse_tag_from_url("https://github.com/soldier-cv/feisuo/releases"), None);
    }

    #[test]
    fn asset_pick_prefers_exact_name() {
        let assets = vec![
            AssetDto { name: "source-code.zip".into(), browser_download_url: "https://gitee.com/a.zip".into(), size: 1 },
            AssetDto { name: "Feisuo-Setup-x64.exe".into(), browser_download_url: "https://gitee.com/s.exe".into(), size: 3 },
            AssetDto { name: "Feisuo-win-x64.exe".into(), browser_download_url: "https://gitee.com/b.exe".into(), size: 2 },
        ];
        // 本地测试环境无 Feisuo.installed 标记，默认作为绿色版优先匹配 Feisuo-win-x64.exe
        assert_eq!(pick_asset(&assets).unwrap().name, "Feisuo-win-x64.exe");
        // 一个 exe 都没有时必须返回 None, 而不是随便挑一个 zip
        let only_zip = vec![AssetDto { name: "source-code.zip".into(), browser_download_url: "https://gitee.com/a.zip".into(), size: 1 }];
        assert!(pick_asset(&only_zip).is_none());
    }

    /// 真实下载端到端：从 Gitee 拉一小段，验证**重定向链每一跳都在白名单内**。
    ///
    /// 这条测的是 `real_gitee_release_is_parseable` 之后的下一环：
    /// 地址白名单过了不算完 —— reqwest 还要连跳两次重定向才会落到
    /// `foruda.gitee.com` 的实体文件上。早先客户端只校验**初始** URL，
    /// 重定向完全不受约束，白名单形同虚设。
    ///
    /// 这里只读前 64KB 就断开，避免真的把 7MB 拉完。
    #[test]
    #[ignore = "需要公网：cargo test -- --ignored real_gitee_download_follows_trusted_redirects"]
    fn real_gitee_download_follows_trusted_redirects() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if is_trusted_download_url(attempt.url().as_str()) {
                        attempt.follow()
                    } else {
                        attempt.error("重定向目标不在受信任域名内")
                    }
                }))
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap();

            let url = "https://gitee.com/huaxudong/feisuo/releases/download/v0.1.8/Feisuo-Setup-x64.exe";
            assert!(is_trusted_download_url(url), "初始地址必须过白名单");

            let mut resp = client.get(url).send().await.expect("Gitee 下载应当可达");
            assert!(resp.status().is_success(), "下载必须返回成功, 实际 {}", resp.status());
            println!("最终落地: {}", resp.url());
            assert!(
                is_trusted_download_url(resp.url().as_str()),
                "重定向后的最终地址也必须在白名单内, 实际: {}",
                resp.url()
            );
            let declared = resp.content_length().unwrap_or(0);
            assert!(
                declared > 1024 * 1024,
                "安装包体积应当 >1MB（{declared} 字节），否则是错误页而不是 exe"
            );

            // 读一小段确认真的是数据流（不是 HTML 错误页）
            let mut got = 0usize;
            while got < 64 * 1024 {
                match resp.chunk().await {
                    Ok(Some(c)) => got += c.len(),
                    _ => break,
                }
            }
            assert!(got > 0, "必须真的读到字节");
            println!("已读 {got} 字节, 声明总体积 {declared}");
        });
    }

    #[test]
    fn open_url_refuses_non_http() {
        assert!(open_url_in_browser("file:///C:/Windows/System32/cmd.exe").is_err());
        assert!(open_url_in_browser("javascript:alert(1)").is_err());
    }

    /// 重定向白名单：Gitee 的 CDN 子域必须放行，外部主机必须拒绝。
    ///
    /// 这条规则是 download 时逐跳校验用的（`redirect::Policy::custom`）。
    /// 早先只校验**初始** URL，于是"gitee.com 给的地址"可以 302 到任意主机 ——
    /// 白名单看着在，实际上一次跳转就能绕过。
    #[test]
    fn redirect_hop_must_stay_trusted() {
        // 真实链路：gitee.com → gitee.com/attach_files → foruda.gitee.com
        for url in [
            "https://gitee.com/huaxudong/feisuo/releases/download/v0.1.8/Feisuo-Setup-x64.exe",
            "https://gitee.com/huaxudong/feisuo/attach_files/3417476/download/Feisuo-Setup-x64.exe",
            "https://foruda.gitee.com/attach_file/1792243452011646866/feisuo-setup-x64.exe?token=x&ts=1",
        ] {
            assert!(is_trusted_download_url(url), "真实重定向链上的地址必须放行: {url}");
        }

        // 任何一跳跳到站外都要被拒
        for url in [
            "https://evil.example.com/Feisuo-Setup-x64.exe",
            "https://gitee.com.evil.com/x.exe",
            "https://evil-gitee.com/x.exe",
            "http://127.0.0.1:8080/x.exe",
            "file:///C:/Windows/System32/cmd.exe",
        ] {
            assert!(!is_trusted_download_url(url), "站外地址必须被拒: {url}");
        }
    }

    /// **真实网络**端到端：Gitee 的 JSON 必须能被解析、挑出 exe、且地址通过白名单。
    ///
    /// ## 为什么这条测试必须真的打网络
    ///
    /// 这一轮的 bug 是**死锁**，它把 `note_stage()` 挂住，网络请求根本发不出去。
    /// 用 mock 测"解析逻辑对不对"完全抓不到 —— 解析代码一行都没变过。
    /// 真正的判据是：**能不能走到网络并拿回一个可下载的附件**。
    ///
    /// 用 `#[ignore]` 标成手动/CI 可选：它依赖公网。想跑就
    /// `cargo test -p feisuo-desktop --ignored real_gitee`。
    /// 这里顺带钉住"Gitee 的 assets 里 `size` 字段确实缺失"这个事实 ——
    /// 客户端必须能容忍 `size: 0`（改由 Content-Length 兜底），
    /// 而 `download()` 里的复用判断正是按 `asset_size == 0` 放行的。
    #[test]
    #[ignore = "需要公网：cargo test --ignored real_gitee_release_is_parseable"]
    fn real_gitee_release_is_parseable() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap();
            let resp = client
                .get(GITEE_LATEST_API)
                .header("Accept", "application/json")
                .send()
                .await
                .expect("Gitee 应当可达");
            assert_eq!(resp.status().as_u16(), 200, "Gitee 最新版本接口");
            let dto: ReleaseDto = resp.json().await.expect("Gitee JSON 必须可解析");
            assert!(!dto.tag_name.is_empty(), "Gitee 响应必须有 tag_name");

            let asset = pick_asset(&dto.assets)
                .expect("Gitee Release 里必须有可下载的 .exe 附件");
            assert!(
                asset.name.ends_with(".exe"),
                "挑出来的附件必须是 exe, 实际: {}",
                asset.name
            );
            assert!(
                is_trusted_download_url(&asset.browser_download_url),
                "Gitee 给的下载地址必须过白名单, 实际: {}",
                asset.browser_download_url
            );
            // 真实存在的夹具：这条 URL 必须能拿到 3xx/200（不跟随也能看出可用）
            println!(
                "Gitee 最新: {} / 附件 {} (size={}) / {}",
                dto.tag_name, asset.name, asset.size, asset.browser_download_url
            );
        });
    }
}
