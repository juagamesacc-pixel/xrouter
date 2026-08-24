use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub requests_success: AtomicU64,
    pub requests_error: AtomicU64,
    pub tier_exhausted: AtomicU64,
    pub retries_total: AtomicU64,
    pub upstream_timeouts: AtomicU64,
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
}

pub type SharedMetrics = Arc<Metrics>;
