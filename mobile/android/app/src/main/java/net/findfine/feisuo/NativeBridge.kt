package net.findfine.feisuo

import android.util.Log

/**
 * Rust core 与 Android 宿主之间的 JNI 桥。
 *
 * 对应的 Rust 实现在 `core/src/mobile.rs`, 随 `libfeisuo_core.so` 一起打包进 APK。
 * 产物由 `mobile/android/build-native.ps1` 交叉编译生成。
 *
 * 拆分到单独文件的原因: JNI 符号名 (`Java_net_findfine_feisuo_NativeBridge_xxx`)
 * 必须与包名 + 类名严格对应, 混在业务类里极易在重构时悄悄失配,
 * 而失配的表现是运行期 `UnsatisfiedLinkError`, 编译期完全看不出来。
 *
 * 因此改动本文件的任何 `external` 方法, 必须同步改 Rust 侧同名函数。
 *
 * 线程约定: 所有 native 方法都会阻塞在 core 内部的 tokio runtime 上,
 * 调用方**不要**在主线程调用 [initEngine] (冷启动要建 runtime + 绑端口,
 * 实测十几到几十毫秒), 否则会触发 ANR。
 *
 * @author xudong.hua,gemini
 * @since 2026-09-30 10:20 星期三
 */
object NativeBridge {

    private const val TAG = "FeisuoNative"
    private const val LIB_NAME = "feisuo_core"

    /** [initEngine] 失败时返回的哨兵句柄, 表示"引擎没起来" */
    const val INVALID_HANDLE: Long = 0L

    @Volatile
    private var loaded = false

    /**
     * 以指定数据目录启动引擎, 返回不透明句柄; 失败返回 [INVALID_HANDLE]。
     *
     * 数据目录必须是 app 私有目录 (`context.filesDir/feisuo`):
     * core 只能按传入路径读写配置、设备私钥与信任库, 沙箱路径猜不到。
     * 传错会导致每次冷启动都换一个新身份, 用户被迫反复配对。
     *
     * 重复调用是安全的 —— Rust 侧是进程内单例, 第二次会直接返回同一个句柄,
     * 不会重复绑定传输端口。
     */
    fun initEngine(appDir: String): Long {
        ensureLoaded()
        val handle = nativeInitEngine(appDir)
        if (handle == INVALID_HANDLE) {
            Log.e(TAG, "core 引擎启动失败, 数据目录=$appDir")
        } else {
            Log.i(TAG, "core 引擎已启动, handle=$handle, deviceId=${deviceId()}")
        }
        return handle
    }

    /** 停止引擎并释放资源。句柄为 [INVALID_HANDLE] 时是空操作。 */
    fun shutdownEngine(handle: Long) {
        if (!loaded || handle == INVALID_HANDLE) return
        nativeShutdownEngine(handle)
        Log.i(TAG, "core 引擎已停止")
    }

    /** 引擎是否已启动 */
    fun isEngineRunning(): Boolean = loaded && nativeIsEngineRunning() != 0

    /**
     * 本机设备指纹 (Ed25519 公钥派生), 界面与配对流程都要展示。
     * 引擎未启动时返回空串。
     */
    fun deviceId(): String = if (loaded) (nativeGetDeviceId() ?: "") else ""

    /** 当前配置的 JSON 快照; 引擎未启动时返回空串 */
    fun configJson(): String = if (loaded) (nativeGetConfigJson() ?: "") else ""

    /**
     * 已配对设备的 JSON 数组快照 (供系统分享流程挑选推送目标)。
     * 引擎未启动或读取失败时返回 "[]", 不会是 null。
     */
    fun trustedDevicesJson(): String = if (loaded) (nativeGetTrustedDevicesJson() ?: "[]") else "[]"

    /**
     * 主动探测对端 IP, 拿它**当前**的传输端口。
     *
     * 信任库里只记了 last_ip、没有记端口, 而端口可以在对端设置里改;
     * 直接拿默认端口去连必然连不上。返回 null 表示探测失败。
     */
    fun probeDevice(ip: String): String? = if (loaded) nativeProbeDevice(ip) else null

    /**
     * 取下一个待审批的传输请求 (最多等待 [timeoutMs] 毫秒); 没有则返回 null。
     *
     * 补这个接口的原因: 传输服务端在收到未受信设备的文件时是 **fail-closed** 的 ——
     * 找不到能应答的审批者就直接拒绝。此前 engine_host 把审批事件接收端直接丢弃,
     * 于是 Android 上"任何电脑都无法向本机投递文件", 且用户连"有传输在等你确认"
     * 都看不到。现在宿主可以真正把请求取出来, 弹通知让用户决定。
     */
    fun nextApproval(timeoutMs: Int): String? = if (loaded) nativeNextApproval(timeoutMs) else null

    /**
     * 回应审批请求。返回是否命中(已超时或已处理过时为 false)。
     */
    fun respondApproval(approvalId: String, allow: Boolean): Boolean =
        if (loaded) nativeRespondApproval(approvalId, allow) else false

    /**
     * 用 6 位 PIN 与指定设备完成双向绑定。
     *
     * 补这个接口的原因: 界面一直写着"输入配对码", 说明里也写着
     * "在下方输入桌面端显示的 6 位配对码", 实际却只弹一句
     * "配对界面将在后续版本接入"。配不上就等于分享推送永远命中
     * "尚未配对任何电脑", 整条发送链路不可达 —— 按钮与文案都在说谎。
     *
     * @return null 表示成功; 非 null 为可展示的中文失败原因。
     */
    fun pairWithDevice(ip: String, port: Int, pin: String): String? {
        if (!loaded) return "引擎未启动"
        val raw = nativePairWithDevice(ip, port, pin) ?: return "core 未返回结果"
        return runCatching {
            val obj = org.json.JSONObject(raw)
            if (obj.has("error")) obj.optString("error") else null
        }.getOrElse { "解析配对结果失败: ${it.message}" }
    }

    /**
     * 向指定设备发送一批本地文件。
     *
     * @return null 表示成功; 非 null 为可展示的中文失败原因。
     *   刻意不用 Boolean —— 宿主需要把"为什么失败"(未配对 / 对方拒绝 /
     *   端口不通)展示给用户, 一个 false 只会得到一句"发送失败"。
     */
    fun sendFiles(
        ip: String,
        port: Int,
        deviceId: String,
        deviceName: String,
        paths: Array<String>,
    ): String? = if (loaded) nativeSendFiles(ip, port, deviceId, deviceName, paths) else "引擎未启动"

    private fun ensureLoaded() {
        if (loaded) return
        synchronized(this) {
            if (loaded) return
            System.loadLibrary(LIB_NAME)
            loaded = true
            Log.i(TAG, "已加载 $LIB_NAME.so")
        }
    }

    private external fun nativeInitEngine(appDir: String): Long
    private external fun nativeShutdownEngine(handle: Long)
    private external fun nativeIsEngineRunning(): Int
    private external fun nativeGetDeviceId(): String?
    private external fun nativeGetConfigJson(): String?
    private external fun nativeGetTrustedDevicesJson(): String?
    private external fun nativeProbeDevice(ip: String): String?
    private external fun nativePairWithDevice(ip: String, port: Int, pin: String): String?
    private external fun nativeNextApproval(timeoutMs: Int): String?
    private external fun nativeRespondApproval(approvalId: String, allow: Boolean): Boolean
    private external fun nativeSendFiles(
        ip: String,
        port: Int,
        deviceId: String,
        deviceName: String,
        paths: Array<String>,
    ): String?
}
