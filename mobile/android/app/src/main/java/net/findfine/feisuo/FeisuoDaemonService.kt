package net.findfine.feisuo

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.net.wifi.WifiManager.MulticastLock
import android.os.Build
import android.os.IBinder
import android.util.Log
import android.widget.Toast

/**
 * 飞梭 Android 端前台常驻守护服务 (Foreground Service)。
 * 保证进程高优先级存活，防止系统在锁屏与休眠状态下杀死网络监听，
 * 同时持有 Wi-Fi 组播锁以持续响应局域网自动发现。
 *
 * @author xudong.hua,gemini
 * @since 2026-09-28 11:50 星期一
 */
class FeisuoDaemonService : Service() {

    companion object {
        private const val TAG = "FeisuoDaemonService"
        private const val CHANNEL_ID = "feisuo_daemon_channel"
        private const val ACTION_CHANNEL_ID = "feisuo_approval_channel"
        private const val NOTIFICATION_ID = 42100
        private const val APPROVAL_NOTIFICATION_ID_BASE = 42110
        private const val ACTION_STOP = "net.findfine.feisuo.action.STOP_DAEMON"
        private const val ACTION_ALLOW = "net.findfine.feisuo.action.ALLOW"
        private const val ACTION_REJECT = "net.findfine.feisuo.action.REJECT"
        private const val EXTRA_APPROVAL_ID = "approval_id"
    }

    private var multicastLock: MulticastLock? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    /** 审批监听线程; 停止服务时必须结束它, 否则它会一直占着引擎句柄 */
    private var approvalThread: Thread? = null

    override fun onCreate() {
        super.onCreate()
        Log.i(TAG, "飞梭守护服务初始化中...")

        // 顺序是硬约束, 不能调整:
        //
        // 1) 先建渠道并**立刻** startForeground。BootReceiver 用的是
        //    startForegroundService, 系统要求 5 秒内必须挂上通知, 否则抛
        //    ForegroundServiceDidNotStartInTimeException 并杀掉进程。
        //    所以这一步必须排在所有耗时操作之前。
        createNotificationChannel()
        startForeground(NOTIFICATION_ID, buildPersistentNotification())

        // 2) 引擎启动是**阻塞** JNI 调用(建 tokio runtime + 绑 UDP/TCP 端口
        //    + 打开 SQLite + 生成 Ed25519 私钥), 绝不能放在主线程。
        //    旧实现就是在 onCreate 里同步调的 —— 与 FeisuoRuntime.ensureEngine
        //    自己的文档注释("必须在后台线程调用")直接矛盾, 而且这条路径
        //    每次开机、每次应用更新都会跑一遍, 是稳定的 ANR/卡顿来源。
        startEngineOnWorker()

        // 3) 组播锁与网络监听只是系统服务查询, 很快, 留主线程无妨。
        acquireMulticastLock()
        registerNetworkCallback()
        // 4) 审批监听: 让未配对电脑也能投递文件(需用户确认)。
        startApprovalWatcher()
    }

    /**
     * 监听 core 的审批请求, 弹通知让用户决定允许 / 拒绝。
     *
     * 为什么必须有这个: core 对未受信设备是 **fail-closed** 的 —— 没有能应答的
     * 审批者就直接拒绝。旧实现把审批事件接收端直接丢弃, 结果 Android 上
     * "任何电脑都无法向本机投递文件", 用户连提示都收不到。
     *
     * 跑在独立后台线程: `nativeNextApproval` 会阻塞等待, 绝不能占住主线程。
     */
    private fun startApprovalWatcher() {
        if (approvalThread != null) return
        val t = Thread({
            var seq = 0
            while (!Thread.currentThread().isInterrupted) {
                if (!FeisuoRuntime.isEngineRunning()) {
                    Thread.sleep(1000)
                    continue
                }
                val raw = try {
                    // 2 秒短轮询: 既能及时响应, 又能在服务停止后较快退出
                    NativeBridge.nextApproval(2000)
                } catch (e: Throwable) {
                    Log.w(TAG, "读取审批请求失败: ${e.message}")
                    null
                } ?: continue

                val req = runCatching { org.json.JSONObject(raw) }.getOrNull() ?: continue
                val id = req.optString("approval_id")
                if (id.isEmpty()) continue
                seq += 1
                postApprovalNotification(id, req, seq)
            }
        }, "feisuo-approval-watch")
        t.isDaemon = true
        approvalThread = t
        t.start()
    }

    private fun postApprovalNotification(
        approvalId: String,
        req: org.json.JSONObject,
        seq: Int,
    ) {
        val from = req.optString("sender_name").ifBlank { "未知设备" }
        val count = req.optInt("file_count", 0)
        val size = req.optString("total_size_formatted").ifBlank { "未知大小" }

        // 「每次匹配码」等级：本机生成、显示给本机用户的 6 位码。
        //
        // 旧方向（发起方出码、接收方输入）下这里必须是空串 ——
        // 通知栏没有输入框，所以从通知栏允许必然失败。这是那个方向独有的死结。
        //
        // 现在方向是"接收方出码、发起方输入"，**缺的输入框变成了对端的**，
        // 于是通知栏只要把码显示出来就够了：用户念给对方，对方敲进自己的
        // 界面。见 core/src/transport/server.rs 的 ApprovalManager::challenge_for。
        val needsCode = req.optBoolean("requires_grant_code", false)
        val challenge = req.optString("grant_challenge").orEmpty().trim()
        // 码排成 3+3：念的时候需要在中间停顿，连读 6 位极易听错。
        val challengeText = if (challenge.length == 6) {
            challenge.substring(0, 3) + " " + challenge.substring(3)
        } else {
            challenge
        }

        fun actionPending(action: String, requestCode: Int): PendingIntent {
            val i = Intent(this, FeisuoDaemonService::class.java)
                .setAction(action)
                .putExtra(EXTRA_APPROVAL_ID, approvalId)
            return PendingIntent.getService(
                this, requestCode, i,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        }

        val n = Notification.Builder(this, ACTION_CHANNEL_ID)
            .setContentTitle("$from 请求发送 $count 个文件")
            .setContentText(
                if (needsCode && challengeText.isNotBlank()) {
                    "共 $size · 念给对方核对：$challengeText"
                } else {
                    "共 $size · 点击允许或拒绝"
                }
            )
            .setSmallIcon(R.drawable.ic_stat_feisuo)
            .setAutoCancel(true)
            .addAction(
                Notification.Action.Builder(
                    null, getString(R.string.action_allow),
                    actionPending(ACTION_ALLOW, seq * 2),
                ).build(),
            )
            .addAction(
                Notification.Action.Builder(
                    null, getString(R.string.action_reject),
                    actionPending(ACTION_REJECT, seq * 2 + 1),
                ).build(),
            )
            .build()

        getSystemService(NotificationManager::class.java)
            ?.notify(APPROVAL_NOTIFICATION_ID_BASE + seq, n)
        Log.i(TAG, "收到审批请求: 来自 $from, $count 个文件, $size")
    }

    private fun resolveApproval(approvalId: String, allow: Boolean) {
        val hit = NativeBridge.respondApproval(approvalId, allow)
        Log.i(TAG, "审批 $approvalId -> ${if (allow) "允许" else "拒绝"}, 命中=$hit")
        if (!hit) {
            // 请求已过期(core 侧只等 60 秒)或已被处理过。
            // 必须说出来, 否则用户点了按钮却像没反应。
            Toast.makeText(
                this,
                "该请求已超时（等待超过 60 秒），请让对方重新发送",
                Toast.LENGTH_LONG,
            ).show()
        }
    }

    /** 在后台线程启动 core 引擎, 避免阻塞主线程 */
    private fun startEngineOnWorker() {
        Thread({
            val ctx = applicationContext
            try {
                FeisuoRuntime.ensureStarted(ctx)
                if (FeisuoRuntime.isEngineRunning()) {
                    Log.i(TAG, "core 引擎就绪, 自连待命中")
                    // 引擎就绪是"稍后重试"承诺的第一个兑现时机: 之前分享
                    // 失败而暂存下来的文件, 在这里补发。
                    runCatching { ShareDispatcher.retryPending(ctx) }
                        .onFailure { Log.w(TAG, "补发暂存文件失败: ${it.message}") }
                } else {
                    Log.e(TAG, "core 引擎未就绪: ${FeisuoRuntime.failedReason}")
                }
            } catch (e: Throwable) {
                Log.e(TAG, "启动 core 引擎异常: ${e.message}", e)
            }
        }, "feisuo-engine-boot").apply { isDaemon = true }.start()
    }

    /**
     * 在后台线程补发暂存文件。
     *
     * 网络恢复是第二个兑现时机: 分享时电脑离线是最常见的失败原因,
     * 等它重新出现在局域网里就该自动补发, 而不是让用户手动重新分享。
     */
    private fun retryPendingOnWorker() {
        Thread({
            if (!FeisuoRuntime.isEngineRunning()) return@Thread
            runCatching { ShareDispatcher.retryPending(applicationContext) }
                .onFailure { Log.w(TAG, "网络恢复后补发失败: ${it.message}") }
        }, "feisuo-share-retry").apply { isDaemon = true }.start()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // 审批的允许/拒绝走同一个 Service 的 action, 因为通知按钮的
        // PendingIntent 指向本服务最省事, 且不依赖用户是否装了启动器 Activity。
        if (intent != null &&
            (ACTION_ALLOW == intent.action || ACTION_REJECT == intent.action)
        ) {
            val id = intent.getStringExtra(EXTRA_APPROVAL_ID).orEmpty()
            if (id.isNotEmpty()) resolveApproval(id, ACTION_ALLOW == intent.action)
            return START_NOT_STICKY
        }
        if (intent != null && ACTION_STOP == intent.action) {
            Log.i(TAG, "收到停止指令, 结束守护服务")
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                // API 24+ 起 stopForeground(int) 取代已废弃的 stopForeground(boolean)
                stopForeground(STOP_FOREGROUND_REMOVE)
            } else {
                @Suppress("DEPRECATION")
                stopForeground(true)
            }
            // 这里**只**摘通知并停服务; 真正停引擎放在 onDestroy 里做。
            // 用户点"停止"的语义是"这台设备从此不再出现在电脑的在线列表里",
            // 光 stopSelf() 达不到 —— 引擎还活着, UDP 心跳照发。
            stopSelf()
            return START_NOT_STICKY
        }
        // 通知已在 onCreate 挂好(必须早于 5 秒限制), 这里只做状态日志。
        Log.i(TAG, "前台服务已挂载通知，处于自连待命状态")
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? {
        return null
    }

    override fun onDestroy() {
        // 必须先停审批线程: 它循环调用 nativeNextApproval, 会一直占着
        // 引擎; 服务已销毁而线程还活着, 下一个服务实例起来时会多出一个消费者。
        approvalThread?.interrupt()
        approvalThread = null
        unregisterNetworkCallback()
        releaseMulticastLock()
        // 必须真正停掉 core 引擎, 否则:
        //   1) 电脑侧仍能看到这台设备在线(UDP 心跳循环还在跑);
        //   2) 传输端口继续被占, 下次拉起服务会绑定失败。
        // 放在 onDestroy 而不是 ACTION_STOP 分支里, 是因为系统回收服务
        // (低内存/用户强停) 同样会走到这里, 引擎的生命周期归这个 Service 所有。
        FeisuoRuntime.shutdown()
        super.onDestroy()
        Log.i(TAG, "飞梭守护服务停止")
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                getString(R.string.daemon_channel_name),
                NotificationManager.IMPORTANCE_LOW
            ).apply {
                description = getString(R.string.daemon_channel_desc)
                setShowBadge(false)
            }
            // 审批通知必须用高优先级: 它要求用户在一分钟内做决定,
            // 静默渠道(Low/ MIN)下用户可能根本注意不到, 请求就直接超时了。
            val approval = NotificationChannel(
                ACTION_CHANNEL_ID,
                getString(R.string.approval_channel_name),
                NotificationManager.IMPORTANCE_HIGH
            ).apply {
                description = getString(R.string.approval_channel_desc)
                setShowBadge(true)
            }
            val nm = getSystemService(NotificationManager::class.java)
            nm?.createNotificationChannel(channel)
            nm?.createNotificationChannel(approval)
        }
    }

    private fun buildPersistentNotification(): Notification {
        // minSdk 已经是 26, Notification.Builder(context, channelId) 恒可用,
        // 旧的单参构造从 API 26 起已废弃, 没必要再保留分支。
        val builder = Notification.Builder(this, CHANNEL_ID)

        // 常驻通知必须给用户一个"关掉它"的出口，否则一旦自启就只能强行停止应用
        val stopIntent = Intent(this, FeisuoDaemonService::class.java).setAction(ACTION_STOP)
        val stopPendingIntent = PendingIntent.getService(
            this,
            1,
            stopIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        // 旧实现直接用 getLaunchIntentForPackage 的返回值构造 PendingIntent，
        // 一旦应用没有 LAUNCHER 入口就得到 null Intent，
        // 用户点通知会触发 IntentSenderRecord.sendInner 里的 NPE 而崩溃。
        val launchIntent = packageManager.getLaunchIntentForPackage(packageName)

        return builder
            .setContentTitle(getString(R.string.notification_title))
            .setContentText(getString(R.string.notification_text))
            // TODO: 工程补齐 res/drawable 后应替换为应用自有的单色矢量图标,
            //       框架内置图标在状态栏会被系统着色变形
            // 应用自有的纯白单色矢量图标。
            // 不能用框架内置的 stat_notify_sync: 它在部分 ROM 上会被裁切变形。
            .setSmallIcon(R.drawable.ic_stat_feisuo)
            .apply {
                if (launchIntent != null) {
                    setContentIntent(
                        PendingIntent.getActivity(
                            this@FeisuoDaemonService,
                            0,
                            launchIntent,
                            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
                        )
                    )
                }
            }
            .addAction(
                Notification.Action.Builder(
                    null,
                    getString(R.string.action_stop_daemon),
                    stopPendingIntent
                ).build()
            )
            .setOngoing(true)
            .build()
    }

    private fun acquireMulticastLock() {
        // BOOT_COMPLETED 触发时 Wi-Fi 往往还没关联成功，
        // 旧实现失败后不再重试，导致整个进程生命周期内 mDNS 全被过滤。
        if (multicastLock?.isHeld == true) {
            return
        }
        try {
            val wifiManager = applicationContext.getSystemService(WifiManager::class.java) ?: return
            multicastLock = wifiManager.createMulticastLock("FeisuoMulticastLock").apply {
                setReferenceCounted(true)
                acquire()
            }
            Log.i(TAG, "Wi-Fi 组播锁已成功获取")
        } catch (e: Exception) {
            Log.w(TAG, "获取组播锁失败, 等待网络恢复后重试: ${e.message}")
        }
    }

    private fun releaseMulticastLock() {
        multicastLock?.let {
            if (it.isHeld) {
                it.release()
                Log.i(TAG, "Wi-Fi 组播锁已释放")
            }
        }
        multicastLock = null
    }

    /**
     * 监听网络可用性: 网络一旦恢复就重新获取组播锁并提示 Rust 核心重连，
     * 避免"息屏 / 切换 Wi-Fi 之后局域网自连永久失效"。
     */
    private fun registerNetworkCallback() {
        if (networkCallback != null) return
        try {
            val connectivityManager = getSystemService(ConnectivityManager::class.java) ?: return
            val callback = object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    Log.i(TAG, "网络可用: $network, 重新确认组播锁")
                    acquireMulticastLock()
                    // 电脑大概率是刚重新出现在局域网里, 把上次分享失败
                    // 而暂存的文件补发出去。
                    retryPendingOnWorker()
                }

                override fun onLost(network: Network) {
                    Log.w(TAG, "网络断开: $network")
                }
            }
            val request = NetworkRequest.Builder()
                .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .build()
            connectivityManager.registerNetworkCallback(request, callback)
            networkCallback = callback
            Log.i(TAG, "已注册网络状态监听")
        } catch (e: Exception) {
            Log.w(TAG, "注册网络监听失败: ${e.message}")
        }
    }

    private fun unregisterNetworkCallback() {
        val callback = networkCallback ?: return
        try {
            getSystemService(ConnectivityManager::class.java)?.unregisterNetworkCallback(callback)
        } catch (e: Exception) {
            Log.w(TAG, "注销网络监听失败: ${e.message}")
        }
        networkCallback = null
    }
}
