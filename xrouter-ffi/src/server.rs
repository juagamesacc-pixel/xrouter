//! Server lifecycle management for FFI
//!
//! Uses SO_LINGER(0) on the TCP socket so the port is released immediately
//! when the server shuts down — critical on Android where TIME_WAIT can
//! hold the port for 60+ seconds.

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use xrouter_config::Config;
use xrouter_server::AppState;
use tracing::info;

use crate::config;
use crate::state::{self, ServerInfo};

/// Atomic flag: true only after the TCP port is actually bound and listening.
static SERVER_LISTENING: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn set_last_error(msg: &str) {
    if let Ok(mut guard) = LAST_ERROR.lock() {
        *guard = Some(msg.to_string());
    }
}

/// Initialize and start the xrouter server on a background tokio runtime.
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

    // Create shutdown channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    state::set_shutdown_sender(shutdown_tx);

    // Spawn the server on the background runtime
    let server_addr = addr.clone();
    runtime.spawn(async move {
        match run_server_inner(server_addr.clone(), app_state, shutdown_rx).await {
            Ok(()) => info!("Server stopped gracefully"),
            Err(e) => {
                let msg = format!("Server error: {}", e);
                tracing::error!("{}", msg);
                set_last_error(&msg);
            }
        }
        SERVER_LISTENING.store(false, Ordering::SeqCst);
    });

    info!("xrouter server spawned on {}", addr);
    Ok(addr)
}

/// Inner server run — creates a TCP listener with SO_LINGER(0) so the port
/// is released immediately on drop. Watches for shutdown via oneshot receiver.
async fn run_server_inner(
    addr: String,
    state: AppState,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    state.refresh_model_cache().await;

    // Use socket2 to set SO_LINGER(0) before binding.
    // This forces an immediate TCP RST on close instead of TIME_WAIT,
    // releasing the port instantly — essential on Android.
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::SocketAddr;

    let socket_addr: SocketAddr = addr.parse()?;
    let domain = if socket_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };

    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_linger(Some(std::time::Duration::from_secs(0)))?;
    socket.bind(&socket_addr.into())?;
    socket.listen(128)?;

    let std_listener: std::net::TcpListener = socket.into();
    std_listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(std_listener)?;

    SERVER_LISTENING.store(true, Ordering::SeqCst);
    info!("Listening on {}", addr);

    let app = xrouter_server::create_router(state);

    let shutdown_fut = async {
        let _ = shutdown_rx.await;
    };

    tokio::select! {
        result = axum::serve(listener, app) => {
            result?;
        }
        _ = shutdown_fut => {
            info!("Shutdown signal received, server task exiting");
        }
    }

    Ok(())
}

/// Shutdown the server and release the TCP port.
pub fn shutdown_server() {
    SERVER_LISTENING.store(false, Ordering::SeqCst);

    // Signal the server task to exit (drops the TCP listener, releases the port)
    state::signal_shutdown();

    // Drop the global state so no new requests can arrive
    if let Some(global_state) = state::get_global_state() {
        if let Ok(mut guard) = global_state.try_write() {
            *guard = None;
        }
    }

    state::clear_server_info();

    // Block briefly to let the server task process the shutdown signal.
    // With SO_LINGER(0), the port is released instantly when the listener drops.
    if state::get_runtime().is_some() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let _ = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = done_tx.send(());
        });
        let _ = done_rx.recv_timeout(std::time::Duration::from_millis(300));
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