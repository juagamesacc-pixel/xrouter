//! C FFI exports for Android JNI consumption
//!
//! All functions follow the pattern:
//!   - Accept C-compatible types (pointers, integers)
//!   - Return C-compatible results (0 = success, non-zero = error)
//!   - Use #[no_mangle] for symbol visibility
//!   - Use `extern "C"` for C calling convention

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use crate::config;
use crate::server;
use crate::state;

/// Initialize xrouter with a config file path.
///
/// # Arguments
/// * `config_path` - Path to the TOML config file (null-terminated C string).
///                   Pass null to use default path.
///
/// # Returns
/// * 0 on success
/// * -1 on error (check logcat for details)
///
/// # Safety
/// `config_path` must be a valid null-terminated UTF-8 C string or null.
#[no_mangle]
pub unsafe extern "C" fn xrouter_init(config_path: *const c_char) -> i32 {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("xrouter=info,xrouter_ffi=info")
        .try_init();

    let path = if config_path.is_null() {
        None
    } else {
        match CStr::from_ptr(config_path).to_str() {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!("Invalid config path: {}", e);
                return -1;
            }
        }
    };

    match config::load_or_create_config(path) {
        Ok(cfg) => {
            let server_cfg = config::load_server_config();
            match server::start_server(cfg, server_cfg.port) {
                Ok(addr) => {
                    tracing::info!("Server started on {}", addr);
                    0
                }
                Err(e) => {
                    let msg = format!("Server start failed: {}", e);
                    tracing::error!("{}", msg);
                    server::set_last_error_for_ui(&msg);
                    -1
                }
            }
        }
        Err(e) => {
            let msg = format!("Config load failed: {}", e);
            tracing::error!("{}", msg);
            server::set_last_error_for_ui(&msg);
            -1
        }
    }
}

/// Get the server's bound address as a C string.
///
/// # Returns
/// * Pointer to null-terminated C string (e.g., "127.0.0.1:3001")
/// * Null if server info is not set
///
/// # Safety
/// Caller must free the returned string with `xrouter_free_string`.
#[no_mangle]
pub unsafe extern "C" fn xrouter_get_address() -> *mut c_char {
    let info = match state::get_server_info_sync() {
        Some(info) => info,
        None => return ptr::null_mut(),
    };

    let addr = format!("{}:{}", info.host, info.port);
    match CString::new(addr) {
        Ok(s) => s.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

/// Check if the server is currently running.
///
/// # Returns
/// * 1 if running (state initialized AND listening)
/// * 0 if not running
#[no_mangle]
pub extern "C" fn xrouter_is_running() -> i32 {
    if server::is_running() { 1 } else { 0 }
}

/// Get the current configuration as a JSON C string.
///
/// # Returns
/// * Pointer to null-terminated JSON C string
/// * Null on error
///
/// # Safety
/// Caller must free the returned string with `xrouter_free_string`.
#[no_mangle]
pub unsafe extern "C" fn xrouter_get_config_json() -> *mut c_char {
    let runtime = match state::get_runtime() {
        Some(r) => r,
        None => return ptr::null_mut(),
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
        Ok(json) => CString::new(json).map(|s| s.into_raw()).unwrap_or(ptr::null_mut()),
        Err(e) => {
            tracing::error!("Failed to get config: {}", e);
            ptr::null_mut()
        }
    }
}

/// Update the configuration (hot reload).
///
/// # Arguments
/// * `config_json` - New configuration as JSON C string
///
/// # Returns
/// * 0 on success
/// * -1 on error
///
/// # Safety
/// `config_json` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn xrouter_update_config(config_json: *const c_char) -> i32 {
    if config_json.is_null() {
        return -1;
    }

    let json_str = match CStr::from_ptr(config_json).to_str() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Invalid config JSON: {}", e);
            return -1;
        }
    };

    // Parse JSON and convert to TOML config
    let json_value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to parse config JSON: {}", e);
            return -1;
        }
    };

    // For now, write the JSON as-is and let the config loader handle it
    // In a full implementation, we'd convert JSON -> TOML properly
    match config::save_config_json(&json_value) {
        Ok(()) => {
            // Reload config
            match config::load_or_create_config(None) {
                Ok(cfg) => {
                    if let Err(e) = server::reload_config(cfg) {
                        tracing::error!("Failed to reload config: {}", e);
                        return -1;
                    }
                    0
                }
                Err(e) => {
                    tracing::error!("Failed to reload config: {}", e);
                    -1
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to save config: {}", e);
            -1
        }
    }
}

/// Shutdown the server and free resources.
///
/// # Returns
/// * 0 on success
/// * -1 on error
#[no_mangle]
pub extern "C" fn xrouter_shutdown() -> i32 {
    server::shutdown_server();
    0
}

/// Get the server URL (e.g., "http://127.0.0.1:3001") for other apps to use.
///
/// # Returns
/// * Pointer to null-terminated C string with full URL
/// * Null if server info is not set
///
/// # Safety
/// Caller must free the returned string with `xrouter_free_string`.
#[no_mangle]
pub unsafe extern "C" fn xrouter_get_url() -> *mut c_char {
    let info = match state::get_server_info_sync() {
        Some(info) => info,
        None => return ptr::null_mut(),
    };

    let url = format!("http://{}:{}", info.host, info.port);
    match CString::new(url) {
        Ok(s) => s.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

/// Get the last error message from the server.
///
/// # Returns
/// * Pointer to null-terminated C string with error message
/// * Null if no error
///
/// # Safety
/// Caller must free the returned string with `xrouter_free_string`.
#[no_mangle]
pub unsafe extern "C" fn xrouter_get_last_error() -> *mut c_char {
    match server::get_last_error() {
        Some(msg) => CString::new(msg).map(|s| s.into_raw()).unwrap_or(ptr::null_mut()),
        None => ptr::null_mut(),
    }
}

/// Free a string allocated by xrouter.
///
/// # Safety
/// `s` must be a pointer previously returned by an xrouter function.
#[no_mangle]
pub unsafe extern "C" fn xrouter_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}