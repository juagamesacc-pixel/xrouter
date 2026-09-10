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

    // JString.is_null() returns true when Java passes null.
    let path: Option<String> = if config_path.is_null() {
        None
    } else {
        match env.get_string(&config_path) {
            Ok(s) => Some(s.into()),
            Err(_) => None,
        }
    };
    // Normalize empty string to None
    let path = path.filter(|s| !s.is_empty());

    match config::load_or_create_config(path.as_deref()) {
        Ok(cfg) => {
            let server_cfg = config::load_server_config();
            match server::start_server(cfg, server_cfg.port) {
                Ok(addr) => {
                    tracing::info!("JNI: Server started on {}", addr);
                    1 // true
                }
                Err(e) => {
                    let msg = format!("Server start failed: {}", e);
                    tracing::error!("JNI: {}", msg);
                    server::set_last_error_for_ui(&msg);
                    0 // false
                }
            }
        }
        Err(e) => {
            let msg = format!("Config load failed: {}", e);
            tracing::error!("JNI: {}", msg);
            server::set_last_error_for_ui(&msg);
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
    let info = match state::get_server_info_sync() {
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
/// Returns true only if the server state is initialized AND the TCP port is bound.
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_isRunning(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    if server::is_running() { 1 } else { 0 }
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
    let host = match state::get_server_info_sync() {
        Some(info) => info.host,
        None => config::load_server_config().host,
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
    let info = match state::get_server_info_sync() {
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
                    let msg = format!("Server restart failed: {}", e);
                    tracing::error!("JNI: {}", msg);
                    server::set_last_error_for_ui(&msg);
                    0 // false
                }
            }
        }
        Err(e) => {
            let msg = format!("Config reload failed: {}", e);
            tracing::error!("JNI: {}", msg);
            server::set_last_error_for_ui(&msg);
            0 // false
        }
    }
}

/// JNI: NativeBridge.saveServerConfig(host: String, port: Int) -> Boolean
/// Save the server binding configuration (host and port).
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_saveServerConfig(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
) -> jboolean {
    let host_str: String = if host.is_null() {
        "127.0.0.1".to_string()
    } else {
        match env.get_string(&host) {
            Ok(s) => s.into(),
            Err(_) => "127.0.0.1".to_string(),
        }
    };

    let cfg = config::ServerConfig {
        host: host_str,
        port: port as u16,
    };

    match config::save_server_config(&cfg) {
        Ok(()) => {
            tracing::info!("JNI: Server config saved: {}:{}", cfg.host, cfg.port);
            1
        }
        Err(e) => {
            tracing::error!("JNI: Failed to save server config: {}", e);
            0
        }
    }
}

/// JNI: NativeBridge.loadServerConfig() -> String?
/// Load the server binding config as JSON string {"host":"...","port":...}.
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_loadServerConfig(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let cfg = config::load_server_config();
    let json = match serde_json::to_string(&cfg) {
        Ok(j) => j,
        Err(_) => return std::ptr::null_mut(),
    };
    match env.new_string(&json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// JNI: NativeBridge.getLastErrorMessage() -> String?
/// Returns the last error message from server startup or operation failures.
/// The error is cleared after being read (consume-once pattern).
#[no_mangle]
pub extern "system" fn Java_com_xrouter_app_NativeBridge_getLastErrorMessage(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    match server::get_last_error() {
        Some(msg) => match env.new_string(&msg) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}
