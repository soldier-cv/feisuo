# 飞梭 (Feisuo) — Agent 工作规约

## 0. 项目定位

飞梭 (Feisuo) 是一款面向 Windows 桌面端（Tauri v2 + Rust）与 Android 移动端（Kotlin + Rust Core JNI）的无感局域网文件传输工具。
- 仓库地址：`https://github.com/soldier-cv/feisuo.git`
- 默认主分支：`main`
- 架构白皮书：参考 [FRAMEWORK_DESIGN.md](FRAMEWORK_DESIGN.md) 与 [DESIGN_TRUST_SHUTTLE.md](DESIGN_TRUST_SHUTTLE.md)。

## 1. 运行环境事实

- **核心端口与组播地址**：
  - 文件传输端口：TCP `42100`（`DEFAULT_TRANSFER_PORT`）
  - 设备发现端口：UDP `42101`（`DEFAULT_DISCOVERY_PORT`）
  - 组播地址：`239.255.42.100`（`DISCOVERY_MULTICAST_ADDR`）
- **本地存储与数据库**：
  - 信任证书与传输历史数据库：SQLite `trust_store.db`（开启 WAL 模式），位于应用数据目录（Windows: `%LOCALAPPDATA%\feisuo\`，Android: 应用私有数据目录）。
  - 设备私钥：`device_identity.key`（Ed25519 32 字节私钥，绝对不可出设备与入库）。
  - 应用配置：`config.json`（由 UI/系统设置读写，不依赖环境变量）。
  - 默认文件落盘目录：Windows 为 `%USERPROFILE%\feisuo\`，Android 为 `/sdcard/Download/feisuo/`。
- **发布产物规范**：
  - Windows 安装版：`Feisuo-Setup-x64.exe`（Inno Setup 编译打包，支持静默更新）。
  - Windows 绿色便携版：`Feisuo-win-x64.exe`（单文件免安装，换名接力更新）。
  - Android 移动端：`Feisuo-v${ver}-arm64.apk`。
- **Gitee 镜像与更新四处同构真源**：
  - `.github/workflows/sync-gitee.yml`
  - `.github/workflows/release.yml`
  - `desktop/src-tauri/src/updater.rs`
  - `mobile/android/app/src/main/java/net/findfine/feisuo/UpdateChecker.kt`

## 2. 硬规则（违反必有可执行检查）

1. **凭据防泄露（最高红线）**：
   - 严禁将任何 `.env`、密钥文件（`*.key`、`*.pem`、`*.jks`、`*.keystore`）、令牌或凭据提交入库。
   - 验证命令：`git ls-files | grep -E '\.env$|\.pem$|\.key$|credentials\.json$|\.p12$'`（输出必须为空）。
2. **提交与推送纪律**：
   - 严禁未经用户明确指示擅自执行 `git commit` 或 `git push`。
   - Commit message 必须遵循 Conventional Commits 且必须使用规范中文动宾描述（如 `fix: 修复托盘唤出窗口状态`）。
   - 验证命令：`git log -1 --pretty=%B`（检查最新一次提交说明）。
3. **界面文本纪律（华总 2026-11-16 定）**：
   - 严禁将实现思路、需求说明、技术原理解释文字直接堆到页面 UI 上；页面文案仅保留面向最终用户的状态、操作与明确错误提示。
   - 验证命令：`git grep -nE '本功能实现|为了满足需求|该功能基于|设计思路' ui/src/`（命中必须为 0）。
4. **Tauri Invoke 参数严格对齐**：
   - 前端 `invoke` 传递的参数名必须与 Rust 侧 `#[command]` 结构体入参精确一致，禁止冗余无用 key（Tauri v2 会静默丢弃未声明参数导致功能断链）。
   - 验证命令：`python3 scripts/audit-invoke-args.py --strict`（退出码必须为 0）。
5. **UI 入口行为一致性守卫**：
   - 多入口操作（直发、拖拽、穿梭）必须遵循统一准入检查、撤销窗口、落点路径传递与目录判断。
   - 验证命令：`python3 scripts/audit-ui-invariant.py`（退出码必须为 0）。
6. **脚本控制台编码安全**：
   - 任何新增或修改的 Python 工具脚本必须在 `def main(` 前显式调用 `_ci_utf8.force_utf8_stdout()`，防止在英文 Windows / CI runner 上因中文字符编码崩溃。
   - 验证命令：`python3 scripts/check-ci-encoding.py`（退出码必须为 0）。
7. **CSS 游离声明与样式规范**：
   - CSS 声明严禁裸露在样式块外部（会被 Vite 忽略并在运行时静默失效）。
   - 验证命令：`python3 scripts/check-css-stray-declarations.py`（退出码必须为 0）。
8. **主题常驻前景/底色对比度达标**：
   - 深色与浅色双套主题必须满足正文 4.5:1、图标 3:1 对比度要求。
   - 验证命令：`python3 scripts/check-contrast.py`（退出码必须为 0）。

## 3. 开发与验证流程

1. **开工第一步（强制同步远端）**：
   - 任何代码修改前必须拉齐远端主分支，防止并行开发产生非快进冲突：
     ```bash
     git fetch && git pull --ff-only origin main
     ```
2. **改动后本地必跑验证（秒级只读审计）**：
   ```bash
   python3 scripts/check-ci-encoding.py
   python3 scripts/audit-invoke-args.py --strict
   python3 scripts/check-css-stray-declarations.py
   python3 scripts/check-contrast.py
   python3 scripts/audit-ui-invariant.py
   ```
3. **完整构建与测试（在具备对应工具链的环境中）**：
   ```bash
   # 前端类型检查与打包
   pnpm --dir ui build

   # Rust 工作区检查与测试（含 JNI / 协议集成测试）
   cargo check --workspace --all-targets
   cargo test --workspace
   ```

## 4. 已知坑（真实事故与防踩指南）

1. **本地残留安装包劫持检查更新（v0.1.25 教训）**：
   - 升级器检查更新时，**必须远端检查结果优先**；若先查本地缓存安装包，会将旧包判定为“已就绪”跳过网络检查，引发重复重启却未真正升级的死循环。本地包仅当版本与远端完全一致时才复用，启动时必须清理已安装历史包。
2. **Tauri v2 invoke 参数静默丢弃**：
   - 前端增加参数而后端遗漏时，Tauri 不报错不打日志，仅静默丢弃该字段。必须通过 `audit-invoke-args.py` 双向差集审计拦截。
3. **CI Windows Runner 中文输出崩溃**：
   - 英文 Windows runner（cp1252 代码页）在输出包含中文的日志时会抛出 `UnicodeEncodeError` 导致 CI 误报失败，所有检查脚本必须提前导入 `_ci_utf8` 进行 stdout 包装。
4. **跨平台换行符干扰**：
   - Windows 下检出 CRLF 可能导致特定正则与脚本校验不匹配，通过 `.gitattributes` 全局规范文本文件的行尾行为。
5. **Gitee 镜像四处修改遗漏**：
   - 仓库地址与标识修改必须四处同步（两个 workflow + 两个客户端 updater），任一处遗漏将导致国内更新链路静默失效。
6. **落盘路径越界逃逸防护**：
   - 局域网接收文件落地位置严格受限于预设收件根目录，双栏穿梭仅允许在对方当前收件子路径内写入，严禁跨出收件目录或访问系统敏感目录。

## 5. 需求台账 / 决策日志

- **2026-11**: 确立架构白皮书 [FRAMEWORK_DESIGN.md](FRAMEWORK_DESIGN.md) 与增量设计 [DESIGN_TRUST_SHUTTLE.md](DESIGN_TRUST_SHUTTLE.md)。
- **2026-11**: 废除外部包装脚本 `build.ps1`，标准化本地开发与 GitHub Actions 流水线构建命令。
- **2026-11**: Windows 桌面端确立安装版（主推静默升级）与单文件绿色便携版双形态分发机制。
- **2026-11**: 修复 v0.1.25 本地残留安装包导致的两轮重启与更新劫持缺陷。
- **2026-11**: 仓库全面对齐开源与工程规范（补齐 MIT LICENSE、.gitattributes、.editorconfig，加固 .gitignore 凭据防泄漏规则）。
