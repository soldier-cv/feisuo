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
//! ## 安全约束
//!
//! 下载地址来自远端 JSON 响应, 因此 [`is_trusted_download_url`] 是硬闸门:
//! 只放行 `*.gitee.com` / `*.github.com` / `*.githubusercontent.com`。
//! 少了这一条, 一个被劫持的 API 响应就能让客户端执行任意 exe。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::os_shim::{open_url_in_browser, spawn_detached};

const REPO_OWNER: &str = "soldier-cv";
const REPO_NAME: &str = "feisuo";

const GITEE_LATEST_API: &str = "https://gitee.com/api/v5/repos/huaxudong/feisuo/releases/latest";
const GITHUB_LATEST_API: &str = "https://api.github.com/repos/soldier-cv/feisuo/releases/latest";
const GITHUB_LATEST_PAGE: &str = "https://github.com/soldier-cv/feisuo/releases/latest";
const GITEE_RELEASES_PAGE: &str = "https://gitee.com/huaxudong/feisuo/releases";

/// 绿色版 exe 的产物名。release.yml 正是按这个名字上传的。
const ASSET_NAME: &str = "Feisuo-win-x64.exe";

/// 换版时旧 exe 的临时文件名。Windows 上正在运行的 exe 无法删除,
/// 只能先改名 —— 改完名旧映像仍被占用, 于是新文件用**另一个名字**落地,
/// 由下一次启动做"换名接力"(见 [`UpdateService::apply_pending`])。
const ROLLOVER_SUFFIX: &str = ".new";

/// 更新包体积上限。飞梭绿色版约 18 MB, 给到 200 MB 足够宽松,
/// 同时能挡住"Content-Length 谎报超大体积把磁盘写满"。
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;

const API_TIMEOUT: Duration = Duration::from_secs(8);
/// 镜像落后复核用的 GitHub 探测超时（见 `check_for_update`）。
///
/// 比 [`API_TIMEOUT`] 短，因为这次请求**只用来证伪**"已经最新"这个结论，
/// 拿不到结果就维持原结论。它每 12 小时最多发生一次（用户已是最新时），
/// 3.5 秒不会让人等不下去；反过来，若这里用 8 秒，国内用户每次点
/// 「检查更新」都要先白等 8 秒才知道自己没更新。
const STALE_PROBE_TIMEOUT: Duration = Duration::from_millis(3500);
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
    /// 串行化"检查 + 下载", 防止用户连点与后台调度撞在一起
    gate: Mutex<()>,
    status: Mutex<UpdateStatus>,
    /// 已下载待安装的更新
    pending: Mutex<Option<PendingUpdate>>,
    /// 连续失败计数, 达到 MAX_RETRY_ATTEMPTS 后熔断到下一个常规周期
    failures: Mutex<u32>,
}

impl UpdateService {
    pub fn new(app: AppHandle) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!("Feisuo/{} (+https://github.com/{REPO_OWNER}/{REPO_NAME})", current_version()))
            .connect_timeout(API_TIMEOUT)
            // 不设全局 timeout: 大 exe 下载可能很久, 由 DOWNLOAD_IDLE_TIMEOUT
            // 逐次读超时兜底 —— 全局超时会误杀正在正常下载的慢速连接。
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let updates_dir = feisuo_core::AppConfig::get_app_dir().join("updates");

        Self {
            updates_dir,
            client,
            app,
            gate: Mutex::new(()),
            status: Mutex::new(UpdateStatus::idle()),
            pending: Mutex::new(None),
            failures: Mutex::new(0),
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
            let mut p = self.pending.lock().await;
            if p.is_none() {
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
                *p = Some(found);
            }
        }
    }

    /// 手动检查(用户点了「检查更新」)。
    ///
    /// `interactive` 只影响**失败时是否主动弹窗**; 后台静默调度传 false,
    /// 免得一天弹 7 次错误。
    pub async fn check_and_download(&self, interactive: bool) -> UpdateStatus {
        let _guard = self.gate.lock().await;

        // 已经有就绪的更新包, 且确实比当前版本新 —— 不必重新走网络。
        if let Some(p) = self.pending.lock().await.clone() {
            if p.local_path.exists() && is_newer(&p.version, &current_version()) {
                let st = self
                    .publish(UpdateStatus {
                        phase: UpdatePhase::Ready,
                        message: format!("新版本 {} 已下载就绪", p.tag_name),
                        latest_version: p.version.clone(),
                        tag_name: p.tag_name.clone(),
                        pending_path: p.local_path.to_string_lossy().to_string(),
                        ..UpdateStatus::idle()
                    })
                    .await;
                return st;
            }
        }

        self.publish(UpdateStatus {
            phase: UpdatePhase::Checking,
            message: "正在检查更新…".into(),
            ..self.status.lock().await.clone()
        })
        .await;

        let check = match self.check_for_update().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("检查更新失败: {e}");
                self.bump_failure().await;
                let st = self
                    .publish(UpdateStatus {
                        phase: UpdatePhase::Failed,
                        message: e,
                        busy: false,
                        ..self.status.lock().await.clone()
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
                self.publish(UpdateStatus {
                    phase: UpdatePhase::UpToDate,
                    message: format!("当前已是最新版本 v{}", current_version()),
                    busy: false,
                    ..self.status.lock().await.clone()
                })
                .await
            }
            Some(release) => {
                let tag = release.tag_name.clone();
                let version = release.version.clone();
                self.publish(UpdateStatus {
                    phase: UpdatePhase::Downloading,
                    message: format!("发现新版本 {tag}，正在下载"),
                    latest_version: version,
                    tag_name: tag.clone(),
                    busy: true,
                    ..self.status.lock().await.clone()
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

                        self.publish(UpdateStatus {
                            phase: UpdatePhase::Ready,
                            message: format!("新版本 {} 已下载就绪", release.tag_name),
                            latest_version: release.version.clone(),
                            tag_name: release.tag_name.clone(),
                            pending_path: path.to_string_lossy().to_string(),
                            bytes_received: size,
                            bytes_total: size,
                            busy: false,
                            ..self.status.lock().await.clone()
                        })
                        .await
                    }
                    Err(e) => {
                        tracing::warn!("下载更新失败: {e}");
                        self.bump_failure().await;
                        self.publish(UpdateStatus {
                            phase: UpdatePhase::Failed,
                            message: format!("发现新版本 {tag}，但下载失败: {e}。可在设置页用浏览器手动下载。"),
                            latest_version: release.version,
                            tag_name: tag,
                            busy: false,
                            ..self.status.lock().await.clone()
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
    async fn check_for_update(&self) -> Result<Option<ReleaseInfo>, String> {
        let gitee = self.try_gitee_api().await;
        match &gitee {
            Ok(Some(v)) => return Ok(Some(v.clone())),
            // Gitee 说已是最新：**先别信**，见本函数文档。
            Ok(None) => {
                let probe = self.probe_github_api().await;
                if let Ok(Some(v)) = &probe {
                    tracing::info!("Gitee 镜像落后, 已改用 GitHub 的 v{}", v.tag_name);
                }
                return Self::prefer_probe_over_up_to_date(probe);
            }
            Err(e) => tracing::warn!("Gitee 通道不可用: {e}，降级 GitHub"),
        }

        let github = self.try_github_api().await;
        match &github {
            Ok(v) => return Ok(v.clone()),
            Err(e) => tracing::warn!("GitHub API 通道不可用: {e}，降级 Releases 页面"),
        }

        let page = self.try_github_page().await;
        match &page {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(format!("检查更新失败（已尝试 Gitee / GitHub / 页面）: {e}")),
        }
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

        // 附件缺失**不能**当成"没有新版本": 否则用户永远停在旧版本却毫无提示。
        let Some(asset) = pick_asset(&dto.assets) else {
            return Err(format!(
                "发现新版本 {tag}，但没有 {ASSET_NAME} 附件。可手动访问 {GITEE_RELEASES_PAGE}"
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

        let dest = self
            .updates_dir
            .join(format!("Feisuo-win-x64-{}.exe", release.version));
        let part = dest.with_extension("exe.part");

        if dest.exists()
            && (release.asset_size == 0
                || std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) == release.asset_size)
        {
            return Ok(dest);
        }

        let resp = self
            .client
            .get(&release.download_url)
            .send()
            .await
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
                self.publish(UpdateStatus {
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
                    ..self.status.lock().await.clone()
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

        // 3) 拉起新版本。沿用原有参数: 开机自启是 `--daemon`,
        //    漏掉会让自启场景更新后窗口凭空弹出来。
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let relaunch_dir = dir.to_path_buf();
        spawn_detached(&current_exe, &args)
            .map_err(|e| format!("启动新版本失败: {e}"))?;
        tracing::info!("已拉起新版本, 目录: {}", relaunch_dir.display());

        // 4) 退出旧实例
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
            // 命名规则: Feisuo-win-x64-<version>.exe
            let Some(rest) = name
                .strip_prefix("Feisuo-win-x64-")
                .and_then(|s| s.strip_suffix(".exe"))
            else {
                continue;
            };
            if rest.is_empty() || !is_newer(rest, &current_version()) {
                continue;
            }
            // 512 字节以下不可能是真实 exe, 多半是错误页
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > 512 => {}
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
                if name.starts_with("Feisuo-win-x64-") {
                    let _ = tokio::fs::remove_file(p).await;
                }
            }
        }
    }

    async fn publish(&self, status: UpdateStatus) -> UpdateStatus {
        *self.status.lock().await = status.clone();
        // 事件推给前端: 后台调度触发的更新必须主动反映到界面上,
        // 托盘常驻下用户不会去点"检查更新"才发现新版本。
        if let Err(e) = self.app.emit(UPDATE_EVENT, status.clone()) {
            tracing::warn!("推送更新状态事件失败: {e}");
        }
        status
    }

    async fn bump_failure(&self) {
        let mut f = self.failures.lock().await;
        *f += 1;
    }

    async fn reset_failures(&self) {
        *self.failures.lock().await = 0;
    }

    /// 启动静默检查调度: 8 秒后首次, 成功后每 12 小时, 失败则 3 分钟后重试,
    /// 连续失败 7 次熔断到下一个常规周期。
    ///
    /// 为什么要熔断: 国内直连 Gitee 时, 拔网线 / 公司防火墙会让每次检查都失败。
    /// 不熔断的话就是每 3 分钟一次无效网络请求, 笔记本指示灯常年亮。
    pub fn spawn_scheduler(self: &std::sync::Arc<Self>, auto_check: bool) {
        let service = self.clone();
        tokio::spawn(async move {
            if !auto_check {
                tracing::info!("自动检查更新已在设置中关闭, 不启动调度器");
                return;
            }
            tokio::time::sleep(INITIAL_DELAY).await;
            loop {
                let status = service.check_and_download(false).await;
                let ok = matches!(status.phase, UpdatePhase::UpToDate | UpdatePhase::Ready);

                if ok {
                    service.reset_failures().await;
                    tokio::time::sleep(NORMAL_INTERVAL).await;
                    continue;
                }

                let failures = service.consecutive_failures().await;
                if failures < MAX_RETRY_ATTEMPTS {
                    tracing::info!(
                        "自动检查更新失败 ({failures}/{MAX_RETRY_ATTEMPTS}), {} 秒后重试",
                        RETRY_INTERVAL.as_secs()
                    );
                    tokio::time::sleep(RETRY_INTERVAL).await;
                } else {
                    tracing::warn!("自动检查更新已连续失败 {MAX_RETRY_ATTEMPTS} 次, 熔断到下个常规周期");
                    service.reset_failures().await;
                    tokio::time::sleep(NORMAL_INTERVAL).await;
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

/// 挑选 Windows x64 绿色版附件。
///
/// 刻意**不接受任何其他附件**(源码包、校验和): 客户端只更新 exe,
/// 认错文件类型会导致下载到 tar.gz 然后当 exe 启动。
fn pick_asset(assets: &[AssetDto]) -> Option<&AssetDto> {
    assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(ASSET_NAME))
        .or_else(|| {
            assets
                .iter()
                .find(|a| a.name.to_ascii_lowercase().ends_with(".exe") && !a.name.contains("Setup"))
        })
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

    /// 镜像落后时的复核规则（`prefer_probe_over_up_to_date`）。
    ///
    /// 三个方向都必须钉住, 因为这个规则**曾经整体是反的**:
    /// 镜像说「已是最新」就直接下结论, 用户永远停在旧版本且毫无提示。
    #[test]
    fn stale_mirror_probe_rule() {
        let mk = |v: &str| ReleaseInfo {
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
            AssetDto { name: "Feisuo-win-x64.exe".into(), browser_download_url: "https://gitee.com/b.exe".into(), size: 2 },
        ];
        assert_eq!(pick_asset(&assets).unwrap().name, "Feisuo-win-x64.exe");
        // 一个 exe 都没有时必须返回 None, 而不是随便挑一个 zip
        let only_zip = vec![AssetDto { name: "source-code.zip".into(), browser_download_url: "https://gitee.com/a.zip".into(), size: 1 }];
        assert!(pick_asset(&only_zip).is_none());
    }

    #[test]
    fn open_url_refuses_non_http() {
        assert!(open_url_in_browser("file:///C:/Windows/System32/cmd.exe").is_err());
        assert!(open_url_in_browser("javascript:alert(1)").is_err());
    }
}
