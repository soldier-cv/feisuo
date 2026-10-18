//! 「应用内更新」守卫。
//!
//! 与 `wiring_guard` 同理: 更新链路的每一环都跨了语言边界, 失配在编译期
//! 完全无感, 运行时表现为"检查更新按钮点了没反应", 而且**极难自查** ——
//! 用户只会认为"这个软件不爱更新", 不会想到是代码没接上。
//!
//! 这里把四类真实会发生的失配钉成测试失败:
//!  1. 后端加了命令 / 前端忘了接线 (由 `wiring_guard` 覆盖, 这里做正面确认)
//!  2. 版本号来源不一致 —— 检查更新会永远说"已是最新"或永远说有新版
//!  3. 下载地址缺少主机名白名单 —— 远端一个被劫持的 JSON 就能让客户端执行任意 exe
//!  4. 换名接力的回滚被删掉 —— 失败时用户直接"没程序可用了"

use std::path::{Path, PathBuf};

fn desktop_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn ui_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("ui")
        .join("src")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e))
}

fn updater_src() -> String {
    read(&desktop_src().join("updater.rs"))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn workflow_src(name: &str) -> String {
    read(&repo_root().join(".github").join("workflows").join(name))
}

#[test]
fn guarded_files_must_exist() {
    assert!(
        desktop_src().join("updater.rs").is_file(),
        "找不到 updater.rs"
    );
    assert!(
        ui_src().join("api").join("feisuoBridge.ts").is_file(),
        "找不到 feisuoBridge.ts, 守卫会静默空跑"
    );
    assert!(
        ui_src().join("App.vue").is_file(),
        "找不到 App.vue, 守卫会静默空跑"
    );
}

/// 更新链路的每一环都必须真的存在。
///
/// 特别盯住「有检查无应用」: 只接了 `check_for_update` 而没接 `apply_update`,
/// 界面上会显示"新版本已下载就绪"然后**永远没有下一步** ——
/// 比没有更新功能更糟, 因为它让用户以为程序坏了。
#[test]
fn update_pipeline_must_be_fully_wired() {
    let main_rs = read(&desktop_src().join("main.rs"));
    let commands = read(&desktop_src().join("commands.rs"));
    let updater = updater_src();
    let bridge = read(&ui_src().join("api").join("feisuoBridge.ts"));
    let app = read(&ui_src().join("App.vue"));

    // 1) 四个命令: 查状态 / 检查 / 应用 / 兜底下载页, 缺一不可
    for cmd in [
        "get_update_status",
        "check_for_update",
        "apply_update",
        "open_update_page",
    ] {
        assert!(
            main_rs.contains(&format!("commands::{cmd}")),
            "{cmd} 没有在 generate_handler! 里注册 —— 点了没反应"
        );
        assert!(
            commands.contains(&format!("pub async fn {cmd}")) || commands.contains(&format!("pub fn {cmd}")),
            "commands.rs 缺少 {cmd} 的实现"
        );
        assert!(
            bridge.contains(&format!("\"{cmd}\"")),
            "桥接层缺少 {cmd} —— 后端能力前端从不调用, 等于死代码"
        );
    }

    // 2) 应用更新的实现在 updater 里, 且**真的**会被 command 调用
    assert!(
        updater.contains("pub async fn apply_pending("),
        "updater 缺少 apply_pending 实现"
    );
    assert!(
        commands.contains("apply_pending(&app)"),
        "apply_update 命令必须真的调用 apply_pending —— \
         否则命令永远返回成功, 点了「立即更新」程序却什么都不做"
    );

    // 3) 状态事件必须双向都有: 后端 emit, 前端 listen
    assert!(
        updater.contains("feisuo://update-status"),
        "updater 必须 emit feisuo://update-status —— \
         后台调度在窗口隐藏时完成下载, 没有事件界面永远不更新"
    );
    assert!(
        bridge.contains("onUpdateStatus") && bridge.contains("feisuo://update-status"),
        "桥接层必须订阅 feisuo://update-status"
    );
    let at = app
        .find("onUpdateStatus(")
        .expect("App.vue 必须订阅更新状态事件");
    let seg: String = app[at..].chars().take(600).collect();
    assert!(
        seg.contains("updateStatus.value = s"),
        "更新状态事件必须真的写进 updateStatus —— 只订阅不赋值等于没接"
    );

    // 4) 界面必须有可点的入口
    assert!(
        app.contains("function checkForUpdate(") && app.contains("function applyUpdate("),
        "App.vue 缺少 checkForUpdate / applyUpdate 处理函数"
    );
    assert!(
        app.contains("@click=\"applyUpdate\""),
        "「立即更新」按钮必须绑定 applyUpdate —— 下载完了却点不动"
    );

    // 5) 托盘入口: 托盘常驻是飞梭的主要使用形态,
    //    没有托盘菜单用户根本找不到更新入口
    let tray = read(&desktop_src().join("tray.rs"));
    assert!(
        tray.contains("check_update") && tray.contains("检查更新"),
        "托盘菜单缺少「检查更新」入口"
    );
}

/// 版本号必须只有一个来源。
///
/// 检查更新的一切都建立在"当前版本"之上, 两处不一致会直接产生
/// 两种荒谬行为:
///  * Cargo 版本 < tag: 永远提示"发现新版本", 装了还是提示有新版;
///  * Cargo 版本 > tag: 永远"已是最新", 新版本对所有用户都不可见。
#[test]
fn current_version_must_come_from_cargo_pkg() {
    let updater = updater_src();
    assert!(
        updater.contains(r#"env!("CARGO_PKG_VERSION")"#),
        "当前版本必须取自 CARGO_PKG_VERSION —— \
         写死字符串会在每次发版后与真实版本脱节"
    );
    // 不允许在 updater 的生产代码里出现手写版本字面量
    let test_start = updater.find("mod tests").unwrap_or(updater.len());
    let head: String = updater[..test_start].to_string();
    for literal in ["\"0.1.0\"", "\"1.0.0\""] {
        assert!(
            !head.contains(literal),
            "updater 生产代码里出现了写死的版本号 {literal}, \
             必须改用 CARGO_PKG_VERSION"
        );
    }

    // release.yml 必须同时改这两处, 否则前端显示的版本与实际不符
    // (CARGO_MANIFEST_DIR = desktop/src-tauri, 往上两级才是仓库根)
    let workflow = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(".github")
        .join("workflows")
        .join("release.yml");
    assert!(
        workflow.is_file(),
        "找不到 release.yml —— 没有发版流水线, 检查更新永远查不到东西"
    );
    let yml = read(&workflow);
    assert!(
        yml.contains("Cargo.toml") && yml.contains("tauri.conf.json"),
        "release.yml 必须同时给 Cargo.toml 与 tauri.conf.json 打版本号, \
         漏一处就会让检查更新永久失灵"
    );
    assert!(
        yml.contains("Feisuo-win-x64.exe") && yml.contains("Feisuo-Setup-x64.exe"),
        "release.yml 必须同时上传 Feisuo-Setup-x64.exe (安装版) 与 Feisuo-win-x64.exe (绿色版)"
    );
}

/// 镜像落后时必须能自愈, 不得停在旧版本。
///
/// ## 这条守卫对应一个真实发出去过的行为
///
/// Gitee 是主通道(国内 GitHub API 常超时), 但它可能**静默落后**:
/// `release.yml` 的 Gitee 同步是 `continue-on-error`, 附件上传连
/// curl 退出码都没检查。早先 `check_for_update` 写成
/// `Ok(v) => return`, 于是 Gitee 返回一个旧 tag ⇒ `is_newer` 为 false
/// ⇒ 告诉用户「当前已是最新版本」, 而且**再也不问 GitHub**。
///
/// 用户就这么卡在旧版, 界面上没有任何异常提示 ——
/// 与 `finish()` 里"附件缺失不能当成没有新版本"是同一个道理,
/// 方向相反的同一个错误。
#[test]
fn stale_mirror_must_not_end_the_update_check() {
    let updater = updater_src();

    // 复核用的短超时通道必须存在, 且被 `check_for_update` 真的调用
    assert!(
        updater.contains("STALE_PROBE_TIMEOUT"),
        "缺少镜像落后复核的超时常量"
    );
    assert!(
        updater.contains("fn probe_github_api("),
        "缺少 probe_github_api —— 镜像说『已是最新』时没有第二意见"
    );

    let at = updater
        .find("async fn check_for_update(")
        .expect("找不到 check_for_update");
    let body: String = updater[at..].chars().take(2600).collect();
    assert!(
        body.contains("probe_github_api"),
        "check_for_update 必须在 Gitee 返回『无更新』时调用 probe_github_api —— \
         镜像落后时用户会永远停在旧版本, 且界面看不出任何异常"
    );

    // Gitee 那一臂必须**区分** Ok(Some) 与 Ok(None)。
    // 只写 `Ok(v) => return` 就说明两种情况被合并了, 复核永远不会被触发。
    assert!(
        body.contains("Ok(Some("),
        "check_for_update 对 Gitee 结果必须区分 Ok(Some(_)) 与 Ok(None) —— \
         合并成 `Ok(v) => return` 就等于镜像说『没更新』时直接下结论"
    );

    // 复核失败必须维持原结论, 不能反过来提示"有新版"
    assert!(
        updater.contains("fn prefer_probe_over_up_to_date("),
        "缺少 prefer_probe_over_up_to_date —— 复核规则的落点"
    );
    let at = updater
        .find("fn prefer_probe_over_up_to_date(")
        .expect("找不到 prefer_probe_over_up_to_date");
    let body: String = updater[at..].chars().take(700).collect();
    assert!(
        body.contains("Ok(None)"),
        "复核失败时必须回到『已是最新』—— \
         拿不到证据不等于有更新, 反过来会让断网时凭空提示有新版"
    );
}

/// Gitee 同步失败必须**可见**, 且不得留下"有 Release 无附件"的空壳。
///
/// 客户端是 Gitee 优先的, 所以镜像不是一个可有可无的副本。
/// 早先脚本无条件打印「上传完毕！」(curl 退出码都没看), 拿不到 Release id
/// 也只 `Write-Warning` —— 而这两条路径都发生在 `continue-on-error` 里,
/// 于是镜像可以长期半死不活, 客户端每次检查更新都命中它。
#[test]
fn gitee_mirror_must_verify_its_own_upload() {
    let workflow = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(".github")
        .join("workflows")
        .join("release.yml");
    let yml = read(&workflow);

    let at = yml
        .find("Sync Release to Gitee")
        .expect("找不到 Gitee 同步步骤");
    let step: String = yml[at..].chars().take(9000).collect();

    assert!(
        step.contains("curl.exe"),
        "Gitee 同步步骤里没有 curl 上传 —— 镜像结构变了? 客户端会一直报『没有附件』"
    );
    // curl 之后必须读退出码（定位于上传附件的最后一个 curl 调用）
    let curl_at = step.rfind("curl.exe").expect("找不到 curl 调用");
    let after: String = step[curl_at..].chars().take(1200).collect();
    assert!(
        after.contains("LASTEXITCODE"),
        "curl 上传之后必须检查退出码 —— \
         网络失败时附件根本没上传, 而 Release 已经建好了, \
         客户端会报『发现新版本但没有附件』"
    );
    assert!(
        after.contains("throw"),
        "curl 失败必须 throw 而不是继续 —— \
         continue-on-error 会把它藏起来, 镜像就成了有 Release 无附件的空壳"
    );

    // 拿不到 release id 同样必须 throw
    assert!(
        step.contains("未能获取到有效的 Gitee Release ID") && !step.contains("Write-Warning \"未能获取"),
        "拿不到 Gitee Release ID 必须 throw —— \
         只警告的话会留下一个客户端永远命中、却拿不到任何东西的 Release"
    );

    // 没有 token 是**国内用户拿不到高速通道**, 不是可以忽略的小事
    assert!(
        step.contains("::error::"),
        "缺 GITEE_TOKEN 时必须用 ::error:: 标注 —— \
         客户端 Gitee 优先, 没有镜像等于国内用户全程走常超时的 GitHub"
    );
}

/// 发版必须与本地走**同一条构建命令**。
///
/// 早先 release.yml 用 `cargo build --release -p feisuo-desktop`,
/// 而本地用的是 `cargo tauri build --no-bundle`。
/// 后者会带上 Tauri CLI 注入的构建期环境(资源、图标、`TAURI_ENV_*`),
/// 两条路径的产物可能不同 —— 而差异只在用户下载之后才暴露,
/// 本地怎么测都测不出来。
///
/// ## 为什么判据落在文档而不是某个脚本
///
/// 原来这里同时检查 `release.yml` 与 `build.ps1` 两处，
/// 因为那时"本地怎么构建"是由脚本定义的。
///
/// 脚本删除后（用户要求改为手动执行命令，见 README「手动编译」），
/// **本地构建路径的唯一来源变成了文档**。所以判据跟着换成
/// 「README 里给出的命令必须是 `cargo tauri build`」——
/// 否则这条守卫会退化成只检查 CI 的单向断言，
/// 恰好丢掉它本来要防的那一半：本地与发版不一致。
#[test]
fn release_must_build_the_same_way_as_local() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let yml = read(&root.join(".github").join("workflows").join("release.yml"));
    let readme = read(&root.join("README.md"));

    assert!(
        yml.contains("cargo tauri build"),
        "release.yml 必须用 `cargo tauri build` 与本地一致 —— \
         `cargo build -p feisuo-desktop` 不带 Tauri CLI 注入的构建期环境"
    );
    assert!(
        readme.contains("cargo tauri build"),
        "README「手动编译」一节必须给出 `cargo tauri build` —— \
         脚本已删除，本地构建命令的唯一来源就是这份文档。\
         若文档里写的是 `cargo build -p feisuo-desktop`, \
         本地测过的 exe 与发出去的 exe 就不是同一个构建"
    );

    // 版本号落地必须**回读确认**, 不能只检查"替换发生了"
    assert!(
        yml.contains("Verify version stamp landed in both files")
            || yml.contains("期望"),
        "release.yml 必须回读校验两处版本号真的相等 —— \
         tauri.conf.json 的正则只检查『有没有替换』, 不检查『替换成了什么』"
    );
}

/// 版本号不得在流水线里写死。
///
/// AGENTS.md 规则 6: 版本号只有一个来源。早先 `release.yml` 写死
/// `$ver = '0.1.0'` 作为回退 —— tag 不是语义化版本时正则不匹配,
/// **静默**发出一个名为 `vnext`、内容却是 0.1.0 的 Release,
/// 两处配置被一起改成 0.1.0、守卫全部通过, 而所有在线用户从此
/// 永远收到「已是最新」。
#[test]
fn release_must_not_hardcode_a_version() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let yml = read(&root.join(".github").join("workflows").join("release.yml"));

    // 只查版本解析那一步：$ver = '0.1.0' 之类的回退
    for line in yml.lines() {
        let t = line.trim();
        if t.starts_with("$ver") && t.contains("'") {
            assert!(
                !t.contains("0.1.0") && !t.contains("0.0.0") && !t.contains("1.0.0"),
                "release.yml 里写死了版本号回退: {t} —— \
                 规则 6 要求版本号只来自 Cargo.toml / tag"
            );
        }
    }
    // 语义化 tag 解析失败必须硬失败, 不能回退
    assert!(
        yml.contains("不是 v<major>.<minor>.<patch>") || yml.contains("拒绝发版"),
        "release.yml 必须对非语义化 tag 硬失败 —— \
         静默回退会发出内容与标签不符的包, 并把所有用户的更新通道锁死"
    );
}

/// 下载地址必须过主机名白名单。
///
/// 这是更新功能里**唯一的远程代码执行面**: 下载 URL 完全来自远端 JSON 响应。
/// 少了 `is_trusted_download_url` 这道闸, 一个被 DNS 劫持或 API 被中间人
/// 篡改的响应, 就能让客户端下载并执行任意 exe。
#[test]
fn download_url_must_be_allowlisted() {
    let updater = updater_src();

    assert!(
        updater.contains("fn is_trusted_download_url("),
        "缺少下载地址白名单校验 —— 远端一个被篡改的 JSON 就能让客户端执行任意 exe"
    );

    // 白名单必须在**下载前**被真正调用, 而不是定义了就完事
    let download_body: String = updater
        .split("async fn download(")
        .nth(1)
        .expect("找不到 download 实现")
        .chars()
        .take(2000)
        .collect();
    assert!(
        download_body.contains("is_trusted_download_url"),
        "download 必须在发起请求前校验地址 —— \
         定义了白名单却不调用等于没有"
    );

    // 白名单里必须同时有 Gitee 与 GitHub: 只有其一会在另一侧网络故障时全盘失灵
    for host in ["gitee.com", "github.com", "githubusercontent.com"] {
        assert!(
            updater.contains(host),
            "下载白名单缺少 {host} —— 国内/海外将有一侧完全无法更新"
        );
    }

    // 浏览器兜底同样要挡住非 http 协议
    assert!(
        updater.contains("open_url_in_browser("),
        "open_release_page 必须走 os_shim 的 open_url_in_browser —— \
         用裸 Command 打开 URL 会闪出控制台黑框"
    );
    let os_shim = read(&desktop_src().join("os_shim.rs"));
    assert!(
        os_shim.contains("fn open_url_in_browser("),
        "os_shim 缺少 open_url_in_browser"
    );
    assert!(
        os_shim.contains("拒绝打开非 http(s) 链接"),
        "open_url_in_browser 必须拒绝非 http(s) 协议 —— \
         file:// 会把远端可控的 URL 变成任意本地执行入口"
    );
}

/// 换名接力的**回滚**不能被删掉。
///
/// 守卫：pnpm 版本必须**锁死**，且两份 workflow 一致。
///
/// ## 为什么不能写 `version: 9`
///
/// 浮动 major 意味着 pnpm 10 发布后 CI 自动升级到 10，而 pnpm 10 的
/// lockfile 格式与 9 **不兼容** —— `pnpm install --frozen-lockfile` 直接失败。
///
/// 报错说的是"lockfile 不满足 specifier"，现场看起来像**依赖写错了**，
/// 实际是工具链自己变了 —— 排查方向会被完全带偏。
///
/// 在 `ci.yml` 里这只是"检查挂了"；在 `release.yml` 里是**发版失败、
/// 版本打不出来**，而发版失败的现场同样看不出是工具链变了。
///
/// ## 为什么还要两份一致
///
/// 只改一份的话，CI 用 pnpm 10、发版用 pnpm 9（或反过来），于是
/// "CI 绿的版本"与"发版构建的产物"可能不是同一个 —— 而**没人会发现**。
#[test]
fn pnpm_version_must_be_pinned_and_identical_across_workflows() {
    let ci = workflow_src("ci.yml");
    let release = workflow_src("release.yml");

    // 抓 `pnpm/action-setup` 那一段里的 `version:` 值
    //
    // ⚠️ 必须**跳过注释行**。第一版没跳, 而我自己在注释里写了
    // "不能写 `version: 9`" —— 解析器命中的是那行注释, 于是断言
    // "找不到 version" 而失败。**注释里出现被检查的字面量, 就会污染检查。**
    // 这是写"源码级守卫"时反复踩的一类坑, 与其事后修解析器,
    // 不如从一开始就按"配置里有注释"这个事实来写。
    let grab = |src: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = src;
        while let Some(at) = rest.find("pnpm/action-setup") {
            // 只看 `uses:` 那一行的**缩进之内** —— 否则窗口会一路吃到下一个
            // step，把别处的 `version:` 也算进来。缩进相同且不是注释的行
            // 就是这个 step 的边界。
            let base_indent = rest[..at]
                .rsplit('\n')
                .next()
                .map(|l| l.len() - l.trim_start().len())
                .unwrap_or(0);
            for line in rest[at..].lines().skip(1) {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let indent = line.len() - trimmed.len();
                // `with:` 与 `uses:` **同级**（都 indent 8），子键才更深（10）。
                // 所以判据是"同级且不是 with:" 才算走到下一个 step。
                if indent <= base_indent && !trimmed.starts_with("with:") {
                    break;
                }
                if let Some(v) = trimmed.strip_prefix("version:") {
                    out.push(v.trim().trim_matches(['"', '\'']).to_string());
                }
            }
            rest = &rest[at + 1..];
        }
        out
    };

    let ci_v = grab(&ci);
    let rel_v = grab(&release);

    assert!(
        !ci_v.is_empty(),
        "ci.yml 里找不到 pnpm/action-setup 的 version —— 解析可能已失效"
    );
    assert_eq!(
        ci_v.len(),
        1,
        "ci.yml 里 pnpm 版本出现 {} 次（{ci_v:?}）—— 解析可能已失效",
        ci_v.len()
    );
    assert_eq!(
        rel_v.len(),
        1,
        "release.yml 里 pnpm 版本出现 {} 次（{rel_v:?}）—— 解析可能已失效",
        rel_v.len()
    );
    assert_eq!(
        ci_v[0], rel_v[0],
        "ci.yml 与 release.yml 的 pnpm 版本不一致: {} vs {} —— \
         『CI 绿的版本』与『发版构建的产物』可能不是同一个，而没人会发现",
        ci_v[0], rel_v[0]
    );

    // 必须是**具体版本** x.y.z，不能是浮动 major（"9"）
    let parts: Vec<&str> = ci_v[0].split('.').collect();
    assert_eq!(
        parts.len(),
        3,
        "pnpm 版本写的是 {:?} 而不是 x.y.z —— 浮动 major 会在 pnpm 10 发布后 \
         自动升级，而它的 lockfile 格式与 9 不兼容，--frozen-lockfile 直接失败",
        ci_v[0]
    );

    // 版本必须与 lockfile 配套（lockfileVersion 9.0 ↔ pnpm 9.x）
    let lock = read(&repo_root().join("ui").join("pnpm-lock.yaml"));
    let lock_line = lock
        .lines()
        .find(|l| l.trim_start().starts_with("lockfileVersion:"))
        .unwrap_or_else(|| panic!("ui/pnpm-lock.yaml 里找不到 lockfileVersion"));
    let lock_v = lock_line
        .trim()
        .trim_start_matches("lockfileVersion:")
        .trim()
        .trim_matches(['\'', '"']);
    let lock_major: u32 = lock_v
        .split('.')
        .next()
        .unwrap()
        .parse()
        .unwrap_or_else(|_| panic!("lockfileVersion {lock_v:?} 解析不出 major"));
    let pnpm_major: u32 = parts[0]
        .parse()
        .unwrap_or_else(|_| panic!("pnpm 版本 {:?} 的 major 解析不出数字", ci_v[0]));
    assert_eq!(
        lock_major, pnpm_major,
        "pnpm {} 与 lockfileVersion {} 不配套 —— \
         升级 pnpm 时必须同时重新生成 ui/pnpm-lock.yaml",
        ci_v[0], lock_v
    );
}

/// 守卫：仓库标识在**三处**必须一致（AGENTS.md 规则 5）。
///
/// ## 为什么这条必须自动化检查
///
/// AGENTS.md 规则 5 明文要求"改仓库名必须同步改这三处"，但三处散落在
/// 两个 workflow 和一个 Rust 常量区里，靠人记不住：
///
/// - `.github/workflows/sync-gitee.yml` —— 分支/Tag 镜像的**目标**
/// - `.github/workflows/release.yml` —— Release 与附件的**目标**
/// - `desktop/src-tauri/src/updater.rs` —— 客户端**检查更新的三条通道**
///
/// 改漏任何一处的后果都是**静默**的：镜像推到别处、发版传到别处、
/// 客户端永远停在旧版本且界面上看不出任何异常。没有一条会报错。
///
/// 所以规则 5 写在文档里不够，得有测试。
///
/// ## 附件名为什么也在守卫里
///
/// `Feisuo-win-x64.exe` 被客户端 `pick_asset` 精确匹配（找不到就不更新，
/// 而不是下错文件）。改附件名 = 所有在线用户永远解析不到附件。
#[test]
fn repo_identifiers_must_be_consistent_across_the_three_places() {
    let updater = updater_src();
    let sync = workflow_src("sync-gitee.yml");
    let release = workflow_src("release.yml");

    // ---- 1) 附件名：updater 是权威（客户端按它匹配）----
    let asset = updater
        .lines()
        .find_map(|l| l.trim().strip_prefix("const ASSET_NAME: &str = "))
        .map(|v| v.trim_end_matches(';').trim().trim_matches('"'))
        .unwrap_or_else(|| panic!("updater.rs 里找不到 ASSET_NAME"));
    assert!(
        release.contains(asset),
        "release.yml 里找不到附件名 {asset:?} —— \
         发版产出的文件名与客户端期望的不一致。客户端 `pick_asset` 按这个名字\
         精确匹配, 改名等于**所有在线用户永远更新不上**（不会下错文件, 只是找不到）"
    );
    assert!(
        release.contains(&format!("publish/{}", asset)),
        "release.yml 的暂存路径应形如 publish/{asset}"
    );

    let setup_asset = updater
        .lines()
        .find_map(|l| l.trim().strip_prefix("pub const SETUP_ASSET_NAME: &str = "))
        .map(|v| v.trim_end_matches(';').trim().trim_matches('"'))
        .unwrap_or_else(|| panic!("updater.rs 里找不到 SETUP_ASSET_NAME"));
    assert!(
        release.contains(setup_asset),
        "release.yml 里找不到安装包附件名 {setup_asset:?} —— \
         发版产出缺少安装包产物"
    );
    assert!(
        release.contains(&format!("publish/{}", setup_asset)),
        "release.yml 的暂存路径应形如 publish/{setup_asset}"
    );

    // ---- 2) 仓库标识：owner/repo 两两配对 ----
    // 从 updater.rs 抽出权威的一对
    let owner = updater
        .lines()
        .find_map(|l| l.trim().strip_prefix("const REPO_OWNER: &str = "))
        .map(|v| v.trim_end_matches(';').trim().trim_matches('"'))
        .unwrap_or_else(|| panic!("updater.rs 里找不到 REPO_OWNER"));
    let repo = updater
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("const REPO_NAME: &str = ")
                .map(|v| v.trim_end_matches(';').trim().trim_matches('"'))
        })
        .unwrap_or_else(|| panic!("updater.rs 里找不到 REPO_NAME"));

    // 三处都应出现 owner/repo 这一对
    for (label, src) in [("updater.rs", &updater), ("sync-gitee.yml", &sync), ("release.yml", &release)] {
        assert!(
            src.contains(&format!("{owner}/{repo}")),
            "{label} 里找不到仓库标识 {owner}/{repo} —— \
             与 updater.rs 不一致。改仓库名必须三处同步（AGENTS.md 规则 5）, \
             漏改的后果是**静默**的：镜像/发版传到别处, 或客户端永远查不到更新"
        );
    }

    // ---- 3) Gitee 是另一套标识（镜像目标）----
    // updater.rs 里有 Gitee 的 API 与页面地址；sync-gitee.yml 里的推送目标
    // 必须与它们同源，否则客户端查 Gitee、镜像推到别处。
    let gitee_api = updater
        .lines()
        .find_map(|l| l.trim().strip_prefix("const GITEE_LATEST_API: &str = "))
        .map(|v| v.trim_end_matches(';').trim().trim_matches('"'))
        .unwrap_or_else(|| panic!("updater.rs 里找不到 GITEE_LATEST_API"));
    // https://gitee.com/api/v5/repos/<owner>/<repo>/releases/latest
    // → 取 "/repos/" 之后的第一段, 它就是 "owner/repo"
    let gitee_repo = gitee_api
        .split("/repos/")
        .nth(1)
        .and_then(|s| s.split("/releases").next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            panic!("无法从 GITEE_LATEST_API 解析出 gitee owner/repo: {gitee_api}")
        });
    assert!(
        gitee_repo.contains('/'),
        "Gitee 标识 {gitee_repo:?} 里没有 '/' —— 解析逻辑与 URL 形态不符, 守卫本身失效"
    );
    assert!(
        sync.contains(&format!("gitee.com/{gitee_repo}")),
        "sync-gitee.yml 的推送目标与 updater.rs 的 Gitee 通道不一致 —— \
         客户端查 gitee.com/{gitee_repo}, 而镜像推到别处"
    );
}

/// Windows 上正在运行的 exe 不能被覆写, 所以更新走"改运行中 exe 的名 →
/// 新文件占位 → 重启"的接力。危险在于第 2 步失败: 此时原路径已经空了,
/// 没有回滚的话用户电脑上就**再也没有飞梭了**, 只能重装。
#[test]
fn rollover_must_rollback_on_failure() {
    let updater = updater_src();

    let fn_at = updater
        .find("fn apply_rollover(")
        .expect("找不到 apply_rollover");
    let body: String = updater[fn_at..].chars().take(1600).collect();
    assert!(
        body.contains("std::fs::rename(current_exe, backup)"),
        "apply_rollover 必须先把当前 exe 改名让出路径"
    );
    assert!(
        body.contains("let _ = std::fs::rename(backup, current_exe)"),
        "新版本换名失败时必须把旧程序换回原位 —— \
         没有回滚的话用户会直接失去飞梭, 只能重装"
    );
}

/// 换名接力的收尾必须在启动最早期执行。
///
/// 晚一步(Tauri 起来之后)就可能与引擎抢占同一批文件句柄, 表现为
/// "更新后偶发启动失败", 而且极难复现。
#[test]
fn rollover_cleanup_must_run_at_startup() {    let main_rs = read(&desktop_src().join("main.rs"));
    let updater = updater_src();

    assert!(
        updater.contains("pub fn apply_rollover_if_needed("),
        "缺少 apply_rollover_if_needed 收尾入口"
    );
    assert!(
        main_rs.contains("updater::apply_rollover_if_needed();"),
        "main.rs 必须调用 apply_rollover_if_needed —— \
         否则每次更新都会在程序目录留下 .new 残留"
    );

    // 必须在 load_or_default 之前: 配置加载会写盘, 与文件换名抢同一个目录
    let cleanup_at = main_rs
        .find("updater::apply_rollover_if_needed()")
        .expect("main.rs 未调用 apply_rollover_if_needed");
    let config_at = main_rs
        .find("AppConfig::load_or_default()")
        .expect("main.rs 未加载配置");
    assert!(
        cleanup_at < config_at,
        "apply_rollover_if_needed 必须在配置加载之前执行"
    );
}

/// 换名接力必须先让旧进程退出，再拉起新版本。
///
/// ## 这条守卫守的是一个"更新成功但程序打不开"的致命竞态
///
/// `tauri-plugin-single-instance` 在 Windows 上用一把**按 identifier
/// 命名**的内核互斥体做单实例判定（`net.findfine.feisuo-sim`，与版本号
/// 无关 —— `semver` feature 没启用）。旧进程还活着时 spawn 出来的新进程
/// 会在插件 setup 里发现互斥体被占 ⇒ 判定为"第二实例" ⇒
/// `std::process::exit(0)`，**新进程刚启动就自杀**；紧接着旧进程也退出。
///
/// 实测（旧顺序「先 spawn 再 app.exit」）：12/12 全部失败，
/// `survived=0 died=12`。用户看到的是"更新成功"，然后什么都没有 ——
/// 比"更新失败"更糟，因为失败还会给错误信息。
///
/// 正确顺序只能是「先退出旧进程 → 互斥体释放 → 再拉起」，
/// 而 `app.exit(0)` 之后本进程无法继续执行，所以必须交给外部中继。
#[test]
fn apply_must_relaunch_after_exit_not_before() {
    let src = updater_src();
    let start = src
        .find("pub async fn apply_pending(")
        .expect("找不到 apply_pending");
    let body_start = src[start..]
        .find('{')
        .map(|i| start + i)
        .expect("apply_pending 缺少函数体");
    let end = src[body_start..]
        .find("\n    async fn ")
        .map(|i| body_start + i)
        .unwrap_or(src.len());
    let whole = &src[body_start..end];

    // 只检查**绿色版换名接力那一支**。
    //
    // `apply_pending` 里有两个分支，各自都有自己的 `app.exit(0)`：
    // 安装版（启动 Setup 静默安装）在前，绿色版换名接力在后。
    // 安装版分支不需要中继 —— Inno Setup 自己会等待并接管，
    // 所以必须先把它切掉，否则"exit 之前的最后一个 exit"必然取到它。
    let relay_at = whole
        .find("spawn_relaunch_after_exit")
        .expect(
            "apply_pending 的绿色版分支没有使用退出后拉起的中继 —— \
             直接 spawn 新版本会被单实例互斥体判成第二实例并立刻退出",
        );
    let green = &whole[relay_at..];

    // 中继必须在**它所在的这一支**的 app.exit 之前注册。
    let exit_at = green.find("app.exit(0)").expect("绿色版分支缺少 app.exit");
    assert!(
        green.find("spawn_relaunch_after_exit").unwrap() < exit_at,
        "中继必须在 app.exit(0) 之前注册 —— 退出之后就执行不到任何代码了"
    );

    // 3) 中继实现本身：必须等待目标 PID 消失。
    let relay_fn = src
        .find("fn spawn_relaunch_after_exit(")
        .expect("缺少 spawn_relaunch_after_exit 实现");
    let relay_body: String = src[relay_fn..].chars().take(2600).collect();
    assert!(
        relay_body.contains("Wait-Process") && relay_body.contains("-Id"),
        "中继没有等待旧进程退出 —— 必须等互斥体释放再拉起，\
         否则新进程会被判成第二实例"
    );
    // 用 PowerShell 而不是 cmd：cmd 的 goto 只认行首标签，
    // `&:label&` 写法无效（实测会静默地什么都不做）。
    assert!(
        !relay_body.contains("goto wait"),
        "中继不要用 cmd 的 goto 循环 —— 实测 `&:label&` 在单行 cmd 里无效，\
         会导致更新后再也打不开程序"
    );
    assert!(
        relay_body.contains("powershell.exe"),
        "中继应使用 PowerShell（Wait-Process 精确等待）"
    );
    // 参数必须跟着传过去，否则开机自启的 `--daemon` 会丢，
    // 更新后窗口凭空弹出。
    assert!(
        relay_body.contains("args"),
        "中继没有转发启动参数 —— 开机自启的 --daemon 会丢"
    );
}

/// 安装版必须用**提权**方式启动安装包。
///
/// ## 这条守卫对应的实测结论
///
/// `spawn_detached` 走 `CreateProcess`，它**不触发 UAC**。而 Inno Setup
/// （本项目 `PrivilegesRequired=admin`）在发现自己不是管理员时会
/// **静默放弃**：实测非管理员下跑到静默安装结束，`Program Files` 里的
/// exe 时间戳纹丝不动，连一条安装日志都没产生。
///
/// 而这条分支随后就 `app.exit(0)` —— 用户看到"更新成功"、窗口关掉，
/// 程序还是旧版，且更新入口已经没了。
#[test]
fn installer_upgrade_must_request_elevation() {
    let src = updater_src();
    let at = src
        .find("let is_setup_pkg =")
        .expect("找不到安装包分支");
    let tail: String = src[at..].chars().take(2600).collect();

    assert!(
        tail.contains("spawn_elevated"),
        "安装包分支没有用提权启动 —— CreateProcess 不触发 UAC，\
         Inno 会静默放弃安装，用户看到「更新成功」但程序还是旧版"
    );
    assert!(
        !tail.contains("spawn_detached(&pending.local_path"),
        "安装包不能再走 spawn_detached（不触发提权）"
    );

    // os_shim 里必须真的用 ShellExecuteW + runas。
    let shim = read(&desktop_src().join("os_shim.rs"));
    let elev = shim
        .find("pub fn spawn_elevated(")
        .expect("缺少 spawn_elevated");
    let elev_body: String = shim[elev..].chars().take(2400).collect();
    assert!(
        elev_body.contains("ShellExecuteW"),
        "spawn_elevated 必须用 ShellExecuteW —— CreateProcess 无法提权"
    );
    assert!(
        elev_body.contains("runas"),
        "spawn_elevated 必须使用 runas 动词才会弹 UAC"
    );
    // 用户点"取消"（错误码 1223）要能被识别成"取消"，不是崩溃。
    assert!(
        elev_body.contains("1223"),
        "spawn_elevated 必须识别 UAC 取消（ERROR_CANCELLED = 1223）"
    );
}

/// 守卫：安装器**不得提供程序本身不支持的语言**。
///
/// ## 这条守卫防的是什么
///
/// `Feisuo.iss` 的 `[Languages]` 里曾并列 `english` 一行，看起来像
/// "安装器支持英文"。实际上只有**向导自己的按钮和提示**会变成英文，
/// 而下面这些全都不会跟着变：
///
/// - `AppName` / `DefaultGroupName` / 桌面与开始菜单快捷方式都叫「飞梭」；
/// - 防火墙那一步的 `StatusMsg` 是写死的中文；
/// - 装完之后程序本体（界面、托盘菜单、错误提示）没有任何英文。
///
/// 结果比不提供英文更糟：用户以为选到了英文版，装出来仍是一个中文程序，
/// 而且无从判断是自己选错了还是软件就这样。
///
/// ## 为什么是"只许中文"而不是"检查英文资源存不存在"
///
/// 英文版不是加一个 locale 文件就算完成的：前端约 470 条硬编码文案、
/// 后端 31 个返回 `Result<_, String>` 的命令里的中文错误串、托盘菜单、
/// 窗口标题、`<html lang>` 全都要双语，而且后端必须改成 `code + params`
/// 才能让前端翻译错误。这些**没法**靠一条 `contains()` 断言证明 ——
/// 硬凑一个"某个文件存在"的判据只会给出假绿灯，那比没有守卫更糟。
///
/// 所以这里只钉住**当前事实**：程序只有中文，安装器就只能提供中文。
/// 真做完英文版时，请连同这条守卫一起改 —— 改之前先确认上面那几处
/// 真的都双语了。
#[test]
fn installer_must_not_offer_languages_the_app_cannot_speak() {
    let iss = read(&repo_root().join("desktop").join("setup").join("Feisuo.iss"));

    // 取 [Languages] 段：到下一个 `[Section]` 为止（含行首缩进的情况）
    let head = iss
        .find("[Languages]")
        .expect("Feisuo.iss 里找不到 [Languages] 段 —— 安装包脚本结构变了, 守卫需重写");
    let tail = &iss[head + "[Languages]".len()..];
    let end = tail.find("\n[").map(|i| head + "[Languages]".len() + i).unwrap_or(iss.len());
    let block = &iss[head..end];

    // `Name: "chinesesimp"; MessagesFile: ...` / 也允许不带引号
    let declared: Vec<&str> = block
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            // 注释行不算：本段的注释里就提到了 `english` 这个词
            if t.starts_with(';') || t.starts_with('#') {
                return None;
            }
            let v = t.strip_prefix("Name:")?.trim();
            let name = match v.strip_prefix('"') {
                Some(q) => q.split('"').next().unwrap_or(""),
                None => v.split([';', ' ']).next().unwrap_or(""),
            };
            Some(name)
        })
        .filter(|s| !s.is_empty())
        .collect();

    assert!(
        !declared.is_empty(),
        "[Languages] 段里解析不出任何语言 —— 解析可能已失效, 先修守卫"
    );
    assert_eq!(
        declared,
        vec!["chinesesimp"],
        "安装器提供了程序本身不支持的语言: {declared:?}。\n\
         程序目前只有中文文案（`ui/index.html` 写死 lang=\"zh-CN\"、\
         托盘菜单与后端错误串全是中文），多出一个 language 行只会让用户\
         以为选到了英文版, 装出来却是中文程序。\n\
         要么删掉这一行, 要么先真的把程序做成双语（前端文案、托盘菜单、\
         窗口标题、后端错误串 code 化）, 再连同这条守卫一起改。"
    );

    // 语言文件必须真的在仓库里 —— CI 只校验了这一个文件存在,
    // 而安装包在本地手动编译时也要能编过。
    let msg_file = block
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with(';') && !t.starts_with('#')
        })
        .find_map(|l| {
            let at = l.find("MessagesFile:")?;
            let v = l[at + "MessagesFile:".len()..].trim();
            let q = v.strip_prefix('"')?;
            Some(q.split('"').next().unwrap_or("").to_string())
        })
        .expect("[Languages] 段里找不到 MessagesFile —— 解析可能已失效");
    assert!(
        !msg_file.starts_with("compiler:"),
        "MessagesFile 指向编译器内置 {msg_file:?} —— 那是英文向导文案, \
         与本项目只提供中文的事实不符"
    );
    let path = repo_root()
        .join("desktop")
        .join("setup")
        .join(msg_file.replace('\\', "/"));
    assert!(
        path.is_file(),
        "语言文件不存在: {} —— 本地手动编译安装包会直接失败",
        path.display()
    );

    // `#define` 了一个名字却从不使用 = 当初只起了个头。
    // `MyAppEnglishName` 就是这么留下的: 定义了 "Feisuo", 全文只此一处,
    // 于是它在 .iss 里看起来像"已经支持英文", 实际什么也没做。
    for line in iss.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("#define ") else { continue };
        let name = rest.split_whitespace().next().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let uses = iss.matches(name).count();
        assert!(
            uses >= 2,
            "`#define {name}` 定义了却从未被使用（全文只出现 {uses} 次）—— \
             这类死定义会让脚本看起来支持某个能力, 实际什么也没做。\
             要么用上它, 要么删掉"
        );
    }
}

/// 接力失败时, `.old` 是用户**唯一**的程序 —— 不得在没有替代品时删掉它。
///
/// ## 这个场景怎么来的
///
/// `apply_rollover` 的步骤 1 把当前 exe rename 成 `.old`(该映像**仍在运行**,
/// Windows 独占它, 所以此刻 `remove_file` 必然失败)。步骤 2 把新版本
/// 换到正式路径。
///
/// 若步骤 2 失败, 回滚会把 `.old` 换回正式路径 —— 正常情况下用户没事。
/// 但如果**回滚之后、进程退出之前**进程被杀(断电、任务管理器、崩溃),
/// 磁盘上就是:正式路径空着、`.old` 是唯一那份能跑的旧版本。
///
/// 下次启动走 `apply_rollover_if_needed`, 早先它**无条件**删 `.old` ——
/// 于是用户既没有正式程序、备份也被删了, 目录里只剩一个跑不起来的残骸。
///
/// 守卫要求: 删 `.old` 之前必须先确认正式路径上存在可执行文件。
#[test]
fn rollover_must_not_delete_sole_working_copy() {
    let src = updater_src();
    let start = src
        .find("pub fn apply_rollover_if_needed()")
        .expect("找不到 apply_rollover_if_needed");
    let body_start = src[start..]
        .find('{')
        .map(|i| start + i)
        .expect("apply_rollover_if_needed 缺少函数体");
    // 取到下一个顶层 `pub fn` 之前
    let end = src[body_start..]
        .find("\npub fn ")
        .map(|i| body_start + i)
        .unwrap_or(src.len());
    let body = &src[body_start..end];

    let delete_at = body
        .find("remove_file(backup)")
        .expect("apply_rollover_if_needed 里找不到对 .old 的清理");
    let guard_at = body
        .find("!current_exe.exists()")
        .expect(
            "apply_rollover_if_needed 无条件删除 .old —— \
             接力失败且进程被杀时, .old 是用户唯一能跑的副本, 删掉它就彻底没程序了。\
             必须在删除前确认 `current_exe.exists()`。",
        );

    assert!(
        guard_at < delete_at,
        "对 .old 的存在性检查必须**在**删除之前（现在是删除在前、检查在后）"
    );
    assert!(
        body.contains("tracing::warn!") || body.contains("warn!"),
        "放弃清理时必须说清楚为什么 —— 静默返回会让『程序不见了』变成无头案"
    );
}

/// 后台调度必须能关掉, 且有熔断。
///
/// 熔断的理由很实际: 用户拔了网线或公司防火墙拦截时, 没有熔断就是
/// 每 3 分钟一次无效网络请求, 笔记本指示灯常年亮, 笔记本电池被吃光。
#[test]
fn scheduler_must_respect_setting_and_have_circuit_breaker() {
    let updater = updater_src();
    let config = read(&Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("core")
        .join("src")
        .join("config.rs"));

    assert!(
        config.contains("pub auto_check_update: bool"),
        "AppConfig 缺少 auto_check_update —— 用户无法关闭自动检查"
    );
    assert!(
        updater.contains("MAX_RETRY_ATTEMPTS"),
        "缺少连续失败上限 —— 断网时会变成每 3 分钟一次无效请求"
    );
    assert!(
        updater.contains("fn spawn_scheduler("),
        "缺少后台静默检查调度"
    );

    let at = updater
        .find("fn spawn_scheduler(")
        .expect("找不到 spawn_scheduler");
    let body: String = updater[at..].chars().take(3200).collect();

    // 开关必须**每轮现读**，而不是启动时读一次。
    //
    // 早先它是值参数 `spawn_scheduler(auto_check: bool)`，只在 setup 里
    // 读一次配置 —— 用户在设置页关掉开关后，后台仍按旧周期每 12 小时
    // 跑一次；重新打开也要等下次启动。而界面上的开关**立刻**就翻过去了，
    // 于是"关了还在联网"在界面上完全看不出来。
    assert!(
        !body.contains("auto_check: bool"),
        "spawn_scheduler 又变回按值接收开关 —— 那样开关只在启动时生效，\
         用户改了设置必须重启程序"
    );
    assert!(
        body.contains("auto_check_update"),
        "调度循环没有从配置里读 auto_check_update —— 开关不生效"
    );
    assert!(
        body.contains("notified()"),
        "调度循环的长睡没有可被叫醒的通道 —— 关掉开关后仍要睡满一个周期\
         （最长 12 小时）才会真正停下"
    );
    assert!(
        body.contains("MAX_RETRY_ATTEMPTS"),
        "调度器必须实现熔断"
    );
    assert!(
        body.contains("NORMAL_INTERVAL") && body.contains("RETRY_INTERVAL"),
        "调度器必须区分常规间隔与重试间隔"
    );

    // 改配置的入口必须真的去叫醒它。
    let commands = read(&desktop_src().join("commands.rs"));
    assert!(
        commands.contains("notify_config_changed()"),
        "update_app_config 没有通知调度器 —— 开关改了也不会当场生效"
    );
}

/// 下载请求**绝不能**带总超时。
///
/// ## 这条守卫对应一个真实发出去过的 bug
///
/// v0.1.6 把下载请求改成了 `.timeout(API_TIMEOUT)`（6~8 秒），本意是
/// "补齐连接与头部超时控制"。但 reqwest 的请求级超时是**总超时**：
/// 它覆盖"从连接开始到响应体读完"（`TotalTimeoutBody`）。于是 22 MB 的
/// 绿色版 exe 需要约 3.7 MB/s 的持续速度才能下完——慢速网络下每次都在
/// 半途被掐断，重启重试还是一样：**自动下载整体失效**，而且它并不报错，
/// 只是永远停在失败态。
///
/// 正确的分工写在常量注释里:
/// - 响应头用 `DOWNLOAD_HEADERS_TIMEOUT`（连上了但不给响应头, 早失败早降级）;
/// - 响应体用 `DOWNLOAD_IDLE_TIMEOUT` 逐块兜底（只看有没有进展, 纵容慢速）。
#[test]
fn download_must_not_be_capped_by_api_timeout() {
    let updater = updater_src();
    let at = updater
        .find("async fn download(")
        .expect("找不到 download 实现");
    let tail = &updater[at..];
    let status_at = tail
        .find("let status = resp.status()")
        .expect("download 里找不到对响应状态的处理");
    let request_seg = &tail[..status_at];

    assert!(
        request_seg.contains("DOWNLOAD_HEADERS_TIMEOUT"),
        "下载请求必须用 DOWNLOAD_HEADERS_TIMEOUT 单独兜住『连上了但不给响应头』那一段"
    );
    assert!(
        !request_seg.contains(".timeout("),
        "下载请求上出现了总超时: reqwest 的请求级 timeout 覆盖整个响应体, \
         会把正常下载中的大文件在半途掐断 —— 表现是『发现新版本但下载永远失败』"
    );
    assert!(
        tail.contains("DOWNLOAD_IDLE_TIMEOUT"),
        "响应体必须保留逐块读超时（只看有没有进展）, 否则真正的停滞无人兜底"
    );
}

/// 整轮检查必须有**总预算**, 且超预算必须落到终态。
///
/// 单通道超时挡不住"某一段本身卡死"（DNS 挂起 / 系统代理黑洞 / TLS 假死）:
/// 各段超时都"正常"，但按钮永远显示「进行中…」，用户唯一能做的是重启程序。
#[test]
fn check_must_have_a_total_budget() {
    let updater = updater_src();

    assert!(
        updater.contains("const CHECK_BUDGET:"),
        "缺少检查总预算常量 —— 三级通道各自超时兜不住整体挂起"
    );
    assert!(
        updater.contains("tokio::time::timeout(CHECK_BUDGET, self.check_for_update())"),
        "check_and_download 必须用 CHECK_BUDGET 包住整轮检查"
    );

    // 超预算必须变成一次**失败**（而不是继续挂着 busy）
    let at = updater
        .find("tokio::time::timeout(CHECK_BUDGET")
        .expect("找不到总预算的落点");
    let body: String = updater[at..].chars().take(900).collect();
    assert!(
        body.contains("Err(") && body.contains("超时"),
        "超预算时必须返回可读的超时原因 —— 不然界面只会显示一句无意义的英文错误"
    );
}

/// 「上次检查时间」与"永不卡死"必须端到端接上。
///
/// 两者都跨了语言边界（Rust 快照字段 → TS 接口 → 界面文案/回调），
/// 任一环漏掉都不报错：界面只是永远不显示"上次检查"，或者永远停在
/// 「进行中…」——都属于"用户以为程序坏了、而代码静默通过"的一类。
#[test]
fn last_check_time_and_anti_stuck_must_be_wired() {
    let updater = updater_src();
    let config = read(&Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("core")
        .join("src")
        .join("config.rs"));
    let bridge = read(&ui_src().join("api").join("feisuoBridge.ts"));
    let app = read(&ui_src().join("App.vue"));

    // 1) 持久化字段: 重启后「上次检查」不能假装从未检查
    assert!(
        config.contains("pub last_update_check_utc"),
        "AppConfig 缺少 last_update_check_utc —— 重启后「上次检查」会退回『尚未检查』"
    );

    // 2) 后端: 记录时机 + 快照字段 + 序号
    assert!(
        updater.contains("async fn record_check_time("),
        "缺少记录『检查跑完时刻』的实现"
    );
    assert!(
        updater.contains("self.record_check_time().await"),
        "check_and_download 必须真的调用 record_check_time"
    );
    assert!(
        updater.contains("last_checked_at_ms"),
        "状态快照没带 last_checked_at_ms"
    );
    assert!(
        updater.contains("pub seq: u64"),
        "状态快照缺 seq —— 前端没有办法丢弃迟到的旧快照"
    );

    // 3) 桥接层: 字段名与 Rust 侧 camelCase 对齐（对不齐就是 undefined）
    assert!(
        bridge.contains("lastCheckedAtMs") && bridge.contains("seq: number"),
        "feisuoBridge.ts 的 UpdateStatus 缺 lastCheckedAtMs / seq 字段"
    );

    // 4) 界面: 显示「上次检查」+ 丢弃旧快照 + 看门狗
    assert!(
        app.contains("上次检查：") && app.contains("lastCheckedText"),
        "设置页没有「上次检查」文案"
    );
    assert!(
        app.contains("s.seq < updateStatus.value.seq"),
        "更新状态事件回调缺少 seq 判断 —— 迟到的旧快照会把终态盖回「进行中…」"
    );
    assert!(
        app.contains("function armUpdateWatchdog("),
        "缺少看门狗: 后端一个字节都不回时, 按钮会永远停在「进行中…」"
    );
    // 看门狗自己必须带超时。
    //
    // 早先后端 `publish` 死锁时，`get_update_status` 命令也卡在取锁上，
    // 于是看门狗那个 `await` 永不返回 —— 最后一道防线跟着一起失效。
    // 判据: 看门狗函数体里必须出现 Promise.race（或等价的超时手段）。
    let wd_at = app
        .find("function armUpdateWatchdog(")
        .expect("App.vue 缺少 armUpdateWatchdog");
    let wd_body: String = app[wd_at..].chars().take(2600).collect();
    assert!(
        wd_body.contains("Promise.race("),
        "看门狗询问后端的 await 没有超时 —— 后端一旦卡在锁上，\
         看门狗也会永远挂住，用户只能重启程序"
    );

    // 同一条防线要覆盖到真正的调用点：
    //  - `loadUpdateStatus` 会在切设置页时被 await，不能拖住整页；
    //  - `checkForUpdate` 的返回值 await 不返回时，finally 不执行、按钮永久禁用。
    let load_at = app
        .find("async function loadUpdateStatus(")
        .expect("App.vue 缺少 loadUpdateStatus");
    let load_body: String = app[load_at..].chars().take(1400).collect();
    assert!(
        load_body.contains("Promise.race("),
        "loadUpdateStatus 没有超时 —— 后端卡住时切到设置页会整页卡住"
    );

    let chk_at = app
        .find("async function checkForUpdate(")
        .expect("App.vue 缺少 checkForUpdate");
    let chk_body: String = app[chk_at..].chars().take(1800).collect();
    assert!(
        chk_body.contains("Promise.race("),
        "checkForUpdate 的 checkForUpdate(true) 调用没有超时 —— \
         后端不返回时 finally 不执行，按钮永久停在「进行中…」"
    );
    let at = app
        .find("async function checkForUpdate(")
        .expect("App.vue 缺少 checkForUpdate");
    let body: String = app[at..].chars().take(2000).collect();
    assert!(
        body.contains("armUpdateWatchdog()"),
        "手动「检查更新」必须武装看门狗 —— 这是界面卡死的最后一道防线"
    );
}

/// `publish` 的参数里**不许**再取 `self.status` 的锁。
///
/// ## 这条守卫守的是一个把整个更新子系统锁死的死锁
///
/// ```ignore
/// self.publish(UpdateStatus { ..self.status.lock().await.clone() }).await;
/// ```
///
/// 看起来完全正常，实际必然死锁。原因**不是**业务逻辑：Rust 的临时量
/// 生命周期规定，`..expr` 里的 `MutexGuard` 是个临时量，它的 drop 在
/// **整条语句**结束时才发生 —— 而这条语句包含 `publish(...).await`。
/// `publish` 内部第一件事就是取同一把 `status` 锁 ⇒ 自己等自己，永不返回。
///
/// 后果不是"更新偶尔失败"，是**更新从来没有成功过**：
/// - `note_stage()` 是检查流程的第一个动作，它一挂，网络请求根本没发出；
/// - 日志里连一行 `开始检查应用更新` 都没有（那行在 `note_stage` 之后）；
/// - `record_check_time()` 也轮不到跑，「上次检查」永远是「尚未检查」；
/// - `get_update_status` 命令也要取同一把锁 ⇒ 前端 45 秒看门狗
///   连"问一下后端"都做不到，界面永久停在「正在检查更新… · 进行中…」。
///
/// 编译期完全无感（`await` 之后仍是合法代码），clippy 也不报。
/// 所以只能靠这条守卫把它钉住：派生快照必须走 `publish_from`。
#[test]
fn publish_must_not_nest_the_status_lock() {
    let updater = updater_src();

    // 把注释剥掉 —— 本文件的说明文字里就写着这个反例，
    // 不剥的话守卫会对着自己的文档报警。
    let mut code = String::with_capacity(updater.len());
    let mut in_block = false;
    for line in updater.lines() {
        let t = line.trim();
        if in_block {
            if t.contains("*/") {
                in_block = false;
            }
            continue;
        }
        if t.starts_with("/*") {
            if !t.contains("*/") {
                in_block = true;
            }
            continue;
        }
        if t.starts_with("//") {
            continue;
        }
        code.push_str(line);
        code.push('\n');
    }

    // 把 `publish` 的调用逐个切出来看参数里有没有 `status.lock()`。
    // 只匹配**调用点**（`self.publish` / `.publish(`），不匹配定义
    // （`async fn publish(`）—— 定义里当然要取锁，那是它的职责。
    let mut offenders: Vec<String> = Vec::new();
    for (idx, _) in code.match_indices(".publish(") {
        // 定义处是 `async fn publish(`，前面没有点号；这里已经要求 `.publish(`
        // 所以天然排除。再排除 `publish_from`（它没有的，名字不同）。
        let tail = &code[idx..];
        // 取到配对的右括号，参数可能跨多行
        let mut depth = 0i32;
        let mut end = None;
        for (i, c) in tail.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { continue };
        let args = &tail[..=end];
        if args.contains("status.lock()") || args.contains("self.status.lock") {
            let line_no = code[..idx].lines().count();
            offenders.push(format!("第 {line_no} 行: {}", args.lines().next().unwrap_or("").trim()));
        }
    }

    assert!(
        offenders.is_empty(),
        "发现 `publish(...)` 的参数里取了 `self.status` 的锁 —— 这必然死锁，\
         而且会把整个更新功能锁死（连前端看门狗一起挂住）：\n  {}\n\
         正确写法：`self.publish_from(|cur| UpdateStatus {{ ..cur.clone(), .. }}).await`，\
         或先把 `self.status.lock().await.clone()` 绑到变量上再用。",
        offenders.join("\n  ")
    );

    // 正面的锚点：安全入口必须存在，否则上面的断言可以靠"什么都不写"通过。
    assert!(
        updater.contains("async fn publish_from<F>("),
        "缺少 publish_from —— 派生快照的唯一安全入口"
    );
    assert!(
        updater.contains("let snapshot = self.status.lock().await.clone();"),
        "publish_from 必须先在**独立语句**里取快照（分号结束即释放锁），\
         不能在调用 publish 的同一句里展开"
    );
}

