//! JNI 契约守卫: Rust 导出符号必须与 Kotlin `external` 声明严格一致。
//!
//! JNI 符号名由 `package + 类名 + 方法名` 拼成
//! (`Java_net_findfine_feisuo_NativeBridge_nativeXxx`)。任何一侧改名,
//! 编译期**完全看不出来**, 表现是运行期 `UnsatisfiedLinkError` ——
//! 而 Android 端目前没有真机联调, 也就是说这类失配会一路带到用户手上。
//!
//! 这里做两件事:
//! 1. 静态比对: 扫 `mobile.rs` 里 `#[no_mangle]` 导出的 JNI 函数名,
//!    与 `NativeBridge.kt` 里 `external fun` 的方法名, 必须双向完全一致。
//!    这不需要 NDK, 因此能在普通 `cargo test` 里跑。
//! 2. 若 `libfeisuo_core.so` 已经构建出来, 再用 `llvm-nm` 核对实际导出,
//!    防止"源码写了 `#[no_mangle]` 但没被链接器导出"(例如被优化掉)。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// 仓库根目录 (desktop/src-tauri -> ../..)
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// JNI 符号的固定前缀, 对应包名 + 类名
const JNI_PREFIX: &str = "Java_net_findfine_feisuo_NativeBridge_";

fn core_src() -> PathBuf {
    repo_root().join("core").join("src")
}

fn native_bridge_kt() -> PathBuf {
    repo_root()
        .join("mobile")
        .join("android")
        .join("app")
        .join("src")
        .join("main")
        .join("java")
        .join("net")
        .join("findfine")
        .join("feisuo")
        .join("NativeBridge.kt")
}

#[test]
fn jni_contract_files_must_exist() {
    assert!(
        core_src().join("mobile.rs").is_file(),
        "找不到 core/src/mobile.rs, JNI 守卫会静默空跑"
    );
    assert!(
        native_bridge_kt().is_file(),
        "找不到 NativeBridge.kt, JNI 守卫会静默空跑"
    );
}

/// 从 Rust 源码里收集 JNI 导出函数名。
///
/// 只认紧跟在 `#[no_mangle]` 之后的 `pub extern "system" fn <名字>`,
/// 避免把内部辅助函数也算进去。
fn rust_exported_symbols() -> BTreeSet<String> {
    let src = std::fs::read_to_string(core_src().join("mobile.rs"))
        .expect("读取 core/src/mobile.rs 失败");
    let mut out = BTreeSet::new();
    let mut pending_no_mangle = false;
    for line in src.lines() {
        let t = line.trim();
        if t.starts_with("#[no_mangle]") {
            pending_no_mangle = true;
            continue;
        }
        if !pending_no_mangle {
            continue;
        }
        // 允许 #[no_mangle] 与 fn 之间夹着别的属性
        if t.starts_with("#[") || t.is_empty() {
            continue;
        }
        if t.starts_with("pub extern") || t.starts_with("extern") {
            if let Some(idx) = t.find("fn ") {
                let rest = &t[idx + 3..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    out.insert(name);
                }
            }
        }
        pending_no_mangle = false;
    }
    out
}

/// 从 Rust 源码里收集 JNI 导出的**方法名与业务参数个数**。
///
/// 业务参数 = 签名里扣掉 JNI 规范固定的那两个（`JNIEnv`、`jclass`）。
/// 那两个不是业务参数；不减的话每个函数都会多 2，永远对不上 Kotlin 侧。
///
/// ## 这里踩过的坑（都记在这里）
///
/// 1. 调用约定是 `extern "system"` 而不是 `extern "C"`（JNI 要求）。
///    只匹配 `"C"` 会**一个符号都收不到**，全表报"漏导出"——
///    一条匹配式的错误，看起来像十来个代码缺陷。
/// 2. 符号名后面**可能**有 `<'local>` 生命周期参数，也可能没有
///    ——不需要 `JNIEnv` 的函数（如只吃一个句柄的）就没有。
///    强制要求 `<` 会漏掉它们。*把"大多数情况"当"全部"。*
fn rust_exported_symbols_with_arity() -> BTreeMap<String, usize> {
    let src = std::fs::read_to_string(core_src().join("mobile.rs"))
        .expect("读取 core/src/mobile.rs 失败");
    let mut out = BTreeMap::new();
    let mut rest = src.as_str();
    while let Some(idx) = rest.find("#[no_mangle]") {
        let after = &rest[idx + "#[no_mangle]".len()..];
        // 跳过 `#[no_mangle]` 与 `fn` 之间可能夹着的其它属性
        let mut cursor = after;
        loop {
            let t = cursor.trim_start();
            if let Some(stripped) = t.strip_prefix("#[") {
                match stripped.find(']') {
                    Some(end) => cursor = &stripped[end + 1..],
                    None => break,
                }
                continue;
            }
            if t.is_empty() {
                break;
            }
            cursor = t;
            break;
        }
        let Some(fn_at) = cursor.find("fn ") else {
            rest = after;
            continue;
        };
        let after_fn = &cursor[fn_at + 3..];
        let name: String = after_fn
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // 跳过可能存在的 `<'local, 'a>` 泛型参数
        let mut tail = &after_fn[name.len()..];
        if let Some(stripped) = tail.trim_start().strip_prefix('<') {
            match stripped.find('>') {
                Some(end) => tail = &stripped[end + 1..],
                None => tail = "",
            }
        }
        let Some(open) = tail.find('(') else {
            rest = after;
            continue;
        };
        let params_src = &tail[open + 1..];
        let mut depth = 0i32;
        let mut end = params_src.len();
        for (i, c) in params_src.char_indices() {
            match c {
                '(' | '[' | '<' => depth += 1,
                ')' | ']' | '>' => {
                    if depth == 0 {
                        end = i;
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        let total = if params_src[..end].trim().is_empty() {
            0
        } else {
            count_top_level_args(&params_src[..end])
        };
        // 只收 JNI 导出（符号以固定包前缀开头），跳过内部辅助函数
        if name.starts_with(JNI_PREFIX) {
            let method = name.trim_start_matches(JNI_PREFIX).to_string();
            out.insert(method, total.saturating_sub(2));
        }
        rest = &params_src[end..];
    }
    out
}

/// 从 Kotlin 源码里收集 `external fun` 方法名。
fn kotlin_external_methods() -> BTreeSet<String> {
    kotlin_external_methods_with_arity().into_keys().collect()
}

/// 从 Kotlin 源码里收集 `external fun` 的**方法名与参数个数**。
///
/// 为什么必须连参数个数一起收：符号名对得上但参数个数错位时，
/// JNI 不会报任何错 —— 它按调用方的栈取参数，于是读出**垃圾值**。
/// 表现是"某个参数永远是 0 / 空串 / null"，极难联想到是签名问题。
///
/// ⚠️ 必须**跨行**匹配：参数多的时候声明会折行（`nativeSendFiles` 有
/// 7 个参数、实际占 9 行）。逐行匹配会漏掉它，然后双向差集把它报成
/// "Rust 有死导出" —— 方向完全反了。
fn kotlin_external_methods_with_arity() -> BTreeMap<String, usize> {
    let src = std::fs::read_to_string(native_bridge_kt()).expect("读取 NativeBridge.kt 失败");
    let mut out = BTreeMap::new();
    let mut rest = src.as_str();
    while let Some(idx) = rest.find("external fun") {
        let after = &rest[idx + "external fun".len()..];
        let Some(open) = after.find('(') else {
            rest = after;
            continue;
        };
        let name: String = after[..open]
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // 参数表：从 '(' 起按**圆括号配平**取到配对的 ')'。
        // 泛型里也有括号（`List<String>` 无括号，但 `Map<K, V>` 有逗号），
        // 所以逗号计数要按**尖括号/圆括号/方括号**的嵌套深度来。
        let params_src = &after[open + 1..];
        let mut depth = 0i32;
        let mut end = params_src.len();
        for (i, c) in params_src.char_indices() {
            match c {
                '(' | '[' | '<' => depth += 1,
                ')' | ']' | '>' => {
                    if depth == 0 {
                        end = i;
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        let arity = if params_src[..end].trim().is_empty() {
            0
        } else {
            count_top_level_args(&params_src[..end])
        };
        if !name.is_empty() {
            out.insert(name, arity);
        }
        rest = &params_src[end..];
    }
    out
}

/// 按**顶层**逗号计数（泛型 `Map<String, Int>` 里那个逗号不算）。
fn count_top_level_args(s: &str) -> usize {
    let mut depth = 0i32;
    let mut count = 0usize;
    let mut has = false;
    for c in s.chars() {
        match c {
            '(' | '[' | '<' => depth += 1,
            ')' | ']' | '>' => depth -= 1,
            ',' if depth == 0 => {
                count += 1;
                has = false;
            }
            c if !c.is_whitespace() => has = true,
            _ => {}
        }
    }
    count + usize::from(has)
}

#[test]
fn rust_exports_must_match_kotlin_declarations() {
    let rust = rust_exported_symbols();
    let kt = kotlin_external_methods();

    assert!(
        !rust.is_empty(),
        "没从 core/src/mobile.rs 解析出任何 JNI 导出, 匹配器可能已失效"
    );
    assert!(
        !kt.is_empty(),
        "没从 NativeBridge.kt 解析出任何 external 声明, 匹配器可能已失效"
    );

    // Kotlin 侧的方法名 -> 期望的完整符号名
    let expected: BTreeSet<String> = kt
        .iter()
        .map(|m| format!("{}{}", JNI_PREFIX, m))
        .collect();

    let missing_in_rust: Vec<&String> = expected.difference(&rust).collect();
    let missing_in_kotlin: Vec<&String> = rust.difference(&expected).collect();

    assert!(
        missing_in_rust.is_empty() && missing_in_kotlin.is_empty(),
        "JNI 契约失配 (运行期表现为 UnsatisfiedLinkError, 编译期看不出来):\n  \
         Kotlin 声明了但 Rust 未导出: {:?}\n  \
         Rust 导出了但 Kotlin 未声明: {:?}",
        missing_in_rust,
        missing_in_kotlin
    );
}

/// 守卫：**参数个数**必须两侧一致。
///
/// 符号名对得上但个数错位时，JNI **不会报任何错** —— 它按调用方的栈
/// 取参数，于是读出垃圾值。表现是"某个参数永远是 0 / 空串 / null"，
/// 极难联想到是签名问题。
///
/// 现有测试只比对方法名，那是**不够**的：多一个参数同样能编译通过、
/// 同样能过名字比对。
#[test]
fn jni_parameter_counts_must_match() {
    let rust = rust_exported_symbols_with_arity();
    let kt = kotlin_external_methods_with_arity();

    assert!(
        !rust.is_empty(),
        "没从 core/src/mobile.rs 解析出任何带参数个数的 JNI 导出, 解析器可能已失效"
    );
    assert!(
        !kt.is_empty(),
        "没从 NativeBridge.kt 解析出任何带参数个数的 external 声明, 解析器可能已失效"
    );

    let mut mismatches: Vec<String> = Vec::new();
    for (name, k_arity) in &kt {
        match rust.get(name) {
            None => {
                // 名字缺失已由 rust_exports_must_match_kotlin_declarations 报,
                // 这里不重复报, 免得同一条缺陷出现两次。
            }
            Some(r_arity) if r_arity != k_arity => {
                mismatches.push(format!(
                    "{}: Rust {} 个 / Kotlin {} 个（Rust 侧已扣掉 JNIEnv+jclass）",
                    name, r_arity, k_arity
                ));
            }
            _ => {}
        }
    }
    assert!(
        mismatches.is_empty(),
        "JNI 参数个数失配 —— 不会 UnsatisfiedLinkError, 而是按栈读出垃圾值:\n  {}",
        mismatches.join("\n  ")
    );
}

/// 守卫：参数个数的**收集器**本身没坏。
///
/// 没有这一条，上面那条守卫可能因为解析器失效而**恒绿**：
/// 两边都收到空集 → 循环体一次都不执行 → 断言通过。
/// 而"没发现问题"正是解析失效时的输出 —— 那种守卫比没有更糟。
#[test]
fn jni_arity_collectors_agree_with_name_only_collectors() {
    let rust_names = rust_exported_symbols();
    let rust_arity = rust_exported_symbols_with_arity();
    let kt_names = kotlin_external_methods();
    let kt_arity = kotlin_external_methods_with_arity();

    assert_eq!(
        rust_names.len(),
        rust_arity.len(),
        "带参数个数的收集器漏了符号（{} vs {}）—— 解析器失效会让参数检查恒绿",
        rust_names.len(),
        rust_arity.len()
    );
    assert_eq!(
        kt_names.len(),
        kt_arity.len(),
        "带参数个数的收集器漏了 external 声明（{} vs {}）—— 解析器失效会让参数检查恒绿",
        kt_names.len(),
        kt_arity.len()
    );

    // 至少要有一个"有参数"的函数, 否则"扣掉 JNIEnv/jclass"这段逻辑
    // 从来没被真正验过。
    assert!(
        rust_arity.values().any(|n| *n > 0),
        "所有导出都被算成 0 参数 —— 减 JNIEnv/jclass 的逻辑多半写错了"
    );
}

#[test]
fn jni_prefix_matches_kt_package_and_object() {    // 符号前缀是硬编码在 Rust 里的, 一旦 Kotlin 改包名/类名就会失配。
    // 这里从 Kotlin 源码里把包名与 object 名解析出来, 重新拼出期望前缀,
    // 与 Rust 侧实际使用的常量比对 —— 避免"改了一处忘了另一处"。
    let src = std::fs::read_to_string(native_bridge_kt()).expect("读取 NativeBridge.kt 失败");

    let package = src
        .lines()
        .find_map(|l| l.trim().strip_prefix("package "))
        .map(|s| s.trim().to_string())
        .expect("NativeBridge.kt 缺少 package 声明");
    let object = src
        .lines()
        .find_map(|l| {
            let t = l.trim();
            t.strip_prefix("object ")
                .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
        })
        .expect("NativeBridge.kt 缺少 object 声明");

    let rebuilt = format!("Java_{}_{}_", package.replace('.', "_"), object);
    assert_eq!(
        rebuilt, JNI_PREFIX,
        "Rust 侧的 JNI 前缀与 Kotlin 的 package/object 不一致, \
         改包名或类名时必须同步 core/src/mobile.rs 里的 JNI_PREFIX"
    );
}

/// 若 `.so` 已构建, 再核对实际导出符号。
/// 没构建就跳过 —— 这一条是加分项, 前一条(源码比对)才是常驻守卫。
#[test]
fn built_so_exports_the_declared_symbols() {
    let so = repo_root()
        .join("target")
        .join("aarch64-linux-android")
        .join("release")
        .join("libfeisuo_core.so");
    if !so.is_file() {
        eprintln!(
            "跳过: {} 尚未构建 (交叉编译步骤见 README「Android 原生库交叉编译」)",
            so.display()
        );
        return;
    }

    // 找 NDK 里的 llvm-nm
    let nm = std::env::var("FEISUO_NDK_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let base = std::env::var("LOCALAPPDATA").ok()?;
            let ndk_root = Path::new(&base).join("Android").join("Sdk").join("ndk");
            std::fs::read_dir(ndk_root)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .find(|p| p.join("toolchains").is_dir())
        })
        .map(|ndk| ndk.join("toolchains").join("llvm").join("prebuilt").join("windows-x86_64").join("bin").join("llvm-nm.exe"));

    let Some(nm) = nm.filter(|p| p.is_file()) else {
        eprintln!("跳过: 找不到 llvm-nm (设置 FEISUO_NDK_HOME 或安装 Android NDK)");
        return;
    };

    // llvm-nm 是控制台子系统程序, 这里同样走 os_shim 的隐藏封装,
    // 免得测试本身就闪一个黑窗口(虽然只在测试里发生)。
    let out = crate::os_shim::run_hidden(
        nm.to_str().unwrap_or("llvm-nm.exe"),
        &[
            "-D",
            "--defined-only",
            so.to_str().unwrap_or_default(),
        ],
    )
    .expect("执行 llvm-nm 失败");

    let text = String::from_utf8_lossy(&out.stdout);
    let exported: BTreeSet<String> = text
        .lines()
        .filter_map(|l| l.split_whitespace().last())
        .filter(|s| s.starts_with(JNI_PREFIX))
        .map(|s| s.to_string())
        .collect();

    let expected = rust_exported_symbols();
    assert!(
        !exported.is_empty(),
        "{} 里没有找到任何 JNI 符号, 产物可能有问题",
        so.display()
    );

    let missing: Vec<&String> = expected.difference(&exported).collect();
    assert!(
        missing.is_empty(),
        "源码里写了 #[no_mangle] 但 .so 未实际导出 (运行期 UnsatisfiedLinkError): {:?}",
        missing
    );
}
