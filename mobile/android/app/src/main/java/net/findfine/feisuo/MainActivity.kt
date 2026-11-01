package net.findfine.feisuo

import android.Manifest
import android.app.Activity
import android.app.NotificationManager
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Typeface
import android.os.Build
import android.os.Bundle
import android.provider.DocumentsContract
import android.provider.Settings
import android.text.InputType
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject
import java.io.File

/**
 * 飞梭 Android 端主界面。
 *
 * 核心功能：
 * 1. 呈现飞梭核心引擎运行状态（设备指纹、传输端口等）；
 * 2. 提供极简配对表单（输入电脑 IP 与 6 位配对码，单向握手即享免密互传）；
 * 3. 展示当前局域网受信设备列表；
 * 4. 启动会话级前台守护服务（FeisuoDaemonService），并引导授予通知权限，
 *    确保用户开启应用后即使锁屏也能持续在局域网待命接收文件；
 * 5. 用户退出应用时（返回键退出或划掉任务卡片），优雅关闭服务，零后台驻留。
 *
 * @author xudong.hua,gemini
 * @since 2026-10-08 19:40 星期四
 */
class MainActivity : Activity() {

    companion object {
        private const val TAG = "FeisuoMainActivity"
        private const val REQUEST_POST_NOTIFICATIONS = 1001
    }

    private lateinit var root: LinearLayout
    private var engineStatusView: TextView? = null
    private var deviceIdView: TextView? = null
    private var configView: TextView? = null
    private var trustedView: TextView? = null
    private var ipInput: EditText? = null
    private var pinInput: EditText? = null
    private var pairButtonRef: Button? = null
    private var deviceNameInput: EditText? = null
    private var nameSaveButton: Button? = null
    private var receiveDirView: TextView? = null
    private var openDirButton: Button? = null
    private var notificationStatusView: TextView? = null
    private var notificationActionButton: Button? = null
    private var updateStatusView: TextView? = null
    private var updateCheckButton: Button? = null
    /** 检查通过、下载前的一次点击；文本随状态在「下载并安装 / 立即安装」之间切换 */
    private var updateActionButton: Button? = null
    /** 已下载并校验通过、等待安装的更新包 */
    private var pendingApk: File? = null
    /** 检查出的新版本（下载时还要用它，例如体积与 tag 文案） */
    private var pendingRelease: UpdateChecker.ReleaseInfo? = null
    /** 用户被带去开「安装未知应用」权限，回到界面时要续上那次安装 */
    private var awaitingUnknownSources = false
    /** 检查更新是纯网络操作，但同样不能占主线程 */
    private var updateThread: Thread? = null
    /** 引擎启动/配置读取都要跑在 core 的 tokio runtime 上, 不能在主线程做 */
    private var worker: Thread? = null
    /** 配对是网络操作, 绝不能在主线程跑(见 android_guard) */
    private var pairThread: Thread? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        buildUi()

        // 数据目录必须在任何 core 调用之前确定; 引擎是进程内单例, 重复调用安全。
        // 但 nativeInitEngine 会建 runtime + 绑端口, 实测几十毫秒, 主线程调用会掉帧,
        // 放到后台线程, 结果回主线程刷新界面。
        startEngineAsync()

        startDaemonService()
        requestNotificationPermissionIfNeeded()
        updateNotificationStatus()

        // 启动时静默检查一次：用户不会为了"看看有没有新版"主动点按钮，
        // 而失败时**不打扰**（离线用户每次开应用都弹红条只会让人关掉通知）。
        checkForUpdate(silent = true)
    }

    override fun onResume() {
        super.onResume()
        // 用户可能刚从系统设置里开关过通知，回到界面就要反映真实状态 ——
        // 只在 onCreate 查一次，用户改完设置回来看到的还是旧结论。
        // 这里只读系统服务，不碰任何阻塞的 core 接口。
        updateNotificationStatus()

        // 同理：用户为了装更新去了「安装未知应用」授权页，回来后要把
        // 刚才被打断的那次安装续上。不续的话，界面上明明写着"授权后即可继续"，
        // 实际什么都没发生 —— 用户只能再点一次（而且不知道该点哪个）。
        if (awaitingUnknownSources && ApkInstaller.canInstallPackages(this)) {
            awaitingUnknownSources = false
            pendingApk?.takeIf { it.isFile }?.let { installPendingApk(it) }
        }
    }

    override fun onDestroy() {
        worker?.interrupt()
        worker = null
        pairThread?.interrupt()
        pairThread = null
        if (isFinishing) {
            stopService(Intent(this, FeisuoDaemonService::class.java))
        }
        super.onDestroy()
    }

    // ---------------------------------------------------------------- UI

    private fun buildUi() {
        val scroll = ScrollView(this)
        root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(28), dp(20), dp(28))
        }
        scroll.addView(root, ViewGroup.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        ))
        setContentView(scroll)

        addTitle("飞梭", 22f)
        addSubtitle("局域网互传 · 无人值守待命")

        // section() 内部已经 root.addView(it)，外面再包一层会把同一个 View
        // 添加两次，onCreate 直接抛 IllegalStateException（"specified child
        // already has a parent"）——应用一启动就闪退。
        section("运行状态")
        engineStatusView = addRow("引擎", "正在启动…")
        deviceIdView = addRow("设备指纹", "读取中…")
        configView = addRow("配置", "读取中…")

        // 设备名此前只能在电脑上看到、无法在手机上调 —— 而局域网里
        // 三台"飞梭设备"同时在线时，用户根本分不清哪台是自己。
        section("本机设备名")
        addParagraph(
            "这个名字会显示在局域网内其他飞梭设备上（对方看到的“来自哪里”就是它）。" +
                "保存后最多一个广播周期（约 3 秒）就会被对端刷新，已发生的历史记录不会改名。"
        )
        deviceNameInput = addInput("设备名", "例如 客厅手机")
        val saveName = Button(this).apply {
            text = "保存设备名"
            isEnabled = false
            setOnClickListener { saveDeviceName() }
        }
        nameSaveButton = saveName
        root.addView(saveName, lp(top = dp(8)))

        section("接收目录")
        addParagraph(
            "电脑发来的文件默认保存在下面这个位置。点按钮会用系统文件管理器打开它，" +
                "之后可以直接查看、转发或删除。"
        )
        receiveDirView = addRow("保存位置", "读取中…")
        val openDir = Button(this).apply {
            text = "打开接收目录"
            isEnabled = false
            setOnClickListener { openReceiveDir() }
        }
        openDirButton = openDir
        root.addView(openDir, lp(top = dp(8)))

        section("通知权限与锁屏可用")
        // 通知栏是用户判断"飞梭到底还开着没"的唯一可见证据：
        // 没有这条通知，前台服务在用户眼里等于不存在。
        notificationStatusView = addRow("通知栏", "检查中…")
        val notificationAction = Button(this).apply {
            text = "开启通知"
            setOnClickListener { onNotificationAction() }
        }
        notificationActionButton = notificationAction
        root.addView(notificationAction, lp(top = dp(8)))
        addParagraph(
            "飞梭在打开期间通过前台通知维持局域网待命与 Wi-Fi 组播监听，" +
                "确保手机在锁屏、切 Wi-Fi 之后仍能被电脑秒搜并投递文件。" +
                "Android 13+ 需要你授权通知，否则锁屏后系统可能冻结网络连接。"
        )

        section("如何配对")
        addParagraph(
            "1. 在 Windows 桌面端打开「双栏穿梭」或「系统设置」, 点击「查看本机配对码」。\n" +
                "2. 在下方填入电脑的局域网 IP 与那 6 位配对码, 点击「配对」。\n" +
                "3. 绑定一次即可, 之后两台设备在局域网内相遇会静默互认, 无需再确认。\n" +
                "4. 收到的文件默认落在 /sdcard/Download/feisuo/。"
        )

        // 真实可用的配对表单。
        // 旧实现只有一个写着"输入配对码"的按钮, 点击后弹"配对界面将在后续版本接入",
        // 而上方说明却写着"在下方输入桌面端显示的 6 位配对码" —— 文案与按钮都在说谎,
        // 且配不上意味着整条分享推送链路永远不可达。
        ipInput = addInput("电脑 IP", "例如 192.168.1.10")
        pinInput = addInput("6 位配对码", "123456").apply {
            inputType = InputType.TYPE_CLASS_NUMBER
        }
        val pairButton = Button(this).apply {
            text = "配对"
            isEnabled = false
            setOnClickListener { pairWithDesktop() }
        }
        pairButtonRef = pairButton
        root.addView(pairButton, lp(top = dp(16)))

        section("已配对设备")
        trustedView = addRow("受信设备", "读取中…")

        val refresh = Button(this).apply {
            text = "刷新状态"
            setOnClickListener { startEngineAsync() }
        }
        root.addView(refresh, lp(top = dp(8)))

        section("版本与更新")
        addParagraph(
            "在线升级直接从 Release 下载 APK（国内优先走 Gitee），" +
                "下载完成会先校验签名，再由系统安装器完成安装 —— " +
                "首次使用需要在系统的「安装未知应用」里允许一次。"
        )
        // 版本号来自 BuildConfig（源头是仓库根 Cargo.toml），与电脑端同一个版本号。
        // 写死字符串的话，用户装了新版界面还显示旧版号，而且不报错。
        addRow("当前版本", BuildConfig.VERSION_NAME)
        updateStatusView = addRow("更新状态", "尚未检查")
        val checkBtn = Button(this).apply {
            text = "检查更新"
            setOnClickListener { checkForUpdate(silent = false) }
        }
        updateCheckButton = checkBtn
        root.addView(checkBtn, lp(top = dp(8)))

        // 只在"确实有可做的事"时出现：没有新版本时挂一个灰按钮，
        // 用户会反复点它并怀疑功能坏了。
        val actionBtn = Button(this).apply {
            text = "下载并安装"
            visibility = View.GONE
            setOnClickListener { downloadAndInstall() }
        }
        updateActionButton = actionBtn
        root.addView(actionBtn, lp(top = dp(8)))

        section("随用随走 · 零后台")
        addParagraph(
            "• 锁屏可用：只要本应用仍在任务列表中，锁屏状态下依然可秒速接收局域网文件。\n" +
                "• 退出即关：使用完毕后，按返回键退出或在多任务列表中划掉卡片即可彻底停止，不占用后台与电量。"
        )
    }

    /**
     * 执行配对。
     *
     * 先探测再配对: 信任库/信标里记的是 IP, 传输端口是对端可在设置里改的,
     * 直接用默认端口连必然失败。探测拿到对端当前声明的真实端口再发起配对。
     *
     * 全程在后台线程: probe 与 pair 都是阻塞 JNI(内部 runtime().block_on),
     * 放主线程必然 ANR。desktop 侧有 android_guard 盯住这一点。
     */
    private fun pairWithDesktop() {
        if (!FeisuoRuntime.isEngineRunning()) {
            toast("引擎未就绪, 请稍后重试")
            return
        }
        val ip = ipInput?.text?.toString()?.trim().orEmpty()
        val pin = pinInput?.text?.toString()?.trim().orEmpty()
        if (ip.isEmpty()) {
            toast("请先填写电脑的局域网 IP")
            return
        }
        if (pin.length != 6 || !pin.all { it.isDigit() }) {
            toast("配对码必须是 6 位数字")
            return
        }

        pairButtonRef?.isEnabled = false
        pairThread?.interrupt()
        val t = Thread({
            val result = runCatching {
                val probed = NativeBridge.probeDevice(ip)
                    ?: return@runCatching "无法连接 $ip, 请确认与电脑在同一局域网"
                val port = runCatching {
                    org.json.JSONObject(probed).optInt("transfer_port", 0)
                }.getOrDefault(0)
                if (port <= 0) return@runCatching "未获取到电脑的传输端口"

                val err = NativeBridge.pairWithDevice(ip, port, pin)
                if (err != null) err else null
            }.getOrElse { "配对过程出错: ${it.message}" }

            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                pairButtonRef?.isEnabled = true
                if (result == null) {
                    toast("配对成功, 之后可与该电脑互传文件")
                    loadTrustedDevices()
                } else {
                    toast(result)
                }
            }
        }, "feisuo-pair")
        t.isDaemon = true
        pairThread = t
        t.start()
    }

    /**
     * 保存本机设备名。
     *
     * 消毒 / 长度 / 空串判定全在 core（`engine_host::set_device_name`），
     * 这里只做"别让用户点一个必然失败的按钮"这一层：空串直接拦下。
     * 写配置是阻塞 JNI，必须放后台线程（见 android_guard 的 BLOCKING_METHODS）。
     */
    private fun saveDeviceName() {
        val name = deviceNameInput?.text?.toString()?.trim().orEmpty()
        if (name.isEmpty()) {
            toast("设备名称不能为空")
            return
        }
        if (!FeisuoRuntime.isEngineRunning()) {
            toast("引擎未就绪, 请稍后重试")
            return
        }
        nameSaveButton?.isEnabled = false
        val t = Thread({
            val err = runCatching { NativeBridge.setDeviceName(name) }
                .getOrElse { "修改设备名失败: ${it.message}" }
            // 改名成功后重新读一次配置：core 会消毒（去控制字符 / 截断），
            // 界面上该显示**真正生效**的名字，而不是用户敲进去的原文。
            val cfgJson = if (err == null) {
                runCatching { NativeBridge.configJson() }.getOrDefault("")
            } else {
                ""
            }
            val actual = runCatching {
                JSONObject(cfgJson).optString("device_name")
            }.getOrDefault("")
            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                nameSaveButton?.isEnabled = true
                if (err == null) {
                    val shown = actual.ifBlank { name }
                    deviceNameInput?.setText(shown)
                    if (cfgJson.isNotBlank()) configView?.text = summarizeConfig(cfgJson)
                    // 通知栏上也写着设备名，改完不刷新就等于文案说谎。
                    FeisuoDaemonService.refreshNotification(this, shown)
                    toast("设备名称已更新为「$shown」")
                } else {
                    toast(err)
                }
            }
        }, "feisuo-rename")
        t.isDaemon = true
        t.start()
    }

    /**
     * 打开接收目录（默认 `/sdcard/Download/feisuo`）。
     *
     * 目录**从配置里读**而不是写死：用户可以在电脑端改收件目录，
     * 写死会让这个按钮把他带到上一个位置。
     *
     * 读配置是阻塞 JNI，放后台线程；真正的 `startActivity` 回主线程做。
     */
    private fun openReceiveDir() {
        if (!FeisuoRuntime.isEngineRunning()) {
            toast("引擎未就绪, 请稍后重试")
            return
        }
        val t = Thread({
            val dir = runCatching {
                JSONObject(NativeBridge.configJson()).optString("receive_dir")
            }.getOrDefault("").ifBlank { "/sdcard/Download/feisuo" }
            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                launchFileManager(dir)
            }
        }, "feisuo-open-receive-dir")
        t.isDaemon = true
        t.start()
    }

    /**
     * 用系统文件管理器打开目录。
     *
     * 三种 intent 依次尝试，因为"打开一个文件夹"在 Android 上没有唯一标准：
     * 1. 外部存储 DocumentsProvider 的**目录 document URI**（AOSP DocumentsUI 认）；
     * 2. 外部存储**根** document URI（部分 ROM 的文件管理器只认根）；
     * 3. `ACTION_OPEN_DOCUMENT_TREE` 并把初始位置指到该目录（任何设备都有选择器）。
     * 三个都不行就明确告诉用户路径，而不是点了没反应。
     */
    private fun launchFileManager(dir: String) {
        val normalized = dir.trim().trimEnd('/')
        if (normalized.isEmpty()) {
            toast("接收目录未知, 请稍后重试")
            return
        }
        val externalRoot = "/storage/emulated/0"
        val onExternalStorage = normalized == externalRoot ||
            normalized.startsWith("$externalRoot/") ||
            normalized == "/sdcard" ||
            normalized.startsWith("/sdcard/")
        if (!onExternalStorage) {
            // 无外部存储权限时 core 会退回应用私有目录（/data/...），
            // 那个路径文件管理器根本进不去 —— 说清楚比一个没反应的点击好。
            toast("接收目录在应用私有空间, 文件管理器打不开:\n$normalized")
            return
        }
        val rel = if (normalized.startsWith("/sdcard")) {
            normalized.removePrefix("/sdcard")
        } else {
            normalized.removePrefix(externalRoot)
        }.trim('/')

        val authority = "com.android.externalstorage.documents"
        val folderUri = DocumentsContract.buildDocumentUri(authority, "primary:$rel")
        val candidates = listOf(
            Intent(Intent.ACTION_VIEW)
                .setDataAndType(folderUri, DocumentsContract.Document.MIME_TYPE_DIR),
            Intent(Intent.ACTION_VIEW)
                .setData(DocumentsContract.buildRootUri(authority, "primary")),
            Intent(Intent.ACTION_OPEN_DOCUMENT_TREE)
                .putExtra(DocumentsContract.EXTRA_INITIAL_URI, folderUri),
        )
        for (intent in candidates) {
            intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_GRANT_READ_URI_PERMISSION)
            try {
                startActivity(intent)
                return
            } catch (e: ActivityNotFoundException) {
                Log.i(TAG, "没有应用能处理 $intent, 尝试下一个")
            } catch (e: Exception) {
                Log.w(TAG, "打开目录失败: ${e.message}")
            }
        }
        toast("没有找到可用的文件管理器, 接收目录:\n$normalized")
    }

    /** 刷新已配对设备列表。配对成功后必须能看到结果, 否则用户无从确认。 */
    private fun loadTrustedDevices() {
        val t = Thread({
            val summary = runCatching {
                val arr = org.json.JSONArray(NativeBridge.trustedDevicesJson())
                (0 until arr.length()).mapNotNull { i ->
                    val o = arr.optJSONObject(i) ?: return@mapNotNull null
                    if (!o.optBoolean("is_trusted", false)) return@mapNotNull null
                    val name = o.optString("device_name").ifBlank { "未命名设备" }
                    val ip = o.optString("last_ip").ifBlank { "未知 IP" }
                    "$name ($ip)"
                }
            }.getOrDefault(emptyList())

            val text = when {
                !FeisuoRuntime.isEngineRunning() -> "引擎未就绪"
                summary.isEmpty() -> "尚未配对"
                else -> summary.joinToString("\n")
            }
            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                trustedView?.text = text
            }
        }, "feisuo-trusted-refresh")
        t.isDaemon = true
        t.start()
    }

    private fun startEngineAsync() {
        engineStatusView?.text = "正在启动…"
        worker?.interrupt()
        worker = Thread {
            // nativeInitEngine 失败返回 0, 不会抛异常;
            // 但 .so 缺失时 loadLibrary 会抛 UnsatisfiedLinkError
            val ok = try {
                FeisuoRuntime.ensureStarted(applicationContext)
                FeisuoRuntime.isEngineRunning()
            } catch (e: Throwable) {
                Log.e(TAG, "启动引擎异常", e)
                false
            }
            val deviceId = if (ok) runCatching { NativeBridge.deviceId() }.getOrDefault("") else ""
            val configJson = if (ok) runCatching { NativeBridge.configJson() }.getOrDefault("") else ""
            val failure = FeisuoRuntime.failedReason
            // 设备名 / 接收目录都从配置来（用户在电脑上可能改过），
            // 不写死默认值：写死的地方一旦和 core 的默认值不同步，
            // 界面就会显示一个用户根本不存在的目录。
            val cfgObj = runCatching { JSONObject(configJson) }.getOrNull()
            val deviceName = cfgObj?.optString("device_name").orEmpty()
            val receiveDir = cfgObj?.optString("receive_dir").orEmpty()

            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                engineStatusView?.text = when {
                    ok -> "运行中"
                    failure != null -> "未就绪 — $failure"
                    else -> "未就绪"
                }
                deviceIdView?.text = deviceId.ifEmpty { "—" }
                configView?.text = summarizeConfig(configJson)
                // 引擎就绪后才允许配对, 免得用户点下去必然失败
                pairButtonRef?.isEnabled = ok
                nameSaveButton?.isEnabled = ok
                openDirButton?.isEnabled = ok
                if (deviceName.isNotBlank()) deviceNameInput?.setText(deviceName)
                receiveDirView?.text = receiveDir.ifBlank { "—" }
            }
            if (ok) loadTrustedDevices()
        }.also {
            it.isDaemon = true
            it.start()
        }
    }

    /** 把配置 JSON 压成一行可读摘要, 避免把整个 JSON 糊到界面上 */
    private fun summarizeConfig(json: String): String {
        if (json.isBlank()) return "—"
        return try {
            val o = JSONObject(json)
            val name = o.optString("device_name", "?")
            val tp = o.optInt("transfer_port", 0)
            val dp = o.optInt("discovery_port", 0)
            "$name · 传输 $tp · 发现 $dp"
        } catch (e: Exception) {
            "解析失败"
        }
    }

    // ---------------------------------------------------------------- 在线升级

    /**
     * 检查是否有新版本。
     *
     * 全程后台线程：网络请求在弱网下可能卡满超时，主线程做这件事直接 ANR。
     *
     * @param silent 启动时自动检查传 true —— **失败不打扰**。离线用户每次
     *        开应用都被弹一句"检查更新失败"，得到的结论是"这软件毛病多"，
     *        而不是"我现在没网"。
     */
    private fun checkForUpdate(silent: Boolean) {
        if (updateThread?.isAlive == true) return
        updateCheckButton?.isEnabled = false
        updateActionButton?.isEnabled = false
        if (!silent) updateStatusView?.text = "正在检查…"

        val t = Thread({
            val result = runCatching { UpdateChecker.check(BuildConfig.VERSION_NAME) }
                .getOrElse {
                    UpdateChecker.CheckResult.Failed("检查更新失败：${it.message}")
                }
            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                updateCheckButton?.isEnabled = true
                applyCheckResult(result, silent)
            }
        }, "feisuo-update-check")
        t.isDaemon = true
        updateThread = t
        t.start()
    }

    private fun applyCheckResult(result: UpdateChecker.CheckResult, silent: Boolean) {
        when (result) {
            is UpdateChecker.CheckResult.UpToDate -> {
                pendingRelease = null
                pendingApk = null
                updateStatusView?.text = "已是最新版本 v${BuildConfig.VERSION_NAME}"
                updateActionButton?.visibility = View.GONE
            }

            is UpdateChecker.CheckResult.Newer -> {
                pendingRelease = result.release
                updateStatusView?.text = "发现新版本 ${result.release.tagName}"
                updateActionButton?.apply {
                    visibility = View.VISIBLE
                    isEnabled = true
                    text = "下载并安装 ${result.release.tagName}"
                }
            }

            is UpdateChecker.CheckResult.Failed -> {
                pendingRelease = null
                updateStatusView?.text = if (silent) "尚未检查" else result.message
                if (!silent) toast(result.message)
                updateActionButton?.visibility = View.GONE
            }
        }
    }

    /**
     * 下载（下载完直接进入安装）。
     *
     * 一次点击走完"下载 → 校验 → 拉起系统安装器"：中间只让用户看进度，
     * 不要求他再点一次 —— 多一次点击只会多一次"我点了怎么没反应"。
     * 只有在系统要求授权「安装未知应用」时，才会真的打断一次。
     */
    private fun downloadAndInstall() {
        val ready = pendingApk
        if (ready != null && ready.isFile) {
            installPendingApk(ready)
            return
        }
        val release = pendingRelease ?: return

        updateCheckButton?.isEnabled = false
        updateActionButton?.isEnabled = false
        updateStatusView?.text = "正在下载 ${release.tagName}…"

        val t = Thread({
            val dl = UpdateChecker.download(this, release) { received, total ->
                // 回调在**下载线程**上，必须切回主线程再碰 View
                runOnUiThread {
                    if (isFinishing || isDestroyed) return@runOnUiThread
                    updateStatusView?.text = "正在下载 ${release.tagName} · " +
                        progressText(received, total)
                }
            }
            // 解析 APK 也要读几百 KB 的 zip 目录，放后台线程做
            val verifyError = if (dl is UpdateChecker.DownloadResult.Ok) {
                ApkInstaller.verify(this, dl.file)
            } else {
                null
            }

            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                updateCheckButton?.isEnabled = true
                when {
                    dl is UpdateChecker.DownloadResult.Failed -> {
                        updateStatusView?.text = dl.message
                        updateActionButton?.apply {
                            visibility = View.VISIBLE
                            isEnabled = true
                            text = "重试下载 ${release.tagName}"
                        }
                        toast(dl.message)
                    }

                    verifyError != null -> {
                        // 校验失败时**不装**，并清掉这份包：留着它只会让下一次
                        // "重试"复用一个错误的文件，错误会一直复现。
                        pendingApk = null
                        UpdateChecker.clearDownloads(this)
                        updateStatusView?.text = verifyError
                        updateActionButton?.apply {
                            visibility = View.VISIBLE
                            isEnabled = true
                            text = "重新下载 ${release.tagName}"
                        }
                        toast(verifyError)
                    }

                    else -> {
                        val file = (dl as UpdateChecker.DownloadResult.Ok).file
                        pendingApk = file
                        updateStatusView?.text = "${release.tagName} 已下载并校验通过"
                        updateActionButton?.apply {
                            visibility = View.VISIBLE
                            isEnabled = true
                            text = "立即安装"
                        }
                        installPendingApk(file)
                    }
                }
            }
        }, "feisuo-update-download")
        t.isDaemon = true
        updateThread = t
        t.start()
    }

    /**
     * 把更新包交给系统安装器。
     *
     * 「安装未知应用」没开时**先带用户去开**，并记下这次被打断的安装 ——
     * 回到界面由 [onResume] 续上。直接拉起安装器在那种状态下是
     * `SecurityException` 或什么都不发生，两种都不该让用户看到。
     */
    private fun installPendingApk(apk: File) {
        if (!ApkInstaller.canInstallPackages(this)) {
            awaitingUnknownSources = true
            updateStatusView?.text = "需要允许「安装未知应用」，授权后回到飞梭会自动继续"
            if (!ApkInstaller.openUnknownSourcesSettings(this)) {
                awaitingUnknownSources = false
                toast("请到「设置 → 应用 → 飞梭 → 安装未知应用」里允许后重试")
            }
            return
        }
        val err = ApkInstaller.startInstall(this, apk)
        if (err != null) {
            updateStatusView?.text = err
            toast(err)
        } else {
            updateStatusView?.text = "已交给系统安装器，按系统提示完成安装"
        }
    }

    private fun progressText(received: Long, total: Long): String {
        val got = formatMegabytes(received)
        return if (total > 0) {
            val pct = (received * 100 / total).coerceIn(0, 100)
            "$pct%（$got / ${formatMegabytes(total)}）"
        } else {
            got
        }
    }

    private fun formatMegabytes(bytes: Long): String = when {
        bytes >= 1024 * 1024 -> String.format("%.1f MB", bytes / (1024.0 * 1024.0))
        bytes >= 1024 -> String.format("%.0f KB", bytes / 1024.0)
        else -> "$bytes B"
    }

    // ---------------------------------------------------------------- 引擎与服务

    private fun startDaemonService() {
        val serviceIntent = Intent(this, FeisuoDaemonService::class.java)
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                startForegroundService(serviceIntent)
            } else {
                startService(serviceIntent)
            }
            Log.i(TAG, "已请求拉起飞梭守护服务")
        } catch (e: Exception) {
            Log.e(TAG, "拉起守护服务失败: ${e.message}", e)
        }
    }

    private fun requestNotificationPermissionIfNeeded() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
            return
        }
        val granted = checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED
        if (granted) {
            return
        }
        try {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQUEST_POST_NOTIFICATIONS)
        } catch (e: Exception) {
            Log.w(TAG, "申请通知权限失败: ${e.message}")
        }
    }

    /**
     * 通知是否**真的**能显示出来。
     *
     * 三层都要查，少一层就会给出错误结论：
     * 1. Android 13+ 的运行时权限（没授权 = 通知被系统丢弃）；
     * 2. 应用级通知总开关（用户可能在系统设置里关掉）；
     * 3. 通知渠道本身（渠道被关时应用级开关仍是"开"）。
     */
    private fun notificationsUsable(): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return false
        }
        val nm = getSystemService(NotificationManager::class.java) ?: return false
        if (!nm.areNotificationsEnabled()) return false
        val channel = nm.getNotificationChannel(FeisuoDaemonService.CHANNEL_ID)
        return channel == null || channel.importance != NotificationManager.IMPORTANCE_NONE
    }

    /** 通知栏状态的人话描述：用户能据此判断"锁屏后还收不收得到文件"。 */
    private fun notificationStateText(): String {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return "未授权 · 锁屏后可能收不到文件"
        }
        val nm = getSystemService(NotificationManager::class.java) ?: return "未知"
        if (!nm.areNotificationsEnabled()) return "已关闭 · 锁屏后可能收不到文件"
        val channel = nm.getNotificationChannel(FeisuoDaemonService.CHANNEL_ID)
        if (channel != null && channel.importance == NotificationManager.IMPORTANCE_NONE) {
            return "已关闭 · 请打开「飞梭局域网传输服务」通知渠道"
        }
        return "已开启 · 锁屏也能接收文件"
    }

    /** 同步状态行与操作按钮（不可用时才显示按钮，避免界面上挂着无用入口）。 */
    private fun updateNotificationStatus() {
        notificationStatusView?.text = notificationStateText()
        val usable = notificationsUsable()
        notificationActionButton?.apply {
            visibility = if (usable) View.GONE else View.VISIBLE
            text = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
                checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
                PackageManager.PERMISSION_GRANTED
            ) {
                "开启通知"
            } else {
                "去系统设置开启"
            }
        }
    }

    /**
     * 通知不可用时的按钮动作。
     *
     * 先分清楚"缺权限"还是"用户自己关了"：前者还能弹系统授权框，
     * 后者只能把他带到系统设置那一页 —— 再弹一次权限框系统会直接无视，
     * 用户会以为按钮坏了。
     */
    private fun onNotificationAction() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            requestNotificationPermissionIfNeeded()
            return
        }
        try {
            startActivity(
                Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                    .putExtra(Settings.EXTRA_APP_PACKAGE, packageName)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            )
        } catch (e: Exception) {
            Log.w(TAG, "打开通知设置失败: ${e.message}")
            toast("无法打开系统通知设置, 请到「设置 → 应用 → 飞梭 → 通知」里手动开启")
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQUEST_POST_NOTIFICATIONS) return
        val granted = grantResults.isNotEmpty() &&
            grantResults[0] == PackageManager.PERMISSION_GRANTED
        updateNotificationStatus()
        if (granted) {
            Log.i(TAG, "通知权限已授予, 前台通知可见, 锁屏待命可用")
            // 权限授予**之前**那条常驻通知已经被系统丢弃，不重挂就永远看不到。
            FeisuoDaemonService.refreshNotification(this)
        } else {
            Log.w(TAG, "通知权限被拒绝: 前台通知不可见, 锁屏后系统可能冻结网络")
            toast("未授予通知权限，锁屏后可能无法接收局域网文件")
        }
    }

    // ---------------------------------------------------------------- 小工具

    private fun addTitle(text: String, sizeSp: Float): TextView =
        TextView(this).apply {
            this.text = text
            setTextSize(TypedValue.COMPLEX_UNIT_SP, sizeSp)
            setTextColor(Color.WHITE)
            setTypeface(typeface, Typeface.BOLD)
        }.also { root.addView(it) }

    private fun addSubtitle(text: String): TextView =
        TextView(this).apply {
            this.text = text
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFF9AA4B2.toInt())
            setPadding(0, dp(4), 0, dp(18))
        }.also { root.addView(it) }

    private fun section(text: String): TextView =
        TextView(this).apply {
            this.text = text
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF2FBF8A.toInt())
            setTypeface(typeface, Typeface.BOLD)
            setPadding(0, dp(20), 0, dp(6))
        }.also { root.addView(it) }

    private fun addRow(label: String, value: String): TextView {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(0, dp(6), 0, dp(6))
        }
        val l = TextView(this).apply {
            text = label
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFF9AA4B2.toInt())
            layoutParams = LinearLayout.LayoutParams(dp(88), ViewGroup.LayoutParams.WRAP_CONTENT)
        }
        val v = TextView(this).apply {
            text = value
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(Color.WHITE)
            // 设备指纹是长字符串, 必须允许换行, 否则会被截断成看不出是什么
            setTextIsSelectable(true)
        }
        row.addView(l)
        row.addView(v, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        root.addView(row)
        return v
    }

    private fun addParagraph(text: String): TextView =
        TextView(this).apply {
            this.text = text
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFFCBD3DE.toInt())
            setLineSpacing(dp(4).toFloat(), 1f)
        }.also { root.addView(it, lp(top = dp(4))) }

    /** 单行输入框, 配对表单用 */
    private fun addInput(label: String, hint: String): EditText =
        EditText(this).apply {
            this.hint = "$label · $hint"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(Color.WHITE)
            setHintTextColor(0xFF6B7686.toInt())
            setSingleLine(true)
        }.also { root.addView(it, lp(top = dp(8))) }

    private fun toast(msg: String) {
        runOnUiThread {
            if (!isFinishing && !isDestroyed) {
                Toast.makeText(this, msg, Toast.LENGTH_LONG).show()
            }
        }
    }

    private fun lp(top: Int) = LinearLayout.LayoutParams(
        ViewGroup.LayoutParams.MATCH_PARENT,
        ViewGroup.LayoutParams.WRAP_CONTENT
    ).apply { topMargin = top }

    private fun dp(v: Int): Int =
        TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v.toFloat(), resources.displayMetrics)
            .toInt()
}
