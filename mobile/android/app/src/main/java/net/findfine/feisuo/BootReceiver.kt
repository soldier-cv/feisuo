package net.findfine.feisuo

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log

/**
 * 飞梭 Android 端开机自启广播接收器。
 * 监听系统启动完成 / 应用更新完成事件，自动在后台拉起飞梭守护服务。
 *
 * 补齐 MY_PACKAGE_REPLACED 的原因：系统更新应用时会杀掉进程，
 * 而 START_STICKY 只在"进程被系统回收且服务仍被期望"时才会重启，
 * 更新安装属于主动 kill，自连能力会静默消失直到下次重启手机。
 *
 * @author xudong.hua,gemini
 * @since 2026-09-28 11:50 星期一
 */
class BootReceiver : BroadcastReceiver() {

    companion object {
        private const val TAG = "FeisuoBootReceiver"
        private const val ACTION_QUICKBOOT_POWERON = "android.intent.action.QUICKBOOT_POWERON"
    }

    override fun onReceive(context: Context, intent: Intent) {
        val action = intent.action
        Log.i(TAG, "收到系统广播: $action")

        val shouldStart = Intent.ACTION_BOOT_COMPLETED == action ||
            ACTION_QUICKBOOT_POWERON == action ||
            Intent.ACTION_MY_PACKAGE_REPLACED == action
        if (!shouldStart) return

        val serviceIntent = Intent(context, FeisuoDaemonService::class.java)
        // 开机阶段抛异常会直接导致进程崩溃（用户看到的是"桌面反复重启"），
        // 所以这里必须整体兜底。
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(serviceIntent)
            } else {
                context.startService(serviceIntent)
            }
            Log.i(TAG, "飞梭后台常驻守护服务已拉起")
        } catch (e: Exception) {
            Log.e(TAG, "拉起守护服务失败: ${e.message}", e)
        }
    }
}
