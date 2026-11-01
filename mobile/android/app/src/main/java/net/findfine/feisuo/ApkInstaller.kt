package net.findfine.feisuo

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.content.pm.Signature
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.core.content.FileProvider
import java.io.File
import java.security.MessageDigest

/**
 * 把下载好的 APK 交给系统安装器，并在此之前做**装前校验**。
 *
 * ## 为什么不直接 `startActivity(ACTION_VIEW)`
 *
 * 少了这三步，用户会看到一堆系统英文报错：`INSTALL_PARSE_FAILED`、
 * `INSTALL_FAILED_UPDATE_INCOMPATIBLE`、`INSTALL_FAILED_INVALID_APK`，
 * 而真正的原因（下载被截断 / 两台设备的签名密钥不同 / 镜像被换了包）
 * 一个都不在其中。
 *
 * ## 装前校验的三条，缺一不可
 *
 * 1. **包名**：必须是本应用。少了它，一个被替换的 Release 可以让用户装上
 *    另一个应用（虽然仍要过系统确认框，但用户根本不会看包名）；
 * 2. **版本号必须更高**：否则装上一次降级，Android 直接拒绝；
 * 3. **签名指纹必须与当前已安装版本一致**：这是唯一挡住"镜像被换成同名
 *    恶意 APK"的一步 —— 攻击者换得掉文件，换不掉我们的私钥。
 *    同时它也是 `INSTALL_FAILED_UPDATE_INCOMPATIBLE` 的唯一正确解释：
 *    本地自己用 debug 密钥打过包、又去装正式版，签名必然对不上。
 *
 * ## 为什么需要「安装未知应用」权限
 *
 * Android 8.0 起，从应用内拉起 APK 安装器要求**发起方**持有
 * `REQUEST_INSTALL_PACKAGES`，且用户为该应用单独开启开关
 * （`canRequestPackageInstalls()`）。没开时系统安装器根本不出现，
 * 界面上表现为"点了没反应"——所以这里必须显式引导到设置页。
 *
 * @author xudong.hua,deepseek
 * @since 2026-11-01 15:55 星期日
 */
object ApkInstaller {

    private const val TAG = "FeisuoInstall"
    private const val MIME_APK = "application/vnd.android.package-archive"
    /** 与 AndroidManifest 里的 provider authority 必须一致 */
    private const val AUTHORITY_SUFFIX = ".fileprovider"

    /**
     * 装前校验。
     *
     * @return null 表示可以安装；否则为**给用户看的中文原因**。
     */
    fun verify(context: Context, apk: File): String? {
        if (!apk.isFile || apk.length() <= 0) return "更新包不存在或为空，请重新下载"

        val pm = context.packageManager
        val incoming = try {
            pm.getPackageArchiveInfo(apk.absolutePath, signatureFlags())
        } catch (e: Exception) {
            Log.w(TAG, "解析更新包失败：${e.message}")
            null
        } ?: return "更新包无法解析（多半是下载不完整），请重新下载"

        if (incoming.packageName != context.packageName) {
            return "更新包的包名是 ${incoming.packageName}，不是飞梭，已拒绝安装"
        }

        val current = currentPackage(context)
            ?: return "读取当前版本信息失败，请手动到设置里确认"

        if (incoming.longVersionCodeSafe() <= current.longVersionCodeSafe()) {
            return "更新包版本不比当前版本新，已拒绝安装"
        }

        val mine = signerDigests(current)
        val theirs = signerDigests(incoming)
        if (mine.isEmpty() || theirs.isEmpty()) {
            return "无法读取签名信息，已拒绝安装"
        }
        if (mine != theirs) {
            // 这条不是"奇怪的情况"：本地 debug 包的密钥与正式版不同，
            // 用户装上正式版时必然走到这里，必须给出可执行的下一步。
            return "更新包签名与已安装版本不一致（可能是不同密钥打的包），请先卸载当前版本再安装新包"
        }
        return null
    }

    /** 用户是否已经允许本应用安装 APK（Android 8.0+ 恒有这个开关）。 */
    fun canInstallPackages(context: Context): Boolean =
        context.packageManager.canRequestPackageInstalls()

    /**
     * 打开「安装未知应用」的授权页。
     *
     * 必须带 `package:` 参数才会直接落到**本应用**那一项；
     * 不带就只是打开一个列表，用户很可能在自己的应用名上找不到入口。
     */
    fun openUnknownSourcesSettings(context: Context): Boolean {
        return try {
            context.startActivity(
                Intent(
                    Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                    Uri.parse("package:${context.packageName}")
                ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            )
            true
        } catch (e: Exception) {
            Log.w(TAG, "打开「安装未知应用」设置失败：${e.message}")
            false
        }
    }

    /**
     * 拉起系统安装器。
     *
     * 走 FileProvider 的 `content://`：Android 7.0 起 `file://` 的 APK 一律
     * 触发 `FileUriExposedException`。URI 只授 `FLAG_GRANT_READ_URI_PERMISSION`，
     * 安装器读得到、别的应用读不到。
     *
     * @return null 表示已交给系统；否则为给用户看的中文原因。
     */
    fun startInstall(context: Context, apk: File): String? {
        return try {
            val uri = FileProvider.getUriForFile(
                context,
                context.packageName + AUTHORITY_SUFFIX,
                apk
            )
            val intent = Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, MIME_APK)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
            context.startActivity(intent)
            null
        } catch (e: ActivityNotFoundException) {
            "系统里没有可用的安装器，请手动打开：${apk.absolutePath}"
        } catch (e: Exception) {
            Log.w(TAG, "拉起系统安装器失败：${e.message}")
            "无法拉起系统安装器：${e.message}"
        }
    }

    // ------------------------------------------------------------------

    /**
     * 取 versionCode，兼容 Android 8.x。
     *
     * `PackageInfo.longVersionCode` 是 API 28 才有的字段，而本项目 minSdk 是 26
     * （Android 8.0）。直接读它在这两个版本上会抛 `NoSuchFieldError` ——
     * 不是编译错误（字段存在），是运行时崩溃，而且只在这两台设备上崩。
     */
    private fun PackageInfo.longVersionCodeSafe(): Long =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            longVersionCode
        } else {
            @Suppress("DEPRECATION")
            versionCode.toLong()
        }

    private fun signatureFlags(): Int =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            PackageManager.GET_SIGNING_CERTIFICATES
        } else {
            @Suppress("DEPRECATION")
            PackageManager.GET_SIGNATURES
        }

    private fun currentPackage(context: Context): PackageInfo? = try {
        @Suppress("DEPRECATION")
        context.packageManager.getPackageInfo(context.packageName, signatureFlags())
    } catch (e: Exception) {
        Log.w(TAG, "读取本应用包信息失败：${e.message}")
        null
    }

    /**
     * 取出所有签名证书的 SHA-256（十六进制）。
     *
     * 用**证书集合**而不是单个签名：多签名的包只有集合完全相同才能互相覆盖，
     * 只比第一张会让"多加一个签名者"的包通过校验、却在系统安装时失败。
     */
    private fun signerDigests(info: PackageInfo): Set<String> {
        val certs: Array<Signature?> = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            info.signingInfo?.apkContentsSigners ?: emptyArray()
        } else {
            @Suppress("DEPRECATION")
            info.signatures ?: emptyArray()
        }
        return certs.filterNotNull().map { sha256(it.toByteArray()) }.toSet()
    }

    private fun sha256(bytes: ByteArray): String {
        val digest = MessageDigest.getInstance("SHA-256").digest(bytes)
        return digest.joinToString("") { "%02x".format(it) }
    }
}
