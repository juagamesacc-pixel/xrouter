//! Server lifecycle management for FFI

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use xrouter_config::Config;
use xrouter_server::AppState;
use tracing::info;

use crate::config;
use crate::state::{self, ServerInfo};

/// Atomic flag: true only after the TCP port is actually bound and listening.
static SERVER_LISTENING: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Set the last error message for diagnostic retrieval from JNI/JS
fn set_last_error(msg: &str) {
    if let Ok(mut guard) = LAST_ERROR.lock() {
        *guard = Some(msg.to_string());
    }
}

/// Initialize and start the xrouter server on a background tokio runtime.
/// Returns the bound address (host:port) on success.
pub fn start_server(config: Config, port: u16) -> Result<String> {
    let runtime = state::init_runtime();
    let server_cfg = config::load_server_config();
    let host = server_cfg.host;
    let addr = format!("{}:{}", host, port);

    // Create AppState from config
    let app_state = AppState::new(config);

    // Store state globally (for reload access, etc.)
    let global_state = state::init_global_state();
    {
        let mut guard = runtime.block_on(async { global_state.write().await });
        *guard = Some(app_state.clone());
    }

    // Store server info BEFORE spawning (needed by getAddress, etc.)
    state::set_server_info(ServerInfo {
        host: host.clone(),
        port,
    });

    // Create shutdown channel — the server task watches this for exit signal
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    state::set_shutdown_sender(shutdown_tx);

    // Spawn the server on the background runtime
    let server_addr = addr.clone();
    runtime.spawn(async move {
        match run_server_inner(server_addr.clone(), app_state, shutdown_rx).await {
            Ok(()) => {
                info!("Server stopped gracefully");
            }
            Err(e) => {
                let msg = format!("Server error: {}", e);
                tracing::error!("{}", msg);
                set_last_error(&msg);
            }
        }
        // Mark as not listening when server task ends (port unbound)
        SERVER_LISTENING.store(false, Ordering::SeqCst);
    });

    info!("xrouter server spawned on {}", addr);
    Ok(addr)
}

/// Initialize with persisted server config
pub fn start_server_with_persisted_config(config: Config) -> Result<String> {
    let server_cfg = config::load_server_config();
    start_server(config, server_cfg.port)
}

/// Inner server run function (async) — uses xrouter_server::run_server.
/// Watches for shutdown signal via oneshot receiver.
async fn run_server_inner(
    addr: String,
    state: AppState,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    state.refresh_model_cache().await;

    // Wrap the receiver so it implements Future (oneshot::Receiver doesn't by itself
    // in some tokio versions — convert to a futures-compatible future)
    let shutdown_fut = async {
        let _ = shutdown_rx.await;
    };

    // Mark as listening right before bind
    SERVER_LISTENING.store(true, Ordering::SeqCst);

    // Run the server and simultaneously watch for shutdown signal.
    // When shutdown_fut completes, select drops the server future, which drops
    // the TCP listener and releases the port immediately.
    tokio::select! {
        result = xrouter_server::run_server(addr, state) => {
            result?;
        }
        _ = shutdown_fut => {
            info!("Shutdown signal received, server task exiting");
        }
    }

    Ok(())
}

/// Shutdown the server and release the TCP port.
/// Sends the shutdown signal to the server task, which exits and drops the listener.
pub fn shutdown_server() {
    SERVER_LISTENING.store(false, Ordering::SeqCst);

    // Signal the server task to exit (drops the TCP listener, releases the port)
    state::signal_shutdown();

    // Drop the global state (AppState) so no new requests can arrive.
    if let Some(global_state) = state::get_global_state() {
        if let Ok(mut guard) = global_state.try_write() {
            *guard = None;
        }
    }

    // Clear server info so getAddress/etc return None.
    state::clear_server_info();

    // Block briefly on the runtime to let the server task actually process the
    // shutdown signal and drop the TCP listener. This is critical on Android
    // where the server task runs on a background tokio runtime.
    if let Some(runtime) = state::get_runtime() {
        // Spawn a blocking task that waits for the server task to finish
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let _ = std::thread::spawn(move || {
            // Give the server task up to 500ms to exit gracefully
            std::thread::sleep(std::time::Duration::from_millis(500));
            let _ = done_tx.send(());
        });
        // Block the calling thread until either the wait completes or timeout
        let _ = done_rx.recv_timeout(std::time::Duration::from_millis(600));
    }

    info!("xrouter server shut down (port released)");
}

/// Check if the server is actually listening (TCP port bound)
pub fn is_server_listening() -> bool {
    SERVER_LISTENING.load(Ordering::SeqCst)
}

/// Check if the server state is initialized (config loaded, app state exists)
pub fn is_initialized() -> bool {
    if let Some(global_state) = state::get_global_state() {
        if let Some(runtime) = state::get_runtime() {
            let guard = runtime.block_on(async { global_state.read().await });
            guard.is_some()
        } else {
            false
        }
    } else {
        false
    }
}

/// Check if the server is running (both state initialized AND listening)
pub fn is_running() -> bool {
    is_initialized() && is_server_listening()
}

/// Get the last error message, if any
pub fn get_last_error() -> Option<String> {
    LAST_ERROR.lock().ok().and_then(|guard| guard.clone())
}

/// Set the last error message for UI consumption (public for JNI bridge)
pub fn set_last_error_for_ui(msg: &str) {
    set_last_error(msg);
}

/// Reload config (hot reload)
pub fn reload_config(_new_config: Config) -> Result<()> {
    if let Some(runtime) = state::get_runtime() {
        runtime.block_on(async {
            if let Some(global_state) = state::get_global_state() {
                let guard = global_state.read().await;
                if let Some(state) = guard.as_ref() {
                    state.reload()?;
                }
            }
            Ok(())
        })
    } else {
        Err(anyhow::anyhow!("Server not initialized"))
    }
}