//! JNI bridge methods for Android consumption.
//!
//! These are the entry points called from Kotlin via `System.loadLibrary`.
//! Each method maps 1:1 to a method in `NativeBridge.kt`.
//!
//! JNI type mapping:
//!   Java `boolean` -> Rust `jboolean` (u8), 0=false, non-zero=true
//!   Java `String`  -> Rust `jstring` (raw pointer, or null for null)
//!   Java `int`     -> Rust `jint` (i32)

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring};

use crate::config;
use crate::server;
use crate::state;

/// JNI: NativeBridge.init(configPath: String?) -> Boolean
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_init(
    mut env: JNIEnv,
    _class: JClass,
    config_path: JString,
) -> jboolean {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("xrouter=info,xrouter_ffi=info")
        .try_init();

    let path: Option<String> = if config_path.is_null() {
        None
    } else {
        match env.get_string(&config_path) {
            Ok(s) => {
                let s: String = s.into();
                if s.is_empty() { None } else { Some(s) }
            }
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", format!("Invalid config path: {}", e));
                return 0;
            }
        }
    };

    match config::load_or_create_config(path.as_deref()) {
        Ok(cfg) => {
            let server_cfg = config::load_server_config();
            match server::start_server(cfg, server_cfg.port) {
                Ok(addr) => {
                    tracing::info!("JNI: Server started on {}", addr);
                    1 // true
                }
                Err(e) => {
                    tracing::error!("JNI: Failed to start server: {}", e);
                    0 // false
                }
            }
        }
        Err(e) => {
            tracing::error!("JNI: Failed to load config: {}", e);
            0 // false
        }
    }
}

/// JNI: NativeBridge.getAddress() -> String?
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_getAddress(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let runtime = match state::get_runtime() {
        Some(r) => r,
        None => return std::ptr::null_mut(),
    };

    let info = match runtime.block_on(state::get_server_info()) {
        Some(info) => info,
        None => return std::ptr::null_mut(),
    };

    let addr = format!("{}:{}", info.host, info.port);
    match env.new_string(&addr) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// JNI: NativeBridge.isRunning() -> Boolean
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_isRunning(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    let runtime = match state::get_runtime() {
        Some(r) => r,
        None => return 0,
    };

    match runtime.block_on(state::is_running()) {
        true => 1,
        false => 0,
    }
}

/// JNI: NativeBridge.getConfigJson() -> String?
/// Get the current configuration as a JSON string.
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_getConfigJson(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let runtime = match state::get_runtime() {
        Some(r) => r,
        None => return std::ptr::null_mut(),
    };

    let result = runtime.block_on(async {
        if let Some(global_state) = state::get_global_state() {
            let guard = global_state.read().await;
            if let Some(state) = guard.as_ref() {
                let cfg = state.get_config();
                return config::config_to_json(&cfg);
            }
        }
        Err(anyhow::anyhow!("Server not initialized"))
    });

    match result {
        Ok(json) => match env.new_string(&json) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        Err(e) => {
            tracing::error!("JNI: Failed to get config: {}", e);
            std::ptr::null_mut()
        }
    }
}

/// JNI: NativeBridge.updateConfig(configJson: String) -> Boolean
/// Update configuration from a JSON string (hot reload).
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_updateConfig(
    mut env: JNIEnv,
    _class: JClass,
    config_json: JString,
) -> jboolean {
    if config_json.is_null() {
        return 0;
    }

    let json_str: String = match env.get_string(&config_json) {
        Ok(s) => s.into(),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", format!("Invalid config JSON: {}", e));
            return 0;
        }
    };

    let json_value: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("JNI: Failed to parse config JSON: {}", e);
            return 0;
        }
    };

    match config::save_config_json(&json_value) {
        Ok(()) => {
            match config::load_or_create_config(None) {
                Ok(cfg) => {
                    if let Err(e) = server::reload_config(cfg) {
                        tracing::error!("JNI: Failed to reload config: {}", e);
                        return 0;
                    }
                    1 // true
                }
                Err(e) => {
                    tracing::error!("JNI: Failed to reload config: {}", e);
                    0
                }
            }
        }
        Err(e) => {
            tracing::error!("JNI: Failed to save config: {}", e);
            0
        }
    }
}

/// JNI: NativeBridge.shutdown() -> Boolean
/// Shutdown the server and free resources.
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_shutdown(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    server::shutdown_server();
    1 // true
}

/// JNI: NativeBridge.getServerPort() -> Int
/// Get the configured server port from the persisted server config.
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_getServerPort(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    let server_cfg = config::load_server_config();
    server_cfg.port as jint
}

/// JNI: NativeBridge.getListenHost() -> String
/// Get the configured listen host.
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_getListenHost(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let host = if let Some(runtime) = state::get_runtime() {
        if let Some(info) = runtime.block_on(state::get_server_info()) {
            info.host
        } else {
            config::load_server_config().host
        }
    } else {
        config::load_server_config().host
    };

    match env.new_string(&host) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// JNI: NativeBridge.getServerUrl() -> String?
/// Get the full server URL (e.g., "http://127.0.0.1:3001") for other apps to use.
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_getServerUrl(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let runtime = match state::get_runtime() {
        Some(r) => r,
        None => return std::ptr::null_mut(),
    };

    let info = match runtime.block_on(state::get_server_info()) {
        Some(info) => info,
        None => return std::ptr::null_mut(),
    };

    let url = format!("http://{}:{}", info.host, info.port);
    match env.new_string(&url) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// JNI: NativeBridge.getDeviceIp() -> String
/// Detect the device's actual IP address on the primary network interface.
/// Useful for other apps on the device to know how to reach the server
/// when it's bound to 0.0.0.0.
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_getDeviceIp(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    // UDP "connect" to a public address determines which local interface would be used.
    // No actual packet is sent — this is just routing table lookup.
    let ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            Ok(s.local_addr()?.ip().to_string())
        })
        .unwrap_or_else(|_| "127.0.0.1".to_string());

    match env.new_string(&ip) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// JNI: NativeBridge.restartServer() -> Boolean
/// Shutdown and reinitialize the server.
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_restartServer(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    server::shutdown_server();

    match config::load_or_create_config(None) {
        Ok(cfg) => {
            let server_cfg = config::load_server_config();
            match server::start_server(cfg, server_cfg.port) {
                Ok(addr) => {
                    tracing::info!("JNI: Server restarted on {}", addr);
                    1 // true
                }
                Err(e) => {
                    tracing::error!("JNI: Failed to restart server: {}", e);
                    0 // false
                }
            }
        }
        Err(e) => {
            tracing::error!("JNI: Failed to load config for restart: {}", e);
            0 // false
        }
    }
}
