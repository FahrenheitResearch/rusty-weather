use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Days, TimeZone, Utc};
use fs4::{FileExt, TryLockError};
use rustwx_core::{CycleSpec, ModelId, ModelRunRequest, ResolvedUrl, SourceId};
use rustwx_models::{model_summary, resolve_urls, supported_forecast_hours};
use rw_ingest::{IngestConfig, ingest_hour_serial, model_ingest_capability};
use serde::Serialize;

use crate::RunCandidate;
use crate::config::SchedulerConfig;
use crate::coverage::{RunCoverage, ValidTime, verify_run_json};
use crate::error::{SchedulerError, SchedulerResult};
use crate::origin::{
    CapacityAuditStatus, OriginCatalogPlanConfig, OriginCatalogState, OriginCatalogStateStore,
    OriginLane, OriginLaneSelector,
};
use crate::plan::{ExpectedValidTime, JobPlan, cycle_origin_unix};
use crate::retention::{
    RetentionExecution, RunKey, ensure_owner_marker, execute_retention, plan_owned_retention,
};
use crate::state::{JobRecord, JobState, JobStateStore, RetryPolicy};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredCycle {
    pub cycle: CycleSpec,
    pub source: SourceId,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DiscoveryReport {
    pub discovered: Vec<DiscoveredModelCycle>,
    pub errors: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredModelCycle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    pub model: ModelId,
    pub cycle: CycleSpec,
    pub source: SourceId,
}

/// Injectable so tests stay local. Production uses registry availability
/// probes and validates their result against the current UTC rollback window.
pub trait CycleDiscovery: Send + Sync {
    fn discover(
        &self,
        model: ModelId,
        source: Option<SourceId>,
        now_unix: i64,
        rollback_days: u16,
    ) -> SchedulerResult<DiscoveredCycle>;

    /// Discover the provider cycle for a specific public-origin lane. Custom
    /// implementations may override this to model independent lane feeds. The
    /// default preserves compatibility for ordinary injected discovery while
    /// rejecting a non-extended cycle for the longest-horizon lane.
    fn discover_origin_lane(
        &self,
        lane: OriginLane,
        source: Option<SourceId>,
        now_unix: i64,
        rollback_days: u16,
    ) -> SchedulerResult<DiscoveredCycle> {
        let discovered = self.discover(lane.model, source, now_unix, rollback_days)?;
        if lane.selector == OriginLaneSelector::NewestCompleteLongestHorizon
            && !is_longest_horizon_cycle(lane.model, discovered.cycle.hour_utc)
        {
            return Err(SchedulerError::InvalidState(format!(
                "discovery returned {:02}z for origin lane '{}', but that cycle does not have the model's longest horizon",
                discovered.cycle.hour_utc, lane.id
            )));
        }
        Ok(discovered)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderCycleDiscovery {
    total_timeout: Duration,
    probe_timeout: Duration,
    cancelled: Arc<AtomicBool>,
}

impl ProviderCycleDiscovery {
    pub fn new(total_timeout: Duration, probe_timeout: Duration) -> SchedulerResult<Self> {
        Self::with_cancellation(
            total_timeout,
            probe_timeout,
            Arc::new(AtomicBool::new(false)),
        )
    }

    pub(crate) fn with_cancellation(
        total_timeout: Duration,
        probe_timeout: Duration,
        cancelled: Arc<AtomicBool>,
    ) -> SchedulerResult<Self> {
        if total_timeout.is_zero() || probe_timeout.is_zero() || probe_timeout > total_timeout {
            return Err(SchedulerError::InvalidConfig(
                "provider discovery timeouts must be nonzero and the probe timeout must not exceed the total timeout".to_string(),
            ));
        }
        Ok(Self {
            total_timeout,
            probe_timeout,
            cancelled,
        })
    }
}

impl CycleDiscovery for ProviderCycleDiscovery {
    fn discover(
        &self,
        model: ModelId,
        source: Option<SourceId>,
        now_unix: i64,
        rollback_days: u16,
    ) -> SchedulerResult<DiscoveredCycle> {
        let started = Instant::now();
        check_discovery_cancellation(&self.cancelled)?;
        let now = Utc.timestamp_opt(now_unix, 0).single().ok_or_else(|| {
            SchedulerError::InvalidConfig("current timestamp is out of range".to_string())
        })?;
        let products = model_ingest_capability(model)
            .products
            .iter()
            .map(|product| product.product)
            .collect::<Vec<_>>();
        let summary = model_summary(model);
        let representative_hour = summary
            .cycle_hours_utc
            .iter()
            .filter_map(|cycle_hour| {
                supported_forecast_hours(model, *cycle_hour)
                    .first()
                    .copied()
            })
            .min()
            .ok_or_else(|| {
                SchedulerError::InvalidConfig(format!(
                    "model '{model}' has no schedulable forecast hours"
                ))
            })?;
        let allowed_dates = (0..=rollback_days)
            .filter_map(|days| {
                now.date_naive()
                    .checked_sub_days(Days::new(u64::from(days)))
            })
            .map(|date| date.format("%Y%m%d").to_string())
            .collect::<Vec<_>>();
        let allowed_date_set = allowed_dates.iter().cloned().collect::<BTreeSet<_>>();
        let budget = DiscoveryBudget {
            now_unix,
            started,
            total_timeout: self.total_timeout,
            probe_timeout: self.probe_timeout,
            cancelled: &self.cancelled,
        };
        let mut diagnostics = Vec::new();
        for date in &allowed_dates {
            match latest_available_run_with_deadline(
                model,
                source,
                date,
                &products,
                representative_hour,
                &budget,
            ) {
                Ok(latest) => {
                    let origin = cycle_origin_unix(&latest.cycle)?;
                    if origin <= now_unix && allowed_date_set.contains(&latest.cycle.date_yyyymmdd)
                    {
                        return Ok(DiscoveredCycle {
                            cycle: latest.cycle,
                            source: latest.source,
                        });
                    }
                    diagnostics.push(format!(
                        "{} {:02}z was outside the configured UTC window",
                        latest.cycle.date_yyyymmdd, latest.cycle.hour_utc
                    ));
                }
                Err(error) => diagnostics.push(error.to_string()),
            }
        }
        Err(SchedulerError::InvalidState(format!(
            "no available cycle found for '{model}' within {rollback_days} rollback day(s): {}",
            diagnostics.join("; ")
        )))
    }

    fn discover_origin_lane(
        &self,
        lane: OriginLane,
        source: Option<SourceId>,
        now_unix: i64,
        rollback_days: u16,
    ) -> SchedulerResult<DiscoveredCycle> {
        self.discover_with_selector(lane.model, lane.selector, source, now_unix, rollback_days)
    }
}

impl ProviderCycleDiscovery {
    fn discover_with_selector(
        &self,
        model: ModelId,
        selector: OriginLaneSelector,
        source: Option<SourceId>,
        now_unix: i64,
        rollback_days: u16,
    ) -> SchedulerResult<DiscoveredCycle> {
        let started = Instant::now();
        check_discovery_cancellation(&self.cancelled)?;
        let now = Utc.timestamp_opt(now_unix, 0).single().ok_or_else(|| {
            SchedulerError::InvalidConfig("current timestamp is out of range".to_string())
        })?;
        let products = model_ingest_capability(model)
            .products
            .iter()
            .map(|product| product.product)
            .collect::<Vec<_>>();
        let (forecast_hour, eligible_hours) = discovery_shape_for_selector(model, selector)?;
        let allowed_dates = (0..=rollback_days)
            .filter_map(|days| {
                now.date_naive()
                    .checked_sub_days(Days::new(u64::from(days)))
            })
            .map(|date| date.format("%Y%m%d").to_string())
            .collect::<Vec<_>>();
        let allowed_date_set = allowed_dates.iter().cloned().collect::<BTreeSet<_>>();
        let budget = DiscoveryBudget {
            now_unix,
            started,
            total_timeout: self.total_timeout,
            probe_timeout: self.probe_timeout,
            cancelled: &self.cancelled,
        };
        let mut diagnostics = Vec::new();
        for date in &allowed_dates {
            match latest_available_run_with_deadline_for_hours(
                model,
                source,
                date,
                &products,
                forecast_hour,
                &eligible_hours,
                &budget,
            ) {
                Ok(latest) => {
                    let origin = cycle_origin_unix(&latest.cycle)?;
                    if origin <= now_unix && allowed_date_set.contains(&latest.cycle.date_yyyymmdd)
                    {
                        return Ok(latest);
                    }
                    diagnostics.push(format!(
                        "{} {:02}z was outside the configured UTC window",
                        latest.cycle.date_yyyymmdd, latest.cycle.hour_utc
                    ));
                }
                Err(error) => diagnostics.push(error.to_string()),
            }
        }
        Err(SchedulerError::InvalidState(format!(
            "no origin-lane cycle found for '{model}' within {rollback_days} rollback day(s): {}",
            diagnostics.join("; ")
        )))
    }
}

pub(crate) fn discovery_shape_for_selector(
    model: ModelId,
    selector: OriginLaneSelector,
) -> SchedulerResult<(u16, BTreeSet<u8>)> {
    let summary = model_summary(model);
    let longest = summary
        .cycle_hours_utc
        .iter()
        .filter_map(|hour| supported_forecast_hours(model, *hour).into_iter().max())
        .max()
        .ok_or_else(|| {
            SchedulerError::InvalidConfig(format!(
                "model '{model}' has no schedulable forecast hours"
            ))
        })?;
    match selector {
        OriginLaneSelector::NewestAvailable => {
            let first = summary
                .cycle_hours_utc
                .iter()
                .filter_map(|hour| supported_forecast_hours(model, *hour).into_iter().min())
                .min()
                .ok_or_else(|| {
                    SchedulerError::InvalidConfig(format!(
                        "model '{model}' has no schedulable forecast hours"
                    ))
                })?;
            Ok((first, summary.cycle_hours_utc.iter().copied().collect()))
        }
        OriginLaneSelector::NewestCompleteLongestHorizon => Ok((
            longest,
            summary
                .cycle_hours_utc
                .iter()
                .copied()
                .filter(|hour| is_longest_horizon_cycle(model, *hour))
                .collect(),
        )),
    }
}

fn is_longest_horizon_cycle(model: ModelId, cycle_hour_utc: u8) -> bool {
    let candidate_max = supported_forecast_hours(model, cycle_hour_utc)
        .into_iter()
        .max();
    let declared_max = model_summary(model)
        .cycle_hours_utc
        .iter()
        .flat_map(|hour| supported_forecast_hours(model, *hour))
        .max();
    candidate_max.is_some() && candidate_max == declared_max
}

#[derive(Debug, Clone, Copy)]
struct DiscoveryBudget<'a> {
    now_unix: i64,
    started: Instant,
    total_timeout: Duration,
    probe_timeout: Duration,
    cancelled: &'a AtomicBool,
}

fn latest_available_run_with_deadline(
    model: ModelId,
    source: Option<SourceId>,
    date_yyyymmdd: &str,
    products: &[&str],
    forecast_hour: u16,
    budget: &DiscoveryBudget<'_>,
) -> SchedulerResult<DiscoveredCycle> {
    let eligible_hours = model_summary(model)
        .cycle_hours_utc
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    latest_available_run_with_deadline_for_hours(
        model,
        source,
        date_yyyymmdd,
        products,
        forecast_hour,
        &eligible_hours,
        budget,
    )
}

fn latest_available_run_with_deadline_for_hours(
    model: ModelId,
    source: Option<SourceId>,
    date_yyyymmdd: &str,
    products: &[&str],
    forecast_hour: u16,
    eligible_hours: &BTreeSet<u8>,
    budget: &DiscoveryBudget<'_>,
) -> SchedulerResult<DiscoveredCycle> {
    check_discovery_cancellation(budget.cancelled)?;
    let summary = model_summary(model);
    let sources = summary
        .sources
        .iter()
        .filter(|candidate| source.map(|wanted| wanted == candidate.id).unwrap_or(true))
        .map(|candidate| candidate.id)
        .collect::<Vec<_>>();
    if sources.is_empty() {
        return Err(SchedulerError::InvalidState(format!(
            "model '{model}' has no eligible remote provider"
        )));
    }
    let mut products = products.to_vec();
    products.sort_unstable();
    products.dedup();
    let agent = discovery_agent(budget.probe_timeout)?;

    for hour_utc in summary.cycle_hours_utc.iter().rev().copied() {
        check_discovery_cancellation(budget.cancelled)?;
        if !eligible_hours.contains(&hour_utc)
            || !supported_forecast_hours(model, hour_utc).contains(&forecast_hour)
        {
            continue;
        }
        let cycle = CycleSpec::new(date_yyyymmdd.to_string(), hour_utc)?;
        if cycle_origin_unix(&cycle)? > budget.now_unix {
            continue;
        }
        for candidate_source in sources.iter().copied() {
            check_discovery_cancellation(budget.cancelled)?;
            let mut complete = true;
            for product in &products {
                check_discovery_cancellation(budget.cancelled)?;
                let remaining = budget
                    .total_timeout
                    .saturating_sub(budget.started.elapsed());
                if remaining.is_zero() {
                    return Err(SchedulerError::InvalidState(format!(
                        "provider discovery for '{model}' exceeded its {:.1}s total budget",
                        budget.total_timeout.as_secs_f64()
                    )));
                }
                let request = ModelRunRequest::new(model, cycle.clone(), forecast_hour, *product)?;
                let available = resolve_urls(&request)?
                    .into_iter()
                    .filter(|resolved| resolved.source == candidate_source)
                    .any(|resolved| {
                        let remaining = budget
                            .total_timeout
                            .saturating_sub(budget.started.elapsed());
                        !remaining.is_zero()
                            && availability_probe_ok(
                                &agent,
                                &resolved,
                                budget.probe_timeout.min(remaining),
                            )
                    });
                check_discovery_cancellation(budget.cancelled)?;
                if !available {
                    complete = false;
                    break;
                }
            }
            if complete {
                return Ok(DiscoveredCycle {
                    cycle,
                    source: candidate_source,
                });
            }
        }
    }
    Err(SchedulerError::InvalidState(format!(
        "no available cycle found for '{model}' on {date_yyyymmdd}"
    )))
}

fn check_discovery_cancellation(cancelled: &AtomicBool) -> SchedulerResult<()> {
    if cancelled.load(Ordering::Acquire) {
        return Err(SchedulerError::InvalidState(
            "provider discovery cancelled by scheduler shutdown".to_string(),
        ));
    }
    Ok(())
}

fn discovery_agent(timeout: Duration) -> SchedulerResult<ureq::Agent> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_per_call(Some(timeout))
        .timeout_resolve(Some(timeout.min(Duration::from_secs(2))))
        .timeout_connect(Some(timeout.min(Duration::from_secs(3))))
        .timeout_send_request(Some(timeout.min(Duration::from_secs(2))))
        .timeout_recv_response(Some(timeout.min(Duration::from_secs(4))))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::WebPki)
                .unversioned_rustls_crypto_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .build(),
        )
        .build();
    Ok(config.new_agent())
}

fn availability_probe_ok(agent: &ureq::Agent, resolved: &ResolvedUrl, timeout: Duration) -> bool {
    let url = if resolved.source == SourceId::Nomads {
        &resolved.grib_url
    } else {
        resolved.availability_probe_url()
    };
    let request = if resolved.source == SourceId::Nomads {
        agent.get(url).header("Range", "bytes=0-0")
    } else {
        agent.head(url)
    }
    .config()
    .timeout_global(Some(timeout))
    .build();
    request.call().is_ok()
}

/// Injectable execution seam for deterministic scheduler tests and alternate
/// local ingest implementations. Production uses the built-in rw-ingest path.
pub trait JobExecution: Send + Sync {
    fn execute(&self, plan: &JobPlan) -> SchedulerResult<RunCoverage>;
}

/// Injectable exact run validator. Production always deep-opens rw-store
/// manifests, the grid, and every referenced hour through `verify_run_json`.
pub trait RunValidation: Send + Sync {
    fn verify(&self, plan: &JobPlan) -> SchedulerResult<RunCoverage>;
}

#[derive(Debug, Clone)]
struct StoreRunValidation {
    store_root: PathBuf,
}

impl RunValidation for StoreRunValidation {
    fn verify(&self, plan: &JobPlan) -> SchedulerResult<RunCoverage> {
        verify_run_json(plan, &run_json_path(&self.store_root, plan))
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExecutionReport {
    pub admitted: Vec<String>,
    pub skipped: Vec<String>,
    pub succeeded: Vec<String>,
    pub retrying: Vec<String>,
    pub failed: Vec<String>,
    pub discovery_errors: BTreeMap<String, String>,
    pub origin_catalog: Option<OriginCatalogState>,
    pub retention: Option<RetentionExecution>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub queued: usize,
    pub running: usize,
    pub retry_backoff: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub records: Vec<JobRecord>,
    pub origin_catalog: Option<OriginCatalogState>,
}

#[derive(Clone)]
pub struct SchedulerHost {
    config: SchedulerConfig,
    discovery: Arc<dyn CycleDiscovery>,
    cancelled: Arc<AtomicBool>,
    hour_gate: Arc<ConcurrencyGate>,
    injected_execution: Option<Arc<dyn JobExecution>>,
    validation: Arc<dyn RunValidation>,
}

struct HostLease {
    file: File,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeCapacity {
    job_concurrency: usize,
}

impl HostLease {
    fn acquire(state_root: &std::path::Path) -> SchedulerResult<Self> {
        fs::create_dir_all(state_root)?;
        let metadata = fs::symlink_metadata(state_root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SchedulerError::InvalidState(format!(
                "scheduler state root '{}' must be a real directory",
                state_root.display()
            )));
        }
        let path = state_root.join(".rw-scheduler-host.lock");
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(SchedulerError::InvalidState(format!(
                "scheduler host lock '{}' must be a real regular file",
                path.display()
            )));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Self { file }),
            Err(TryLockError::WouldBlock) => Err(SchedulerError::Capacity(format!(
                "another rw-scheduler process holds '{}'",
                path.display()
            ))),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl Drop for HostLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl SchedulerHost {
    pub fn new(config: SchedulerConfig) -> SchedulerResult<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let discovery = Arc::new(ProviderCycleDiscovery::with_cancellation(
            Duration::from_secs(config.discovery_timeout_seconds),
            Duration::from_secs(config.discovery_probe_timeout_seconds),
            Arc::clone(&cancelled),
        )?);
        Self::with_discovery_and_cancellation(config, discovery, cancelled)
    }

    pub fn with_discovery(
        config: SchedulerConfig,
        discovery: Arc<dyn CycleDiscovery>,
    ) -> SchedulerResult<Self> {
        Self::with_discovery_and_cancellation(config, discovery, Arc::new(AtomicBool::new(false)))
    }

    fn with_discovery_and_cancellation(
        config: SchedulerConfig,
        discovery: Arc<dyn CycleDiscovery>,
        cancelled: Arc<AtomicBool>,
    ) -> SchedulerResult<Self> {
        config.validate()?;
        let hour_gate = Arc::new(ConcurrencyGate::new(config.max_concurrent_hours));
        Ok(Self {
            validation: Arc::new(StoreRunValidation {
                store_root: config.store_root.clone(),
            }),
            config,
            discovery,
            cancelled,
            hour_gate,
            injected_execution: None,
        })
    }

    /// Fully injected host used by local tests and embedders. The injected
    /// validator remains authoritative for alias publication; returning bytes
    /// from an executor alone can never advance a lane.
    pub fn with_components(
        config: SchedulerConfig,
        discovery: Arc<dyn CycleDiscovery>,
        execution: Arc<dyn JobExecution>,
        validation: Arc<dyn RunValidation>,
    ) -> SchedulerResult<Self> {
        config.validate()?;
        let hour_gate = Arc::new(ConcurrencyGate::new(config.max_concurrent_hours));
        Ok(Self {
            config,
            discovery,
            cancelled: Arc::new(AtomicBool::new(false)),
            hour_gate,
            injected_execution: Some(execution),
            validation,
        })
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    pub fn request_shutdown(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn shutdown_requested(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn runtime_capacity(&self) -> SchedulerResult<RuntimeCapacity> {
        let Some(origin) = &self.config.origin_catalog_plan else {
            return Ok(RuntimeCapacity {
                job_concurrency: self.config.max_concurrent_jobs,
            });
        };
        if origin.capacity_audit != CapacityAuditStatus::Complete {
            return Err(SchedulerError::Capacity(
                "origin catalog execution is disabled until the direct host capacity audit is complete"
                    .to_string(),
            ));
        }
        let audited_jobs = origin.max_concurrent_jobs.ok_or_else(|| {
            SchedulerError::Capacity(
                "origin catalog has no audited job-concurrency value".to_string(),
            )
        })?;
        origin.disk_budget_bytes.ok_or_else(|| {
            SchedulerError::Capacity("origin catalog has no audited disk budget".to_string())
        })?;
        Ok(RuntimeCapacity {
            job_concurrency: self.config.max_concurrent_jobs.min(audited_jobs),
        })
    }

    /// Offline plan view: newest registry cycle not later than `now`, without
    /// claiming provider availability.
    pub fn plan_at(&self, now_unix: i64) -> SchedulerResult<Vec<JobPlan>> {
        let mut plans = Vec::new();
        for model in self.config.expanded_models()? {
            let cycle = latest_registry_cycle(model, now_unix, self.config.rollback_days)?;
            let profile = self.config.profile_for(model)?;
            plans.push(JobPlan::build_with_profile_and_source(
                model,
                cycle,
                &profile,
                self.config.source_for(model)?,
            )?);
        }
        Ok(plans)
    }

    pub fn status(&self) -> SchedulerResult<StatusReport> {
        let records = JobStateStore::new(&self.config.state_root).load_all()?;
        let origin_catalog = self
            .config
            .origin_catalog_plan
            .as_ref()
            .map(|origin| {
                OriginCatalogStateStore::new(&self.config.store_root).load_or_empty(origin)
            })
            .transpose()?;
        let mut report = StatusReport {
            queued: 0,
            running: 0,
            retry_backoff: 0,
            succeeded: 0,
            failed: 0,
            records,
            origin_catalog,
        };
        for record in &report.records {
            match record.state {
                JobState::Queued => report.queued += 1,
                JobState::Running { .. } => report.running += 1,
                JobState::RetryBackoff { .. } => report.retry_backoff += 1,
                JobState::Succeeded { .. } => report.succeeded += 1,
                JobState::Failed { .. } => report.failed += 1,
            }
        }
        Ok(report)
    }

    /// Metadata-only provider preflight. This performs bounded HEAD/range
    /// probes and never creates scheduler roots, state, cache, or store data.
    pub fn discover_at(&self, now: i64) -> SchedulerResult<DiscoveryReport> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(SchedulerError::InvalidState(
                "scheduler shutdown has been requested".to_string(),
            ));
        }
        let capacity = self.runtime_capacity()?;
        let allowlist = self
            .config
            .expanded_models()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut report = DiscoveryReport::default();
        if let Some(origin) = &self.config.origin_catalog_plan {
            for (lane, result) in
                self.discover_origin_lanes(origin, now, capacity.job_concurrency)?
            {
                match result {
                    Ok(discovered) => report.discovered.push(DiscoveredModelCycle {
                        lane: Some(lane.id.to_string()),
                        model: lane.model,
                        cycle: discovered.cycle,
                        source: discovered.source,
                    }),
                    Err(error) => {
                        report.errors.insert(lane.id.to_string(), error);
                    }
                }
            }
        } else {
            for (model, result) in
                self.discover_allowed(&allowlist, now, capacity.job_concurrency)?
            {
                match result {
                    Ok(discovered) => report.discovered.push(DiscoveredModelCycle {
                        lane: None,
                        model,
                        cycle: discovered.cycle,
                        source: discovered.source,
                    }),
                    Err(error) => {
                        report.errors.insert(model.as_str().to_string(), error);
                    }
                }
            }
        }
        Ok(report)
    }

    pub fn run_once(&self) -> SchedulerResult<ExecutionReport> {
        self.run_once_at(now_unix()?)
    }

    pub fn run_once_at(&self, now: i64) -> SchedulerResult<ExecutionReport> {
        // The audit gate is intentionally checked before creating roots,
        // acquiring locks, discovering providers, or mutating durable state.
        let capacity = self.runtime_capacity()?;
        self.config.prepare_roots()?;
        let _host_lease = HostLease::acquire(&self.config.state_root)?;
        if self.cancelled.load(Ordering::Acquire) {
            return Err(SchedulerError::InvalidState(
                "scheduler shutdown has been requested".to_string(),
            ));
        }
        if self.config.origin_catalog_plan.is_some() {
            self.check_space()?;
        }
        let store = JobStateStore::new(&self.config.state_root);
        let recovered = store.recover_running(now)?;
        let allowlist = self
            .config
            .expanded_models()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut report = ExecutionReport::default();
        let published_protected = self
            .config
            .origin_catalog_plan
            .as_ref()
            .map(|origin| {
                OriginCatalogStateStore::new(&self.config.store_root)
                    .load_or_empty(origin)?
                    .protected()
            })
            .transpose()?
            .unwrap_or_default();
        // Persisted active work is scheduled before discovering a newer cycle;
        // restart cannot strand yesterday's queued/retrying job. Published
        // active/rollback generations are also revalidated and repaired even
        // when their job state was terminal before the restart.
        let mut records = BTreeMap::new();
        for record in recovered {
            let key = RunKey::new(record.plan.model, record.plan.run_id.clone())?;
            if allowlist.contains(&record.plan.model)
                && (record.state.is_active() || published_protected.contains(&key))
            {
                records.insert(record.plan.job_id.clone(), record);
            }
        }
        let mut newly_admitted = Vec::new();

        let discoveries = if let Some(origin) = &self.config.origin_catalog_plan {
            self.discover_origin_lanes(origin, now, capacity.job_concurrency)?
                .into_iter()
                .map(|(lane, result)| (lane.id.to_string(), lane.model, result))
                .collect::<Vec<_>>()
        } else {
            self.discover_allowed(&allowlist, now, capacity.job_concurrency)?
                .into_iter()
                .map(|(model, result)| (model.as_str().to_string(), model, result))
                .collect::<Vec<_>>()
        };
        for (discovery_key, model, discovery) in discoveries {
            let discovered = match discovery {
                Ok(discovered) => discovered,
                Err(error) => {
                    report.discovery_errors.insert(discovery_key, error);
                    continue;
                }
            };
            let profile = self.config.profile_for(model)?;
            let plan = JobPlan::build_with_profile_and_source(
                model,
                discovered.cycle,
                &profile,
                Some(discovered.source),
            )?;
            let state_path = self.config.state_root.join(format!("{}.json", plan.job_id));
            let record = if let Some(record) = records.get(&plan.job_id) {
                record.clone()
            } else if state_path.exists() {
                store.load(&plan.job_id)?
            } else {
                let record = JobRecord::new(plan, now)?;
                newly_admitted.push(record.plan.job_id.clone());
                record
            };
            records.insert(record.plan.job_id.clone(), record);
        }

        let job_capacity = self
            .config
            .max_queued_jobs
            .checked_add(self.config.max_concurrent_jobs)
            .ok_or_else(|| {
                SchedulerError::Capacity(
                    "max_queued_jobs + max_concurrent_jobs overflows usize".to_string(),
                )
            })?;
        if records.len() > job_capacity {
            return Err(SchedulerError::Capacity(format!(
                "{} active jobs exceed max_concurrent_jobs + max_queued_jobs ({})",
                records.len(),
                job_capacity
            )));
        }
        for job_id in newly_admitted {
            let record = records.get(&job_id).ok_or_else(|| {
                SchedulerError::InvalidState(format!(
                    "newly admitted job '{job_id}' disappeared before persistence"
                ))
            })?;
            store.save(record)?;
            report.admitted.push(job_id);
        }

        let mut runnable = Vec::new();
        for (_, mut record) in records {
            match record.state {
                JobState::RetryBackoff { retry_at_unix } if retry_at_unix <= now => {
                    record.release_retry(now)?;
                    store.save(&record)?;
                    runnable.push(record);
                }
                JobState::Queued => runnable.push(record),
                JobState::Succeeded { .. } => match self.validation.verify(&record.plan) {
                    Ok(coverage) if coverage.is_complete() => {
                        report.skipped.push(record.plan.job_id.clone());
                    }
                    outcome => {
                        let reason = match outcome {
                            Ok(_) => "terminal run is no longer complete".to_string(),
                            Err(error) => {
                                format!("terminal run failed deep validation: {error}")
                            }
                        };
                        record.requeue_for_repair(now, &reason)?;
                        store.save(&record)?;
                        runnable.push(record);
                    }
                },
                JobState::RetryBackoff { .. } | JobState::Failed { .. } => {
                    report.skipped.push(record.plan.job_id.clone());
                }
                JobState::Running { .. } => unreachable!("running jobs were recovered"),
            }
        }
        runnable.sort_by(|left, right| left.plan.job_id.cmp(&right.plan.job_id));

        for chunk in runnable.chunks(capacity.job_concurrency) {
            let results = Mutex::new(Vec::new());
            thread::scope(|scope| {
                for record in chunk.iter().cloned() {
                    let results = &results;
                    scope.spawn(move || {
                        let result = self.execute_one(record);
                        results.lock().expect("result mutex poisoned").push(result);
                    });
                }
            });
            for result in results.into_inner().map_err(|_| {
                SchedulerError::InvalidState("scheduler result mutex poisoned".to_string())
            })? {
                let record = result?;
                match record.state {
                    JobState::Succeeded { .. } => report.succeeded.push(record.plan.job_id),
                    JobState::RetryBackoff { .. } => report.retrying.push(record.plan.job_id),
                    JobState::Failed { .. } => report.failed.push(record.plan.job_id),
                    JobState::Queued if self.cancelled.load(Ordering::Acquire) => {
                        report.skipped.push(record.plan.job_id)
                    }
                    _ => {
                        return Err(SchedulerError::InvalidState(format!(
                            "job '{}' ended in a non-terminal state",
                            record.plan.job_id
                        )));
                    }
                }
            }
            if self.cancelled.load(Ordering::Acquire) {
                break;
            }
        }

        let records = store
            .load_all()?
            .into_iter()
            .filter(|record| allowlist.contains(&record.plan.model))
            .collect::<Vec<_>>();
        let protected = if let Some(origin) = &self.config.origin_catalog_plan {
            let catalog = self.refresh_origin_catalog(origin, &records, now)?;
            let protected = catalog.protected()?;
            report.origin_catalog = Some(catalog);
            protected
        } else {
            BTreeSet::new()
        };

        if self.config.retention.enabled {
            let plan = plan_owned_retention(
                &records,
                &protected,
                self.config.retention.keep_latest_per_model,
            )?;
            let execution = execute_retention(
                &self.config.store_root,
                &records,
                &plan,
                self.config.retention.dry_run,
            )?;
            if !execution.dry_run {
                for label in &execution.state_prunable {
                    if let Some(record) = records.iter().find(|record| {
                        format!("{}:{}", record.plan.model, record.plan.run_id) == *label
                    }) {
                        store.remove_terminal(&record.plan.job_id)?;
                    }
                }
            }
            report.retention = Some(execution);
        }
        Ok(report)
    }

    fn refresh_origin_catalog(
        &self,
        origin: &OriginCatalogPlanConfig,
        records: &[JobRecord],
        now: i64,
    ) -> SchedulerResult<OriginCatalogState> {
        let catalog_store = OriginCatalogStateStore::new(&self.config.store_root);
        let current = catalog_store.load_or_empty(origin)?;
        let mut candidates = Vec::new();
        let mut validation_errors = BTreeMap::new();
        for record in records {
            match self.validation.verify(&record.plan) {
                Ok(coverage) => {
                    if let Err(error) = validate_queryable_coverage(&record.plan, &coverage) {
                        validation_errors.insert(
                            RunKey::new(record.plan.model, record.plan.run_id.clone())?,
                            error.to_string(),
                        );
                        continue;
                    }
                    if !coverage.present.is_empty() {
                        let profile = record.plan.ingest_profile.to_profile()?;
                        let profile_result = origin
                            .lanes()
                            .into_iter()
                            .filter(|lane| lane.model == record.plan.model)
                            .try_for_each(|lane| lane.validate_profile(&profile));
                        match profile_result {
                            Ok(()) => candidates
                                .push(RunCandidate::from_coverage(&record.plan, &coverage)?),
                            Err(error) => {
                                validation_errors.insert(
                                    RunKey::new(record.plan.model, record.plan.run_id.clone())?,
                                    error.to_string(),
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    validation_errors.insert(
                        RunKey::new(record.plan.model, record.plan.run_id.clone())?,
                        error.to_string(),
                    );
                }
            }
        }

        // A previously published alias is a stronger invariant than an
        // unselected historical record. If it can no longer be deeply
        // validated, retain the old durable catalog and abort before
        // retention rather than silently publishing or deleting around it.
        for key in current.protected()? {
            if candidates.iter().any(|candidate| {
                candidate.model() == key.model() && candidate.run_id() == key.run_id()
            }) {
                continue;
            }
            let reason = validation_errors
                .get(&key)
                .cloned()
                .unwrap_or_else(|| "matching durable scheduler state is absent".to_string());
            return Err(SchedulerError::InvalidCoverage(format!(
                "published origin generation '{}:{}' failed revalidation: {reason}",
                key.model(),
                key.run_id()
            )));
        }

        let next = OriginCatalogState::from_candidates(origin, &candidates, now)?;
        if next.lanes == current.lanes {
            return Ok(current);
        }
        catalog_store.save(origin, &next)?;
        Ok(next)
    }

    fn discover_allowed(
        &self,
        allowlist: &BTreeSet<ModelId>,
        now: i64,
        job_concurrency: usize,
    ) -> SchedulerResult<Vec<(ModelId, Result<DiscoveredCycle, String>)>> {
        let models = allowlist.iter().copied().collect::<Vec<_>>();
        let mut all = Vec::with_capacity(models.len());
        for chunk in models.chunks(job_concurrency) {
            let results = Mutex::new(Vec::with_capacity(chunk.len()));
            thread::scope(|scope| {
                for model in chunk.iter().copied() {
                    let results = &results;
                    scope.spawn(move || {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            let source = self.config.source_for(model)?;
                            self.discovery
                                .discover(model, source, now, self.config.rollback_days)
                        }))
                        .map_err(|_| "provider discovery panicked".to_string())
                        .and_then(|result| result.map_err(|error| error.to_string()));
                        if let Ok(mut results) = results.lock() {
                            results.push((model, result));
                        }
                    });
                }
            });
            all.extend(results.into_inner().map_err(|_| {
                SchedulerError::InvalidState("discovery result mutex poisoned".to_string())
            })?);
            if self.cancelled.load(Ordering::Acquire) {
                break;
            }
        }
        all.sort_by_key(|(model, _)| *model);
        Ok(all)
    }

    fn discover_origin_lanes(
        &self,
        origin: &OriginCatalogPlanConfig,
        now: i64,
        job_concurrency: usize,
    ) -> SchedulerResult<Vec<(OriginLane, Result<DiscoveredCycle, String>)>> {
        let lanes = origin.lanes();
        let mut all = Vec::with_capacity(lanes.len());
        for chunk in lanes.chunks(job_concurrency) {
            let results = Mutex::new(Vec::with_capacity(chunk.len()));
            thread::scope(|scope| {
                for lane in chunk.iter().copied() {
                    let results = &results;
                    scope.spawn(move || {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            let source = self.config.source_for(lane.model)?;
                            self.discovery.discover_origin_lane(
                                lane,
                                source,
                                now,
                                self.config.rollback_days,
                            )
                        }))
                        .map_err(|_| "provider origin-lane discovery panicked".to_string())
                        .and_then(|result| result.map_err(|error| error.to_string()));
                        if let Ok(mut results) = results.lock() {
                            results.push((lane, result));
                        }
                    });
                }
            });
            all.extend(results.into_inner().map_err(|_| {
                SchedulerError::InvalidState(
                    "origin-lane discovery result mutex poisoned".to_string(),
                )
            })?);
            if self.cancelled.load(Ordering::Acquire) {
                break;
            }
        }
        all.sort_by_key(|(lane, _)| lane.id);
        Ok(all)
    }

    fn execute_one(&self, mut record: JobRecord) -> SchedulerResult<JobRecord> {
        let store = JobStateStore::new(&self.config.state_root);
        let policy = self.config.retry.policy()?;
        let started = now_unix()?;
        record.start(started, policy)?;
        store.save(&record)?;

        let outcome = catch_unwind(AssertUnwindSafe(|| self.execute_running(&record)))
            .unwrap_or_else(|_| Err(SchedulerError::Ingest("worker panicked".to_string())));
        let finished = now_unix()?;
        match outcome {
            Ok(coverage) if coverage.is_complete() => {
                self.check_space()?;
                record.finish_success(finished, &coverage)?;
            }
            Ok(coverage) => {
                validate_queryable_coverage(&record.plan, &coverage)?;
                let delay = deterministic_jittered_delay(
                    policy,
                    record.attempts,
                    &record.plan.job_id,
                    self.config.retry.jitter_percent,
                )?;
                record.finish_failure_with_delay(
                    finished,
                    &format!(
                        "validated run remains incomplete ({} of {} expected valid times present)",
                        coverage.present.len(),
                        coverage.expected.len()
                    ),
                    policy,
                    delay,
                )?;
            }
            Err(error) if self.cancelled.load(Ordering::Acquire) => {
                record.last_error = Some(format!("shutdown: {error}"));
                record.recover_after_restart(finished)?;
            }
            Err(error) => {
                let delay = deterministic_jittered_delay(
                    policy,
                    record.attempts,
                    &record.plan.job_id,
                    self.config.retry.jitter_percent,
                )?;
                record.finish_failure_with_delay(finished, &error.to_string(), policy, delay)?;
            }
        }
        store.save(&record)?;
        Ok(record)
    }

    fn execute_running(&self, record: &JobRecord) -> SchedulerResult<RunCoverage> {
        self.check_space()?;
        if let Some(execution) = &self.injected_execution {
            let coverage = execution.execute(&record.plan)?;
            if !coverage.matches_plan(&record.plan) || !coverage.storage_validated {
                return Err(SchedulerError::InvalidCoverage(format!(
                    "injected execution returned unvalidated coverage for job '{}'",
                    record.plan.job_id
                )));
            }
            return Ok(coverage);
        }
        let run_dir = self
            .config
            .store_root
            .join(record.plan.model.as_str())
            .join(&record.plan.run_id);
        ensure_owner_marker(&run_dir, record)?;
        let run_json = run_dir.join("run.json");
        let existing = if run_json.exists() {
            Some(verify_run_json(&record.plan, &run_json)?)
        } else {
            None
        };
        if let Some(coverage) = existing.as_ref().filter(|coverage| coverage.is_complete()) {
            return Ok(coverage.clone());
        }
        let validated_times = existing
            .as_ref()
            .map(|coverage| {
                let mismatched = coverage
                    .slot_mismatches
                    .iter()
                    .map(|mismatch| mismatch.expected)
                    .collect::<BTreeSet<_>>();
                coverage
                    .present
                    .difference(&mismatched)
                    .copied()
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let missing = record
            .plan
            .expected_valid_times
            .iter()
            .copied()
            .filter(|expected| !validated_times.contains(&ValidTime::from(*expected)))
            .collect::<Vec<_>>();
        self.ingest_hours(&record.plan, missing)?;
        verify_run_json(&record.plan, &run_json)
    }

    fn check_space(&self) -> SchedulerResult<()> {
        space_gate(
            fs4::available_space(&self.config.store_root)?,
            self.config.free_space_reserve_bytes,
            "store",
        )?;
        if self.config.use_cache {
            space_gate(
                fs4::available_space(&self.config.cache_root)?,
                self.config.free_space_reserve_bytes,
                "cache",
            )?;
        }
        if let Some(origin) = &self.config.origin_catalog_plan {
            let budget = origin.disk_budget_bytes.ok_or_else(|| {
                SchedulerError::Capacity(
                    "origin catalog execution requires an audited disk budget".to_string(),
                )
            })?;
            let used = directory_bytes_no_symlinks(&self.config.store_root)?;
            if used > budget {
                return Err(SchedulerError::Capacity(format!(
                    "origin catalog store uses {used} bytes, above audited budget {budget}"
                )));
            }
        }
        Ok(())
    }

    fn ingest_hours(&self, plan: &JobPlan, hours: Vec<ExpectedValidTime>) -> SchedulerResult<()> {
        if hours.is_empty() {
            return Ok(());
        }
        let queue = Arc::new(Mutex::new(VecDeque::from(hours)));
        let first_error = Arc::new(Mutex::new(None::<String>));
        let failed = Arc::new(AtomicBool::new(false));
        let workers = self.config.max_concurrent_hours.min(
            queue
                .lock()
                .map_err(|_| SchedulerError::InvalidState("hour queue mutex poisoned".to_string()))?
                .len(),
        );
        let profile = plan.ingest_profile.to_profile()?;
        thread::scope(|scope| {
            for _ in 0..workers {
                let queue = Arc::clone(&queue);
                let first_error = Arc::clone(&first_error);
                let failed = Arc::clone(&failed);
                let cancelled = Arc::clone(&self.cancelled);
                let hour_gate = Arc::clone(&self.hour_gate);
                let profile = profile.clone();
                scope.spawn(move || {
                    loop {
                        if failed.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) {
                            break;
                        }
                        let expected = match queue.lock() {
                            Ok(mut queue) => queue.pop_front(),
                            Err(_) => {
                                failed.store(true, Ordering::Release);
                                if let Ok(mut error) = first_error.lock() {
                                    error.get_or_insert_with(|| "hour queue mutex poisoned".into());
                                }
                                break;
                            }
                        };
                        let Some(expected) = expected else { break };
                        let result = (|| {
                            let _permit = hour_gate
                                .acquire(&cancelled)?
                                .ok_or_else(|| SchedulerError::Ingest("cancelled".to_string()))?;
                            if failed.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            self.check_space()?;
                            let progress = |_event| {};
                            let config = IngestConfig {
                                model: plan.model,
                                cycle: &plan.cycle,
                                source_override: plan.source_override,
                                cache_root: &self.config.cache_root,
                                use_cache: self.config.use_cache,
                                store_root: &self.config.store_root,
                                model_slug: plan.model.as_str(),
                                run_slug: &plan.run_id,
                                profile: &profile,
                                verify: self.config.verify,
                                progress: &progress,
                                cancel: &cancelled,
                            };
                            ingest_hour_serial(&config, expected.forecast_hour)
                                .map_err(|error| SchedulerError::Ingest(error.to_string()))?;
                            self.check_space()
                        })();
                        if let Err(error) = result {
                            failed.store(true, Ordering::Release);
                            if let Ok(mut first) = first_error.lock() {
                                first.get_or_insert_with(|| {
                                    format!("f{:03}: {error}", expected.forecast_hour)
                                });
                            }
                            break;
                        }
                    }
                });
            }
        });
        if self.cancelled.load(Ordering::Acquire) {
            return Err(SchedulerError::Ingest("cancelled".to_string()));
        }
        if let Some(error) = first_error
            .lock()
            .map_err(|_| SchedulerError::InvalidState("error mutex poisoned".to_string()))?
            .take()
        {
            return Err(SchedulerError::Ingest(error));
        }
        Ok(())
    }
}

fn validate_queryable_coverage(plan: &JobPlan, coverage: &RunCoverage) -> SchedulerResult<()> {
    if !coverage.matches_plan(plan)
        || !coverage.storage_validated
        || !coverage.unexpected.is_empty()
        || !coverage.slot_mismatches.is_empty()
        || coverage.validated_slots.len() != coverage.present.len()
    {
        return Err(SchedulerError::InvalidCoverage(format!(
            "run '{}' did not pass exact queryability validation",
            plan.job_id
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct ConcurrencyGate {
    limit: usize,
    active: Mutex<usize>,
    changed: Condvar,
}

impl ConcurrencyGate {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            active: Mutex::new(0),
            changed: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>, cancelled: &AtomicBool) -> SchedulerResult<Option<HourPermit>> {
        let mut active = self.active.lock().map_err(|_| {
            SchedulerError::InvalidState("hour concurrency gate mutex poisoned".to_string())
        })?;
        while *active >= self.limit {
            if cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            let (next, _) = self
                .changed
                .wait_timeout(active, Duration::from_millis(100))
                .map_err(|_| {
                    SchedulerError::InvalidState(
                        "hour concurrency gate mutex poisoned while waiting".to_string(),
                    )
                })?;
            active = next;
        }
        *active += 1;
        Ok(Some(HourPermit(Arc::clone(self))))
    }
}

struct HourPermit(Arc<ConcurrencyGate>);

impl Drop for HourPermit {
    fn drop(&mut self) {
        if let Ok(mut active) = self.0.active.lock() {
            *active = active.saturating_sub(1);
            self.0.changed.notify_one();
        }
    }
}

/// Capped exponential delay with stable, process-independent jitter. The FNV
/// key avoids randomized hash seeds, so identical job/attempt pairs retain the
/// same delay across restarts.
pub fn deterministic_jittered_delay(
    policy: RetryPolicy,
    attempt: u32,
    key: &str,
    jitter_percent: u8,
) -> SchedulerResult<u64> {
    if jitter_percent > 50 {
        return Err(SchedulerError::InvalidConfig(
            "jitter_percent must be in 0..=50".to_string(),
        ));
    }
    let base = policy.delay_after_attempt(attempt)?;
    if jitter_percent == 0 {
        return Ok(base);
    }
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in key.bytes().chain(attempt.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let span = base.saturating_mul(u64::from(jitter_percent)) / 100;
    if span == 0 {
        return Ok(base);
    }
    let width = span.saturating_mul(2).saturating_add(1);
    let offset = i128::from(hash % width) - i128::from(span);
    let jittered = (i128::from(base) + offset).max(1) as u64;
    Ok(jittered.min(policy.max_backoff_seconds))
}

pub(crate) fn space_gate(available: u64, reserve: u64, label: &str) -> SchedulerResult<()> {
    if available < reserve {
        return Err(SchedulerError::Capacity(format!(
            "{label} filesystem has {available} bytes available, below reserve {reserve}"
        )));
    }
    Ok(())
}

fn latest_registry_cycle(
    model: ModelId,
    now_unix: i64,
    rollback_days: u16,
) -> SchedulerResult<CycleSpec> {
    let now = Utc.timestamp_opt(now_unix, 0).single().ok_or_else(|| {
        SchedulerError::InvalidConfig("current timestamp is out of range".to_string())
    })?;
    let summary = model_summary(model);
    for days in 0..=rollback_days {
        let date = now
            .date_naive()
            .checked_sub_days(Days::new(u64::from(days)))
            .ok_or_else(|| SchedulerError::InvalidConfig("date rollback overflow".to_string()))?;
        for hour in summary.cycle_hours_utc.iter().rev().copied() {
            let cycle = CycleSpec::new(date.format("%Y%m%d").to_string(), hour)?;
            if cycle_origin_unix(&cycle)? <= now_unix {
                return Ok(cycle);
            }
        }
    }
    Err(SchedulerError::InvalidConfig(format!(
        "model '{model}' has no cycle in the configured rollback window"
    )))
}

fn run_json_path(store_root: &std::path::Path, plan: &JobPlan) -> PathBuf {
    store_root
        .join(plan.model.as_str())
        .join(&plan.run_id)
        .join("run.json")
}

fn directory_bytes_no_symlinks(root: &std::path::Path) -> SchedulerResult<u64> {
    const MAX_ENTRIES: usize = 1_000_000;
    let mut total = 0_u64;
    let mut seen = 0_usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            seen = seen.checked_add(1).ok_or_else(|| {
                SchedulerError::Capacity("origin store entry count overflow".to_string())
            })?;
            if seen > MAX_ENTRIES {
                return Err(SchedulerError::Capacity(format!(
                    "origin store exceeds the {MAX_ENTRIES}-entry accounting limit"
                )));
            }
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(SchedulerError::InvalidState(format!(
                    "origin store accounting refuses symlink '{}'",
                    entry.path().display()
                )));
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                total = total.checked_add(entry.metadata()?.len()).ok_or_else(|| {
                    SchedulerError::Capacity("origin store byte accounting overflow".to_string())
                })?;
            } else {
                return Err(SchedulerError::InvalidState(format!(
                    "origin store accounting refuses special entry '{}'",
                    entry.path().display()
                )));
            }
        }
    }
    Ok(total)
}

fn now_unix() -> SchedulerResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            SchedulerError::InvalidState(format!("clock before Unix epoch: {error}"))
        })?;
    i64::try_from(duration.as_secs())
        .map_err(|_| SchedulerError::InvalidState("clock exceeds timestamp range".to_string()))
}
