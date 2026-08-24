use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(feature = "bench")]
use std::sync::Mutex;
#[cfg(feature = "bench")]
use std::time::Duration;

#[derive(Debug)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub requests_success: AtomicU64,
    pub requests_error: AtomicU64,
    pub tier_exhausted: AtomicU64,
    pub retries_total: AtomicU64,
    pub upstream_timeouts: AtomicU64,

    // --- bench-only instrumentation (compiled out unless `bench` feature is on) ---
    #[cfg(feature = "bench")]
    pub latency_hist: Arc<Mutex<hdrhistogram::Histogram<u64>>>,
    #[cfg(feature = "bench")]
    pub bench_requests: AtomicU64,
    #[cfg(feature = "bench")]
    pub bench_bytes: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            requests_success: AtomicU64::new(0),
            requests_error: AtomicU64::new(0),
            tier_exhausted: AtomicU64::new(0),
            retries_total: AtomicU64::new(0),
            upstream_timeouts: AtomicU64::new(0),
            #[cfg(feature = "bench")]
            latency_hist: Arc::new(Mutex::new(
                // Auto-resizing histogram, 3 significant figures. new(3) is
                // infallible for a valid sigfig, so expect is unreachable here.
                hdrhistogram::Histogram::<u64>::new(3).expect("create latency histogram"),
            )),
            #[cfg(feature = "bench")]
            bench_requests: AtomicU64::new(0),
            #[cfg(feature = "bench")]
            bench_bytes: AtomicU64::new(0),
        }
    }
}

impl Metrics {
    pub fn inc_requests(&self) { self.requests_total.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_success(&self) { self.requests_success.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_error(&self) { self.requests_error.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_tier_exhausted(&self) { self.tier_exhausted.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_retries(&self) { self.retries_total.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_timeouts(&self) { self.upstream_timeouts.fetch_add(1, Ordering::Relaxed); }

    pub fn to_prometheus(&self) -> String {
        format!(
            "# HELP xrouter_requests_total Total requests\n# TYPE xrouter_requests_total counter\nxrouter_requests_total {}\n# HELP xrouter_requests_success Successful\n# TYPE xrouter_requests_success counter\nxrouter_requests_success {}\n# HELP xrouter_requests_error Errors\n# TYPE xrouter_requests_error counter\nxrouter_requests_error {}\n# HELP xrouter_tier_exhausted Tier exhausted\n# TYPE xrouter_tier_exhausted counter\nxrouter_tier_exhausted {}\n# HELP xrouter_retries_total Retries\n# TYPE xrouter_retries_total counter\nxrouter_retries_total {}\n# HELP xrouter_upstream_timeouts Timeouts\n# TYPE xrouter_upstream_timeouts counter\nxrouter_upstream_timeouts {}\n",
            self.requests_total.load(Ordering::Relaxed),
            self.requests_success.load(Ordering::Relaxed),
            self.requests_error.load(Ordering::Relaxed),
            self.tier_exhausted.load(Ordering::Relaxed),
            self.retries_total.load(Ordering::Relaxed),
            self.upstream_timeouts.load(Ordering::Relaxed),
        )
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "requests_total": self.requests_total.load(Ordering::Relaxed),
            "requests_success": self.requests_success.load(Ordering::Relaxed),
            "requests_error": self.requests_error.load(Ordering::Relaxed),
            "tier_exhausted": self.tier_exhausted.load(Ordering::Relaxed),
            "retries_total": self.retries_total.load(Ordering::Relaxed),
            "upstream_timeouts": self.upstream_timeouts.load(Ordering::Relaxed),
        })
    }

    // --- bench-only API -------------------------------------------------------

    /// Record a single request latency. Stored in microseconds inside the
    /// histogram; also bumps the bench request counter.
    #[cfg(feature = "bench")]
    pub fn record_latency(&self, d: Duration) {
        self.bench_requests.fetch_add(1, Ordering::Relaxed);
        let micros = d.as_micros() as u64;
        if let Ok(mut hist) = self.latency_hist.lock() {
            // record() only fails if the value is out of bounds; the histogram
            // is auto-resizing so this is effectively unreachable.
            let _ = hist.record(micros);
        }
    }

    /// Accumulate observed payload bytes (request body size) for throughput.
    #[cfg(feature = "bench")]
    pub fn record_bytes(&self, n: u64) {
        self.bench_bytes.fetch_add(n, Ordering::Relaxed);
    }

    /// Produce a JSON snapshot with latency percentiles, memory stats, and
    /// throughput counters. Safe against mutex poisoning (recovers inner).
    #[cfg(feature = "bench")]
    pub fn snapshot_bench(&self) -> serde_json::Value {
        let hist = match self.latency_hist.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        let latency = if hist.len() == 0 {
            serde_json::json!({
                "p50": 0.0, "p90": 0.0, "p95": 0.0, "p99": 0.0,
                "min": 0.0, "max": 0.0, "mean": 0.0, "samples": 0,
            })
        } else {
            serde_json::json!({
                "p50": hist.value_at_quantile(0.50) as f64 / 1000.0,
                "p90": hist.value_at_quantile(0.90) as f64 / 1000.0,
                "p95": hist.value_at_quantile(0.95) as f64 / 1000.0,
                "p99": hist.value_at_quantile(0.99) as f64 / 1000.0,
                "min": hist.min() as f64 / 1000.0,
                "max": hist.max() as f64 / 1000.0,
                "mean": hist.mean() / 1000.0,
                "samples": hist.len(),
            })
        };

        let memory = collect_memory();
        let throughput = serde_json::json!({
            "requests": self.bench_requests.load(Ordering::Relaxed),
            "bytes": self.bench_bytes.load(Ordering::Relaxed),
        });

        serde_json::json!({
            "latency": latency,
            "memory": memory,
            "throughput": throughput,
        })
    }
}

/// Gather system + process memory statistics via sysinfo.
#[cfg(feature = "bench")]
fn collect_memory() -> serde_json::Value {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let process_bytes = sys.process(pid).map(|p| p.memory()).unwrap_or(0);
    serde_json::json!({
        "total_bytes": sys.total_memory(),
        "used_bytes": sys.used_memory(),
        "available_bytes": sys.available_memory(),
        "process_bytes": process_bytes,
    })
}

pub type SharedMetrics = Arc<Metrics>;
