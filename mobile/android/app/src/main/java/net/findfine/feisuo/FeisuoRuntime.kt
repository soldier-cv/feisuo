package net.findfine.feisuo

import android.content.Context
import android.util.Log
import java.io.File

/**
 * 飞梭 Android 端的进程启动参数契约。
 *
 * [android.content.Context] 的 dataDir 无法通过 JNI 传给 Rust,
 * 这里定义一个显式的注入点: Android 宿主用 [publish] 把 app 私有目录
 * 写进本进程的环境变量, core 的 `resolve_app_dir()` 读取该变量。
 *
 * 为什么必须这样: core 原先在 Android 上会退化成 CWD 相对路径,
 * 而 Android 应用的 CWD 不是可持久化位置 —— 结果是每次冷启动
 * 换一个目录, 设备指纹与信任库全部丢失, 等于每重启一次就要重新配对,
 * 直接违背"一次配对、终生免密"的产品红线。
 *
 * @author xudong.hua,gemini
 * @since 2026-09-30 10:20 星期三
 */
object FeisuoRuntime {

    private const val TAG = "FeisuoRuntime"

    /** 引擎是否已尝试初始化过 (成功与否都置位, 避免反复 JNI 调用) */
    private var started = false

    /** core 数据根目录 (配置 / 信任库 / 设备私钥 / 日志) */
    fun appDir(context: Context): File {
        val dir = File(context.filesDir, "feisuo")
        if (!dir.exists()) dir.mkdirs()
        return dir
    }

    /** 分享文件的暂存目录 */
    fun stagingDir(context: Context): File {
        val dir = File(appDir(context), "share-staging")
        if (!dir.exists()) dir.mkdirs()
        return dir
    }

    /**
     * 启动 core 引擎, 数据目录固定在 app 私有目录。
     *
     * 引擎是**进程内单例**: 每调一次 core 初始化都会重新构造
     * DiscoveryService / TransferServer / TransferClient, 结果是同一个
     * 进程里起了两套网络栈 —— 传输端口被自己占住, 第二个实例直接启动失败。
     * Service 的 onCreate 与 MainActivity 都会走到这里, 所以必须做幂等。
     * (Rust 侧的 `ENGINE` 静态单例也做了同样兜底, 两层保护。)
     *
     * 必须在**后台线程**调用: 它会阻塞在 core 的 tokio runtime 上建 runtime
     * 并绑定 UDP/TCP 端口, 实测十几到几十毫秒, 在主线程调用会触发 ANR。
     */
    @Synchronized
    private fun ensureEngine(context: Context) {
        if (started) return
        started = true
        val dir = appDir(context)
        stagingDir(context)
        Log.i(TAG, "core 数据目录: ${dir.absolutePath}")
        try {
            val handle = NativeBridge.initEngine(dir.absolutePath)
            if (handle == NativeBridge.INVALID_HANDLE) {
                // 句柄 0 = 引擎没起来。必须记下来并让界面能看到,
                // 否则用户只会看到"设备永远搜不到", 完全无从判断是
                // 网络问题还是引擎根本没启动。
                failedReason = "core 引擎启动失败 (数据目录不可写或端口被占用)"
                Log.e(TAG, failedReason!!)
                return
            }
            engine = handle
            failedReason = null
            Log.i(TAG, "core 引擎已启动, handle=$engine")
        } catch (e: UnsatisfiedLinkError) {
            // libfeisuo_core.so 缺失或 ABI 不匹配 (打包时漏了
            // mobile/android/build-native.ps1 的产物)。
            failedReason = "core 动态库缺失, 请重新构建 APK"
            Log.e(TAG, "$failedReason: ${e.message}")
        } catch (e: Throwable) {
            failedReason = "core 引擎异常: ${e.message}"
            Log.e(TAG, failedReason, e)
        }
    }

    /** 供 Service / Activity 在 onCreate 里调用, 确保引擎已就绪 */
    fun ensureStarted(context: Context) {
        ensureEngine(context.applicationContext)
    }

    /**
     * 停掉 core 引擎并复位单例状态, 使后续 [ensureStarted] 能重新拉起。
     *
     * 为什么必须有这个方法: 通知栏上那个"停止"按钮, 旧实现只调了
     * `stopSelf()` —— Service 死了, 但 Rust 引擎还**活在同一个进程里**:
     * UDP 发现广播循环继续发心跳(电脑侧照样看到这台手机在线), 传输端口
     * 也继续被占着(再拉起服务会因端口被占而失败)。用户看到"已停止"却
     * 设备仍在线, 属于功能与语义直接矛盾。
     *
     * 非阻塞: core 侧的 shutdown 只做 `stop()`(置标志 + abort 任务), 不
     * 等待网络栈收尾, 所以可以安全地放在主线程调用。
     */
    @Synchronized
    fun shutdown() {
        val h = engine
        if (h == NativeBridge.INVALID_HANDLE) {
            started = false
            return
        }
        try {
            NativeBridge.shutdownEngine(h)
            Log.i(TAG, "core 引擎已停止")
        } catch (e: Throwable) {
            // 停不掉也不能让 Service 的收尾路径崩掉 —— 进程即将退出,
            // 引擎会随进程一起消失。
            Log.e(TAG, "停止 core 引擎异常(将随进程退出而释放): ${e.message}", e)
        } finally {
            engine = NativeBridge.INVALID_HANDLE
            failedReason = null
            // 必须复位, 否则引擎停掉后再也拉不起来。
            started = false
        }
    }

    /** 引擎是否真的在跑 (未启动 / 启动失败都为 false) */
    fun isEngineRunning(): Boolean = engine != NativeBridge.INVALID_HANDLE

    /**
     * 启动失败原因; null 表示正常。
     * 界面应当把它显示出来, 而不是让用户对着一个"搜不到设备"的界面发呆。
     */
    @Volatile
    var failedReason: String? = null
        private set

    /** 引擎句柄 (core 侧持有, 这里只做生命周期传递) */
    @Volatile
    var engine: Long = NativeBridge.INVALID_HANDLE
        private set
}
