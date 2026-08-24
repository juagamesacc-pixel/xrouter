//! Per-request tracker with off-RAM, extremely-compressed persistence.
//!
//! When `--track` is NOT enabled on `xrouter serve`, `Tracker::new(false)`
//! produces a tracker that does nothing: no file is opened, no buffer is
//! allocated on the hot path, and every `record` call early-returns. This
//! keeps tracking strictly zero-overhead unless explicitly requested.
//!
//! When enabled, entries are buffered in a bounded in-memory ring
//! (`VecDeque`) and flushed to `~/.local/share/xrouter/track.bin` either
//! every 1 second or as soon as 100 entries accumulate. Each flush writes a
//! length-prefixed frame: `[u32 LE compressed_len][zstd(level=3) of
//! bincode(Vec<TrackEntry>)]`. This keeps the on-disk footprint tiny while
//! remaining append-only and trivially readable.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A single tracked request. `error_code` is stored as a compact `u16` code
/// (never the raw string) so the on-disk representation stays small and
/// stable; the human string is derived on read via [`error_string_for_code`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackEntry {
    pub error_code: u16,
    pub timestamp: u64, // unix millis
    pub provider: String,
    pub model: String,
    pub tier: String,
    pub latency_ms: u64,
    pub is_success: bool,
}

/// Map an HTTP upstream status to the compact stored error code.
pub fn error_code_for_status(status: u16) -> u16 {
    match status {
        200..=299 => 200,
        400 => 400,
        401 => 401,
        408 => 408,
        429 => 429,
        500 => 500,
        502 => 502,
        503 => 503,
        504 => 408, // treat gateway timeout as "timeout"
        _ => 500,
    }
}

/// Reverse mapping: stored code -> HTTP status (kept for symmetry / future use).
pub fn status_for_code(code: u16) -> u16 {
    match code {
        200 => 200,
        400 => 400,
        401 => 401,
        408 => 408,
        429 => 429,
        500 => 500,
        502 => 502,
        503 => 503,
        _ => 500,
    }
}

/// Stable, human-readable string for a stored error code.
pub fn error_string_for_code(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "bad_request",
        401 => "auth",
        404 => "unknown_tier",
        408 => "timeout",
        429 => "quota_exceeded",
        500 => "internal_error",
        502 => "bad_gateway",
        503 => "tier_exhausted",
        _ => "unknown",
    }
}

/// All known codes -> string, for the `/admin/track/codes` map.
pub fn code_map() -> Vec<(u16, &'static str)> {
    vec![
        (200, "OK"),
        (400, "bad_request"),
        (401, "auth"),
        (404, "unknown_tier"),
        (408, "timeout"),
        (429, "quota_exceeded"),
        (500, "internal_error"),
        (502, "bad_gateway"),
        (503, "tier_exhausted"),
    ]
}

fn track_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/xrouter/track.bin")
}

struct Inner {
    enabled: bool,
    buffer: Mutex<VecDeque<TrackEntry>>,
    path: Option<PathBuf>,
}

/// Cloneable handle to the tracker. Cheap to clone (wraps an `Arc`).
#[derive(Clone)]
pub struct Tracker {
    inner: Arc<Inner>,
}

impl Tracker {
    pub fn new(enabled: bool) -> Self {
        let inner = Arc::new(Inner {
            enabled,
            buffer: Mutex::new(VecDeque::new()),
            path: if enabled { Some(track_path()) } else { None },
        });
        let t = Self { inner };
        if enabled {
            if let Some(p) = &t.inner.path {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
            t.spawn_flush_loop();
        }
        t
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.enabled
    }

    /// Record a single request. Early-returns (zero cost) when tracking is
    /// disabled. Flushes immediately once the in-memory buffer hits 100.
    pub fn record(&self, entry: TrackEntry) {
        if !self.inner.enabled {
            return;
        }
        let mut buf = self.inner.buffer.lock().unwrap();
        buf.push_back(entry);
        if buf.len() >= 100 {
            let entries: Vec<TrackEntry> = buf.drain(..).collect();
            drop(buf);
            self.write_entries(&entries);
        }
    }

    fn write_entries(&self, entries: &[TrackEntry]) {
        let path = match &self.inner.path {
            Some(p) => p,
            None => return,
        };
        let bytes = match bincode::serialize(entries) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("track bincode error: {}", e);
                return;
            }
        };
        let compressed = match zstd::stream::encode_all(&bytes[..], 3) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("track zstd error: {}", e);
                return;
            }
        };
        let len = compressed.len() as u32;
        let mut frame = Vec::with_capacity(4 + compressed.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&compressed);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            if let Err(e) = std::io::Write::write_all(&mut f, &frame) {
                tracing::warn!("track write error: {}", e);
            }
        }
    }

    /// Flush buffered entries to disk. Safe to call when empty (no-op).
    pub fn flush(&self) {
        if !self.inner.enabled {
            return;
        }
        let entries: Vec<TrackEntry> = {
            let mut buf = self.inner.buffer.lock().unwrap();
            if buf.is_empty() {
                return;
            }
            buf.drain(..).collect()
        };
        if !entries.is_empty() {
            self.write_entries(&entries);
        }
    }

    fn spawn_flush_loop(&self) {
        let t = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                t.flush();
            }
        });
    }

    /// Read all persisted entries plus any still buffered (newest last).
    pub fn read_all(&self) -> Vec<TrackEntry> {
        let mut out = Vec::new();
        if let Some(p) = &self.inner.path {
            if let Ok(data) = std::fs::read(p) {
                let mut pos = 0;
                while pos + 4 <= data.len() {
                    let len = u32::from_le_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]) as usize;
                    pos += 4;
                    if pos + len > data.len() {
                        break;
                    }
                    let compressed = &data[pos..pos + len];
                    pos += len;
                    if let Ok(decompressed) = zstd::stream::decode_all(compressed) {
                        if let Ok(mut entries) =
                            bincode::deserialize::<Vec<TrackEntry>>(&decompressed)
                        {
                            out.append(&mut entries);
                        }
                    }
                }
            }
        }
        if let Ok(buf) = self.inner.buffer.lock() {
            for e in buf.iter() {
                out.push(e.clone());
            }
        }
        out
    }
}
