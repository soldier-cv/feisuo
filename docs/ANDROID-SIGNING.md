# Android 签名与升级兼容（必读）

## 为什么会有这份文档

2026-11-01 用户反馈「新 APK 装不上 / 更新失败」。排查结论：

| 版本 | 签发证书 SHA-256 |
|---|---|
| v0.1.17 | `3754ce2c5f395e69b7fe85a25cc6553952a546e9f846c947bd347ffe587b99a5` |
| v0.1.18 | `a4badcb86779fc04e6623885130aea8647829db0acdce49543a5dcfdde348ce8` |

**两版签名不同** → Android 拒绝覆盖安装（`INSTALL_FAILED_UPDATE_INCOMPATIBLE`）。

根因不是谁手工打错了包，而是**发布流水线从来没有正式签名密钥**：
`release.yml` 里 `gradlew assembleRelease` 没有注入任何 keystore，
`app/build.gradle.kts` 在「找不到 release 密钥」时**静默回退到 debug 签名**，
而 GitHub Actions runner 的 debug 密钥是每次临时生成的 ——
**于是每一版 APK 都换一次签名，用户永远升级不上。**

## 正式密钥（已生成，2026-11-01）

| 项 | 值 |
|---|---|
| 别名 | `feisuo` |
| 算法 | RSA 4096 / validity 10000 天（约 27 年，避免证书过期导致全员收不到更新） |
| DN | `CN=Feisuo, OU=Home, O=Feisuo, L=Home, ST=Home, C=CN` |
| **证书 SHA-256** | `02d2ad432970812f55e6e71b89a42d32548c9fe4781940958b7f3c51913ef71f` |

**这个指纹从此是硬约束**：`release.yml` 出包后会用 `apksigner verify --print-certs` 比对，
不符就拒绝发布。要换密钥必须先想清楚「所有已装用户都得卸载重装」。

## GitHub 上要配的四项

仓库 `soldier-cv/feisuo` → **Settings → Secrets and variables → Actions**：

| 名称 | 位置 | 说明 |
|---|---|---|
| `FEISUO_KEYSTORE_B64` | **Secrets** | `.jks` 的 base64（单行） |
| `FEISUO_KEYSTORE_PASSWORD` | **Secrets** | keystore 口令 |
| `FEISUO_KEY_ALIAS` | **Variables** | 固定值 `feisuo` |
| `FEISUO_KEY_PASSWORD` | **Secrets** | key 口令（与上面相同） |

密钥本体与口令不入库。本地开发仍可从 `mobile/android/keystore.properties` 读取。

## 流水线里的三道闸

1. **缺密钥即失败**（`Materialize Android release keystore` 步骤）：任何一项缺失直接 `throw`，
   不会带着 debug 签名继续出包。
2. **产物不许是 debug 签名**：`app/build.gradle.kts` 用 `FEISUO_REQUIRE_RELEASE_KEYSTORE=true`
   在配置期就 `GradleException`；构建脚本再查一次 `*debugkey*.apk` 作双保险。
3. **证书指纹必须等于固化值**：`apksigner verify --print-certs` 与
   `02d2ad43…ef71f` 比对，不符拒绝发布。

## 用户侧（这次只做一次）

- 0.1.17 / 0.1.18 都是临时 debug 密钥，没有任何一版值得继承 →
  **卸载旧版，装一次正式签名的新包**。
- 之后只要流水线一直用这把密钥，就能正常覆盖升级，不再需要卸载。

## 本地自测

```bash
cd mobile/android
cp keystore.properties.example keystore.properties   # 填 storeFile/storePassword/keyAlias/keyPassword
./gradlew assembleRelease
# 产物名不带 -debugkey 且指纹等于固化值，才算配好
```
