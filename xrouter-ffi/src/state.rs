//! Global state management for the FFI boundary

use once_cell::sync::OnceCell;
use std::sync::Arc;
use tokio::sync::RwLock;
use xrouter_server::AppState;

/// Global server state, initialized once via `xrouter_init`
static GLOBAL_STATE: OnceCell<Arc<RwLock<Option<AppState>>>> = OnceCell::new();

/// Global tokio runtime handle for spawning async tasks from FFI
static GLOBAL_RUNTIME: OnceCell<tokio::runtime::Runtime> = OnceCell::new();

/// Server configuration summary (ported to FFI boundary)
#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub host: String,
    pub port: u16,
}

static SERVER_INFO: OnceCell<RwLock<Option<ServerInfo>>> = OnceCell::new();

pub fn init_global_state() -> Arc<RwLock<Option<AppState>>> {
    GLOBAL_STATE.get_or_init(|| Arc::new(RwLock::new(None))).clone()
}

pub fn get_global_state() -> Option<Arc<RwLock<Option<AppState>>>> {
    GLOBAL_STATE.get().cloned()
}

pub fn init_runtime() -> &'static tokio::runtime::Runtime {
    GLOBAL_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("xrouter-ffi")
            .build()
            .expect("Failed to create tokio runtime")
    })
}

pub fn get_runtime() -> Option<&'static tokio::runtime::Runtime> {
    GLOBAL_RUNTIME.get()
}

pub fn set_server_info(info: ServerInfo) {
    let cell = SERVER_INFO.get_or_init(|| RwLock::new(None));
    if let Ok(mut guard) = cell.try_write() {
        *guard = Some(info);
    }
}

pub async fn get_server_info() -> Option<ServerInfo> {
    SERVER_INFO.get()?.read().await.clone()
}

/// Synchronous version — try_read without blocking.
/// Returns None if the lock is currently held (shouldn't happen in practice).
pub fn get_server_info_sync() -> Option<ServerInfo> {
    let cell = SERVER_INFO.get()?;
    cell.try_read().ok().and_then(|guard| guard.clone())
}