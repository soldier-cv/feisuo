// 飞梭 Android 端 —— 顶层构建配置
//
// 仓库顺序说明: 全部走国内镜像优先。Maven Central 直连在国内经常
// 卡在几十 MB 的 Kotlin 编译器下载上, 构建会长时间无响应。
// mirrors 放在前面, 官方仓库保留作为兜底。
pluginManagement {
    repositories {
        maven("https://maven.aliyun.com/repository/gradle-plugin")
        maven("https://maven.aliyun.com/repository/google")
        maven("https://maven.aliyun.com/repository/public")
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        maven("https://maven.aliyun.com/repository/google")
        maven("https://maven.aliyun.com/repository/public")
        google()
        mavenCentral()
    }
}

rootProject.name = "Feisuo"
include(":app")
