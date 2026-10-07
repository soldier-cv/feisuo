# 飞梭 (Feisuo) · 多端无感局域网传输

> **核心体验**：**开机自启 · 入网自连 · 一次信任 · 无人值守 · 极速互传**  
> 重点覆盖 **Windows 桌面端** 与 **Android 安卓端**。

---

## 📖 原型演示使用说明 (`index.html`)

直接在现代浏览器中双击或打开 `index.html` 即可预览完整多端交互原型。

> ⚠️ **本节描述的是交互原型，不是已交付产品的功能清单。**
> 原型里的设备发现、传输进度、速率数字、接收落盘通知**全部是前端模拟**，
> 不代表 `desktop/` 与 `mobile/` 里的真实实现（例如"90~110 MB/s"这个数字
> 从未做过端到端实测）。真实实现状态请以
> [`FRAMEWORK_DESIGN.md`](FRAMEWORK_DESIGN.md) 里的"设计 vs 实际实现"对照表为准。

顶部导航栏支持三重视角模式自由切换：
1. **🖥️ Windows 桌面端模式**：
   - 体验 Windows 11 Fluent/Mica 材质、开机自启常驻托盘守护；
   - 点右上角关闭按钮窗口最小化至托盘，后台持续待命；
   - 体验文件拖拽直发、双栏目录文件穿梭取回；
   - 实时局域网传输速率监控（动态测速跳动 90~110 MB/s）。
2. **📱 Android 安卓端模式**：
   - 旗舰手机打孔屏沉浸式状态栏与 Material 3 现代卡片；
   - 展示 Android 系统**前台守护服务 (Foreground Service)** 状态栏常驻通知；
   - 支持系统开机自启广播 (`BOOT_COMPLETED`) 与电池 Doze 优化白名单说明；
   - 手机一键发照片、发文件，以及“扫一扫”模拟扫描电脑二维码快速互信。
3. **🔄 双端联动演示模式 (Twin View)**：
   - 同屏并列展示 Windows 与 Android 双端；
   - 在手机端点“发最新相片”或电脑端拖拽文件，对侧即时同步触发接收进度条、极速传输动效与系统自动落盘通知！

---

## 🛠️ 技术框架选型与落地方案

飞梭的技术方案白皮书已整理至 [FRAMEWORK_DESIGN.md](file:///d:/dev/agent-repos/feisuo/FRAMEWORK_DESIGN.md)。

### 推荐技术选型：**Tauri v2 + Rust 核心网络传输 + 响应式现代 Web 界面**
- **内存占用极小**：常驻后台仅约 **20MB ~ 35MB**（相比 Electron 节省 85% 内存），自启毫无性能负担；
- **安装包体积小**：全平台仅约 **15MB ~ 25MB**；
- **一套 UI 多端复用**：桌面端呈现 Fluent 窗口，移动端自适应呈现沉浸式交互。

> ⚠️ **关于吞吐**：没有 QUIC、没有零拷贝、没有多路复用。数据通道是
> `tokio::net::TcpStream` + 4 MiB 分块，**端到端吞吐从未实测过**，
> 网上流传的「110MB/s+」没有任何数据支撑。详见
> [FRAMEWORK_DESIGN.md 第 6 节](file:///d:/dev/agent-repos/feisuo/FRAMEWORK_DESIGN.md) 的实现状态对照表。

### 核心机制
- **开机自启**：Windows 任务计划/注册表 `--daemon` 静默启动；Android `BOOT_COMPLETED` 广播拉起前台服务；
- **零配置自连**：基于**签名 JSON 信标**的组播/广播发现与历史 IP 单播回呼。
  ⚠️ 不是 mDNS/DNS-SD；「开机 50ms 内自连」也未达成未实测，
  实际受 3 秒广播间隔与 Wi-Fi 关联耗时支配；
- **免密无人值守**：首次通过 6 位 PIN 交换 Ed25519 长期公钥，之后每次局域网碰头通过双向签名静默放行，文件直接写入预设目标文件夹，无需手动点击确认；
- **传输安全边界**：仅允许已配对设备传输，每帧关键字段（传输清单、分块头）带签名，
  落盘后整文件 BLAKE3 复核。
  ⚠️ **数据体本身不加密**（无传输层 TLS）—— 同网段被动嗅探者能读到文件内容。

---

## 📐 能力边界（请先读这一节）

飞梭**不做中继、不做中间注册服务**。这是一条明确的架构决定，不是待办。
它划出的边界如下，全部是白纸黑字的承诺：

| 场景 | 能不能用 | 靠什么 |
| --- | :---: | --- |
| 同一局域网 / 同一个 Wi-Fi | ✅ | 组播/广播发现 + 直连。**不需要装 ZeroTier** |
| 跨网络（家里 ↔ 公司） | ✅ | **两边都装 ZeroTier**，飞梭走覆盖网地址 |
| 跨网络 + 双方都在 NAT 后面 | ⚠️ | 需要覆盖网（ZeroTier）。见下方硬边界 |

**"不做中继"不等于"没有中继"**：只要用 ZeroTier，ZeroTier 的
planet / root 节点就是中继，只不过是用别人的，飞梭自己不建。

### 硬边界：有些情况下连不上，且这不是"还没做"

如果两台设备**都在对称型 NAT 后面**、又没有覆盖网，飞梭连不上。
原因是网络层面的，不是产品层面的：

- 发现机制（广播 / 单播 / 手动输 IP）解决的都只是**"找到地址"**；
- 而对称型 NAT 下，拿到的公网 IP 是**共享的、不可达的**；
- 唯一能解的是中继，而中继已被明确排除。

遇到这种情况请直接用 ZeroTier，而不是等飞梭"将来支持"。

### 落点边界：写入永远在收件目录之内

「允许写」的语义是**允许写，但落点强制在收件目录**。

- 「双栏穿梭」可以把文件送到**对方收件目录下的某一层**（跟着对方地址栏走）；
- 但无法写到收件目录**之外**。对方浏览真实磁盘（`C:\` / `D:\`）时，
  穿梭会退回收件根并明确提示原因；
- 系统与程序目录（`C:\Windows`、`C:\Program Files` 等）**永远**不可被浏览或取回。

---

## 🚀 手动编译

> 项目里原先的 `build.ps1` / `mobile/android/build-native.ps1` 已按用户要求删除，
> 改为手动执行下面的命令。

### Windows 桌面端（绿色版）

```powershell
# 0) 依赖（首次）
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android   # 仅 Android 需要
cargo install tauri-cli --version "^2" --no-default-features --features rustls  # 若尚未安装 cargo tauri

# 1) 前端：类型检查 + 打包（产物 ui/dist）
pnpm --dir ui install
pnpm --dir ui build

# 2) 桌面端绿色版单文件 exe（产物 target\release\feisuo-desktop.exe）
cargo tauri build --no-bundle
```

### 发布产物改名

`release.yml` 把产物上传为 `Feisuo-win-x64.exe`，客户端 `updater.rs` **按名精确匹配**
（AGENTS.md 规则 7）。本地手动改名即可：

```powershell
Copy-Item target\release\feisuo-desktop.exe target\release\Feisuo-win-x64.exe
```

### 三个守卫（原来由 build.ps1 自动做，请手动过一遍）

1. **版本号一致性** —— `Cargo.toml` 与 `tauri.conf.json` 必须一致
   （不一致会让"检查更新"永久失灵）
2. **绿色版体积下限 1 MB** —— 低于此说明构建残缺
3. **产物文件名** —— 必须是 `Feisuo-win-x64.exe`

一条命令过完三个：

```powershell
# 1) 版本号一致
$ct = (Select-String -Path Cargo.toml -Pattern '^(name|version)\s*=' | Where-Object { $_.Line -match 'version' })
$tc = (Get-Content desktop\src-tauri\tauri.conf.json -Raw | ConvertFrom-Json).version
"Cargo.toml      = $ct"
"tauri.conf.json = $tc"
if ($ct -match 'version\s*=\s*"([^"]+)"' -and $Matches[1] -ne $tc) { throw "版本号不一致！" }

# 2) 体积下限 1MB
$exe = 'target\release\feisuo-desktop.exe'
if (Test-Path $exe) { "{0:N1} MB" -f ((Get-Item $exe).Length/1MB) } else { "还没构建" }
```

### 只做检查（不产出）

```powershell
pnpm --dir ui build                                # 含 vue-tsc 类型检查
cargo check --workspace --all-targets
cargo test --workspace
```

### Android 原生库交叉编译

linker 路径必然含 NDK 的安装位置与版本，每台机器都不同，所以 `.cargo/config.toml`
属于**构建产物**（已在 `.gitignore`），必须现场生成：

```powershell
# 1) 确认 NDK（装法：sdkmanager "ndk;30.0.16248370"）
$ndk = "$env:LOCALAPPDATA\Android\Sdk\ndk\30.0.16248370"
Test-Path "$ndk\source.properties"   # 必须是 True

# 2) 现场生成 .cargo/config.toml（arm64-v8a；换 ABI 改 $triple 与 linker 名）
$triple = 'aarch64-linux-android'; $api = 21
$bin    = "$ndk\toolchains\llvm\prebuilt\windows-x86_64\bin"
$linker = "$bin\aarch64-linux-android$api-clang.cmd"
$llvmAr = "$bin\llvm-ar.exe"

# 路径必须用 TOML **单引号** literal string：双引号里 `\` 是转义符，
# `C:\Users\...` 的 `\U` 不是合法转义，报错还指不到真正原因。
New-Item -ItemType Directory -Force .cargo | Out-Null
@"
[target.$triple]
linker = '$linker'
ar = '$llvmAr'
rustflags = ["-C", "link-arg=-Wl,-z,max-page-size=16384"]

[env]
CC_$(($triple -replace '-','_').ToUpper()) = '$linker'
AR_$(($triple -replace '-','_').ToUpper()) = '$llvmAr'
"@ | Set-Content .cargo\config.toml -Encoding utf8

# 3) 交叉编译并把 .so 放进 jniLibs
cargo build -p feisuo-core --target $triple --profile release
New-Item -ItemType Directory -Force mobile\android\app\src\main\jniLibs\arm64-v8a | Out-Null
Copy-Item "target\$triple\release\libfeisuo_core.so" mobile\android\app\src\main\jniLibs\arm64-v8a\ -Force

# 4) 打 APK
cd mobile\android
.\gradlew.bat assembleDebug --console=plain      # debug APK
.\gradlew.bat assembleRelease --console=plain    # release APK
```

> `.so` 已在 `jniLibs` 里时可跳过第 3 步直接打 APK。
>
> **CI 里这段是内联在 `.github/workflows/ci.yml` 的
> 「Cross-compile libfeisuo_core.so」步骤中**（CI 的 NDK 路径是已知确定的，
> 不需要上面的探测）。

### 本地检查清单

`cargo test --workspace` 请在本地跑；下面是几项**秒级**的只读检查，
提交前顺手过一遍：

```powershell
# 前端类型检查
cd ui; npx vue-tsc --noEmit; cd ..

# 引擎到端到端冒烟（16 节，自建临时收发两端，不碰生产数据）
cargo run -p feisuo-core --example smoke_transfer

# invoke 参数 / Tauri 命令签名对齐
python scripts\audit-invoke-args.py --selftest   # 先自检工具本身
python scripts\audit-invoke-args.py --strict

# CSS 游离声明（落在规则块之外的裸声明）
python scripts\check-css-stray-declarations.py

# 两套主题的前景/底色对比度（已做 alpha 合成）
python scripts\check-contrast.py

# 入口一致性：同一个动作在不同入口下不许有行为差异
python scripts\audit-ui-invariant.py --selftest   # 先证明守卫自己没坏
python scripts\audit-ui-invariant.py

# 检查脚本自身的 stdout 编码加固（见下）
python scripts\check-ci-encoding.py
```

`scripts/migrations/` 下是**已完成的一次性迁移脚本**（圆角收敛、软底色对比度、
硬编码颜色清理），保留是为了记住"为什么是这个值"。**请勿重跑** —— 令牌已经
收敛，重跑会把结果改回发散状态。真正在跑的检查是 `theme_guard.rs` 与
`check-contrast.py`。

`scripts/generate_perfect_icons.py` 不一样：它是图标生成的**源**，改设计后
可以重跑，不属于一次性迁移。

JNI 契约守卫不在 `scripts/` 下，而在 `desktop/src-tauri/src/jni_guard.rs`
（随 `cargo test` 跑）：它比对 Rust 的 `#[no_mangle] extern "system"` 导出与
Kotlin 的 `external fun` 声明 —— **方法名与参数个数都要一致**（Rust 侧已扣掉
JNI 规范固定的 `JNIEnv`/`jclass`），并用 `llvm-nm` 核对 `.so` 的实际导出符号
（防止"写了 `#[no_mangle]` 却被链接器丢掉"）。

### 为什么每个检查脚本都要 `force_utf8_stdout()`

这些脚本的输出全是中文（给读 CI 日志的人看）。Python 在 Windows 上默认用
**控制台代码页**编码 stdout：

- 中文 Windows：cp936（GBK）—— 没事；
- **英文 Windows / GitHub 的 `windows-latest` runner：cp1252** ——
  `print("对比度检查")` 直接抛 `UnicodeEncodeError`，退出码 1。

后果是 **CI 里 5 个检查步骤全红，而被检查的代码完全正确**。这类失败最难查：
日志里只有一个编码异常，看不出跟被检查的代码有没有关系。本项目已实测复现
（强制 `PYTHONIOENCODING=cp1252` 跑 `check-contrast.py` 即崩）。

所以每个脚本在 import 之后立刻调 `scripts/_ci_utf8.py` 的
`force_utf8_stdout()`。`ci.yml` 里的 `PYTHONUTF8=1` 只是 belt-and-braces ——
它只覆盖 CI，而这批脚本还要能在开发机上手动跑、换一台英文系统就炸。
`check-ci-encoding.py` 钉住"没有脚本漏掉加固"，且要求加固调用**早于**
`def main(`。

`audit-invoke-args.py` 值得单独说明：Tauri v2 的 `invoke` 按**已声明的参数名**
反序列化，**多余的 key 被静默丢弃** —— 不报错、不警告、不进日志。
所以"前端加了参数、后端忘了加"在运行期完全隐形，症状是"功能没生效"，
而不是任何可被发现的错误。本项目真实中过一次：
`send_files` 少声明 `dest_sub_path`，穿梭发送"落到对方地址栏当前目录"
因此从未在桌面端生效过，界面上却看不出任何异常。

这个脚本把两边的参数名做差集。**先跑 `--selftest`**：它的输出是
"没发现问题"，而一个坏掉的解析器恰好也输出"没发现问题"——
那种情况下它比没有更糟。

`check-css-stray-declarations.py` 查的是"落在规则块之外的裸声明"。
这类问题 `vite build` 只报 **warning**，而 CSS 解析器会**丢弃**它并继续
解析 —— 构建成功、界面大体正常，只有那一条规则静默失效。
症状是"只有某个交互不对"，极难联想到是语法坏了。

`check-contrast.py` 查两套主题的常驻前景/底色对比度，**做了 alpha 合成**：
把 `rgba(…)` 当不透明色算会得到一堆 `1.00` 的荒谬结果，一屏假警报
等于没有检查（本项目初版就报了 36 处"低于 AA"，绝大多数是这类误报）。
阈值分两档且写明依据：正文 4.5:1，图标与大号文字 3:1。

`audit-ui-invariant.py` 查的是**入口之间的一致性**：准入检查、撤销窗口（含**位置**）、
落点传递、失败时不谎报、清空清单只清本次、三个直发入口都按 `hasDirectory` 分派。
这个项目反复出现的失效模式不是"某个功能坏了"，而是**同一个动作在不同入口下
行为不同** —— 这类差异不会报错也不会崩，界面照样显示"发送成功"。

已中过的五例：准入检查四份拷贝（两份缺 `pending`/`blocked`/`offline`）、落点三条
路径各带各的（一条压根没带，导致"落到对方地址栏当前目录"从未生效）、撤销窗口只有
一条路径有、拖到设备卡片忽略用户拖了什么而发整个待发清单、拖到设备卡片不判目录
（项目目录当场发走）。

**先跑 `--selftest`**。这个守卫自己第一版有四个 bug，一个缺陷都抓不到，其中
一个假阳性报出的句子与真缺陷**完全相同**。它现在喂 11 个合成样例，每条检查都
必须能红 —— 不然一个解析失效的守卫会一直绿，而 CI 显示一切正常。

JNI 契约守卫（`jni_guard.rs`）查的是同一类问题的 Android 版本：符号名对得上
但**参数个数**错位时，JNI 不会报任何错，它按调用方的栈取参数，于是读出垃圾值
—— 表现是"某个参数永远是 0 / 空串 / null"，极难联想到是签名问题。所以它比对
方法名**与**参数个数，并另有一条测试确认**收集器本身没坏**（否则两边都收到空集
→ 循环不执行 → 参数检查恒绿，而"没发现问题"正是解析失效时的输出）。

### 签名密钥

Android release 签名**不入库**。两种配置方式，任选：

```powershell
# 方式 A：properties 文件
copy mobile\android\keystore.properties.example mobile\android\keystore.properties

# 方式 B：环境变量（CI 推荐，密钥和口令都不落盘）
$env:FEISUO_KEYSTORE = "D:\path\feisuo-release.jks"
$env:FEISUO_KEYSTORE_PASSWORD = "..."
$env:FEISUO_KEY_ALIAS = "feisuo"
$env:FEISUO_KEY_PASSWORD = "..."
```

**没配密钥不会构建失败** —— `assembleRelease` 回退到 debug 签名，产物名带
`-debugkey` 后缀，可安装自测但不能上架。故意做成这样：把"没配密钥"
做成构建失败，会让人以为 release 构建坏了，而实际问题只是"你还没创建密钥"。

### Android 的能力边界（与桌面端不同，别按桌面端预期）

| | 桌面端 | Android |
| --- | --- | --- |
| 分享到飞梭 → 发送 | ✅ | ✅ |
| 拖拽直发 / 穿梭双栏 | ✅ | ❌ 未做界面 |
| 指定落点子目录 | ✅ | ❌ 恒为对方收件根 |
| 人工审批窗口 | ✅ | 仅通知栏（显示匹配码） |
| 配对入口 | ✅ | ✅ |
| 前台保活 / 开机自启 | 托盘常驻 | ✅ |

「指定落点」在 Android 上是**能力缺失**而不是 bug：Android 端的发送入口
只有"系统分享到飞梭"一条，而分享没有"对方地址栏当前在哪一层"这个概念。
补它需要先做穿梭界面 —— 那是另一个决定，不该由分享链路顺带打开。

---

## 🔄 检查更新与升级

- 桌面端内置版本检查，**Gitee 优先、GitHub 降级**（国内 GitHub API 常超时）。
  触发方式：托盘右键「检查更新…」或「系统设置 → 关于与更新」。
  静默调度为启动后 8 秒首次检查、之后每 12 小时一次。
- **版本号只有一个来源：`Cargo.toml` 的 `CARGO_PKG_VERSION`**。
  任何位置不得写死版本字符串 —— `desktop/src-tauri/src/update_guard.rs`
  有守卫强制这一点。
- 绿色版是**单文件、免安装**。升级 = 下载新 `Feisuo-win-x64.exe` 覆盖旧文件。
  信任库、配对记录、设置都在 `%LOCALAPPDATA%\feisuo\`，**升级不会丢**。
- 升级前建议先用「系统设置 → 导出诊断报告」留一份基线，
  升级后如果传输行为有变化，可以拿两份报告对比。

### 三条通道与它们的失效表现

| 通道 | 用途 | 挂了会怎样 |
| --- | --- | --- |
| Gitee Release | 国内主要路径 | 自动降级到 GitHub，通常只是慢 |
| GitHub Release | 降级路径 | 国内常超时；已降级也失败才提示 |
| 手动下载 | 兜底 | 覆盖 `Feisuo-win-x64.exe` 即可 |

- 客户端**精确匹配附件名** `Feisuo-win-x64.exe`。改名（加版本号、换成 zip）
  会让所有在线用户永远解析不到附件 —— 表现是"永远提示已是最新"。
  `update_guard::current_version_must_come_from_cargo_pkg` 会拦本地漂移，
  但**拦不住远端改名**，那只能靠发版流程守。
- 仓库地址硬编码在三处：`.github/workflows/sync-gitee.yml`（分支/Tag 镜像）、
  `.github/workflows/release.yml`（Release 与附件）、
  `desktop/src-tauri/src/updater.rs`（客户端检查更新）。**改仓库名要同时改这三处。**
- `GITEE_TOKEN` 必须在 GitHub 仓库 Secrets 里配好，否则镜像同步会**硬失败**。
  这是有意的：静默跳过会让"镜像没生效"变成几个月后才发现的事。
  （`release.yml` 里的 Release 同步是 `continue-on-error` 降级，只发 warning。）

### 排障时先看哪里

| 想知道 | 去哪 |
| --- | --- |
| 传输为什么慢 / 失败 | 界面「传输记录 → 导出诊断报告」，或 `%LOCALAPPDATA%\feisuo\metrics.prom` |
| 设备为什么搜不到 | `%LOCALAPPDATA%\feisuo\logs\feisuo.log`，搜 `[发现] 发送计划` |
| 被谁请求过、拒绝过 | 诊断报告的「安全事件」一节 |

诊断报告的设计目标是**别人拿到后不必再追问**：它带版本号、抬头、
成功/失败条数、逐条归因、链路画像。
