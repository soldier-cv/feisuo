package net.findfine.feisuo

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.app.NotificationManager
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.ColorStateList
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
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * 飞梭 Android 端主界面。
 *
 * 界面是标准的 App 外壳：固定品牌 AppBar + 三个分页 + 底部导航，
 * 不再是「一整页从上往下堆说明文字」的长表单。技术细节（设备指纹、
 * 端口）收进「我的」里默认折叠的「设备信息」。
 *
 * 核心功能：
 * 1. 互传页：引擎状态、已配对设备（可逐台删除）、接收目录；
 * 2. 配对页：输入电脑 IP 与 6 位配对码完成绑定；
 * 3. 我的页：设备名、通知权限、版本与更新、折叠的技术信息；
 * 4. 启动会话级前台守护服务（FeisuoDaemonService），锁屏仍可接收文件；
 * 5. 用户退出应用时优雅关闭服务，零后台驻留。
 *
 * @author xudong.hua,gemini
 * @since 2026-11-07 10:30 星期六
 */
class MainActivity : Activity() {

    companion object {
        private const val TAG = "FeisuoMainActivity"
        private const val REQUEST_POST_NOTIFICATIONS = 1001
    }

    /** 整页根布局：AppBar / 内容区 / 底部导航，竖排三段。 */
    private lateinit var root: LinearLayout
    private lateinit var appBar: LinearLayout
    private lateinit var contentHost: FrameLayout
    private lateinit var bottomNav: LinearLayout
    private val pages = mutableListOf<View>()
    private val navItems = mutableListOf<LinearLayout>()
    private val navIcons = mutableListOf<ImageView>()
    private val navLabels = mutableListOf<TextView>()

    /** 当前分页的内容容器；[section] 建的卡片挂进它。 */
    private var pageBody: LinearLayout? = null
    /** 当前卡片；[addRow] / [addInput] / [addButton] 挂进它。 */
    private var sectionBody: LinearLayout? = null

    /** 互传页：设备列表容器与「还没有配对设备」空态 */
    private var deviceListBody: LinearLayout? = null
    private var deviceEmptyView: TextView? = null
    private var engineStatusPill: TextView? = null
    private var localNameView: TextView? = null
    private var receiveDirViewMine: TextView? = null
    private var deviceInfoBody: LinearLayout? = null
    private var deviceInfoToggle: TextView? = null

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
        // 先定窗口外观（深色底 + 系统栏配色），再搭界面 —— 顺序反了会先闪一下白底。
        configureWindow()
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

    /**
     * 窗口外观：深色底 + 透明系统栏背景。
     *
     * targetSdk 35 在 Android 15 上强制沉浸式布局（edge-to-edge），
     * 内容会画到状态栏下面。这里统一关闭系统自动 inset，改由
     * [buildUi] 里的 insets 监听把内容让出来 —— 否则标题会被状态栏盖住。
     */
    @Suppress("DEPRECATION")
    private fun configureWindow() {
        WindowCompat.setDecorFitsSystemWindows(window, false)
        window.statusBarColor = getColor(R.color.feisuo_bg)
        window.navigationBarColor = getColor(R.color.feisuo_bg)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            isAppearanceLightStatusBars = false
            isAppearanceLightNavigationBars = false
        }
    }

    /**
     * 搭 App 外壳：固定 AppBar + 可切换的内容区 + 底部导航。
     *
     * 老版是「一个 ScrollView 里从上往下堆 8 张说明卡片」——功能都在，
     * 但看起来是一张网页而不是一个 App。这里把信息按"用户当下要做什么"
     * 分到三个分页，说明文字压到一行，技术细节默认折叠。
     */
    private fun buildUi() {
        root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(getColor(R.color.feisuo_bg))
        }
        setContentView(root)

        buildAppBar()
        contentHost = FrameLayout(this).apply {
            layoutParams = LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f
            )
        }
        root.addView(contentHost)
        buildBottomNav()
        buildPages()

        // 状态栏 / 刘海 / 手势条的高度由系统给，不写死数值。
        // AppBar 用品牌卡片色延伸到状态栏下方，手势条让给底栏。
        ViewCompat.setOnApplyWindowInsetsListener(root) { _view, insets ->
            val bars = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
            )
            appBar.setPadding(
                dp(18) + bars.left, dp(14) + bars.top, dp(18) + bars.right, dp(14)
            )
            contentHost.setPadding(bars.left, 0, bars.right, 0)
            bottomNav.setPadding(
                bars.left, dp(10), bars.right, dp(8) + bars.bottom
            )
            insets
        }
        ViewCompat.requestApplyInsets(root)
        switchTab(0)
    }

    /** 顶部品牌栏：图标 + 名称 + 一行状态，右侧是可一眼看到的引擎状态。 */
    private fun buildAppBar() {
        appBar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setBackgroundColor(getColor(R.color.feisuo_card))
            setPadding(dp(18), dp(14), dp(18), dp(14))
        }
        val badge = ImageView(this).apply {
            setImageResource(R.drawable.ic_stat_feisuo)
            imageTintList = ColorStateList.valueOf(getColor(R.color.feisuo_accent))
            setPadding(dp(8), dp(8), dp(8), dp(8))
            setBackgroundResource(R.drawable.bg_logo_badge)
            contentDescription = getString(R.string.app_name)
        }
        appBar.addView(badge, LinearLayout.LayoutParams(dp(38), dp(38)))

        val titles = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(12), 0, 0, 0)
        }
        titles.addView(TextView(this).apply {
            text = getString(R.string.app_name)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 19f)
            setTextColor(getColor(R.color.feisuo_text_primary))
            typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        })
        titles.addView(TextView(this).apply {
            text = "局域网互传"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(getColor(R.color.feisuo_text_secondary))
        })
        appBar.addView(titles, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))

        engineStatusPill = TextView(this).apply {
            text = "启动中"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(getColor(R.color.feisuo_text_secondary))
        }
        appBar.addView(engineStatusPill)
        root.addView(appBar, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT
        ))
    }

    /** 底部导航：三个真实可达的分页，选中项用品牌色。 */
    private fun buildBottomNav() {
        val divider = View(this).apply {
            setBackgroundColor(getColor(R.color.feisuo_card_stroke))
        }
        root.addView(divider, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, dp(1)
        ))

        bottomNav = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(getColor(R.color.feisuo_card))
            setPadding(0, dp(10), 0, dp(8))
        }
        val labels = listOf("互传", "配对", "我的")
        val icons = listOf(
            R.drawable.ic_tab_transfer,
            R.drawable.ic_tab_pair,
            R.drawable.ic_tab_me
        )
        for (i in labels.indices) {
            val item = LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                gravity = Gravity.CENTER
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
                setOnClickListener { switchTab(i) }
            }
            val icon = ImageView(this).apply {
                setImageResource(icons[i])
                layoutParams = LinearLayout.LayoutParams(dp(22), dp(22))
            }
            val label = TextView(this).apply {
                text = labels[i]
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                gravity = Gravity.CENTER
                setPadding(0, dp(3), 0, 0)
            }
            item.addView(icon)
            item.addView(label)
            bottomNav.addView(item)
            navItems.add(item)
            navIcons.add(icon)
            navLabels.add(label)
        }
        root.addView(bottomNav, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT
        ))
    }

    /** 建一个可滚动分页，并把它设为后续 [section] 的挂载目标。 */
    private fun newPage(): ScrollView {
        val scroll = ScrollView(this).apply {
            isFillViewport = true
            clipToPadding = false
            setBackgroundColor(getColor(R.color.feisuo_bg))
        }
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(12), dp(16), dp(24))
        }
        scroll.addView(body, ViewGroup.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT
        ))
        contentHost.addView(scroll, FrameLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT
        ))
        pages.add(scroll)
        pageBody = body
        sectionBody = null
        return scroll
    }

    private fun buildPages() {
        buildTransferPage()
        buildPairPage()
        buildMinePage()
    }

    /** 互传页：状态 + 设备列表（可逐台删除）+ 接收目录。 */
    private fun buildTransferPage() {
        newPage()
        section("本机状态")
        engineStatusView = addRow("引擎", "正在启动…")
        localNameView = addRow("设备名", "读取中…")

        section("已配对设备")
        val list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        deviceListBody = list
        sectionBody?.addView(list, lp(0))
        val empty = TextView(this).apply {
            text = "还没有配对设备"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(getColor(R.color.feisuo_text_secondary))
            setPadding(0, dp(8), 0, dp(8))
        }
        deviceEmptyView = empty
        sectionBody?.addView(empty, lp(0))
        addButton("刷新设备", primary = false) { startEngineAsync() }

        section("接收的文件")
        receiveDirView = addRow("保存位置", "读取中…")
        openDirButton = addButton("打开接收目录", primary = false) { openReceiveDir() }
        openDirButton?.isEnabled = false
        addHint("在其它应用里点「分享」→ 飞梭，即可发送到已配对设备")
    }

    /** 配对页：只留真正要动手的表单，不再铺四步教程。 */
    private fun buildPairPage() {
        newPage()
        section("配对电脑")
        addHint("在电脑端查看 6 位配对码，填入下方 IP 与配对码")
        ipInput = addInput("电脑 IP", "例如 192.168.1.10")
        pinInput = addInput("6 位配对码", "123456").apply {
            inputType = InputType.TYPE_CLASS_NUMBER
        }
        pairButtonRef = addButton("配对", primary = true) { pairWithDesktop() }
        pairButtonRef?.isEnabled = false

        section("配对状态")
        trustedView = addRow("已配对设备", "读取中…")
        addButton("刷新状态", primary = false) { startEngineAsync() }
    }

    /** 我的页：设置项聚合成行；设备指纹 / 端口这类技术信息默认折叠。 */
    private fun buildMinePage() {
        newPage()
        section("本机")
        deviceNameInput = addInput("设备名", "例如 客厅手机")
        nameSaveButton = addButton("保存设备名", primary = true) { saveDeviceName() }
        nameSaveButton?.isEnabled = false
        receiveDirViewMine = addRow("接收目录", "读取中…")

        section("通知")
        notificationStatusView = addRow("通知栏", "检查中…")
        notificationActionButton = addButton("开启通知", primary = false) { onNotificationAction() }

        section("版本与更新")
        // 版本号来自 BuildConfig（源头是仓库根 Cargo.toml），界面不写死。
        addRow("当前版本", BuildConfig.VERSION_NAME)
        updateStatusView = addRow("更新状态", "尚未检查")
        updateCheckButton = addButton("检查更新", primary = false) {
            checkForUpdate(silent = false)
        }
        // 只在"确实有可做的事"时出现：没有新版本时挂一个灰按钮，
        // 用户会反复点它并怀疑功能坏了。
        updateActionButton = addButton("下载并安装", primary = true) {
            downloadAndInstall()
        }.apply { visibility = View.GONE }

        section("设备信息")
        val toggle = TextView(this).apply {
            text = "展开技术信息"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(getColor(R.color.feisuo_text_secondary))
            setPadding(0, dp(4), 0, dp(4))
            setOnClickListener { toggleDeviceInfo() }
        }
        deviceInfoToggle = toggle
        sectionBody?.addView(toggle, lp(0))
        val infoBody = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
        }
        deviceInfoBody = infoBody
        sectionBody?.addView(infoBody, lp(0))
        // 临时把 sectionBody 指到折叠容器，让 addRow 挂进去。
        val outer = sectionBody
        sectionBody = infoBody
        deviceIdView = addRow("设备指纹", "读取中…")
        configView = addRow("配置", "读取中…")
        sectionBody = outer

        addHint("随用随走 · 退出应用即停止")
    }

    private fun toggleDeviceInfo() {
        val expanded = deviceInfoBody?.visibility == View.VISIBLE
        deviceInfoBody?.visibility = if (expanded) View.GONE else View.VISIBLE
        deviceInfoToggle?.text = if (expanded) "展开技术信息" else "收起技术信息"
    }

    /** 切换分页：内容只留当前页可见，导航项用品牌色标出选中态。 */
    private fun switchTab(index: Int) {
        for (i in pages.indices) {
            pages[i].visibility = if (i == index) View.VISIBLE else View.GONE
        }
        for (i in navItems.indices) {
            val color = getColor(
                if (i == index) R.color.feisuo_accent else R.color.feisuo_text_secondary
            )
            navIcons[i].imageTintList = ColorStateList.valueOf(color)
            navLabels[i].setTextColor(color)
            navLabels[i].typeface = Typeface.create(
                Typeface.DEFAULT, if (i == index) Typeface.BOLD else Typeface.NORMAL
            )
        }
    }

    /** 一行轻量提示：只给"下一步做什么"，不写说明书。 */
    private fun addHint(text: String): TextView =
        TextView(this).apply {
            this.text = text
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11.5f)
            setTextColor(getColor(R.color.feisuo_text_tertiary))
            setLineSpacing(dp(2).toFloat(), 1f)
        }.also { addToSection(it, top = dp(10)) }

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
                    JSONObject(probed).optInt("transfer_port", 0)
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

    /** 名册里的一行：id 用于删除，name / ip 用于展示。 */
    private data class DeviceRow(
        val id: String,
        val name: String,
        val ip: String,
        val trusted: Boolean,
    )

    /** 刷新已配对设备列表。配对成功后必须能看到结果, 否则用户无从确认。 */
    private fun loadTrustedDevices() {
        val t = Thread({
            val rows = runCatching {
                val arr = JSONArray(NativeBridge.trustedDevicesJson())
                (0 until arr.length()).mapNotNull { i ->
                    val o = arr.optJSONObject(i) ?: return@mapNotNull null
                    val id = o.optString("device_id")
                    if (id.isBlank()) return@mapNotNull null
                    DeviceRow(
                        id = id,
                        name = o.optString("device_name").ifBlank { "未命名设备" },
                        ip = o.optString("last_ip").ifBlank { "未知 IP" },
                        trusted = o.optBoolean("is_trusted", false),
                    )
                }
            }.getOrDefault(emptyList())

            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                renderDeviceRows(rows)
                trustedView?.text = when {
                    !FeisuoRuntime.isEngineRunning() -> "引擎未就绪"
                    rows.isEmpty() -> "尚未配对"
                    else -> "已配对 ${rows.size} 台"
                }
            }
        }, "feisuo-trusted-refresh")
        t.isDaemon = true
        t.start()
    }

    /** 每台设备一行，右侧给「删除」——改名后残留的旧条目就靠它清掉。 */
    private fun renderDeviceRows(rows: List<DeviceRow>) {
        val body = deviceListBody ?: return
        body.removeAllViews()
        deviceEmptyView?.visibility = if (rows.isEmpty()) View.VISIBLE else View.GONE
        for (d in rows) {
            val row = LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(0, dp(8), 0, dp(8))
            }
            val icon = ImageView(this).apply {
                setImageResource(R.drawable.ic_stat_feisuo)
                imageTintList = ColorStateList.valueOf(getColor(R.color.feisuo_accent))
                alpha = if (d.trusted) 1f else 0.45f
            }
            row.addView(icon, LinearLayout.LayoutParams(dp(20), dp(20)))

            val text = LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(10), 0, 0, 0)
            }
            text.addView(TextView(this).apply {
                this.text = d.name
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setTextColor(getColor(R.color.feisuo_text_primary))
            })
            text.addView(TextView(this).apply {
                this.text = if (d.trusted) d.ip else "${d.ip} · 未信任"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                setTextColor(getColor(R.color.feisuo_text_secondary))
            })
            row.addView(text, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))

            val delete = Button(this).apply {
                this.text = "删除"
                isAllCaps = false
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(getColor(R.color.feisuo_danger))
                setBackgroundResource(R.drawable.bg_button_secondary)
                backgroundTintList = null
                stateListAnimator = null
                minHeight = 0
                minimumHeight = 0
                setPadding(dp(10), dp(4), dp(10), dp(4))
                setOnClickListener { confirmDeleteDevice(d) }
            }
            row.addView(delete)
            body.addView(row, lp(0))
        }
    }

    /** 删除不可撤销，先二次确认；文案只说后果，不铺说明书。 */
    private fun confirmDeleteDevice(dev: DeviceRow) {
        AlertDialog.Builder(this)
            .setTitle("删除设备")
            .setMessage(
                "从设备列表中移除「${dev.name}」？\n\n" +
                    "将删除本机保存的配对记录与已知地址，传输记录会保留。"
            )
            .setNegativeButton("取消", null)
            .setPositiveButton("删除") { _, _ -> executeDeleteDevice(dev) }
            .show()
    }

    /** 删除是一次同步落盘，放后台线程（见 android_guard 的 BLOCKING_METHODS）。 */
    private fun executeDeleteDevice(dev: DeviceRow) {
        val t = Thread({
            val err = runCatching { NativeBridge.deleteDevice(dev.id) }
                .getOrElse { "删除失败: ${it.message}" }
            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                if (err == null) {
                    toast("已删除「${dev.name}」")
                    loadTrustedDevices()
                } else {
                    toast(err)
                }
            }
        }, "feisuo-delete-device")
        t.isDaemon = true
        t.start()
    }

    private fun startEngineAsync() {
        engineStatusView?.text = "正在启动…"
        engineStatusPill?.text = "启动中"
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
                engineStatusView?.setTextColor(
                    getColor(if (ok) R.color.feisuo_accent else R.color.feisuo_danger)
                )
                engineStatusPill?.text = if (ok) "运行中" else "未就绪"
                engineStatusPill?.setTextColor(
                    getColor(if (ok) R.color.feisuo_accent else R.color.feisuo_danger)
                )
                deviceIdView?.text = deviceId.ifEmpty { "—" }
                configView?.text = summarizeConfig(configJson)
                // 引擎就绪后才允许配对, 免得用户点下去必然失败
                pairButtonRef?.isEnabled = ok
                nameSaveButton?.isEnabled = ok
                openDirButton?.isEnabled = ok
                if (deviceName.isNotBlank()) deviceNameInput?.setText(deviceName)
                localNameView?.text = deviceName.ifBlank { "—" }
                receiveDirView?.text = receiveDir.ifBlank { "—" }
                receiveDirViewMine?.text = receiveDir.ifBlank { "—" }
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
                updateStatusView?.setTextColor(getColor(R.color.feisuo_accent))
                updateActionButton?.visibility = View.GONE
            }

            is UpdateChecker.CheckResult.Newer -> {
                pendingRelease = result.release
                updateStatusView?.text = "发现新版本 ${result.release.tagName}"
                updateStatusView?.setTextColor(getColor(R.color.feisuo_accent))
                updateActionButton?.apply {
                    visibility = View.VISIBLE
                    isEnabled = true
                    text = "下载并安装 ${result.release.tagName}"
                }
            }

            is UpdateChecker.CheckResult.Failed -> {
                pendingRelease = null
                updateStatusView?.text = if (silent) "尚未检查" else result.message
                updateStatusView?.setTextColor(
                    getColor(if (silent) R.color.feisuo_text_primary else R.color.feisuo_danger)
                )
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
        notificationStatusView?.setTextColor(
            getColor(if (usable) R.color.feisuo_accent else R.color.feisuo_danger)
        )
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

    /**
     * 分区标题 + 一张卡片。
     *
     * 卡片会被设为 [sectionBody]，后续的 [addRow] / [addInput] /
     * [addButton] / [addHint] 自动挂进这张卡片，直到下一个 [section]。
     * 挂到当前分页 [pageBody]；**不要**在调用处再 root.addView 一次
     * （同一个 View 挂两次会抛 IllegalStateException，应用启动即闪退）。
     */
    private fun section(text: String): TextView {
        val header = TextView(this).apply {
            this.text = text
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(getColor(R.color.feisuo_accent))
            typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
            setPadding(dp(2), dp(18), 0, dp(8))
        }
        val parent = pageBody
        if (parent == null) {
            root.addView(header)
        } else {
            parent.addView(header)
        }

        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(10), dp(16), dp(14))
            setBackgroundResource(R.drawable.bg_card)
        }
        if (parent == null) {
            root.addView(body)
        } else {
            parent.addView(body)
        }
        sectionBody = body
        return header
    }

    private fun addRow(label: String, value: String): TextView {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(0, dp(7), 0, dp(7))
        }
        val labelView = TextView(this).apply {
            text = label
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(getColor(R.color.feisuo_text_secondary))
            layoutParams = LinearLayout.LayoutParams(dp(84), ViewGroup.LayoutParams.WRAP_CONTENT)
        }
        val valueView = TextView(this).apply {
            text = value
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(getColor(R.color.feisuo_text_primary))
            setLineSpacing(dp(3).toFloat(), 1f)
            // 设备指纹是长字符串, 必须允许换行, 否则会被截断成看不出是什么
            setTextIsSelectable(true)
        }
        row.addView(labelView)
        row.addView(valueView, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))

        // 卡片存在时挂进卡片；没有 section 时回退到分页 / 根布局。
        // 这个 if/else 必须保留 root.addView 分支 —— android_guard 靠它识别
        // "自己会挂载"的 helper，删掉后守卫会判定 addRow 不再自挂载。
        val parent = sectionBody ?: pageBody ?: root
        if (parent === root) {
            root.addView(row, lp(top = dp(2)))
        } else {
            parent.addView(row, lp(top = dp(2)))
        }
        return valueView
    }

    /** 把 View 挂进当前卡片；没有 section 时挂到分页 / 根布局。 */
    private fun addToSection(view: View, top: Int = dp(2)) {
        val parent = sectionBody ?: pageBody ?: root
        if (parent === root) {
            root.addView(view, lp(top = top))
        } else {
            parent.addView(view, lp(top = top))
        }
    }

    /** 单行输入框, 配对表单用 */
    private fun addInput(label: String, hint: String): EditText =
        EditText(this).apply {
            this.hint = "$label · $hint"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setTextColor(getColor(R.color.feisuo_text_primary))
            setHintTextColor(getColor(R.color.feisuo_text_tertiary))
            setSingleLine(true)
            setBackgroundResource(R.drawable.bg_input)
            backgroundTintList = null
            setPadding(dp(14), dp(12), dp(14), dp(12))
            minHeight = dp(48)
        }.also { addToSection(it, top = dp(10)) }

    /**
     * 统一样式的操作按钮。
     *
     * @param primary true = 品牌绿实心（页面主操作）；false = 描边次按钮。
     */
    private fun addButton(label: String, primary: Boolean, onClick: () -> Unit): Button =
        Button(this).apply {
            text = label
            isAllCaps = false
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(
                resources.getColorStateList(
                    if (primary) R.color.button_primary_text else R.color.button_secondary_text,
                    theme
                )
            )
            setBackgroundResource(
                if (primary) R.drawable.bg_button_primary else R.drawable.bg_button_secondary
            )
            backgroundTintList = null
            stateListAnimator = null
            minHeight = dp(46)
            minimumHeight = dp(46)
            setPadding(dp(16), 0, dp(16), 0)
            if (primary) typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
            setOnClickListener { onClick() }
        }.also { addToSection(it, top = dp(12)) }

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
