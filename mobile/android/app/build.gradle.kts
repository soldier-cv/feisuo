import java.util.Properties

// 版本号只有一个来源：仓库根的 Cargo.toml（AGENTS.md 规则 6）。
//
// 为什么不能在这里写死 "0.1.0"：
// 桌面端（Tauri）从 tauri.conf.json 读，Rust 从 Cargo.toml 读，
// 而这里如果再写一份字面量，就有了**三个**来源 —— 改了两个忘了第三个时，
// 用户装到的 APK 会显示旧版本号，而它连着同一个设备指纹与信任库。
// 症状是"我明明更新了，怎么还显示旧版"，且没有任何报错。
//
// 解析失败必须**直接失败**：静默退回 "0.0.0" 会让一个构建错误
// 变成一个更难查的运行时版本不一致。
val workspaceCargoToml = rootProject.file("../../Cargo.toml")
val appVersionName: String = run {
    val text = workspaceCargoToml.readText()
    val m = Regex("(?m)^version\\s*=\\s*\"([^\"]+)\"").find(text)
        ?: error(
            "读不到版本号：${workspaceCargoToml.absolutePath} 里没有 version = \"...\"。\n" +
            "        版本号必须只由 Cargo.toml 提供（AGENTS.md 规则 6），请修 Cargo.toml 而不是在这里写死。"
        )
    m.groupValues[1]
}

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// ---------------------------------------------------------------------------
// 签名
//
// 别的开源项目（Termux / Signal 等）都是同一套做法，这里照抄：
//   1. keystore 的**路径与口令**从 `keystore.properties` 或环境变量读，
//      两者都**不入库**（密钥进仓库 = 谁都能冒充你发更新）。
//   2. **没有密钥也照样能出可安装的 APK** —— 回退到 debug 签名。
//
// 第 2 点是刻意的：把"没配密钥"做成**构建失败**是常见的坏做法，
// 它让人以为 release 构建坏了，而实际问题只是"你还没创建密钥"。
// 能不能正式分发是**发布时**的决定，不是构建时该拦下来的事。
// 为了让产物不���被误当正式版，文件名与产物目录都会带 `-debugkey` 标记。
// ---------------------------------------------------------------------------
val keystorePropsFile = rootProject.file("keystore.properties")
val keystoreProps = Properties().apply {
    if (keystorePropsFile.exists()) {
        keystorePropsFile.inputStream().use { load(it) }
    }
}

fun secretFrom(fileKey: String, envKey: String): String? {
    val fromEnv = System.getenv(envKey)
    if (!fromEnv.isNullOrBlank()) return fromEnv
    val fromFile = keystoreProps.getProperty(fileKey)
    if (!fromFile.isNullOrBlank()) return fromFile
    return null
}

val releaseStorePath = secretFrom("storeFile", "FEISUO_KEYSTORE")
val releaseStorePass = secretFrom("storePassword", "FEISUO_KEYSTORE_PASSWORD")
val releaseKeyAlias = secretFrom("keyAlias", "FEISUO_KEY_ALIAS")
val releaseKeyPass = secretFrom("keyPassword", "FEISUO_KEY_PASSWORD")

// 四项必须齐全才算配置好了。
// 只配一半就去签名会得到一个"看起来签名了、实际装不上"的 APK ——
// 报错发生在用户手机上（INSTALL_PARSE_FAILED_NO_CERTIFICATES），
// 比构建期报错难查得多。
val hasReleaseKeystore = listOf(
    releaseStorePath, releaseStorePass, releaseKeyAlias, releaseKeyPass
).all { !it.isNullOrBlank() } && file(releaseStorePath ?: "").exists()

if (!hasReleaseKeystore) {
    logger.lifecycle(
        "[feisuo] 未找到 release 密钥（keystore.properties 或 FEISUO_* 环境变量），" +
        "release 构建将回退到 **debug 签名**。产物可安装、可自测，但不能上架。" +
        "配置方式见 keystore.properties.example"
    )
}

// ---------------------------------------------------------------------------
// NDK 版本：来自 local.properties 的 ndk.version，不在这里写死
//
// AGP 需要 NDK 里的 `llvm-strip` 来裁剪原生库符号表。找不到时**不报错**，
// 只打一条 "Unable to strip the following libraries"，然后把带完整符号表的
// .so 原样塞进 APK —— 实测 7.9 MB vs 6.56 MB，而 Gradle 全程 BUILD SUCCESSFUL。
//
// 这里给的是**版本号**，不是路径。试过 `android { ndkPath = ... }`（路径），
// 实测仍然 "Unable to strip"：AGP 8.7 解析 strip 工具走的是
// `<sdk>/ndk/<版本>/` 那一路，给绝对路径不够。
//
// 版本号本身也不写死，而是由 `mobile\android\build-native.ps1` 从
// `source.properties` 的 `Pkg.Revision` 读出来写进 local.properties ——
// 它本来就要发现 NDK 才能配 linker。写死会有两个问题：换机器必须装同一版本；
// 以及"脚本用 r30 编、Gradle 按 AGP 默认版本（27.0.12077973）找 strip"
// 这种工具链错配。**一个事实只有一处来源。**（AGENTS.md 规则 6 的同一个精神）
//
// 没编过 .so 时这一行不存在 —— 那时退回 AGP 默认行为，也就是"不 strip"，
// 与之前一致：宁可包大 1.3 MB，也不要构建失败。
// ---------------------------------------------------------------------------
val localPropsFile = rootProject.file("local.properties")
val localProps = Properties().apply {
    if (localPropsFile.exists()) {
        localPropsFile.inputStream().use { load(it) }
    }
}
val ndkVersionFromLocal = localProps.getProperty("ndk.version")?.trim()

android {
    namespace = "net.findfine.feisuo"
    // 本机 SDK 只装了 android-36 / android-37.0, 没有 android-35。
    // compileSdk 只决定"能用哪些新 API", 填 36 完全安全;
    // 真正决定运行时行为分叉的是 targetSdk。
    compileSdk = 36

    defaultConfig {
        applicationId = "net.findfine.feisuo"
        minSdk = 26          // 前台服务 + 通知渠道要求 26+
        targetSdk = 35       // 对齐 Android 15 的前台服务类型与 6 小时配额限制
        // 来自仓库根 Cargo.toml，与桌面端 / Rust 端同源（见文件头说明）
        versionName = appVersionName
        // versionCode 必须**单调递增**，否则应用商店拒绝更新、
        // 而 adb install 会静默装不上（INSTALL_FAILED_VERSION_DOWNGRADE）。
        // 它必须是整数，Cargo.toml 的语义版本给不了，所以从它派生：
        //   0.1.0 -> 1 ；1.2.3 -> 10203 ；2.0.0 -> 20000
        // 需要更细的粒度时用 -PfeisuoVersionCode=12345 覆盖。
        versionCode = (project.findProperty("feisuoVersionCode") as String?)
            ?.toInt()
            ?: appVersionName.split(".")
                .map { s -> s.takeWhile { it.isDigit() }.ifEmpty { "0" }.toInt() }
                .let { parts ->
                    val major = parts.getOrElse(0) { 0 }
                    val minor = parts.getOrElse(1) { 0 }
                    val patch = parts.getOrElse(2) { 0 }
                    major * 10000 + minor * 100 + patch
                }

        // Rust core 的 JNI 产物。没有这一段, APK 里不会有 lib/<abi>/libfeisuo_core.so,
        // 运行期表现为 UnsatisfiedLinkError —— 而 Gradle 全程 BUILD SUCCESSFUL,
        // 编译期完全看不出来。
        ndk {
            // 只保留真机需要的 ABI: 每个 ABI 的 .so 约 4.4 MB,
            // 三个都打进去会让 APK 直接翻三倍。
            abiFilters += listOf("arm64-v8a")
        }
    }

    // 本机只装了 build-tools 35.0.0 / 36.0.0, 必须显式指定,
    // 否则 AGP 会按 AGP 8.7 的默认值去找 34.0.0 而失败。
    buildToolsVersion = "35.0.0"

    // NDK 版本。缺失时**不设置** —— 让 AGP 走自己的默认，
    // 而不是让它因为一个写死的版本直接失败。代价是"不 strip"（APK 大 1.3 MB）。
    if (!ndkVersionFromLocal.isNullOrBlank()) {
        ndkVersion = ndkVersionFromLocal
    } else {
        logger.lifecycle(
            "[feisuo] local.properties 里没有 ndk.version，AGP 将无法 strip 原生库" +
            "（APK 约大 1.3 MB）。先跑一次 mobile\\android\\build-native.ps1 即可。"
        )
    }

    signingConfigs {
        if (hasReleaseKeystore) {
            create("release") {
                storeFile = file(releaseStorePath!!)
                storePassword = releaseStorePass
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPass
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            // 有密钥就用 release 密钥，没有就退回 debug 密钥。
            // 无论哪条路 `assembleRelease` 都能产出**可安装**的 APK ——
            // 见文件头第 2 点：把"没配密钥"做成构建失败是坏做法。
            signingConfig = signingConfigs.findByName("release")
                ?: signingConfigs.getByName("debug")
        }
        debug {
            isMinifyEnabled = false
        }
    }

    // 产物命名：debug 密钥打出来的 release 包要**一眼能认出来**，
    // 否则它会和正式版混在一起被人发出去 —— 而 debug 密钥的应用
    // 无法被正式版覆盖安装，用户会以为"更新失败"。
    applicationVariants.all {
        outputs.all {
            val output = this as com.android.build.gradle.internal.api.BaseVariantOutputImpl
            output.outputFileName = if (buildType.name == "release" && !hasReleaseKeystore) {
                "feisuo-${versionName}-${versionCode}-debugkey.apk"
            } else {
                "feisuo-${versionName}-${versionCode}.apk"
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    // kotlinOptions 在 KGP 2.x 已废弃, 改用 compilerOptions
    kotlin {
        compilerOptions {
            jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
        }
    }

    buildFeatures {
        buildConfig = true
    }

    // 清单里不允许再出现 package 属性(AGP 8+ 硬错误), 由上面的 namespace 统一提供
    packaging {
        resources.excludes += setOf("META-INF/*.kotlin_module")
    }
}

dependencies {
    // 仅用平台 API, 不引入第三方运行时依赖 —— 守护服务必须尽量轻量
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
}
