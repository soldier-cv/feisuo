package net.findfine.feisuo

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONObject

/**
 * 飞梭 Android 端主界面。
 *
 * 存在的必要性：
 * 1. 清单里若没有 MAIN/LAUNCHER 过滤器，getLaunchIntentForPackage() 必然返回 null，
 *    常驻通知的点击会因 null Intent 抛 NPE；
 * 2. Android 13+ 的 POST_NOTIFICATIONS 必须由前台界面申请，
 *    否则"常驻通知"这一核心防杀后台手段在 13+ 上根本不可见。
 *
 * **绝对不能在申请权限后立刻 finish()**：
 * 权限弹窗依附于 Activity 的窗口, Activity 在 onCreate 里就 finish 的话,
 * 弹窗还没贴上窗口就被销毁, 系统会把这次申请当作未处理 ——
 * 用户永远看不到弹窗, 通知权限停留在拒绝态, 前台通知不可见,
 * 后台服务随后被系统回收, "开机自连、无人值守"的核心承诺在 13+ 上直接失效。
 * 所以这里保持 Activity 存活到 onRequestPermissionsResult 回调, 之后再退出。
 *
 * @author xudong.hua,gemini
 * @since 2026-09-29 13:40 星期二
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
    private var ipInput: android.widget.EditText? = null
    private var pinInput: android.widget.EditText? = null
    private var pairButtonRef: Button? = null
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
    }

    override fun onDestroy() {
        worker?.interrupt()
        worker = null
        pairThread?.interrupt()
        pairThread = null
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

        root.addView(section("运行状态"))
        engineStatusView = addRow("引擎", "正在启动…")
        deviceIdView = addRow("设备指纹", "读取中…")
        configView = addRow("配置", "读取中…")

        root.addView(section("为什么需要通知权限"))
        addParagraph(
            "飞梭靠一条常驻前台通知保住后台进程, 从而在锁屏、息屏、切 Wi-Fi 之后" +
                "仍能被局域网里的受信设备发现并投递文件。Android 13 起通知需要你手动授权, " +
                "拒绝的话系统会回收后台服务, 无人值守接收随之失效。"
        )

        root.addView(section("如何配对"))
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
            inputType = android.text.InputType.TYPE_CLASS_NUMBER
        }
        val pairButton = Button(this).apply {
            text = "配对"
            isEnabled = false
            setOnClickListener { pairWithDesktop() }
        }
        pairButtonRef = pairButton
        root.addView(pairButton, lp(top = dp(16)))

        root.addView(section("已配对设备"))
        trustedView = addRow("受信设备", "读取中…")

        val refresh = Button(this).apply {
            text = "刷新状态"
            setOnClickListener { startEngineAsync() }
        }
        root.addView(refresh, lp(top = dp(8)))

        // 电池优化白名单。清单里声明了 REQUEST_IGNORE_BATTERY_OPTIMIZATIONS
        // 却从不使用, 等于白申请: Doze 会冻结并回收前台服务,
        // "开机自连、7x24 无人值守"在息屏几小时后必然失效。
        if (!isIgnoringBatteryOptimizations()) {
            root.addView(section("后台保活"))
            addParagraph(
                "系统当前的省电策略会在息屏一段时间后冻结飞梭的后台服务, " +
                    "导致局域网里找不到本机。把飞梭加入电池优化白名单即可解决。"
            )
            root.addView(Button(this).apply {
                text = "加入电池优化白名单"
                setOnClickListener { requestIgnoreBatteryOptimizations() }
            }, lp(top = dp(12)))
        }
    }

    /**
     * 是否已在电池优化白名单里。
     *
     * 用 `PowerManager.isIgnoringBatteryOptimizations` 而不是自己维护一份
     * SharedPreferences 标记: 用户随时可能去系统设置里把它改回去,
     * 自己记的标记会立刻变成过期的假信息。
     */
    private fun isIgnoringBatteryOptimizations(): Boolean {
        val pm = getSystemService(android.os.PowerManager::class.java) ?: return true
        return pm.isIgnoringBatteryOptimizations(packageName)
    }

    private fun requestIgnoreBatteryOptimizations() {
        if (isIgnoringBatteryOptimizations()) {
            android.widget.Toast.makeText(this, "已在白名单中", android.widget.Toast.LENGTH_SHORT).show()
            return
        }
        try {
            // 直接跳系统设置页。ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS 带包名
            // 会弹一个"是否允许"的确认框, 但部分 ROM 上不实现该 action,
            // 因此先试它, 捕获 ActivityNotFoundException 后退回手动引导。
            val direct = Intent(
                android.provider.Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                android.net.Uri.parse("package:$packageName")
            )
            if (direct.resolveActivity(packageManager) != null) {
                startActivity(direct)
            } else {
                startActivity(
                    Intent(android.provider.Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
                )
            }
        } catch (e: Exception) {
            Log.w(TAG, "跳转电池优化设置失败: ${e.message}")
            android.widget.Toast.makeText(
                this,
                "请到「设置 - 应用 - 飞梭 - 电池」中关闭省电限制",
                android.widget.Toast.LENGTH_LONG
            ).show()
        }
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
            // 13 以下没有这个权限, 可以安全退出了
            finish()
            return
        }
        val granted = checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED
        if (granted) {
            finish()
            return
        }
        try {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQUEST_POST_NOTIFICATIONS)
            // 注意: 这里**不能** finish()。等 onRequestPermissionsResult 回调再退。
        } catch (e: Exception) {
            Log.w(TAG, "申请通知权限失败: ${e.message}")
            finish()
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
        if (granted) {
            Log.i(TAG, "通知权限已授予, 前台通知可见, 无人值守接收可用")
        } else {
            // 必须把后果说清楚, 否则用户只会觉得"飞梭好像不工作了"
            Log.w(TAG, "通知权限被拒绝: 前台通知不可见, 息屏后系统极可能回收服务")
            android.widget.Toast
                .makeText(
                    this,
                    "未授予通知权限，息屏后飞梭可能被系统回收，无法无人值守接收文件",
                    android.widget.Toast.LENGTH_LONG
                )
                .show()
        }
        finish()
    }

    // ---------------------------------------------------------------- 小工具

    private fun addTitle(text: String, sizeSp: Float): TextView =
        TextView(this).apply {
            this.text = text
            setTextSize(TypedValue.COMPLEX_UNIT_SP, sizeSp)
            setTextColor(Color.WHITE)
            setTypeface(typeface, android.graphics.Typeface.BOLD)
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
            setTypeface(typeface, android.graphics.Typeface.BOLD)
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
    private fun addInput(label: String, hint: String): android.widget.EditText =
        android.widget.EditText(this).apply {
            this.hint = "$label · $hint"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(Color.WHITE)
            setHintTextColor(0xFF6B7686.toInt())
            setSingleLine(true)
        }.also { root.addView(it, lp(top = dp(8))) }

    private fun toast(msg: String) {
        runOnUiThread {
            if (!isFinishing && !isDestroyed) {
                android.widget.Toast.makeText(this, msg, android.widget.Toast.LENGTH_LONG).show()
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
