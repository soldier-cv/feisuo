# 飞梭 (Feisuo) · Agent 开发规范与协作准则 (AGENTS.md)

> 本文档是飞梭项目的核心工程宪章与 AI / 开发者协作规范。在参与本项目的一切设计、编码、测试和维护工作时，必须严格遵守本文档所规定的所有约束与流程。

---

## 1. 项目定位与核心产品哲学

飞梭 (Feisuo) 是一款面向 **Windows 桌面端** 与 **Android 移动端** 的极致轻量、零配置、无人值守局域网传输工具。

### 核心体验准则
1. **启动与生命周期**：
   - Windows 桌面端支持登录静默启动并常驻系统托盘，关窗不退出；
   - Android 移动端按轻量无打扰设计**去常驻后台、去开机自启**：随应用打开启动服务，退出或划掉卡片彻底退出；运行期间（含锁屏）持有 `MulticastLock` 确保秒级互联与传输不中断；
   - 基于 **mDNS (Multicast DNS)** 与历史 IP 缓存并发探测，局域网内开机 50ms 内瞬间完成自连握手。
2. **一次配对，终生免密**：
   - 首次绑定通过扫码或 6 位 PIN 码交换 Ed25519 设备指纹；
   - 之后局域网相遇基于非对称签名进行双向静默认证（Mutual Auth），全程零弹窗确认。
3. **无人值守，安全落盘**：
   - 受信设备发送的文件直接写入预设目标文件夹（Windows: `~/feisuo/`，Android: `/sdcard/Download/feisuo/` 或相册自动注册）；
   - 同名文件自动增加序号递增保护（如 `file (1).ext`），绝不覆盖原有数据。
4. **极速高吞吐**：
   - 底层由 Rust + QUIC / TCP 零拷贝多路复用驱动，榨干千兆局域网与 Wi-Fi 6 极限吞吐（110MB/s+）。

---

## 2. 核心纪律与操作红线 (CRITICAL RULES)

### 🔴 规则 1：Git 提交纪律与提交信息规范 (最高约束)
- **严禁擅自提交**：在后续开发过程中，**没有用户明确指令，绝对禁止擅自执行 `git commit` 或 `git push`**！
- **目的**：避免产生过多、过杂、无意义的琐碎 git 提交记录，保持远程仓库历史的整洁清晰。
- **提交规范**：仅在用户显式要求提交（如输入“提交代码”、“commit and push”）时，方可将阶段性完整成果进行提交。
- **提交信息语言（强制中文）**：
  - **所有 Git commit message 必须使用清晰、规范的中文描述**（例如：`feat: 桌面端新增 Inno Setup 安装包与单实例互斥`、`fix: 修复深浅色主题切换未持久化问题`、`docs: 规范 CHANGELOG 格式与中文提交准则`）；
  - **严禁使用空洞、无意义或纯英文的提交信息**（如 `feat: update`、`fix bug`、`feat: land the transfer engine...`）。
- **提交格式推荐**：采用语义化前缀配合清晰的中文动宾短语描述：
  - `feat: 新增...`
  - `fix: 修复...`
  - `docs: 更新...`
  - `refactor: 重构...`
  - `perf: 优化...性能`
  - `chore: ...`
  - `ci: ...`

### 🔴 规则 2：安全与测试红线
- **禁止运行测试**：绝对禁止直接执行 `mvn test`、全量测试套件或任何可能污染生产/外部环境的命令。
- **运行环境感知**：当前运行环境为 **Windows**，发出的终端命令、路径拼接和脚本必须严格兼容 Windows（PowerShell / cmd）与跨平台规范。

### 🔴 规则 3：语言与沟通规范
- **语言偏好**：无论何种输入，AI 与开发者的答复和核心代码注释必须主要使用**中文**进行解释。

### 🔴 规则 4：Java / Kotlin 编码规范 (若涉及 Android 原生扩展)
- **Import 导入管理**：严禁在代码正文（如方法体、变量声明、参数列表）中使用全限定类名，必须在文件头部显式 `import` 该类，代码中仅使用简单类名。
- **Javadoc 文档规范**：
  - `@author`: 格式固定为 `[负责人],[AI工具名称]`，示例：`xudong.hua,gemini`
  - `@since`: 格式为 `yyyy-MM-dd HH:mm 星期x`，时间精确到分钟，必须包含中文星期。

---

## 3. 技术栈与工程架构定义

本工程采用 **Tauri v2 + Rust 核心引擎 + 响应式现代 Web 前端** 架构：

```
feisuo/
├── AGENTS.md                  # 本文件 (Agent 核心协作规范)
├── FRAMEWORK_DESIGN.md        # 完整系统架构设计白皮书
├── README.md                  # 项目概览与原型使用指南
├── index.html                 # 多端交互原型 (Windows/Android/联动)
├── core/                      # [待构建] Rust 跨端核心网络与传输引擎
│   ├── Cargo.toml
│   ├── src/
│   │   ├── discovery/         # mDNS 组播广播与局域网节点发现
│   │   ├── security/          # Ed25519 签名、设备指纹、SQLite 受信证书库
│   │   ├── protocol/          # 数据包切片协议、BLAKE3 分块哈希校验
│   │   ├── transport/         # QUIC / TCP 零拷贝流传输服务
│   │   └── storage/           # 自动落盘、同名防覆盖、断点续传管理
├── desktop/                   # [待构建] Tauri v2 Windows 桌面宿主工程
│   ├── src-tauri/
│   │   ├── tauri.conf.json    # 桌面配置、托盘图标、权限配置
│   │   └── src/
│   │       ├── tray.rs        # 系统托盘菜单与窗口显示/隐藏拦截
│   │       └── autostart.rs   # Windows 任务计划/注册表自启配置
├── mobile/                    # Android 移动端工程与传输服务
│   ├── android/
│   │   ├── app/src/main/
│   │   │   ├── AndroidManifest.xml # 前台服务声明、无开机自启
│   │   │   └── java/net/findfine/feisuo/
│   │   │       ├── FeisuoDaemonService.kt   # 会话级前台服务 (含 MulticastLock，支持锁屏互联)
│   │   │       └── ShareTargetActivity.kt   # 系统“分享到飞梭”原生扩展
└── ui/                        # [待构建] 前端跨端通用界面 (Vue 3 + Vite / HTML5)
    ├── src/
    │   ├── components/        # 响应式双端适配组件
    │   ├── views/             # 桌面 Fluent 布局 / 移动端 Material 3 布局
    │   └── stores/            # 设备状态、传输任务队列状态机
```

---

## 4. 开发工作流程指南

当开始落地各个阶段的代码时，按如下步骤执行：
1. **先审阅方案**：以 [FRAMEWORK_DESIGN.md](file:///d:/dev/agent-repos/feisuo/FRAMEWORK_DESIGN.md) 作为架构基准；
2. **模块解耦**：优先实现与系统解耦的 Rust Core 引擎（网络发现、加解密与分块传输协议），确保其可以被桌面和移动端共同引用；
3. **平台集成**：
   - 桌面端：验证关闭窗口进托盘，开机以 `--daemon` 参数静默拉起；
   - 移动端：验证前台通知常驻与开机广播监听；
4. **提交约束**：完成功能阶段开发后，向用户汇报进展，**等待用户明确发出“提交”指令后才执行 Git 操作**。

---

## 5. CI / 发版 / Gitee 镜像规范

工程位于 `.github/workflows/`，与 QuickClip 保持同构，便于统一维护心智模型。

| 工作流 | 触发 | 职责 |
| :--- | :--- | :--- |
| `ci.yml` | push / PR 到 `main`/`master` | 快速执行代码编译与语法检查（Rust + UI 类型检查 + 架构守卫），不打包、不发版 |
| `sync-gitee.yml` | push 到 `main`/`master` / 手动 | 仅将主分支最新提交镜像推送到 Gitee（30秒内结束，源码实时同步，不推 Tag、不触发发版） |
| `release.yml` | tag `v*` 唯一独占 / 手动 | 提取 CHANGELOG 说明 → 构建双端正式包（Win exe + Android APK）→ 发 GitHub Release → 一次性交付 Gitee（推 Tag + 创 Release + 传附件） |

### 🔴 规则 5：Gitee 镜像（强制）

- 仓库地址**硬编码**在 workflow 与客户端代码里，改仓库名必须同步改这三处：
  - `.github/workflows/sync-gitee.yml`（分支/Tag 镜像）
  - `.github/workflows/release.yml`（Release 与附件）
  - `desktop/src-tauri/src/updater.rs`（客户端检查更新的三条通道）
- **这三处的一致性已有守卫**：
  `update_guard::repo_identifiers_must_be_consistent_across_the_three_places`
  会核对仓库标识在三个文件里同时出现、附件名在 `release.yml` 里一致、
  以及 Gitee 推送目标与客户端的 Gitee 通道同源。
  写在这里的"必须同步改"曾经只是文档——**改漏的后果全是静默的**
  （镜像推到别处 / 发版传到别处 / 客户端永远停在旧版本且界面上看不出异常）。
- **`GITEE_TOKEN` 必须配置在 GitHub 仓库 Secrets**，否则同步会**硬失败**
  （`sync-gitee.yml` 有意不做静默跳过：静默跳过会让"镜像没生效"变成一件
  几个月后才发现的事）。未配置时 `release.yml` 里的 Release 同步是
  `continue-on-error` 降级，只发 warning。
- 国内网络环境下 GitHub API 常常超时，因此客户端检查更新
  **Gitee 优先、GitHub 降级**，详见 `FRAMEWORK_DESIGN.md` §7.7。

### 🔴 规则 6：版本号与发布准则 (CHANGELOG 规范)

- 当前版本一律取自 `CARGO_PKG_VERSION`，**任何位置不得写死版本字符串**。
  `release.yml` 的 “Stamp version” 会同时改写 `Cargo.toml` 与
  `tauri.conf.json` —— 漏改任一处，检查更新就会永久失灵
  （两处不一致时，要么永远提示"发现新版本"，要么永远"已是最新"）。
- **严格遵循语义化版本**（[Semantic Versioning](https://semver.org/lang/zh-CN/)）规范；
- **CHANGELOG 遵循 Keep a Changelog 风格**（参考 `quick-clip` 项目）：
  - 严格以 [Keep a Changelog](https://keepachangelog.com/) 规范组织 `CHANGELOG.md`；
  - 顶部预留 `## [Unreleased]` 记录未发布变更，版本节点格式为 `## [x.y.z] — yyyy-mm-dd`；
  - 每个版本下统一按三级分类组织变更（`### 新增`、`### 优化`、`### 修复`、`### 变更` 等）；
  - 具体条目统一使用 Markdown 列表格式 `- **简要标题**：详细说明`，支持两空格缩进二级子项；
  - **严禁向 CHANGELOG.md 堆砌杂质**：严禁将大篇幅调试日志、过程内部分析或随意起草的小标题塞入 `CHANGELOG.md`，保证其纯净度与易读性，同时确保 CI `release.yml` 能够精准提取整洁的更新说明并自动注入 GitHub 与 Gitee Releases。

### 🔴 规则 7：发版产物

- **Windows 桌面端**：双形态分发，**安装版（推荐主推）+ 绿色便携版**。
  - **安装版**：`Feisuo-Setup-x64.exe`，由 Inno Setup 编译打包。自动配置局域网防火墙规则（避免弹安全警报大黄盾）、创建桌面/开始菜单快捷方式、Windows 设置卸载入口；支持客户端自动更新静默覆盖升级；
  - **绿色便携版**：单文件 `Feisuo-win-x64.exe`，双击即用，免安装；通过换名接力（rollover）自更新；
  - 两个文件名被客户端 `updater.rs` 精确匹配，改名会让在线用户解析不到附件（守卫 `update_guard::current_version_must_come_from_cargo_pkg` 与 `update_guard::repo_identifiers_must_be_consistent_across_the_three_places` 会拦截）。
  - 产物体积下限由 `release.yml` 断言（`< 1MB` 视为构建失败），防止残缺产物发版。
- **Android 移动端**：Release 单文件 `Feisuo-v${ver}-arm64.apk`。
  - 产物体积下限由 `release.yml` 断言（`< 2MB` 视为构建失败）。
  - Release 工作流同时向 GitHub Release 和 Gitee Release 镜像上传 Windows 安装包、绿色版 exe 与 Android APK，并严格验证三项产物上传完整性。

### 常见命令

```powershell
# 前端类型检查 + 打包
pnpm --dir ui build

# 全量检查与测试（含所有守卫）
cargo check --workspace --all-targets
cargo test --workspace

# 绿色版构建（产物：target\release\feisuo-desktop.exe）
# 注意：build.ps1 已按用户要求删除，改手动执行；完整步骤见 README「手动编译」。
cargo tauri build --no-bundle
```
