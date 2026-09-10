/// Global state management for the FFI boundary

use once_cell::sync::OnceCell;
use std::sync::Arc;
use tokio::sync::RwLock;
use xrouter_server::AppState;

/// Global server state, initialized once via `xrouter_init`
static GLOBAL_STATE: OnceCell<Arc<RwLock<Option<AppState>>>> = OnceCell::new();

/// Global tokio runtime handle for spawning async tasks from FFI
static GLOBAL_RUNTIME: OnceCell<tokio::runtime::Runtime> = OnceCell::new();

/// Shutdown signal sender — wrapped in Mutex so it can be replaced on restart.
static SHUTDOWN_TX: OnceCell<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>> = OnceCell::new();

/// Server configuration summary (ported to FFI boundary)
#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub host: String,
    pub port: u16,
}

static SERVER_INFO: OnceCell<std::sync::Mutex<Option<ServerInfo>>> = OnceCell::new();

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

/// Store the shutdown sender so we can signal the server task to exit.
/// Can be called multiple times (replaces the previous sender on restart).
pub fn set_shutdown_sender(tx: tokio::sync::oneshot::Sender<()>) {
    let cell = SHUTDOWN_TX.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        *guard = Some(tx);
    }
}

/// Signal the server task to stop by sending on the oneshot channel.
/// After this, the server task will exit, dropping the TCP listener and releasing the port.
pub fn signal_shutdown() {
    if let Some(cell) = SHUTDOWN_TX.get() {
        if let Ok(mut guard) = cell.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(());
            }
        }
    }
}

pub fn set_server_info(info: ServerInfo) {
    let cell = SERVER_INFO.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        *guard = Some(info);
    }
}

pub async fn get_server_info() -> Option<ServerInfo> {
    SERVER_INFO.get()?.lock().ok()?.clone()
}

/// Synchronous version — try_lock without blocking.
pub fn get_server_info_sync() -> Option<ServerInfo> {
    let cell = SERVER_INFO.get()?;
    cell.lock().ok().and_then(|guard| guard.clone())
}

/// Clear server info (called during shutdown)
pub fn clear_server_info() {
    if let Some(cell) = SERVER_INFO.get() {
        if let Ok(mut guard) = cell.lock() {
            *guard = None;
        }
    }
}