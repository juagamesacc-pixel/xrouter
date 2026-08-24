use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use notify::{Watcher, RecursiveMode, Event, EventKind};
use tracing::{info, warn};

use crate::{Config, load_from};

/// Watch config file for changes and invoke `on_reload` with new Config.
/// Runs in background tokio task; returns handle that keeps watcher alive.
/// Caller should hold the returned watcher handle.
pub struct ConfigWatcher {
    _watcher: Box<dyn Watcher + Send>,
    _handle: tokio::task::JoinHandle<()>,
}

impl ConfigWatcher {
    pub fn stop(self) {
        self._handle.abort();
    }
}

/// Spawn a file watcher that reloads config on change.
/// `config` is shared RwLock that will be updated in-place.
/// Debounces 200ms to avoid duplicate events from atomic rename.
pub fn watch_config_file(
    path: std::path::PathBuf,
    shared: Arc<RwLock<Config>>,
    balancer_update: impl Fn(&Config) + Send + Sync + 'static,
) -> anyhow::Result<ConfigWatcher> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(32);
    let watcher_path = path.clone();
    // notify watcher must live as long as the task
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(ev) = res {
            // only forward modify/create events for our file
            let _ = tx.blocking_send(ev);
        }
    })?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if parent.exists() {
        watcher.watch(parent, RecursiveMode::NonRecursive)?;
    } else {
        // watch file directly if parent doesn't exist yet (rare)
        watcher.watch(&path, RecursiveMode::NonRecursive)?;
    }

    let handle = tokio::spawn(async move {
        let mut debounce: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Some(event) => {
                            // filter to relevant path
                            let relevant = event.paths.iter().any(|p| p.ends_with("config.toml") || p == &watcher_path);
                            if !relevant {
                                // also handle parent dir events where file may be renamed
                                let is_modify = matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_));
                                if !is_modify { continue; }
                            }
                            debounce = Some(tokio::time::Instant::now() + Duration::from_millis(200));
                        }
                        None => break,
                    }
                }
                _ = async {
                    if let Some(d) = debounce {
                        tokio::time::sleep_until(d).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    debounce = None;
                    match load_from(&watcher_path) {
                        Ok(cfg) => {
                            balancer_update(&cfg);
                            *shared.write().unwrap() = cfg;
                            info!("config hot-reloaded from {}", watcher_path.display());
                        }
                        Err(e) => {
                            warn!("config reload failed: {}", e);
                        }
                    }
                }
            }
        }
    });

    Ok(ConfigWatcher { _watcher: Box::new(watcher), _handle: handle })
}
