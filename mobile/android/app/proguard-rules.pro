# 飞梭 Android 端 —— 默认 ProGuard 规则
# 当前未开启 minify, 这里先保留占位; 开启混淆时守护服务的 Service/Receiver
# 会被系统按类名反射拉起, 必须显式 keep。
-keep class net.findfine.feisuo.FeisuoDaemonService { *; }
-keep class net.findfine.feisuo.BootReceiver { *; }
-keep class net.findfine.feisuo.ShareTargetActivity { *; }
-keep class net.findfine.feisuo.MainActivity { *; }
