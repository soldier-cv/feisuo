package net.findfine.feisuo

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.Settings
import android.util.Log

/**
 * 公共存储的访问闸门。
 *
 * ## 为什么需要它
 *
 * 收件落盘在 core（Rust）里用普通文件 API 直接写 `/sdcard/Download/feisuo`。
 * Android 11+ 没有「所有文件访问权限」时，这次创建/写入会拿到 EACCES，core
 * 静默退回 `/data/data/<pkg>/files/feisuo` —— 一个文件管理器**永远进不去**
 * 的沙箱目录。用户看到的现象是"点开接收目录，文件无法访问"。
 *
 * 所以界面必须做两件事，缺一不可：
 * 1. 把权限状态**显示出来**（不给权限时明确说"文件在私有空间"）；
 * 2. 能一键跳到系统授权页，回来后重新判断并让 core 重读配置
 *    （`AppConfig::load_or_default_in` 里的迁移会把收件目录搬回公共目录）。
 *
 * @author xudong.hua,deepseek
 * @since 2026-11-13 15:40 星期四
 */
object StorageAccess {

    private const val TAG = "StorageAccess"

    /** 公共下载目录下的收件目录 —— 与 core 的 `ANDROID_PUBLIC_RECEIVE_DIR` 必须一致。 */
    const val PUBLIC_RECEIVE_DIR = "/sdcard/Download/feisuo"

    /** Android 10 及更早走的是运行时权限，需要自己的请求码。 */
    const val REQUEST_LEGACY_STORAGE = 1002

    /**
     * 现在能不能按**路径**读写公共存储。
     *
     * 三个分支都是系统权威判据，不要自己猜：
     * - Android 11+（API 30）：`Environment.isExternalStorageManager()`，即
     *   「所有文件访问权限」的总开关；
     * - Android 6~10：`WRITE_EXTERNAL_STORAGE` 运行时权限；
     * - 更早：安装即授予。
     */
    fun hasBroadAccess(context: Context): Boolean = when {
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.R ->
            Environment.isExternalStorageManager()

        Build.VERSION.SDK_INT >= Build.VERSION_CODES.M ->
            context.checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE) ==
                PackageManager.PERMISSION_GRANTED

        else -> true
    }

    /**
     * 授权是"跳系统设置页"还是"弹运行时权限框"。
     *
     * 界面用它决定文案：前者要用户自己找开关，后者是一次点按。
     */
    fun usesSettingsPage(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R

    /**
     * 拉起系统授权。
     *
     * Android 11+ 先试**本应用专属**的授权页（落点精确，用户不用在列表里找），
     * 再退回全局列表页 —— 部分 ROM 没有实现前者，直接 startActivity 会抛
     * `ActivityNotFoundException`，静默失败等于按钮坏了。
     *
     * @return 是否成功拉起。`false` 表示这台设备两种入口都没有，调用方要给出
     *         明确的兜底文案（否则用户会以为按钮没反应）。
     */
    fun request(activity: Activity): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val appUri = Uri.fromParts("package", activity.packageName, null)
            val candidates = listOf(
                Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, appUri),
                Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION),
            )
            for (intent in candidates) {
                try {
                    activity.startActivity(intent)
                    return true
                } catch (e: Exception) {
                    Log.w(TAG, "无法拉起 $intent: ${e.message}")
                }
            }
            return false
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            activity.requestPermissions(
                arrayOf(
                    Manifest.permission.WRITE_EXTERNAL_STORAGE,
                    Manifest.permission.READ_EXTERNAL_STORAGE,
                ),
                REQUEST_LEGACY_STORAGE
            )
            return true
        }
        // 更早的系统安装即授予，没有"授权页"可去。
        return true
    }

    /**
     * 这条路径是不是位于公共外部存储（也就是文件管理器能打开的地方）。
     *
     * 只做前缀判断：真正能不能读由系统决定，但"路径是不是私有沙箱"这件事
     * 可以直接看出来 —— 而它正是用户看到"文件无法访问"的原因。
     */
    fun isPublicPath(path: String): Boolean {
        val p = path.trim().trimEnd('/')
        if (p.isEmpty()) return false
        return p == "/sdcard" || p.startsWith("/sdcard/") ||
            p == "/storage/emulated/0" || p.startsWith("/storage/emulated/0/")
    }

    /** 把 `/storage/emulated/0/...` 与 `/sdcard/...` 统一成 `/sdcard/...`。 */
    fun normalize(path: String): String {
        val p = path.trim()
        return if (p.startsWith("/storage/emulated/0")) {
            "/sdcard" + p.removePrefix("/storage/emulated/0")
        } else {
            p
        }
    }
}
