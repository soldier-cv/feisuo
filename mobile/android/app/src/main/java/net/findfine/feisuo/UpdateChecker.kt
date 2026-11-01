package net.findfine.feisuo

import android.content.Context
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URI
import java.net.URL

/**
 * 安卓端在线升级的「查版本 + 下安装包」一半（另一半是 [ApkInstaller]）。
 *
 * ## 为什么必须自己做
 *
 * 飞梭不走应用商店（国内也无从依赖 Play），APK 由 GitHub / Gitee Release 直接
 * 提供。没有这条链路，用户升级只能靠"自己想起来去 Releases 页面下"，而
 * 桌面端早就有「检查更新」——两端体验不一致时，用户会认为手机端是残缺的。
 *
 * ## 渠道优先级与桌面端一致：Gitee 优先，GitHub 降级
 *
 * 国内直连 Gitee 毫秒级，GitHub API 常常超时（见 `updater.rs` 的同名说明）。
 * 但 Gitee 是**镜像**，而 `release.yml` 里的镜像同步是 `continue-on-error`：
 * 它可能静默落后。所以"Gitee 说已是最新"时**必须**再用 GitHub 复核一次
 * （[check] 里的 probe），拿不到复核结果就维持"已是最新"——
 * 拿不到证据不等于有更新，反过来会让断网用户凭空看到"发现新版本"。
 *
 * ## 三道安全闸（顺序不能调换）
 *
 * 1. [isTrustedDownloadUrl]：只放行 gitee / github 域名的 https 地址，
 *    且**重定向的每一跳**都查（Gitee 下载会连跳两次，只查初始地址等于没查）；
 * 2. 落盘后按**包名**校验（必须是 `net.findfine.feisuo`）；
 * 3. **签名指纹必须与当前已安装的版本一致** —— 这是唯一挡得住
 *    "镜像被替换成同名恶意 APK" 的一步：攻击者换得掉文件，换不掉私钥。
 *    签名不一致同时也解释了 `INSTALL_FAILED_UPDATE_INCOMPATIBLE`，
 *    比把系统那句英文原样丢给用户强得多。
 *
 * 网络与磁盘操作全部是阻塞的，调用方必须放在后台线程（MainActivity 用 Thread）。
 *
 * @author xudong.hua,deepseek
 * @since 2026-11-01 15:50 星期日
 */
object UpdateChecker {

    private const val TAG = "FeisuoUpdate"

    // ------------------------------------------------------------------
    // 仓库标识：与 desktop/src-tauri/src/updater.rs **逐字一致**。
    //
    // 改仓库名要同时改四处（AGENTS.md 规则 5），漏改任一处都是静默故障：
    // 镜像推到别处 / 发版传到别处 / 客户端永远查不到更新。
    // `update_guard::repo_identifiers_must_be_consistent_across_the_four_places`
    // 会把这里和 updater.rs 的字面量逐个比对。
    // ------------------------------------------------------------------
    private const val REPO_OWNER = "soldier-cv"
    private const val REPO_NAME = "feisuo"

    private const val GITEE_LATEST_API = "https://gitee.com/api/v5/repos/huaxudong/feisuo/releases/latest"
    private const val GITHUB_LATEST_API = "https://api.github.com/repos/soldier-cv/feisuo/releases/latest"
    private const val GITHUB_LATEST_PAGE = "https://github.com/soldier-cv/feisuo/releases/latest"

    /** 手动下载页（网络全挂时的兜底出口，界面上给用户看） */
    const val GITEE_RELEASES_PAGE = "https://gitee.com/huaxudong/feisuo/releases"

    /**
     * 附件名规则：`release.yml` 用 `Feisuo-v${ver}-arm64.apk` 上传。
     *
     * 刻意按**前后缀**匹配而不是拼出完整文件名：`${ver}` 来自 tag，
     * 而 tag 与文件名一旦有一处大小写/前后缀不同，拼出来的名字必然不匹配，
     * 表现为"发现新版本，但没有 APK 附件"——用户完全无从自查。
     */
    private const val APK_NAME_PREFIX = "Feisuo-v"
    private const val APK_NAME_SUFFIX = "-arm64.apk"

    private const val API_TIMEOUT_MS = 6000
    /** 镜像落后复核用的短超时：拿不到就维持原结论，不值得让用户干等 */
    private const val PROBE_TIMEOUT_MS = 3000
    /** 下载只兜"服务器迟迟不给响应头"，响应体交给逐次读超时 */
    private const val DOWNLOAD_HEADERS_TIMEOUT_MS = 20000
    private const val DOWNLOAD_IDLE_TIMEOUT_MS = 45000
    private const val MAX_REDIRECTS = 5
    private const val MAX_APK_BYTES = 200L * 1024 * 1024
    /** 小于这个体积的"APK"必然是错误页或半截文件 */
    private const val MIN_APK_BYTES = 1024L * 1024
    private const val PROGRESS_INTERVAL_MS = 250L

    /** 一次远端发布里，本端真正需要的那几个字段。 */
    data class ReleaseInfo(
        val version: String,
        val tagName: String,
        val downloadUrl: String,
        val assetSize: Long
    )

    /** 检查结果。**没有"异常"这一支** —— 网络失败也是可展示的结论。 */
    sealed class CheckResult {
        object UpToDate : CheckResult()
        data class Newer(val release: ReleaseInfo) : CheckResult()
        data class Failed(val message: String) : CheckResult()
    }

    /** 下载结果。刻意不用 `kotlin.Result`：它作为公开返回类型有额外限制。 */
    sealed class DownloadResult {
        data class Ok(val file: File) : DownloadResult()
        data class Failed(val message: String) : DownloadResult()
    }

    // ==================================================================
    // 检查
    // ==================================================================

    /**
     * 依次尝试 Gitee API → GitHub API → GitHub Releases 页面。
     *
     * @param currentVersion 来自 `BuildConfig.VERSION_NAME`（源头是仓库根
     *        `Cargo.toml`，与桌面端同一个版本号，规则 6）。
     */
    fun check(currentVersion: String): CheckResult {
        Log.i(TAG, "开始检查更新，当前版本 $currentVersion")

        when (val gitee = readLatest(GITEE_LATEST_API, API_TIMEOUT_MS, "Gitee API", currentVersion)) {
            is CheckResult.Newer -> return gitee
            // Gitee 说已是最新：先别信，镜像可能落后（见类注释）。
            is CheckResult.UpToDate -> {
                Log.i(TAG, "Gitee 响应当前已是最新，用 GitHub 复核镜像状态")
                val probe = readLatest(
                    GITHUB_LATEST_API,
                    PROBE_TIMEOUT_MS,
                    "GitHub 复核",
                    currentVersion
                )
                return if (probe is CheckResult.Newer) probe else CheckResult.UpToDate
            }
            is CheckResult.Failed -> Log.w(TAG, "Gitee 通道不可用：${gitee.message}，降级 GitHub")
        }

        when (val github = readLatest(GITHUB_LATEST_API, API_TIMEOUT_MS, "GitHub API", currentVersion)) {
            is CheckResult.Newer -> return github
            is CheckResult.UpToDate -> return github
            is CheckResult.Failed -> Log.w(TAG, "GitHub API 不可用：${github.message}，降级 Releases 页面")
        }

        // 页面兜底不碰 API 限流：跟随 /releases/latest 的 302 从落地地址里取 tag，
        // 附件地址按 release.yml 的命名规则拼出来。
        val page = tryReadReleasePage(currentVersion)
        return page ?: CheckResult.Failed(
            "检查更新失败（Gitee / GitHub / 页面都不通）。可稍后重试，" +
                "或在浏览器打开 $GITEE_RELEASES_PAGE 手动下载"
        )
    }

    private fun readLatest(
        url: String,
        timeoutMs: Int,
        source: String,
        currentVersion: String
    ): CheckResult {
        val dto = fetchJson(url, timeoutMs)
            ?: return CheckResult.Failed("$source 不可达或响应无法解析")
        return readRelease(dto, source, currentVersion)
    }

    /** 把一份 Release JSON 收成一个结论：已是最新 / 有新版本 / 附件不齐。 */
    private fun readRelease(dto: JSONObject, source: String, currentVersion: String): CheckResult {
        val tag = dto.optString("tag_name").trim()
        if (tag.isEmpty()) return CheckResult.Failed("$source 响应里没有 tag_name")

        val version = tag.trimStart('v', 'V')
        if (!isNewer(version, currentVersion)) {
            Log.i(TAG, "检查更新($source)：最新 $tag，当前 $currentVersion，无需更新")
            return CheckResult.UpToDate
        }

        // 附件缺失**不能**当成"没有新版本"：否则用户永远停在旧版且毫无提示。
        val asset = pickApk(dto.optJSONArray("assets"))
            ?: return CheckResult.Failed(
                "发现新版本 $tag，但 Release 里没有 $APK_NAME_PREFIX*$APK_NAME_SUFFIX 附件。" +
                    "可到 $GITEE_RELEASES_PAGE 手动下载"
            )

        val url = asset.optString("browser_download_url").trim()
        if (!isTrustedDownloadUrl(url)) {
            return CheckResult.Failed("新版本下载地址不受信任，已拒绝：$url")
        }
        Log.i(TAG, "发现新版本 $tag（$source）")
        return CheckResult.Newer(
            ReleaseInfo(
                version = version,
                tagName = tag,
                downloadUrl = url,
                assetSize = asset.optLong("size", 0L)
            )
        )
    }

    /** 只认 `Feisuo-v<ver>-arm64.apk`；宁可报"没有附件"也不下错架构的包。 */
    private fun pickApk(assets: JSONArray?): JSONObject? {
        if (assets == null) return null
        for (i in 0 until assets.length()) {
            val a = assets.optJSONObject(i) ?: continue
            val name = a.optString("name").trim()
            if (name.startsWith(APK_NAME_PREFIX, ignoreCase = true) &&
                name.endsWith(APK_NAME_SUFFIX, ignoreCase = true)
            ) {
                return a
            }
        }
        return null
    }

    private fun tryReadReleasePage(currentVersion: String): CheckResult? {
        val res = openFollowing(GITHUB_LATEST_PAGE, API_TIMEOUT_MS, enforceAllowlist = false)
            ?: return null
        val finalUrl = res.finalUrl
        res.conn.disconnect()

        val at = finalUrl.indexOf("/releases/tag/")
        if (at < 0) return null
        val tag = finalUrl.substring(at + "/releases/tag/".length)
            .split('?', '#', '/')
            .firstOrNull()
            ?.trim()
            .orEmpty()
        if (tag.isEmpty()) return null

        val version = tag.trimStart('v', 'V')
        if (!isNewer(version, currentVersion)) return CheckResult.UpToDate

        val dto = JSONObject().apply {
            put("tag_name", tag)
            put(
                "assets",
                JSONArray().apply {
                    put(
                        JSONObject().apply {
                            put("name", "$APK_NAME_PREFIX$version$APK_NAME_SUFFIX")
                            put(
                                "browser_download_url",
                                "https://github.com/$REPO_OWNER/$REPO_NAME/releases/download/" +
                                    "$tag/$APK_NAME_PREFIX$version$APK_NAME_SUFFIX"
                            )
                            put("size", 0L)
                        }
                    )
                }
            )
        }
        return readRelease(dto, "GitHub 页面", currentVersion)
    }

    // ==================================================================
    // 下载
    // ==================================================================

    /**
     * 流式下载先写同目录的 `.part` 文件，完整落盘后再改名为 `.apk`。
     *
     * （这里刻意不写通配符路径：Kotlin 的块注释是**可嵌套**的，
     * 注释里出现连续斜杠加星号会把后面的代码整段吞进注释，
     * 报错却指向几百行之后的某一行。）
     *
     * 半截文件绝不能被当成可用更新：`verify` 会解析失败，但更糟的是
     * 用户看到"已下载就绪"然后装出一个 `INSTALL_PARSE_FAILED`。
     *
     * @param onProgress 在**后台线程**回调（调用方自行切换线程），按 250ms 节流。
     */
    fun download(
        context: Context,
        release: ReleaseInfo,
        onProgress: (received: Long, total: Long) -> Unit
    ): DownloadResult {
        if (!isTrustedDownloadUrl(release.downloadUrl)) {
            return DownloadResult.Failed("下载地址不受信任，已拒绝")
        }
        val dir = File(context.cacheDir, "updates").apply { mkdirs() }
        val dest = File(dir, "$APK_NAME_PREFIX${release.version}$APK_NAME_SUFFIX")
        val part = File(dir, "${dest.name}.part")

        // 上次已经下完且体积对得上：直接复用，别让流量再走一遍。
        if (dest.isFile && dest.length() > MIN_APK_BYTES &&
            (release.assetSize <= 0 || dest.length() == release.assetSize)
        ) {
            Log.i(TAG, "复用已下载的更新包：${dest.absolutePath}")
            return DownloadResult.Ok(dest)
        }

        val res = openFollowing(release.downloadUrl, DOWNLOAD_HEADERS_TIMEOUT_MS, enforceAllowlist = true)
            ?: return DownloadResult.Failed("连接更新服务器失败（网络不可达，或重定向目标不在受信任域名内）")

        if (res.code !in 200..299) {
            res.conn.disconnect()
            return DownloadResult.Failed("下载失败：HTTP ${res.code}")
        }

        val expected = if (res.conn.contentLengthLong > 0) {
            res.conn.contentLengthLong
        } else {
            release.assetSize
        }
        if (expected > MAX_APK_BYTES) {
            res.conn.disconnect()
            return DownloadResult.Failed("更新包体积异常（$expected 字节），已拒绝")
        }

        var received = 0L
        try {
            res.conn.inputStream.use { input ->
                FileOutputStream(part).use { out ->
                    val buf = ByteArray(64 * 1024)
                    var lastEmit = 0L
                    while (true) {
                        val n = input.read(buf)
                        if (n < 0) break
                        out.write(buf, 0, n)
                        received += n
                        if (received > MAX_APK_BYTES) {
                            throw IOException("更新包超过体积上限")
                        }
                        val now = System.currentTimeMillis()
                        if (now - lastEmit >= PROGRESS_INTERVAL_MS) {
                            lastEmit = now
                            onProgress(received, expected)
                        }
                    }
                    out.flush()
                }
            }
        } catch (e: Exception) {
            part.delete()
            return DownloadResult.Failed("下载中断：${e.message}")
        } finally {
            res.conn.disconnect()
        }

        // 体积明显不符时不直接采用：多半是代理截断或 CDN 返回了错误页。
        if (expected > 0 && received < expected) {
            part.delete()
            return DownloadResult.Failed("下载不完整（$received/$expected 字节）")
        }
        if (received < MIN_APK_BYTES) {
            part.delete()
            return DownloadResult.Failed("下载到的文件过小（$received 字节），多半不是 APK")
        }

        dest.delete()
        if (!part.renameTo(dest)) {
            part.delete()
            return DownloadResult.Failed("保存更新包失败，请检查剩余存储空间")
        }
        Log.i(TAG, "更新包已下载：${dest.absolutePath}（$received 字节）")
        onProgress(received, received)
        return DownloadResult.Ok(dest)
    }

    /** 清掉缓存目录里的旧安装包（安装失败重试时不必留着几百 MB）。 */
    fun clearDownloads(context: Context) {
        runCatching {
            File(context.cacheDir, "updates").listFiles()?.forEach { it.delete() }
        }.onFailure { Log.w(TAG, "清理更新缓存失败：${it.message}") }
    }

    // ==================================================================
    // 纯函数（版本比较 / 地址白名单）
    // ==================================================================

    /**
     * `x.y.z` 版本比较，[candidate] 比 [current] 新时为 true。
     *
     * 不引 semver：版本号由本项目自己打 tag，形式固定；
     * 完整 semver 只会带来一堆"我们永远打不出来"的分支。
     * 解析不出来一律返回 false —— 宁可不提示更新，也不要乱提示。
     */
    fun isNewer(candidate: String, current: String): Boolean {
        val c = parseVersion(candidate) ?: return false
        val cur = parseVersion(current) ?: return false
        for (i in 0..2) {
            if (c[i] != cur[i]) return c[i] > cur[i]
        }
        return false
    }

    private fun parseVersion(text: String): IntArray? {
        val core = text.trim().trimStart('v', 'V').split('-', '+').firstOrNull() ?: return null
        val out = IntArray(3)
        core.split('.').take(3).forEachIndexed { i, part ->
            out[i] = part.trim().toIntOrNull() ?: return null
        }
        return out
    }

    /**
     * 只放行官方域名的 **https** 地址。
     *
     * 这是本功能里唯一的远程代码执行面：下载地址来自远端 JSON。
     * 与桌面端 `updater::is_trusted_download_url` 同一套规则，
     * 用真正的 URI 解析而不是按 `://` 切字符串 ——
     * `https://evil.com\@gitee.com/`（反斜杠在 URL 里是分隔符）、
     * 端口、尾部点这些写法都能骗过手写切分。
     */
    fun isTrustedDownloadUrl(url: String): Boolean {
        val parsed = try {
            URI(url)
        } catch (e: Exception) {
            return false
        }
        if (!parsed.scheme.equals("https", ignoreCase = true)) return false
        val host = parsed.host?.trimEnd('.')?.lowercase() ?: return false
        if (host.isEmpty()) return false
        // 子域必须真的以 `.` 分隔：`evilgitee.com` 不能因为 endsWith 就通过
        fun domain(base: String) = host == base || host.endsWith(".$base")
        return domain("gitee.com") ||
            domain("github.com") ||
            domain("githubusercontent.com") ||
            domain("release-assets.githubusercontent.com") ||
            domain("objects.githubusercontent.com")
    }

    // ==================================================================
    // HTTP
    // ==================================================================

    private class HttpResult(
        val code: Int,
        val finalUrl: String,
        val conn: HttpURLConnection
    )

    private fun fetchJson(url: String, timeoutMs: Int): JSONObject? {
        val res = openFollowing(url, timeoutMs, enforceAllowlist = false) ?: return null
        try {
            if (res.code !in 200..299) return null
            val body = res.conn.inputStream.bufferedReader().use { it.readText() }
            return JSONObject(body)
        } catch (e: Exception) {
            Log.w(TAG, "解析 $url 响应失败：${e.message}")
            return null
        } finally {
            res.conn.disconnect()
        }
    }

    /**
     * 手动跟随重定向，并在每一跳之前检查目标地址。
     *
     * 为什么不用 `instanceFollowRedirects = true`：那样拿不到中间跳的目标，
     * 而 Gitee 的下载地址会连跳两次（gitee.com → attach_files → foruda.gitee.com）。
     * 只校验**初始** URL 的话，一个被劫持的 API 响应完全可以返回一个
     * gitee.com 的地址、再用 302 把客户端引到任意主机 —— 白名单等于虚设。
     */
    private fun openFollowing(url: String, timeoutMs: Int, enforceAllowlist: Boolean): HttpResult? {
        var current = url
        for (hop in 0..MAX_REDIRECTS) {
            if (enforceAllowlist && !isTrustedDownloadUrl(current)) {
                Log.w(TAG, "第 $hop 跳地址不在白名单内，已拒绝：$current")
                return null
            }
            val conn = try {
                (URL(current).openConnection() as HttpURLConnection).apply {
                    instanceFollowRedirects = false
                    connectTimeout = timeoutMs
                    readTimeout = if (enforceAllowlist) DOWNLOAD_IDLE_TIMEOUT_MS else timeoutMs
                    requestMethod = "GET"
                    setRequestProperty("User-Agent", "Feisuo-Android")
                    setRequestProperty(
                        "Accept",
                        "application/vnd.github+json, application/json, text/html;q=0.9, */*;q=0.8"
                    )
                }
            } catch (e: Exception) {
                Log.w(TAG, "打开 $current 失败：${e.message}")
                return null
            }

            val code = try {
                conn.responseCode
            } catch (e: Exception) {
                Log.w(TAG, "请求 $current 失败：${e.message}")
                conn.disconnect()
                return null
            }

            if (code in 300..399) {
                val loc = conn.getHeaderField("Location")
                conn.disconnect()
                if (loc.isNullOrBlank()) return null
                current = try {
                    URL(URL(current), loc).toString()
                } catch (e: Exception) {
                    Log.w(TAG, "无法解析重定向目标 $loc：${e.message}")
                    return null
                }
                continue
            }
            return HttpResult(code, current, conn)
        }
        Log.w(TAG, "重定向次数过多（>$MAX_REDIRECTS）")
        return null
    }
}
