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
    federation_probe_successes: Counter<u64, AtomicU64>,
    federation_probe_failures: Counter<u64, AtomicU64>,
    federation_monitor_enabled: Gauge<i64, AtomicI64>,
    federation_origins: Gauge<i64, AtomicI64>,
    federation_origins_healthy: Gauge<i64, AtomicI64>,
    federation_origins_degraded: Gauge<i64, AtomicI64>,
    federation_origins_quarantined: Gauge<i64, AtomicI64>,
    federation_origins_unknown: Gauge<i64, AtomicI64>,
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
        let federation_probe_successes = Counter::default();
        let federation_probe_failures = Counter::default();
        let federation_monitor_enabled = Gauge::default();
        let federation_origins = Gauge::default();
        let federation_origins_healthy = Gauge::default();
        let federation_origins_degraded = Gauge::default();
        let federation_origins_quarantined = Gauge::default();
        let federation_origins_unknown = Gauge::default();
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
            "rw_federation_health_probe_successes",
            "Successful checks of operator-approved public HTTPS origins.",
            federation_probe_successes.clone(),
        );
        registry.register(
            "rw_federation_health_probe_failures",
            "Failed checks of operator-approved public HTTPS origins.",
            federation_probe_failures.clone(),
        );
        registry.register(
            "rw_federation_health_monitor_enabled",
            "Whether active public-origin health monitoring is enabled.",
            federation_monitor_enabled.clone(),
        );
        registry.register(
            "rw_federation_origins",
            "Total operator-approved public origins in the active catalog.",
            federation_origins.clone(),
        );
        registry.register(
            "rw_federation_origins_healthy",
            "Public origins whose most recent health check succeeded.",
            federation_origins_healthy.clone(),
        );
        registry.register(
            "rw_federation_origins_degraded",
            "Public origins with failures below the quarantine threshold.",
            federation_origins_degraded.clone(),
        );
        registry.register(
            "rw_federation_origins_quarantined",
            "Public origins temporarily excluded from failover selection.",
            federation_origins_quarantined.clone(),
        );
        registry.register(
            "rw_federation_origins_unknown",
            "Public origins not yet checked by the active monitor.",
            federation_origins_unknown.clone(),
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
            federation_probe_successes,
            federation_probe_failures,
            federation_monitor_enabled,
            federation_origins,
            federation_origins_healthy,
            federation_origins_degraded,
            federation_origins_quarantined,
            federation_origins_unknown,
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

    pub fn record_federation_probe(&self, healthy: bool) {
        if healthy {
            self.federation_probe_successes.inc();
        } else {
            self.federation_probe_failures.inc();
        }
    }

    pub fn set_federation_health(&self, status: &crate::federation::FederationHealthStatus) {
        self.federation_monitor_enabled
            .set(i64::from(status.monitor_enabled));
        self.federation_origins
            .set(i64::try_from(status.total_origins).unwrap_or(i64::MAX));
        self.federation_origins_healthy
            .set(i64::try_from(status.healthy_origins).unwrap_or(i64::MAX));
        self.federation_origins_degraded
            .set(i64::try_from(status.degraded_origins).unwrap_or(i64::MAX));
        self.federation_origins_quarantined
            .set(i64::try_from(status.quarantined_origins).unwrap_or(i64::MAX));
        self.federation_origins_unknown
            .set(i64::try_from(status.unknown_origins).unwrap_or(i64::MAX));
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
        metrics.record_federation_probe(true);
        metrics.set_federation_health(&crate::federation::FederationHealthStatus {
            schema: "rw.federation.health-status.v1".into(),
            monitor_enabled: true,
            total_origins: 2,
            healthy_origins: 1,
            degraded_origins: 0,
            quarantined_origins: 1,
            unknown_origins: 0,
            last_round_unix: Some(1_786_500_000),
            origins: Vec::new(),
        });
        let encoded = metrics.encode().unwrap();
        assert!(encoded.contains("rw_http_requests_total 1"));
        assert!(encoded.contains("rw_query_rejections_total 1"));
        assert!(encoded.contains("rw_response_cache_hits_total 0"));
        assert!(encoded.contains("rw_federation_health_probe_successes_total 1"));
        assert!(encoded.contains("rw_federation_origins_healthy 1"));
        assert!(encoded.contains("rw_federation_origins_quarantined 1"));
        assert!(!encoded.contains("model="));
        assert!(!encoded.contains("run="));
        assert!(!encoded.contains("origin_id="));
        assert!(!encoded.contains("https://"));
        assert!(!encoded.contains("127.0.0.1"));
    }
}
