# 飞梭 Android 端 · 单机操作手册

> 基于源码 `mobile/android/app/src/main/java/net/findfine/feisuo/` 编写。
> 每条用例按 **路径 / 前置 / 操作 / 验收** 四段式组织，可直接映射为 midscene 用例。
>
> **单机范围**：只验证本机行为（启动、引擎、权限、前台服务、配对表单校验、零后台退出）。
> 跨设备发现、配对握手、文件传输 **不在本手册** —— 那需要两台设备处于同一广播域，
> 而当前两台模拟器都是 `10.0.2.15`（各自独立 QEMU NAT），无法互相发现。
> 等有真机或 TAP 桥接后另写《双机联调用例》。

**当前状态**：17 条用例，**17 条通过**（14 条 dump 判定 + 3 条视觉判定）。

---

## 0. 环境与前置

| 项 | 值 |
| --- | --- |
| 包名 | `net.findfine.feisuo` |
| 启动入口 | `net.findfine.feisuo/.MainActivity`（LAUNCHER，exported） |
| APK | `mobile/android/app/build/outputs/apk/debug/feisuo-<ver>-<code>.apk` |
| 最低版本 | Android 8.0（minSdk 26） |
| 权限 | `POST_NOTIFICATIONS`（Android 13+ 运行时申请） |
| 数据目录 | app 私有 `filesDir/feisuo`（设备指纹、信任库、私钥） |
| 引擎端口 | 传输 `42100` / 发现 `42101` |

**统一前置**：

```bash
# 通知栏必须先收起 —— 上一轮用例展开过的话，后续 dump 抓到的全是
# NotificationShade，表现为「找不到任何界面文本」这种极具误导性的失败。
adb shell cmd statusbar collapse
adb shell pm grant net.findfine.feisuo android.permission.POST_NOTIFICATIONS
adb shell am force-stop net.findfine.feisuo
adb shell am start -W -n net.findfine.feisuo/.MainActivity
sleep 6   # 等引擎在后台线程起来
```

---

## 1. 启动与界面完整性

### TC-01 应用冷启动不崩溃

- **路径**：桌面 → 飞梭图标
- **前置**：应用未运行
- **操作**：冷启动 `MainActivity`
- **验收**：
  - 前台是 `net.findfine.feisuo/.MainActivity`
  - `logcat -b crash` 中**没有**飞梭的 `FATAL EXCEPTION`
  - 能看到标题「飞梭」和副标题「局域网互传 · 无人值守待命」

> **回归来源（已修）**：`MainActivity.kt` 的 `buildUi()` 对 `section()` 返回值
> 又包了一层 `root.addView(...)`，而 `section()` 内部已经 `root.addView(it)`。
> 同一 View 添加两次 → `IllegalStateException: The specified child already has a parent`
> → 启动即闪退。5 处（原第 100/105/112/136/145 行）已修正。

### TC-02 五个分区标题齐全且仅出现一次

- **路径**：同 TC-01
- **操作**：从上到下滚动整页
- **验收**：依次出现且**仅出现一次**：「运行状态」「通知权限与锁屏可用」
  「如何配对」「已配对设备」「随用随走 · 零后台」

> 这是双重添加 bug 的直接观察面：修复前要么闪退，要么标题重复。

---

## 2. 引擎状态

### TC-03 引擎运行中

- **路径**：运行状态 → 第一行「引擎」
- **验收**：右侧显示 **「运行中」**

| 实际显示 | 含义 | 判定 |
| --- | --- | --- |
| 运行中 | 正常 | ✅ |
| 正在启动… | 后台线程未完成 | ⚠️ 再等 3 秒重读 |
| 未就绪 | 启动失败 | ❌ |
| 未就绪 — core 引擎启动失败 (数据目录不可写或端口被占用) | 句柄为 0 | ❌ |
| 未就绪 — core 动态库缺失, 请重新构建 APK | `.so` 缺失/ABI 不匹配 | ❌ |

### TC-04 设备指纹已生成

- **路径**：运行状态 → 「设备指纹」
- **验收**：
  - 非空、不是「读取中…」也不是「—」
  - 长度 ≥ 8，形如 `feisuo-24f626a1e917`

> **回归来源（已修）**：`core/src/mobile.rs` 的 JNI 层把句柄硬编码成 `0`，
> 而 `0 == INVALID_HANDLE`，`engine_for_handle(0)` 立即返回 `None`，
> 于是 `device_id` 永久返回空串。修复前界面稳定显示「—」。
> 同源问题影响 `config_json` / `trusted_devices_json` / `probe_device` /
> `pair_with_device` / `send_files` —— **整个 Android 端核心功能全部静默失效**。
> 修法是新增 `host::current_handle()`，6 处改为取真实句柄。

### TC-05 配置摘要可读

- **路径**：运行状态 → 「配置」
- **验收**：
  - 格式 `<设备名> · 传输 <端口> · 发现 <端口>`
  - 两个端口均为正整数
  - 不是「—」、不是「解析失败」

> `summarizeConfig()` 解析异常时显式返回「解析失败」，所以看到它就是 bug。

---

## 3. 通知权限与前台服务

### TC-06 首次启动申请通知权限

- **前置**：`pm revoke` 撤销通知权限，且应用未运行
- **操作**：冷启动
- **验收**：出现系统权限对话框（`permissioncontroller` 承载）或授权状态仍为未授予

### TC-07 前台服务常驻通知

- **路径**：下拉通知栏
- **验收**：
  - 存在常驻通知（`dumpsys notification` 里带 `ONGOING_EVENT|NO_CLEAR`）
  - 标题「飞梭局域网互传已就绪」
  - 正文「局域网待命中 · 锁屏状态下可正常接收」
  - 带操作按钮「停止待命」

### TC-08 点「停止待命」后服务与引擎一并停止

- **路径**：通知栏 → 该通知 → 「停止待命」
- **前置**：TC-07 通过
- **操作**：点「停止待命」
- **验收**：
  - 常驻通知消失
  - 服务与引擎一并停止（源码在 `onDestroy()` 调 `FeisuoRuntime.shutdown()`）

> **测试方法注意**：
> - Service 是 `exported="false"`，**adb 无法直接调用**（这是正确的安全设计），
>   只能走真实用户路径：下拉通知栏点击。
> - 通知面板的控件**不在 uiautomator 树里**，必须用坐标点击。
> - 坐标必须按 **`wm size` 的真实分辨率**换算。实测这台是 `1280x2856`，
>   「停止待命」在 `(346, 1182)`；若按截图肉眼读的像素（900x2000）会全部点空。
> - 点击前必须确认通知栏**真的展开**（`mCurrentFocus` 含 `NotificationShade`）。

### TC-09 停止后引擎可被重新拉起

- **操作**：再次启动应用
- **验收**：
  - 「引擎」重新显示「运行中」
  - 「设备指纹」**不变**

> 证明 `shutdown` 是干净的：如果引擎没真正停掉，端口（42100/42101）仍被占，
> 重新拉起会绑定失败；如果数据目录不稳定，指纹会变。

### TC-09b 退出应用后零后台

- **操作**：`am force-stop`
- **验收**：
  - `pidof net.findfine.feisuo` 返回空
  - 通知栏无飞梭通知

> 产品红线「随用随走 · 零后台」。

---

## 4. 配对表单

> 全部**可离线验证**：只测输入校验分支，不会真的发网络请求。
> 源码 `pairWithDesktop()` 判定顺序：引擎就绪 → IP 非空 → PIN 为 6 位数字 → 才发请求。

### TC-10 引擎就绪后配对按钮可用

- **验收**：`配对` 按钮 `enabled="true"`

> 源码里引擎未就绪时按钮是 `isEnabled = false`。但这个窗口只有几十毫秒
> （实测引擎启动极快），**外部无法稳定抓到**，所以只验证正向路径。
> 「未就绪时禁用」属于代码可读性保证，不进自动化用例。

### TC-11 IP 为空时给出提示

- **前置**：IP 输入框为空
- **操作**：点「配对」
- **验收**：Toast 显示 **「请先填写电脑的局域网 IP」**

### TC-12 配对码非 6 位数字时给出提示

- **前置**：IP 填 `192.168.1.10`；配对码填 `123`
- **操作**：点「配对」
- **验收**：Toast 显示 **「配对码必须是 6 位数字」**

> **TC-11 / TC-12 必须用视觉验证**：Android 11+ 的 Toast 既不写 logcat
> 也不进 UI 树，只能看屏幕。且 Toast 只停留约 3.5 秒，点击后要**立刻**问模型。

### TC-13 输入框占位文案正确

- **验收**：
  - IP 框占位含「电脑 IP」或「192.168.1.10」
  - 配对码框占位含「6 位配对码」或「123456」

### TC-14 已配对设备状态正确

- **验收**：
  - 「受信设备」**不是**「读取中…」
  - 全新安装显示「尚未配对」；已有记录时显示设备名列表

---

## 5. 刷新与状态重载

### TC-15 「刷新状态」不崩溃且指纹不变

- **操作**：点「刷新状态」
- **验收**：
  - 不崩溃
  - 「设备指纹」与刷新前**一致**

> `startEngineAsync()` → `ensureEngine` 是 `@Synchronized` 幂等的，
> 重复刷新**不应**导致端口被占或指纹变化。

---

## 6. 已知限制（非 bug，不必报）

| 现象 | 原因 |
| --- | --- |
| 界面上没有「搜索到 XX 设备」 | 本端是接收方 UI，设备列表在**桌面端**展示 |
| 配对时提示「无法连接」 | 模拟器 NAT 下无法与另一台设备互通，属环境限制 |
| 组播锁日志提示失败 | Wi-Fi 未关联时会失败并等网络恢复后重试，属设计行为 |
| 跨设备功能无法测 | 两台模拟器都是 `10.0.2.15`，UDP 组播没有可用网络基础 |

---

## 7. 证据采集约定

每条用例失败时必须留三样证据：

```bash
adb -s <serial> exec-out screencap -p > fail-<TC编号>.png
adb -s <serial> shell uiautomator dump /sdcard/w.xml
adb -s <serial> shell cat /sdcard/w.xml > fail-<TC编号>.xml
adb -s <serial> logcat -d -b crash -t 100 > fail-<TC编号>-crash.log
```

---

## 8. 判定方式说明（为什么以 dump 为主）

| 断言类型 | 用什么判定 | 理由 |
| --- | --- | --- |
| 文本是否存在（标题、占位符、状态文案） | `uiautomator dump` | 0.1 秒、零成本、100% 确定 |
| 引擎/服务/进程状态 | `adb shell` + `logcat` | 系统真相，不靠像素 |
| 崩溃 | `logcat -b crash` | 唯一可靠来源 |
| **Toast 提示** | **视觉模型** | 不进 UI 树、不写 logcat |
| 布局错乱、文字截断、控件重叠 | 视觉模型 | 只有这一类必须看图 |

**这个 App 全部是原生 Kotlin 控件**（手写 `LinearLayout`/`TextView`/`EditText`/`Button`），
dump 能完整读取，所以 **14/17 的断言不需要视觉模型**。

---

## 9. 测试脚本

| 脚本 | 覆盖 | 判定方式 |
| --- | --- | --- |
| `scripts/feisuo-test.ts` | TC-01~10, 13~15+（14 条） | dump + logcat |
| `scripts/feisuo-toast.ts` | TC-11、TC-12、TC-10b（3 条） | 视觉（midscene） |
| `scripts/feisuo-visual.ts` | 布局完整性、运行时状态读取 | 视觉（midscene） |

运行：

```powershell
cd midscene-demo\android
$env:ANDROID_HOME="C:\Users\13262\AppData\Local\Android\Sdk"
npx tsx scripts/feisuo-test.ts emulator-5554
npx tsx scripts/feisuo-toast.ts emulator-5554
npx tsx scripts/feisuo-visual.ts emulator-5554
```
