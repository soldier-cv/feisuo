package net.findfine.feisuo

import android.app.Activity
import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.Parcelable
import android.util.Log
import android.widget.Toast
import java.io.File

/**
 * 飞梭 Android 系统原生"分享到飞梭"集成 Activity。
 * 用户在系统相册、文件管理器、微信等应用中点击"分享"，直接唤起飞梭并推送到常用电脑。
 *
 * 关键实现约束（两条，缺一不可）：
 * 1. ACTION_SEND 附带的 URI 授权是 **Activity 作用域** 的，Activity 一旦
 *    finish()，系统立即回收授权。因此必须在 finish() 之前把字节流**同步**
 *    落盘到应用私有暂存区，绝不能"先 finish 再异步读取"。
 * 2. 但**推送绝不能放在主线程**：core 的 sendFiles 会阻塞等待一次真实
 *    文件传输（4 MiB 分块 + 30 秒 IO 超时，大文件可达数分钟）。放在
 *    onCreate 里就是必然 ANR，系统会直接杀掉进程，用户看到"分享后应用闪退"。
 *    所以拆成两段：主线程只做暂存（需要 URI 授权），随后立刻把推送
 *    交给后台线程。
 *
 * @author xudong.hua,gemini
 * @since 2026-09-28 11:50 星期一
 */
class ShareTargetActivity : Activity() {

    companion object {
        private const val TAG = "FeisuoShareTarget"
        private const val MAX_SHARED_FILES = 50
    }

    /** 主线程 Handler: 后台线程完成后把提示切回主线程弹 */
    private val mainHandler = Handler(Looper.getMainLooper())

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        try {
            val staged = stageIncomingShare(intent)
            if (staged.isEmpty()) {
                Toast.makeText(this, "没有识别到可分享的文件", Toast.LENGTH_SHORT).show()
            } else {
                Log.i(TAG, "已暂存 ${staged.size} 个系统分享文件，转后台推送至受信设备")
                // 必须**真的**发出去。旧实现暂存完就 finish, 只弹一句
                // "正在推送到电脑…", 而推送逻辑根本不存在 —— 用户看到提示后
                // 再无下文, 文件永远躺在暂存目录里, 整个"分享到飞梭"等于没接上。
                //
                // 但推送必须离开主线程: 见类注释的约束 2。
                val appContext = applicationContext
                Thread({
                    try {
                        // 暂存区必须有上界: 目标电脑长期离线时, 失败的文件会
                        // 一直堆积, 用户会在不知情的情况下把存储填满。
                        // 裁剪放在后台线程, 避免 listFiles/delete 拖慢主线程。
                        ShareDispatcher.trimStaging(appContext)
                        when (val outcome = ShareDispatcher.dispatchStaged(staged)) {
                            is ShareDispatcher.Outcome.Sent ->
                                toast(appContext, "已发送 ${outcome.count} 个文件到 ${outcome.targetName}")

                            is ShareDispatcher.Outcome.Deferred ->
                                // 说清楚会怎样: 文件已留存, 且引擎会在网络恢复
                                // 或引擎就绪时自动重试(ShareDispatcher.retryPending)。
                                toast(appContext, "${outcome.reason}；${staged.size} 个文件已暂存，电脑上线后会自动重发")

                            is ShareDispatcher.Outcome.Failed ->
                                // 同样是真的: 守护服务会重试, 不是空头承诺。
                                toast(appContext, "发送失败：${outcome.reason}；文件已暂存，电脑上线后会自动重发")
                        }
                    } catch (e: Throwable) {
                        Log.e(TAG, "后台推送异常: ${e.message}", e)
                        toast(appContext, "推送失败：${e.message}")
                    }
                }, "feisuo-share-dispatch").apply { isDaemon = true }.start()
            }
        } catch (e: Exception) {
            Log.e(TAG, "处理分享内容失败: ${e.message}", e)
            Toast.makeText(this, "分享失败: ${e.message}", Toast.LENGTH_LONG).show()
        } finally {
            // 授权在 finish() 后即失效, 所以暂存必须在退出前完成;
            // 推送已交给后台线程, 文件在磁盘上, 不再需要 URI 授权。
            finish()
        }
    }

    /** 线程安全的 Toast: 后台线程不能直接弹, 必须切回主线程 */
    private fun toast(ctx: Context, msg: String) {
        mainHandler.post { Toast.makeText(ctx, msg, Toast.LENGTH_LONG).show() }
    }

    /** 解析并落盘分享内容, 返回暂存文件的绝对路径列表 */
    private fun stageIncomingShare(intent: Intent?): List<String> {
        val action = intent?.action ?: return emptyList()
        val uris = mutableListOf<Uri>()

        when (action) {
            Intent.ACTION_SEND -> extractSingleUri(intent)?.let { uris.add(it) }
            Intent.ACTION_SEND_MULTIPLE -> uris.addAll(extractMultipleUris(intent))
            else -> return emptyList()
        }

        // 微信 / Chrome / 剪贴板类应用经常把内容放在 ClipData 而不是 EXTRA_STREAM,
        // 旧实现只读 EXTRA_STREAM, 这类分享会静默失败(连提示都没有)。
        intent.clipData?.let { clip: ClipData ->
            for (i in 0 until clip.itemCount) {
                clip.getItemAt(i).uri?.let { if (!uris.contains(it)) uris.add(it) }
            }
        }

        if (uris.isEmpty()) return emptyList()

        val stagingDir = FeisuoRuntime.stagingDir(this)
        if (!stagingDir.exists() && !stagingDir.mkdirs()) {
            Log.e(TAG, "创建暂存目录失败: ${stagingDir}")
            return emptyList()
        }

        val staged = mutableListOf<String>()
        for (uri in uris.take(MAX_SHARED_FILES)) {
            // 单个文件失败不应中断整批分享, 故这里不使用 run/continue,
            // 也不用在 inline lambda 里 break/continue (Kotlin 2.x 需显式开启实验特性)
            val target = try {
                stageOne(uri, stagingDir)
            } catch (e: Exception) {
                Log.e(TAG, "暂存文件失败: $uri -> ${e.message}")
                null
            }
            if (target != null) {
                staged.add(target.absolutePath)
            }
        }
        return staged
    }

    /** 暂存单个分享项, 返回落盘文件; 读取失败时抛异常交给调用方处理 */
    private fun stageOne(uri: Uri, stagingDir: File): File {
        val target = File(stagingDir, resolveDisplayName(uri))
        val input = contentResolver.openInputStream(uri)
            ?: throw IllegalStateException("无法读取内容流")
        input.use { stream ->
            target.outputStream().use { output ->
                stream.copyTo(output)
            }
        }
        return target
    }

    private fun extractSingleUri(intent: Intent): Uri? =
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra<Parcelable>(Intent.EXTRA_STREAM) as? Uri
            }
        }.getOrNull()

    /**
     * 必须整体 runCatching：本方法是 exported 的，
     * 任意应用都能塞一个非 Uri 的 Parcelable 进来。
     * 旧实现里 API 33+ 分支直接 clazz.cast()，会抛 ClassCastException 把进程打崩。
     */
    private fun extractMultipleUris(intent: Intent): List<Uri> =
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableArrayListExtra<Parcelable>(Intent.EXTRA_STREAM)
                    ?.filterIsInstance<Uri>()
                    .orEmpty()
            }
        }.getOrElse { e ->
            Log.w(TAG, "解析 EXTRA_STREAM 失败: ${e.message}")
            emptyList()
        }

    private fun resolveDisplayName(uri: Uri): String {
        val raw = runCatching {
            contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                val index = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
                if (index >= 0 && cursor.moveToFirst()) cursor.getString(index) else null
            }
        }.getOrNull()

        val fallback = uri.lastPathSegment?.substringAfterLast('/') ?: "shared_${System.currentTimeMillis()}"
        val name = raw?.takeIf { it.isNotBlank() } ?: fallback

        // 不可信输入必须消毒, 否则 ../ 可以逃出暂存目录
        val safe = name.replace(Regex("[\\\\/:*?\"<>|\\u0000-\\u001F]"), "_").trim { it <= ' ' }
        return safe.takeIf { it.isNotEmpty() && it != "." && it != ".." }
            ?.take(120)
            ?: "shared_${System.currentTimeMillis()}"
    }
}
