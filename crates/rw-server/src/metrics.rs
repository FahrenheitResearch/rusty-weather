use std::fmt;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::time::Duration;

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

pub struct Metrics {
    registry: Registry,
    requests: Counter<u64, AtomicU64>,
    failures: Counter<u64, AtomicU64>,
    rejected: Counter<u64, AtomicU64>,
    cache_hits: Counter<u64, AtomicU64>,
    cache_misses: Counter<u64, AtomicU64>,
    inflight: Gauge<i64, AtomicI64>,
    duration_seconds: Histogram,
}

impl fmt::Debug for Metrics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Metrics").finish_non_exhaustive()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let requests = Counter::default();
        let failures = Counter::default();
        let rejected = Counter::default();
        let cache_hits = Counter::default();
        let cache_misses = Counter::default();
        let inflight = Gauge::default();
        let duration_seconds = Histogram::new(exponential_buckets(0.001, 2.0, 18));

        let mut registry = Registry::default();
        registry.register(
            "rw_http_requests",
            "Total HTTP requests accepted by the service.",
            requests.clone(),
        );
        registry.register(
            "rw_http_failures",
            "Total HTTP responses with a 4xx or 5xx status.",
            failures.clone(),
        );
        registry.register(
            "rw_query_rejections",
            "Total requests rejected by safety limits or admission control.",
            rejected.clone(),
        );
        registry.register(
            "rw_response_cache_hits",
            "Completed responses reused from the snapshot-keyed byte cache.",
            cache_hits.clone(),
        );
        registry.register(
            "rw_response_cache_misses",
            "Completed responses absent from the snapshot-keyed byte cache.",
            cache_misses.clone(),
        );
        registry.register(
            "rw_http_inflight",
            "HTTP requests currently executing.",
            inflight.clone(),
        );
        registry.register(
            "rw_http_duration_seconds",
            "End-to-end HTTP request duration in seconds.",
            duration_seconds.clone(),
        );

        Self {
            registry,
            requests,
            failures,
            rejected,
            cache_hits,
            cache_misses,
            inflight,
            duration_seconds,
        }
    }

    pub fn begin_request(&self) -> InFlightGuard<'_> {
        self.requests.inc();
        self.inflight.inc();
        InFlightGuard {
            metrics: self,
            finished: false,
        }
    }

    pub fn reject(&self) {
        self.rejected.inc();
    }

    pub fn cache_hit(&self) {
        self.cache_hits.inc();
    }

    pub fn cache_miss(&self) {
        self.cache_misses.inc();
    }

    pub fn encode(&self) -> Result<String, fmt::Error> {
        let mut output = String::new();
        encode(&mut output, &self.registry)?;
        Ok(output)
    }

    fn finish(&self, duration: Duration, failed: bool) {
        if failed {
            self.failures.inc();
        }
        self.duration_seconds.observe(duration.as_secs_f64());
        self.inflight.dec();
    }
}

pub struct InFlightGuard<'a> {
    metrics: &'a Metrics,
    finished: bool,
}

impl InFlightGuard<'_> {
    pub fn finish(mut self, duration: Duration, failed: bool) {
        self.metrics.finish(duration, failed);
        self.finished = true;
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.metrics.finish(Duration::ZERO, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_bounded_and_encode_as_openmetrics_text() {
        let metrics = Metrics::new();
        metrics
            .begin_request()
            .finish(Duration::from_millis(5), false);
        metrics.reject();
        let encoded = metrics.encode().unwrap();
        assert!(encoded.contains("rw_http_requests_total 1"));
        assert!(encoded.contains("rw_query_rejections_total 1"));
        assert!(encoded.contains("rw_response_cache_hits_total 0"));
        assert!(!encoded.contains("model="));
        assert!(!encoded.contains("run="));
    }
}
