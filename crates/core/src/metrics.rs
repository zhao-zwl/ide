//! Lightweight metrics module (T14 — observability).
//!
//! A self-contained, dependency-free metrics registry using atomic counters and
//! atomic fixed-bucket histograms (no external metrics crate). Exposes a single
//! [`Metrics::render_prometheus`] that emits Prometheus text exposition format.
//! The completion p95 is *reused* from `ide-probe::speed_test` rather than
//! reimplemented here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Latency histogram buckets in milliseconds (Prometheus-style upper bounds).
pub const LATENCY_BUCKETS_MS: &[f64] = &[
    5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0,
];

/// A fixed-bucket, lock-free latency histogram backed by atomic counters.
pub struct Histogram {
    buckets: &'static [f64],
    counts: Vec<AtomicU64>,
    /// Number of samples (atomic).
    count: AtomicU64,
    /// Sum of samples in microseconds (atomic, integer math to stay lock-free).
    sum_us: AtomicU64,
}

impl Histogram {
    /// Create a histogram over the given bucket upper-bounds (must be sorted,
    /// ascending). The implicit `+Inf` bucket is always tracked via `count`.
    pub fn new(buckets: &'static [f64]) -> Self {
        let counts = buckets.iter().map(|_| AtomicU64::new(0)).collect();
        Self {
            buckets,
            counts,
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
        }
    }

    /// Record a latency observation in milliseconds.
    pub fn observe_ms(&self, latency_ms: f64) {
        let bucket = latency_ms.max(0.0);
        for (i, upper) in self.buckets.iter().enumerate() {
            if bucket <= *upper {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        // Store microseconds as integer to avoid AtomicF64 (not in std).
        let us = (latency_ms * 1000.0) as u64;
        self.sum_us.fetch_add(us, Ordering::Relaxed);
    }

    /// Number of samples observed.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Sum of observed latencies in milliseconds.
    pub fn sum_ms(&self) -> f64 {
        self.sum_us.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Per-bucket cumulative counts (excludes the implicit +Inf bucket, which
    /// equals `count()`).
    pub fn bucket_counts(&self) -> Vec<u64> {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).collect()
    }

    /// Approximate p95 of observed samples (ms) derived by linear interpolation
    /// within the bucket that contains the 95th percentile rank. Cheap and pure;
    /// yields a coarse estimate (histograms trade precision for lock-freedom).
    pub fn p95_ms(&self) -> f64 {
        let total = self.count();
        if total == 0 {
            return 0.0;
        }
        let target = (total as f64 * 0.95).ceil() as u64;
        let mut cum: u64 = 0;
        for (i, upper) in self.buckets.iter().enumerate() {
            let bucket_count = self.counts[i].load(Ordering::Relaxed);
            cum += bucket_count;
            if cum >= target {
                let lower = if i == 0 { 0.0 } else { self.buckets[i - 1] };
                if bucket_count == 0 {
                    return *upper;
                }
                let rank_in_bucket = target.saturating_sub(cum - bucket_count);
                let frac = rank_in_bucket as f64 / bucket_count as f64;
                return lower + (*upper - lower) * frac;
            }
        }
        // Beyond the last bucket => approximate with its upper bound.
        *self.buckets.last().unwrap_or(&0.0)
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new(LATENCY_BUCKETS_MS)
    }
}

/// The Core metrics registry. All fields are atomic so it can be shared across
/// the gRPC server, the admin listener and the worker tasks via `Arc<Metrics>`.
#[derive(Default)]
pub struct Metrics {
    // --- counters ---
    pub requests_total: AtomicU64,
    pub tool_calls_total: AtomicU64,
    pub llm_calls_total: AtomicU64,
    pub completions_total: AtomicU64,
    pub denials_total: AtomicU64,
    /// Total append-only audit events recorded (T13 console "recent audit").
    pub audit_events_total: AtomicU64,
    // --- histograms (latency in ms) ---
    pub request_latency: Histogram,
    pub completion_latency: Histogram,
}

impl Metrics {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    // --- counter helpers (kept short for call sites) ---

    pub fn inc_requests(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_tool_calls(&self) {
        self.tool_calls_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_llm_calls(&self) {
        self.llm_calls_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_completions(&self) {
        self.completions_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_denials(&self) {
        self.denials_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_audit_events(&self) {
        self.audit_events_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn requests(&self) -> u64 {
        self.requests_total.load(Ordering::Relaxed)
    }
    pub fn tool_calls(&self) -> u64 {
        self.tool_calls_total.load(Ordering::Relaxed)
    }
    pub fn llm_calls(&self) -> u64 {
        self.llm_calls_total.load(Ordering::Relaxed)
    }
    pub fn completions(&self) -> u64 {
        self.completions_total.load(Ordering::Relaxed)
    }
    pub fn denials(&self) -> u64 {
        self.denials_total.load(Ordering::Relaxed)
    }
    pub fn audit_events(&self) -> u64 {
        self.audit_events_total.load(Ordering::Relaxed)
    }

    /// Observe a request latency (ms).
    pub fn observe_request(&self, latency_ms: f64) {
        self.request_latency.observe_ms(latency_ms);
    }
    /// Observe a completion latency (ms).
    pub fn observe_completion(&self, latency_ms: f64) {
        self.completion_latency.observe_ms(latency_ms);
    }

    /// Render the registry in Prometheus text exposition format.
    ///
    /// Counters are emitted as `counter` typed metrics; latencies as `histogram`
    /// with `_bucket` / `_sum` / `_count` lines. No trailing newline beyond the
    /// final series so the admin endpoint can append a Content-Length.
    pub fn render_prometheus(&self) -> String {
        let mut s = String::new();
        // --- counters ---
        s.push_str("# HELP aidea_requests_total Total gRPC/CLI requests handled.\n");
        s.push_str("# TYPE aidea_requests_total counter\n");
        s.push_str(&format!("aidea_requests_total {}\n", self.requests()));

        s.push_str("# HELP aidea_tool_calls_total Tool/action executions.\n");
        s.push_str("# TYPE aidea_tool_calls_total counter\n");
        s.push_str(&format!("aidea_tool_calls_total {}\n", self.tool_calls()));

        s.push_str("# HELP aidea_llm_calls_total LLM think/plan invocations.\n");
        s.push_str("# TYPE aidea_llm_calls_total counter\n");
        s.push_str(&format!("aidea_llm_calls_total {}\n", self.llm_calls()));

        s.push_str("# HELP aidea_completions_total NES completions produced.\n");
        s.push_str("# TYPE aidea_completions_total counter\n");
        s.push_str(&format!("aidea_completions_total {}\n", self.completions()));

        s.push_str("# HELP aidea_denials_total Actions denied by the six-bit mask.\n");
        s.push_str("# TYPE aidea_denials_total counter\n");
        s.push_str(&format!("aidea_denials_total {}\n", self.denials()));

        s.push_str("# HELP aidea_audit_events_total Append-only audit events recorded.\n");
        s.push_str("# TYPE aidea_audit_events_total counter\n");
        s.push_str(&format!(
            "aidea_audit_events_total {}\n",
            self.audit_events()
        ));

        // --- histograms ---
        self.render_histogram(&mut s, "aidea_request_latency_ms", &self.request_latency);
        self.render_histogram(
            &mut s,
            "aidea_completion_latency_ms",
            &self.completion_latency,
        );
        s
    }

    fn render_histogram(&self, out: &mut String, name: &str, h: &Histogram) {
        out.push_str(&format!("# HELP {name} Latency in milliseconds.\n"));
        out.push_str(&format!("# TYPE {name} histogram\n"));
        let counts = h.bucket_counts();
        let mut cum: u64 = 0;
        for (i, upper) in h.buckets.iter().enumerate() {
            cum += counts[i];
            out.push_str(&format!("{name}_bucket{{le=\"{upper}\"}} {cum}\n"));
        }
        // +Inf bucket.
        out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {}\n", h.count()));
        out.push_str(&format!("{name}_sum {}\n", h.sum_ms()));
        out.push_str(&format!("{name}_count {}\n", h.count()));
    }
}

/// Shared, cheaply-cloneable metrics handle.
pub type SharedMetrics = Arc<Metrics>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment() {
        let m = Metrics::new();
        m.inc_requests();
        m.inc_requests();
        m.inc_tool_calls();
        m.inc_denials();
        m.inc_audit_events();
        assert_eq!(m.requests(), 2);
        assert_eq!(m.tool_calls(), 1);
        assert_eq!(m.denials(), 1);
        assert_eq!(m.audit_events(), 1);
    }

    #[test]
    fn histogram_buckets_and_p95() {
        let h = Histogram::new(LATENCY_BUCKETS_MS);
        // 10 samples all at 7ms -> fall in the <=10 bucket (index 1).
        for _ in 0..10 {
            h.observe_ms(7.0);
        }
        let bc = h.bucket_counts();
        assert_eq!(bc[0], 0); // <=5 bucket empty
        assert_eq!(bc[1], 10); // <=10 bucket holds all
        assert_eq!(h.count(), 10);
        // Coarse histogram estimate for identical samples lands in [7, 10].
        let p95 = h.p95_ms();
        assert!((7.0..=10.0).contains(&p95), "p95={p95}");
        // Sum is exact: 10 * 7ms = 70ms.
        assert!((h.sum_ms() - 70.0).abs() < 1e-6);
    }

    #[test]
    fn histogram_multi_bucket_distribution() {
        let h = Histogram::new(LATENCY_BUCKETS_MS);
        h.observe_ms(3.0); // <=5
        h.observe_ms(40.0); // <=50
        h.observe_ms(800.0); // <=1000
        let bc = h.bucket_counts();
        assert_eq!(bc[0], 1);
        assert_eq!(bc[3], 1); // <=50
        assert_eq!(bc[7], 1); // <=1000
        assert_eq!(h.count(), 3);
    }

    #[test]
    fn prometheus_render_contains_series() {
        let m = Metrics::new();
        m.inc_requests();
        m.inc_denials();
        m.observe_request(7.0);
        let out = m.render_prometheus();
        assert!(out.contains("aidea_requests_total 1"));
        assert!(out.contains("aidea_denials_total 1"));
        assert!(out.contains("aidea_request_latency_ms_bucket{le=\"10\"}"));
        assert!(out.contains("aidea_request_latency_ms_sum"));
        assert!(out.contains("aidea_request_latency_ms_count 1"));
        // Histogram `# TYPE` declaration must precede its buckets.
        let help_idx = out.find("# TYPE aidea_request_latency_ms histogram").unwrap();
        let bucket_idx = out.find("aidea_request_latency_ms_bucket").unwrap();
        assert!(help_idx < bucket_idx);
        // Completion histogram must also be present (reused metric family).
        assert!(out.contains("# TYPE aidea_completion_latency_ms histogram"));
    }
}
