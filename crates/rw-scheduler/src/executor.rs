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

use crate::config::SchedulerConfig;
use crate::coverage::{RunCoverage, ValidTime, verify_run_json};
use crate::error::{SchedulerError, SchedulerResult};
use crate::plan::{ExpectedValidTime, JobPlan, cycle_origin_unix};
use crate::retention::{
    RetentionExecution, ensure_owner_marker, execute_retention, plan_owned_retention,
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
        if !supported_forecast_hours(model, hour_utc).contains(&forecast_hour) {
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExecutionReport {
    pub admitted: Vec<String>,
    pub skipped: Vec<String>,
    pub succeeded: Vec<String>,
    pub retrying: Vec<String>,
    pub failed: Vec<String>,
    pub discovery_errors: BTreeMap<String, String>,
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
}

#[derive(Clone)]
pub struct SchedulerHost {
    config: SchedulerConfig,
    discovery: Arc<dyn CycleDiscovery>,
    cancelled: Arc<AtomicBool>,
    hour_gate: Arc<ConcurrencyGate>,
}

struct HostLease {
    file: File,
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
            config,
            discovery,
            cancelled,
            hour_gate,
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
        let mut report = StatusReport {
            queued: 0,
            running: 0,
            retry_backoff: 0,
            succeeded: 0,
            failed: 0,
            records,
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
        let allowlist = self
            .config
            .expanded_models()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut report = DiscoveryReport::default();
        for (model, result) in self.discover_allowed(&allowlist, now)? {
            match result {
                Ok(discovered) => report.discovered.push(DiscoveredModelCycle {
                    model,
                    cycle: discovered.cycle,
                    source: discovered.source,
                }),
                Err(error) => {
                    report.errors.insert(model.as_str().to_string(), error);
                }
            }
        }
        Ok(report)
    }

    pub fn run_once(&self) -> SchedulerResult<ExecutionReport> {
        self.run_once_at(now_unix()?)
    }

    pub fn run_once_at(&self, now: i64) -> SchedulerResult<ExecutionReport> {
        self.config.prepare_roots()?;
        let _host_lease = HostLease::acquire(&self.config.state_root)?;
        if self.cancelled.load(Ordering::Acquire) {
            return Err(SchedulerError::InvalidState(
                "scheduler shutdown has been requested".to_string(),
            ));
        }
        let store = JobStateStore::new(&self.config.state_root);
        let recovered = store.recover_running(now)?;
        let allowlist = self
            .config
            .expanded_models()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut report = ExecutionReport::default();
        // Persisted active work is scheduled before discovering a newer cycle;
        // restart cannot strand yesterday's queued/retrying job.
        let mut records = recovered
            .into_iter()
            .filter(|record| allowlist.contains(&record.plan.model) && record.state.is_active())
            .map(|record| (record.plan.job_id.clone(), record))
            .collect::<BTreeMap<_, _>>();
        let mut newly_admitted = Vec::new();

        for (model, discovery) in self.discover_allowed(&allowlist, now)? {
            let discovered = match discovery {
                Ok(discovered) => discovered,
                Err(error) => {
                    report
                        .discovery_errors
                        .insert(model.as_str().to_string(), error);
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
            let record = if state_path.exists() {
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
                JobState::Succeeded { .. } => {
                    let run_json = run_json_path(&self.config.store_root, &record.plan);
                    match verify_run_json(&record.plan, &run_json) {
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
                    }
                }
                JobState::RetryBackoff { .. } | JobState::Failed { .. } => {
                    report.skipped.push(record.plan.job_id.clone());
                }
                JobState::Running { .. } => unreachable!("running jobs were recovered"),
            }
        }
        runnable.sort_by(|left, right| left.plan.job_id.cmp(&right.plan.job_id));

        for chunk in runnable.chunks(self.config.max_concurrent_jobs) {
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

        if self.config.retention.enabled {
            let records = store
                .load_all()?
                .into_iter()
                .filter(|record| allowlist.contains(&record.plan.model))
                .collect::<Vec<_>>();
            let plan = plan_owned_retention(
                &records,
                &BTreeSet::new(),
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

    fn discover_allowed(
        &self,
        allowlist: &BTreeSet<ModelId>,
        now: i64,
    ) -> SchedulerResult<Vec<(ModelId, Result<DiscoveredCycle, String>)>> {
        let models = allowlist.iter().copied().collect::<Vec<_>>();
        let mut all = Vec::with_capacity(models.len());
        for chunk in models.chunks(self.config.max_concurrent_jobs) {
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
            Ok(coverage) => record.finish_success(finished, &coverage)?,
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
                                .map(|_| ())
                                .map_err(|error| SchedulerError::Ingest(error.to_string()))
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

fn now_unix() -> SchedulerResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            SchedulerError::InvalidState(format!("clock before Unix epoch: {error}"))
        })?;
    i64::try_from(duration.as_secs())
        .map_err(|_| SchedulerError::InvalidState("clock exceeds timestamp range".to_string()))
}
