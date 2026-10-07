package net.findfine.feisuo

import android.content.Context
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean

/**
 * 分享文件的推送器：被"系统分享到飞梭"与守护服务的重试流程共用。
 *
 * 抽出来的原因是一个真实的**说谎提示**：早期实现在发送失败时提示
 * "文件已保留，可稍后重试"，但整个代码库里根本不存在任何重试逻辑 ——
 * 用户被告知稍后会自动重发，实际上永远不会，而且失败的文件会**无限累积**
 * 在应用私有目录里（对方电脑离线一次就永久留下一份，直到占满存储）。
 * 既修提示又修行为，就必须有一个两处都能调用的推送入口。
 *
 * 线程约束：本类所有方法都会阻塞在 core 的 JNI 传输上（大文件可达数分钟），
 * **必须在后台线程调用**。desktop 侧有 android_guard 守卫盯住这一点。
 *
 * @author xudong.hua,gemini
 * @since 2026-09-30 10:20 星期三
 */
object ShareDispatcher {

    private const val TAG = "FeisuoShareDispatcher"

    /**
     * 暂存区上限。
     *
     * 必须有上界：如果目标电脑长期离线，而暂存区无界增长，用户会在毫不知情
     * 的情况下把手机存储填满。上界触发时按修改时间从旧到新丢弃，并在日志里
     * 明确记录丢了多少 —— 宁可让用户知道，也不能静默吃掉用户的文件。
     */
    private const val MAX_PENDING_FILES = 200
    private const val MAX_PENDING_BYTES = 256L * 1024 * 1024

    /** 一台可用的推送目标 */
    private data class Target(
        val deviceId: String,
        val deviceName: String,
        val ip: String,
        val port: Int,
    )

    /** 防止引擎就绪与网络恢复两个触发源同时发起重试 */
    private val retrying = AtomicBoolean(false)

    /** 推送结果。 */
    sealed class Outcome {
        /** 全部成功 */
        data class Sent(val targetName: String, val count: Int) : Outcome()

        /** 没有可用的推送目标（未配对 / 对方不在线） */
        data class Deferred(val reason: String) : Outcome()

        /** 推送过程中失败 */
        data class Failed(val reason: String) : Outcome()
    }

    /**
     * 推送一批已暂存的文件。成功才删除暂存副本。
     *
     * @param staged 暂存文件的绝对路径
     */
    fun dispatchStaged(staged: List<String>): Outcome {
        if (staged.isEmpty()) return Outcome.Deferred("没有待发送的文件")

        if (!FeisuoRuntime.isEngineRunning()) {
            return Outcome.Deferred("core 引擎尚未就绪")
        }

        val target = pickTarget()
            ?: return Outcome.Deferred("尚未配对任何电脑，或已配对电脑当前不在线")

        val error = NativeBridge.sendFiles(
            target.ip, target.port, target.deviceId, target.deviceName, staged.toTypedArray(),
        )
        return if (error == null) {
            Log.i(TAG, "已推送 ${staged.size} 个文件至 ${target.deviceName} (${target.ip}:${target.port})")
            staged.forEach { runCatching { File(it).delete() } }
            Outcome.Sent(target.deviceName, staged.size)
        } else {
            // 失败保留暂存副本, 交给 [retryPending] 后续重试。
            Log.e(TAG, "推送至 ${target.deviceName} 失败: $error")
            Outcome.Failed(error)
        }
    }

    /**
     * 重试暂存区里所有未发送完的文件。
     *
     * 由守护服务在"引擎就绪"和"网络恢复"两个时机调用 —— 这正是提示里
     * 承诺的"稍后重试"。返回本次成功发送的文件数。
     */
    fun retryPending(context: Context): Int {
        if (!retrying.compareAndSet(false, true)) {
            Log.d(TAG, "已有重试在进行中, 跳过本次触发")
            return 0
        }
        try {
            val dir = FeisuoRuntime.stagingDir(context)
            val files = dir.listFiles()?.filter { it.isFile }?.sortedBy { it.lastModified() }
                ?: return 0
            if (files.isEmpty()) return 0

            Log.i(TAG, "开始重试暂存区 ${files.size} 个文件")
            when (val outcome = dispatchStaged(files.map { it.absolutePath })) {
                is Outcome.Sent -> return outcome.count
                is Outcome.Deferred -> Log.i(TAG, "暂缓重试: ${outcome.reason}")
                is Outcome.Failed -> Log.w(TAG, "重试仍失败: ${outcome.reason}")
            }
            // 无论成功与否都重新裁剪一次：失败时可能有新分享进来，
            // 不裁剪的话暂存区会再次越过上界。
            trimStaging(context)
            return 0
        } finally {
            retrying.set(false)
        }
    }

    /**
     * 裁剪暂存区，防止无界增长。
     *
     * 按修改时间从旧到新丢弃，直到同时满足文件数与总字节数的上界。
     */
    fun trimStaging(context: Context): Int {
        val dir = FeisuoRuntime.stagingDir(context)
        val files = dir.listFiles()?.filter { it.isFile }?.sortedBy { it.lastModified() }
            ?: return 0

        var totalBytes = files.sumOf { it.length() }
        var dropped = 0
        for (f in files) {
            val overCount = files.size - dropped > MAX_PENDING_FILES
            val overBytes = totalBytes > MAX_PENDING_BYTES
            if (!overCount && !overBytes) break
            val len = f.length()
            if (f.delete()) {
                totalBytes -= len
                dropped++
            }
        }
        if (dropped > 0) {
            // 必须说出来: 静默删用户文件是最不可接受的一种行为。
            Log.w(TAG, "暂存区超出上限(最多 $MAX_PENDING_FILES 个 / $MAX_PENDING_BYTES 字节), 已丢弃最早的 $dropped 个文件")
        }
        return dropped
    }

    /**
     * 选一台可用的推送目标。
     *
     * 取第一台已配对设备，**探测它当前的传输端口**后再返回。探测是必须的：
     * 信任库只记了 `last_ip`，没有记端口，而端口是对端可在设置里改的，
     * 拿默认端口连必然失败。
     */
    private fun pickTarget(): Target? {
        val trusted = runCatching {
            val arr = JSONArray(NativeBridge.trustedDevicesJson())
            (0 until arr.length()).mapNotNull { i ->
                val o = arr.optJSONObject(i) ?: return@mapNotNull null
                // 必须显式过滤 is_trusted。
                // trust_store 的 list_devices() 返回**全部**记录(不过滤该列),
                // 而今天 is_trusted 恒为 1 —— 这是个没有被强制的隐含约定。
                // 一旦将来加入"撤销信任"并把它置 0, 这里就会静默地开始往
                // 已撤销的设备推送文件。显式过滤把隐含约定变成硬事实。
                if (!o.optBoolean("is_trusted", false)) return@mapNotNull null
                Triple(o.optString("device_id"), o.optString("device_name"), o.optString("last_ip"))
            }
        }.getOrDefault(emptyList())

        val first = trusted.firstOrNull { it.third.isNotBlank() } ?: return null
        val (deviceId, deviceName, lastIp) = first

        val probed = runCatching { NativeBridge.probeDevice(lastIp) }.getOrNull()
        if (probed.isNullOrBlank()) {
            Log.i(TAG, "已配对设备 $deviceName ($lastIp) 当前不在线")
            return null
        }
        val port = runCatching { JSONObject(probed).optInt("transfer_port", 0) }.getOrDefault(0)
        if (port <= 0) {
            Log.w(TAG, "从 $deviceName 的信标里取不到传输端口")
            return null
        }
        return Target(deviceId, deviceName, lastIp, port)
    }
}
