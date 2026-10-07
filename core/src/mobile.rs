//! Android 宿主的 JNI 绑定层 (纯 FFI 薄壳)。
//!
//! **所有实质逻辑都在 [`crate::engine_host`] 里并有宿主单元测试覆盖。**
//! 本文件只做三件事:
//! 1. 导出符号名 `Java_net_findfine_feisuo_NativeBridge_nativeXxx`;
//! 2. Java 字符串 <-> Rust 字符串编解码;
//! 3. 在 FFI 边界上兜住 panic。
//!
//! 这样切分的原因: JNI 符号名必须与 `package + 类名 + 方法名` 严格对应,
//! 改名 Package 或类名而不同步这里, 表现是运行期 `UnsatisfiedLinkError`,
//! **编译期完全看不出来**。把这类无逻辑的样板与业务逻辑分开后,
//! 真正需要验证的部分就能用普通测试覆盖, 不必依赖真机。
//!
//! @author xudong.hua,gemini
//! @since 2026-09-30 17:30 星期三

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::engine_host as host;

use jni::objects::{JClass, JObjectArray, JString};
use jni::sys::{jboolean, jint, jlong, jobjectArray, jstring};
use jni::JNIEnv;

/// 启动引擎。返回不透明句柄, 0 表示失败 (原因已写入日志)。
///
/// 对应 Kotlin: `NativeBridge.nativeInitEngine(appDir: String): Long`
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeInitEngine<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    app_dir: JString<'local>,
) -> jlong {
    // extern "C" 里绝不能让 panic 逃出去: 跨 FFI 的 panic 是未定义行为,
    // 在 Android 上表现为整个进程 abort, 且没有任何 Java 侧堆栈。
    let result = catch_unwind(AssertUnwindSafe(|| {
        let dir: String = env
            .get_string(&app_dir)
            .map_err(|e| format!("读取数据目录参数失败: {}", e))?
            .into();
        Ok::<jlong, String>(host::boot(&dir))
    }));

    match result {
        Ok(Ok(handle)) => handle,
        Ok(Err(msg)) => {
            host::log_error(&format!("引擎启动失败: {}", msg));
            host::INVALID_HANDLE as jlong
        }
        Err(_) => {
            host::log_error("引擎启动时发生 panic, 已在 FFI 边界拦截");
            host::INVALID_HANDLE as jlong
        }
    }
}

/// 停止引擎并释放资源。句柄为 0 时是空操作。
///
/// 对应 Kotlin: `NativeBridge.nativeShutdownEngine(handle: Long)`
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeShutdownEngine(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        host::shutdown(handle as i64);
    }));
}

/// 引擎是否已启动。
///
/// 对应 Kotlin: `NativeBridge.nativeIsEngineRunning(): Int`
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeIsEngineRunning(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jboolean {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if host::is_running() {
            1u8
        } else {
            0u8
        }
    }));
    result.unwrap_or(0)
}

/// 取本机设备 ID。引擎未启动时返回空串。
///
/// 对应 Kotlin: `NativeBridge.nativeGetDeviceId(): String?`
///
/// 直接返回 `jstring` 而不是 C 字符串指针: 那样就不需要 `nativeFreeString`
/// 与"谁负责释放"这层约定, 也不可能泄漏。
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeGetDeviceId<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let value = catch_unwind(AssertUnwindSafe(|| host::device_id(0)))
        .unwrap_or_default();
    new_jstring(&mut env, &value)
}

/// 取当前配置的 JSON 快照。引擎未启动时返回空串。
///
/// 对应 Kotlin: `NativeBridge.nativeGetConfigJson(): String?`
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeGetConfigJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let value = catch_unwind(AssertUnwindSafe(|| host::config_json(0))).unwrap_or_default();
    new_jstring(&mut env, &value)
}

/// 取下一个待审批请求 (最多等待 timeoutMs 毫秒); 没有则返回 null。
///
/// 对应 Kotlin: `NativeBridge.nativeNextApproval(timeoutMs: Int): String?`
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeNextApproval<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    timeout_ms: jint,
) -> jstring {
    let ms = if timeout_ms < 0 { 0 } else { timeout_ms as u64 };
    let result = catch_unwind(AssertUnwindSafe(|| host::next_approval(ms)));
    match result {
        Ok(Some(json)) => new_jstring(&mut env, &json),
        Ok(None) => std::ptr::null_mut(),
        Err(_) => {
            host::log_error("读取审批请求时发生 panic, 已在 FFI 边界拦截");
            std::ptr::null_mut()
        }
    }
}

/// 回应一个审批请求。
///
/// 对应 Kotlin: `NativeBridge.nativeRespondApproval(id: String, allow: Boolean): Boolean`
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeRespondApproval<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    approval_id: JString<'local>,
    allow: jboolean,
) -> jboolean {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let id: String = match env.get_string(&approval_id) {
            Ok(v) => v.into(),
            Err(e) => {
                host::log_error(&format!("读取审批 ID 参数失败: {}", e));
                return false;
            }
        };
        host::respond_approval(&id, allow != 0)
    }));
    match result {
        Ok(v) => {
            if v {
                jboolean::from(true)
            } else {
                jboolean::from(false)
            }
        }
        Err(_) => {
            host::log_error("回应审批时发生 panic, 已在 FFI 边界拦截");
            jboolean::from(false)
        }
    }
}

/// 把 Rust 字符串变成 Java `String`。
///
/// 失败时返回 null (`null` 在 Kotlin 侧对应可空类型), 而不是空串 ——
/// 空串会把"引擎未启动"伪装成"设备指纹是空的"。
fn new_jstring<'local>(env: &mut JNIEnv<'local>, s: &str) -> jstring {
    match env.new_string(s) {
        Ok(js) => js.into_raw(),
        Err(e) => {
            host::log_error(&format!("构造 Java String 失败: {}", e));
            std::ptr::null_mut()
        }
    }
}

/// 取已配对设备的 JSON 数组快照, 供宿主挑选推送目标。
///
/// 对应 Kotlin: `NativeBridge.nativeGetTrustedDevicesJson(): String?`
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeGetTrustedDevicesJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let value = catch_unwind(AssertUnwindSafe(|| host::trusted_devices_json(0))).unwrap_or_default();
    new_jstring(&mut env, &value)
}

/// 用 6 位 PIN 与指定设备完成双向绑定。
///
/// 对应 Kotlin: `NativeBridge.nativePairWithDevice(ip, port, pin): String?`
///
/// 成功返回绑定后设备信息的 JSON; 失败返回 `{"error": "原因"}`。
/// 刻意不用"错误串 / JSON 串"混在一个返回值里 —— 那样宿主得靠前缀猜,
/// 一旦 JSON 形状变化就会误判。这里统一成 JSON, 解析方式只有一种。
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativePairWithDevice<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    target_ip: JString<'local>,
    target_port: jint,
    pin: JString<'local>,
) -> jstring {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let ip: String = match env.get_string(&target_ip) {
            Ok(v) => v.into(),
            Err(e) => {
                host::log_error(&format!("读取配对 IP 参数失败: {}", e));
                return Err("读取 IP 参数失败".to_string());
            }
        };
        let pin: String = match env.get_string(&pin) {
            Ok(v) => v.into(),
            Err(e) => {
                host::log_error(&format!("读取配对码参数失败: {}", e));
                return Err("读取配对码参数失败".to_string());
            }
        };
        if target_port <= 0 || target_port > 65535 {
            return Err("目标端口非法".to_string());
        }
        host::pair_with_device(0, &ip, target_port as u16, &pin)
    }));

    let payload = match result {
        Ok(Ok(json)) => json,
        Ok(Err(e)) => format!("{{\"error\": {}}}", json_quote(&e)),
        Err(_) => {
            host::log_error("配对时发生 panic, 已在 FFI 边界拦截");
            "{\"error\": \"配对时发生内部错误\"}".to_string()
        }
    };
    new_jstring(&mut env, &payload)
}

/// 把字符串安全地编码成 JSON 字符串字面量(含引号转义)。
fn json_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"未知错误\"".to_string())
}

/// 主动探测对端 IP, 拿到它当前声明的传输端口。
///
/// 对应 Kotlin: `NativeBridge.nativeProbeDevice(ip: String): String?`
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeProbeDevice<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    ip: JString<'local>,
) -> jstring {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let ip: String = match env.get_string(&ip) {
            Ok(s) => s.into(),
            Err(e) => {
                host::log_error(&format!("读取探测 IP 参数失败: {}", e));
                return String::new();
            }
        };
        match host::probe(0, &ip) {
            Ok(dev) => serde_json::to_string(&dev).unwrap_or_default(),
            Err(e) => {
                host::log_error(&format!("探测 {} 失败: {}", ip, e));
                String::new()
            }
        }
    }))
    .unwrap_or_default();
    new_jstring(&mut env, &result)
}

/// 向指定设备发送一批本地文件。
///
/// 对应 Kotlin:
/// `NativeBridge.nativeSendFiles(ip, port, deviceId, deviceName, paths: Array<String>): String?`
///
/// 失败时返回**错误描述字符串**, 成功返回空串 —— 不能用 boolean,
/// 因为宿主需要把"为什么失败"展示给用户(未配对 / 对方拒绝 / 端口不通),
/// 一个 false 只会让用户看到一句"发送失败"。
#[no_mangle]
pub extern "system" fn Java_net_findfine_feisuo_NativeBridge_nativeSendFiles<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    target_ip: JString<'local>,
    target_port: jint,
    target_device_id: JString<'local>,
    target_device_name: JString<'local>,
    paths: jobjectArray,
) -> jstring {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // 不用闭包统一读取: 闭包捕获 &mut env 会在循环里造成借用冲突,
        // 而且每个 JString 借用的生命周期都绑在 env 上, 闭包返回的
        // String 反而更容易踩到 "does not live long enough"。直接展开最省心。
        let ip: String = env
            .get_string(&target_ip)
            .map_err(|e| format!("读取 IP 参数失败: {}", e))?
            .into();
        let device_id: String = env
            .get_string(&target_device_id)
            .map_err(|e| format!("读取设备 ID 参数失败: {}", e))?
            .into();
        let device_name: String = env
            .get_string(&target_device_name)
            .map_err(|e| format!("读取设备名参数失败: {}", e))?
            .into();

        let mut list: Vec<String> = Vec::new();
        if !paths.is_null() {
            // 用 JNIEnv 提供的封装而不是裸 JObjectArray: 后者只能从
            // as_raw() 构造, 直接 from(raw ptr) 不满足 trait bound。
            let arr = unsafe { JObjectArray::from_raw(paths) };
            let len = env.get_array_length(&arr).unwrap_or(0);
            for i in 0..len {
                if let Ok(obj) = env.get_object_array_element(&arr, i) {
                    // JString::from 是 infallible 的(只是重新包装 JObject)
                    let jstr = JString::from(obj);
                    let v: String = match env.get_string(&jstr) {
                        Ok(v) => v.into(),
                        Err(e) => {
                            host::log_error(&format!("读取路径参数失败: {}", e));
                            continue;
                        }
                    };
                    list.push(v);
                }
            }
        }
        if target_port <= 0 || target_port > 65535 {
            return Err("目标端口非法".to_string());
        }
        host::send_files(
            0,
            &ip,
            target_port as u16,
            &device_id,
            &device_name,
            &list,
        )
    }));

    let msg = match result {
        Ok(Ok(())) => String::new(),
        Ok(Err(e)) => e,
        Err(_) => {
            host::log_error("发送文件时发生 panic, 已在 FFI 边界拦截");
            "发送时发生内部错误".to_string()
        }
    };
    new_jstring(&mut env, &msg)
}
