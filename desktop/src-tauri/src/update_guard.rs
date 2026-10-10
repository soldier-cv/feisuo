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
    let after: String = step[curl_at..].chars().take(600).collect();
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
fn rollover_cleanup_must_run_at_startup() {
    let main_rs = read(&desktop_src().join("main.rs"));
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
    let body: String = updater[at..].chars().take(2400).collect();
    assert!(
        body.contains("auto_check"),
        "调度器必须读 auto_check 配置 —— 否则关不掉"
    );
    assert!(
        body.contains("MAX_RETRY_ATTEMPTS"),
        "调度器必须实现熔断"
    );
    assert!(
        body.contains("NORMAL_INTERVAL") && body.contains("RETRY_INTERVAL"),
        "调度器必须区分常规间隔与重试间隔"
    );
}
