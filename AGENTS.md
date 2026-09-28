# 飞梭 (Feisuo) · Agent 开发规范与协作准则 (AGENTS.md)

> 本文档是飞梭项目的核心工程宪章与 AI / 开发者协作规范。在参与本项目的一切设计、编码、测试和维护工作时，必须严格遵守本文档所规定的所有约束与流程。

---

## 1. 项目定位与核心产品哲学

飞梭 (Feisuo) 是一款面向 **Windows 桌面端** 与 **Android 移动端** 的极致轻量、零配置、无人值守局域网传输工具。

### 核心体验准则
1. **开机自启，入网自连**：
   - Windows 桌面端支持登录静默启动并常驻系统托盘，关窗不退出；
   - Android 移动端具备开机自启广播 (`BOOT_COMPLETED`) 与前台保活服务 (`Foreground Service`)，杜绝被系统杀后台；
   - 基于 **mDNS (Multicast DNS)** 与历史 IP 缓存并发探测，局域网内开机 50ms 内瞬间完成自连握手。
2. **一次配对，终生免密**：
   - 首次绑定通过扫码或 6 位 PIN 码交换 Ed25519 设备指纹；
   - 之后局域网相遇基于非对称签名进行双向静默认证（Mutual Auth），全程零弹窗确认。
3. **无人值守，安全落盘**：
   - 受信设备发送的文件直接写入预设目标文件夹（Windows: `~/飞梭/`，Android: `/sdcard/Download/飞梭/` 或相册自动注册）；
   - 同名文件自动增加序号递增保护（如 `file (1).ext`），绝不覆盖原有数据。
4. **极速高吞吐**：
   - 底层由 Rust + QUIC / TCP 零拷贝多路复用驱动，榨干千兆局域网与 Wi-Fi 6 极限吞吐（110MB/s+）。

---

## 2. 核心纪律与操作红线 (CRITICAL RULES)

### 🔴 规则 1：Git 提交纪律 (最高约束)
- **严禁擅自提交**：在后续开发过程中，**没有用户明确指令，绝对禁止擅自执行 `git commit` 或 `git push`**！
- **目的**：避免产生过多、过杂、无意义的琐碎 git 提交记录，保持远程仓库历史的整洁清晰。
- **提交规范**：仅在用户显式要求提交（如输入“提交代码”、“commit and push”）时，方可将阶段性完整成果以规范的 Conventional Commits 格式进行提交。

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
├── AGETN.md                   # 兼容别名软链文件
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
├── mobile/                    # [待构建] Android 移动端工程与保活服务
│   ├── android/
│   │   ├── app/src/main/
│   │   │   ├── AndroidManifest.xml # BOOT_COMPLETED、前台服务权限声明
│   │   │   └── java/net/findfine/feisuo/
│   │   │       ├── BootReceiver.kt          # 开机广播接收器
│   │   │       ├── FeisuoDaemonService.kt   # 前台常驻保活服务 (Foreground Service)
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
