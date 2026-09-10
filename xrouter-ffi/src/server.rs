//! Server lifecycle management for FFI

use anyhow::Result;
use xrouter_config::Config;
use xrouter_server::AppState;
use tracing::info;

use crate::config;
use crate::state::{self, ServerInfo};

/// Initialize and start the xrouter server on a background tokio runtime.
/// Returns the bound address (host:port) on success.
pub fn start_server(config: Config, port: u16) -> Result<String> {
    let runtime = state::init_runtime();
    let server_cfg = config::load_server_config();
    let host = server_cfg.host;
    let addr = format!("{}:{}", host, port);

    // Create AppState from config
    let app_state = AppState::new(config);

    // Store state globally
    let global_state = state::init_global_state();
    {
        let mut guard = runtime.block_on(async { global_state.write().await });
        *guard = Some(app_state.clone());
    }

    // Spawn the server on the background runtime
    let server_addr = addr.clone();
    runtime.spawn(async move {
        match run_server_inner(server_addr, app_state).await {
            Ok(()) => info!("Server stopped gracefully"),
            Err(e) => tracing::error!("Server error: {}", e),
        }
    });

    // Store server info
    state::set_server_info(ServerInfo {
        host: host.clone(),
        port,
    });

    info!("xrouter server started on {}", addr);
    Ok(addr)
}

/// Initialize with persisted server config
pub fn start_server_with_persisted_config(config: Config) -> Result<String> {
    let server_cfg = config::load_server_config();
    start_server(config, server_cfg.port)
}

/// Inner server run function (async) — uses xrouter_server::run_server
async fn run_server_inner(addr: String, state: AppState) -> Result<()> {
    state.refresh_model_cache().await;
    xrouter_server::run_server(addr, state).await
}

/// Shutdown the server and clean up resources
pub fn shutdown_server() {
    if let Some(runtime) = state::get_runtime() {
        runtime.block_on(async {
            if let Some(global_state) = state::get_global_state() {
                let mut guard = global_state.write().await;
                *guard = None;
            }
        });
    }
    info!("xrouter server shut down");
}

/// Check if the server is running
pub async fn is_running() -> bool {
    if let Some(global_state) = state::get_global_state() {
        let guard = global_state.read().await;
        guard.is_some()
    } else {
        false
    }
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