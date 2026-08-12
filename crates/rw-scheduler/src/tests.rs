use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use rustwx_core::{CycleSpec, GridShape, LatLonGrid, ModelId, SourceId};
use rw_store::RwsExactTime;
use rw_store::format::RwsWriterInfo;
use rw_store::ingest::{DerivedFieldInput, write_hour_from_grid_with_derived_exact};
use rw_store::run::{RwsHourEntry, RwsRunManifest, SCHEMA_RUN, SCHEMA_RUN_V2};

use super::*;

fn cycle(date: &str, hour: u8) -> CycleSpec {
    CycleSpec::new(date, hour).unwrap()
}

fn test_dir(label: &str) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "rw-scheduler-{}-{label}-{}",
        process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn writer() -> RwsWriterInfo {
    RwsWriterInfo {
        name: "rw-scheduler-test".to_string(),
        version: "0".to_string(),
        build: "test".to_string(),
    }
}

fn manifest_for_plan(
    plan: &JobPlan,
    exact: bool,
    omitted_forecast_hour: Option<u16>,
) -> RwsRunManifest {
    let mut hours = BTreeMap::new();
    for expected in &plan.expected_valid_times {
        if Some(expected.forecast_hour) == omitted_forecast_hour {
            continue;
        }
        let key = if exact {
            expected.storage_slot
        } else {
            expected.forecast_hour
        };
        hours.insert(
            key,
            RwsHourEntry {
                file: format!("f{key:03}.rws"),
                lead_seconds: exact.then_some(expected.lead_seconds),
                valid_unix: exact.then_some(expected.valid_unix),
                written_unix: 1,
                encode_ms: 1,
                variables: vec!["temp_2m".to_string()],
                source_provenance: Vec::new(),
            },
        );
    }
    RwsRunManifest {
        schema: if exact { SCHEMA_RUN_V2 } else { SCHEMA_RUN }.to_string(),
        model: plan.model.as_str().to_string(),
        run: plan.run_id.clone(),
        grid_hash: "test-grid".to_string(),
        nx: 2,
        ny: 2,
        hours,
        writer: writer(),
    }
}

#[test]
fn nbm_plan_preserves_native_irregular_cadence_and_exact_times() {
    let plan = JobPlan::build(ModelId::Nbm, cycle("20260731", 12)).unwrap();
    let hours = plan
        .expected_valid_times
        .iter()
        .map(|sample| sample.forecast_hour)
        .collect::<Vec<_>>();

    assert_eq!(hours.len(), 100);
    assert_eq!(&hours[..3], &[1, 2, 3]);
    assert_eq!(&hours[34..38], &[35, 36, 39, 42]);
    assert_eq!(&hours[86..90], &[189, 192, 198, 204]);
    assert_eq!(hours.last(), Some(&264));
    assert!(!hours.contains(&0));
    assert!(!hours.contains(&37));
    assert!(!hours.contains(&193));
    for (slot, sample) in plan.expected_valid_times.iter().enumerate() {
        assert_eq!(usize::from(sample.storage_slot), slot);
        assert_eq!(sample.lead_seconds, u64::from(sample.forecast_hour) * 3_600);
        assert_eq!(
            sample.valid_unix,
            plan.origin_unix().unwrap() + sample.lead_seconds as i64
        );
    }
    assert_eq!(plan.ingest_products[0].product, "core/co");
}

#[test]
fn plan_rejects_a_cycle_the_model_does_not_publish() {
    let error = JobPlan::build(ModelId::Gfs, cycle("20260731", 1)).unwrap_err();
    assert!(matches!(
        error,
        SchedulerError::UnsupportedCycle {
            model: ModelId::Gfs,
            cycle_hour: 1
        }
    ));
}

#[test]
fn persisted_plan_validation_is_structural_and_rejects_tampering() {
    let plan = JobPlan::build(ModelId::Nbm, cycle("20260731", 12)).unwrap();
    let round_trip: JobPlan = serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
    round_trip.validate().unwrap();

    let mut changed_time = plan.clone();
    changed_time.expected_valid_times[0].valid_unix += 60;
    assert!(changed_time.validate().is_err());

    let mut unusable_product = plan;
    unusable_product.ingest_products[0].surface_source = false;
    unusable_product.ingest_products[0].pressure_source = false;
    assert!(unusable_product.validate().is_err());
}

#[test]
fn completion_requires_opening_the_grid_and_every_hour() {
    let dir = test_dir("deep-coverage");
    let store_root = dir.join("store");
    let plan = JobPlan::build(ModelId::Rtma, cycle("20260731", 12)).unwrap();
    assert_eq!(plan.expected_valid_times.len(), 1);
    let grid = LatLonGrid::new(
        GridShape::new(2, 2).unwrap(),
        vec![40.0, 40.0, 41.0, 41.0],
        vec![-100.0, -99.0, -100.0, -99.0],
    )
    .unwrap();
    let values = [280.0, 281.0, 282.0, 283.0];
    let time = plan.expected_valid_times[0];
    write_hour_from_grid_with_derived_exact(
        &store_root,
        plan.model.as_str(),
        &plan.run_id,
        time.storage_slot,
        RwsExactTime::new(time.lead_seconds, time.valid_unix),
        &grid,
        None,
        &[],
        &[DerivedFieldInput {
            name: "temperature_2m",
            units: "K",
            values: &values,
        }],
        &[],
        "rw-scheduler-test",
        1_800_000_000,
    )
    .unwrap();
    let run_json = store_root
        .join(plan.model.as_str())
        .join(&plan.run_id)
        .join("run.json");
    let coverage = verify_run_json(&plan, &run_json).unwrap();
    assert!(coverage.is_complete());
    assert!(coverage.storage_validated);
    assert_eq!(coverage.validated_slots, BTreeSet::from([0]));
    assert_eq!(
        coverage.variable_slots["temperature_2m"],
        BTreeSet::from([0])
    );

    let hour = run_json.parent().unwrap().join("f000.rws");
    fs::remove_file(hour).unwrap();
    assert!(verify_run_json(&plan, &run_json).is_err());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn completeness_detects_an_interior_gap_even_when_later_times_exist() {
    let plan = JobPlan::build(ModelId::Nbm, cycle("20260731", 12)).unwrap();
    let manifest = manifest_for_plan(&plan, false, Some(99));
    let coverage = verify_manifest(&plan, &manifest).unwrap();

    assert!(!coverage.is_complete());
    assert_eq!(coverage.missing.len(), 1);
    assert!(coverage.missing.contains(&ValidTime {
        lead_seconds: 99 * 3_600,
        valid_unix: plan.origin_unix().unwrap() + 99 * 3_600,
    }));
    assert!(
        coverage
            .available
            .iter()
            .any(|time| time.lead_seconds == 264 * 3_600)
    );
}

#[test]
fn exact_manifest_requires_the_expected_ordinal_slots() {
    let plan = JobPlan::build(ModelId::Gdas, cycle("20260731", 12)).unwrap();
    let mut manifest = manifest_for_plan(&plan, true, None);
    let shifted = manifest.hours.clone();
    manifest.hours.clear();
    for (slot, mut entry) in shifted {
        let shifted_slot = slot + 1;
        entry.file = format!("f{shifted_slot:03}.rws");
        manifest.hours.insert(shifted_slot, entry);
    }

    let coverage = verify_manifest(&plan, &manifest).unwrap();
    assert!(coverage.missing.is_empty());
    assert!(!coverage.slot_mismatches.is_empty());
    assert!(!coverage.is_complete());
}

#[test]
fn running_state_is_durably_recovered_to_queued() {
    let dir = test_dir("restart");
    let store = JobStateStore::new(&dir);
    let policy = RetryPolicy::new(3, 5, 20).unwrap();
    let mut record = JobRecord::new(
        JobPlan::build(ModelId::Hrrr, cycle("20260731", 0)).unwrap(),
        10,
    )
    .unwrap();
    record.start(11, policy).unwrap();
    store.save(&record).unwrap();

    let recovered = store.recover_running(20).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].state, JobState::Queued);
    assert_eq!(recovered[0].attempts, 0);
    assert_eq!(recovered[0].recovery_count, 1);

    let loaded = store.load(&record.plan.job_id).unwrap();
    assert_eq!(loaded, recovered[0]);
    let names = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(names, vec![format!("{}.json", record.plan.job_id)]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn retry_backoff_caps_and_the_final_attempt_is_terminal() {
    let policy = RetryPolicy::new(3, 5, 9).unwrap();
    let mut record = JobRecord::new(
        JobPlan::build(ModelId::Hrrr, cycle("20260731", 0)).unwrap(),
        0,
    )
    .unwrap();

    record.start(0, policy).unwrap();
    record.finish_failure(10, "first", policy).unwrap();
    assert_eq!(record.state, JobState::RetryBackoff { retry_at_unix: 15 });
    assert!(record.release_retry(14).is_err());
    record.release_retry(15).unwrap();

    record.start(15, policy).unwrap();
    record.finish_failure(20, "second", policy).unwrap();
    assert_eq!(record.state, JobState::RetryBackoff { retry_at_unix: 29 });
    record.release_retry(29).unwrap();

    record.start(29, policy).unwrap();
    record.finish_failure(30, "third", policy).unwrap();
    assert_eq!(record.state, JobState::Failed { finished_unix: 30 });
    assert_eq!(record.attempts, 3);
}

fn candidate(date: &str, hour: u8, complete: bool, times: &[i64]) -> RunCandidate {
    let cycle = cycle(date, hour);
    RunCandidate::new(
        ModelId::Hrrr,
        cycle.clone(),
        canonical_run_id(&cycle),
        complete,
        times.iter().copied().collect(),
    )
    .unwrap()
}
#[test]
fn aliases_distinguish_complete_available_and_covering_runs() {
    let older = candidate("20260731", 0, true, &[100, 200]);
    let newer = candidate("20260731", 1, false, &[200, 300]);
    let candidates = vec![older, newer];

    assert_eq!(
        select_latest(ModelId::Hrrr, &candidates).unwrap().run_id(),
        "20260731_00z"
    );
    assert_eq!(
        select_latest_available(ModelId::Hrrr, &candidates)
            .unwrap()
            .run_id(),
        "20260731_01z"
    );
    assert_eq!(
        select_latest_covering(ModelId::Hrrr, &candidates, &BTreeSet::from([100, 200]))
            .unwrap()
            .run_id(),
        "20260731_00z"
    );
}

fn retention_run(date: &str, hour: u8, active: bool) -> RetentionRun {
    let cycle = cycle(date, hour);
    RetentionRun::new(
        ModelId::Hrrr,
        cycle.clone(),
        canonical_run_id(&cycle),
        active,
    )
    .unwrap()
}

#[test]
fn retention_never_deletes_active_aliased_or_newest_runs() {
    let old_unprotected = retention_run("20260730", 18, false);
    let aliased = retention_run("20260731", 0, false);
    let active = retention_run("20260731", 6, true);
    let newest = retention_run("20260731", 12, false);
    let aliased_keys = BTreeSet::from([aliased.key().clone()]);
    let runs = vec![
        old_unprotected.clone(),
        aliased.clone(),
        active.clone(),
        newest.clone(),
    ];

    let plan = plan_retention(&runs, &aliased_keys, 1).unwrap();
    assert_eq!(plan.delete, BTreeSet::from([old_unprotected.key().clone()]));
    assert!(plan.keep.contains(aliased.key()));
    assert!(plan.keep.contains(active.key()));
    assert!(plan.keep.contains(newest.key()));
}

#[test]
fn hostile_identifiers_and_mismatched_state_filenames_are_rejected() {
    let valid_cycle = cycle("20260731", 0);
    for hostile in ["../escape", "nested/run", "nested\\run", "C:\\run", "CON"] {
        assert!(
            RunCandidate::new(
                ModelId::Hrrr,
                valid_cycle.clone(),
                hostile,
                false,
                BTreeSet::new()
            )
            .is_err(),
            "'{hostile}' should be rejected"
        );
        assert!(RunKey::new(ModelId::Hrrr, hostile).is_err());
    }

    let dir = test_dir("hostile-state");
    let store = JobStateStore::new(&dir);
    assert!(store.load("../escape").is_err());
    let record = JobRecord::new(JobPlan::build(ModelId::Hrrr, valid_cycle).unwrap(), 0).unwrap();
    store.save(&record).unwrap();
    fs::copy(
        dir.join(format!("{}.json", record.plan.job_id)),
        dir.join("different.json"),
    )
    .unwrap();
    assert!(store.load_all().is_err());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn capacity_admission_respects_running_queue_and_fairness_bounds() {
    let limits = SchedulerLimits::new(2, 3).unwrap();
    assert_eq!(limits.admit(0, 0).unwrap(), AdmissionDecision::StartNow);
    assert_eq!(limits.admit(1, 1).unwrap(), AdmissionDecision::Queue);
    assert_eq!(limits.admit(2, 2).unwrap(), AdmissionDecision::Queue);
    assert_eq!(limits.admit(2, 3).unwrap(), AdmissionDecision::AtCapacity);
    assert!(limits.admit(3, 0).is_err());
    assert!(limits.admit(0, 4).is_err());
    assert!(SchedulerLimits::new(0, 1).is_err());
}

fn scheduler_config(label: &str, models: &[&str]) -> config::SchedulerConfig {
    let root = test_dir(label);
    config::SchedulerConfig {
        store_root: root.join("store"),
        cache_root: root.join("cache"),
        state_root: root.join("state"),
        models: models.iter().map(|model| (*model).to_string()).collect(),
        free_space_reserve_bytes: 0,
        ..config::SchedulerConfig::default()
    }
}

#[test]
fn config_requires_an_allowlist_and_selects_limitation_safe_profiles() {
    let empty = scheduler_config("empty-config", &[]);
    assert!(empty.validate().is_err());

    let all = scheduler_config("all-ready", &["all_ready"]);
    let expanded = all.expanded_models().unwrap();
    assert!(expanded.contains(&ModelId::Hrrr));
    assert!(expanded.contains(&ModelId::Rtma));

    let surface = scheduler_config("surface-profile", &["hiresw"]);
    let profile = surface.profile_for(ModelId::Hiresw).unwrap();
    assert!(!profile.needs_prs());
    assert!(!profile.derived && !profile.heavy);
    assert!(profile.includes_surface_field("composite_reflectivity"));
    assert!(profile.includes_surface_field("apcp_run_total"));

    let nbm = scheduler_config("nbm-surface-profile", &["nbm"]);
    let profile = nbm.profile_for(ModelId::Nbm).unwrap();
    assert!(!profile.needs_prs());
    assert!(profile.includes_surface_field("apcp_run_total"));
    assert!(profile.includes_surface_field("pwat"));

    let mut unsafe_surface = scheduler_config("unsafe-surface-profile", &["hiresw"]);
    unsafe_surface
        .model_profiles
        .insert("hiresw".to_string(), "view".to_string());
    assert!(unsafe_surface.validate().is_err());

    let mut typo = scheduler_config("profile-key-typo", &["hrrr"]);
    typo.model_profiles
        .insert("HRRR".to_string(), "view".to_string());
    assert!(typo.validate().is_err());

    let ensemble = scheduler_config("ensemble-profile", &["href"]);
    let profile = ensemble.profile_for(ModelId::Href).unwrap();
    assert!(!profile.derived && !profile.heavy);

    for model in [ModelId::Aigefs, ModelId::Hgefs] {
        let config = scheduler_config(&format!("ensemble-profile-{model}"), &[model.as_str()]);
        let profile = config.profile_for(model).unwrap();
        assert!(!profile.derived && !profile.heavy);
    }
}

#[test]
fn config_bounds_provider_discovery_timeouts() {
    let mut config = scheduler_config("discovery-timeouts", &["hrrr"]);
    config.discovery_timeout_seconds = 5;
    config.discovery_probe_timeout_seconds = 6;
    assert!(config.validate().is_err());
    config.discovery_probe_timeout_seconds = 5;
    assert!(config.validate().is_ok());
    config.discovery_timeout_seconds = 0;
    assert!(config.validate().is_err());
}

#[test]
fn scheduler_config_validates_the_planning_only_origin_subset() {
    let mut config = scheduler_config("origin-plan", &["hrrr", "gfs", "nbm"]);
    config.origin_catalog_plan = Some(crate::origin::OriginCatalogPlanConfig::default());
    assert!(config.validate().is_ok());

    let mut missing_lane_model = scheduler_config("origin-plan-missing", &["hrrr", "gfs"]);
    missing_lane_model.origin_catalog_plan =
        Some(crate::origin::OriginCatalogPlanConfig::default());
    assert!(missing_lane_model.validate().is_err());

    config
        .model_profiles
        .insert("nbm".to_string(), "analysis".to_string());
    assert!(config.validate().is_err());
}

#[test]
fn state_store_removes_only_terminal_records() {
    let dir = test_dir("remove-terminal-state");
    let store = JobStateStore::new(&dir);
    let mut terminal = JobRecord::new(
        JobPlan::build(ModelId::Hrrr, cycle("20260731", 0)).unwrap(),
        1,
    )
    .unwrap();
    let policy = RetryPolicy::new(1, 1, 1).unwrap();
    terminal.start(1, policy).unwrap();
    terminal.finish_failure(2, "done", policy).unwrap();
    store.save(&terminal).unwrap();
    store.remove_terminal(&terminal.plan.job_id).unwrap();
    assert!(store.load_all().unwrap().is_empty());

    let active = JobRecord::new(
        JobPlan::build(ModelId::Hrrr, cycle("20260731", 1)).unwrap(),
        1,
    )
    .unwrap();
    store.save(&active).unwrap();
    assert!(store.remove_terminal(&active.plan.job_id).is_err());
    assert_eq!(store.load_all().unwrap(), vec![active]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn durable_plan_pins_profile_and_provider() {
    let profile = rw_ingest::ingest_profile::IngestProfile::analysis();
    let plan = JobPlan::build_with_profile_and_source(
        ModelId::Rtma,
        cycle("20260731", 12),
        &profile,
        Some(rustwx_core::SourceId::Aws),
    )
    .unwrap();
    let round_trip: JobPlan = serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
    assert_eq!(round_trip, plan);
    assert_eq!(round_trip.source_override, Some(rustwx_core::SourceId::Aws));
    assert_eq!(round_trip.ingest_profile.to_profile().unwrap(), profile);

    let mut constrained = JobPlan::build(ModelId::Href, cycle("20260731", 12)).unwrap();
    assert!(
        constrained
            .capability_limitations
            .contains(&"derived_products_disabled".to_string())
    );
    constrained.ingest_profile.derived = true;
    assert!(constrained.validate().is_err());

    assert!(
        JobPlan::build_with_profile_and_source(
            ModelId::Rtma,
            cycle("20260731", 12),
            &rw_ingest::ingest_profile::IngestProfile::analysis(),
            Some(rustwx_core::SourceId::Ecmwf),
        )
        .is_err()
    );
}

#[test]
fn aifs_scheduler_accepts_only_the_ecmwf_remote_source() {
    let mut profile = rw_ingest::ingest_profile::IngestProfile::full();
    profile.derived = false;
    profile.heavy = false;

    for source in [SourceId::AifsInference, SourceId::Earth2Archive] {
        let mut config = scheduler_config("aifs-local-source", &["aifs"]);
        config.source = Some(source.to_string());
        assert!(matches!(
            config.source_for(ModelId::Aifs),
            Err(SchedulerError::InvalidConfig(_))
        ));
        assert!(matches!(
            JobPlan::build_with_profile_and_source(
                ModelId::Aifs,
                cycle("20260731", 12),
                &profile,
                Some(source),
            ),
            Err(SchedulerError::InvalidPlan(_))
        ));
    }

    let mut config = scheduler_config("aifs-ecmwf-source", &["aifs"]);
    config.source = Some(SourceId::Ecmwf.to_string());
    assert_eq!(
        config.source_for(ModelId::Aifs).unwrap(),
        Some(SourceId::Ecmwf)
    );
    let plan = JobPlan::build_with_profile_and_source(
        ModelId::Aifs,
        cycle("20260731", 12),
        &profile,
        Some(SourceId::Ecmwf),
    )
    .unwrap();
    plan.validate().unwrap();

    for source in [SourceId::AifsInference, SourceId::Earth2Archive] {
        let mut tampered = plan.clone();
        tampered.source_override = Some(source);
        assert!(matches!(
            tampered.validate(),
            Err(SchedulerError::InvalidPlan(_))
        ));
    }
}

#[test]
fn registry_plan_uses_the_latest_non_future_utc_cycle() {
    let config = scheduler_config("offline-plan", &["hrrr"]);
    let host = SchedulerHost::new(config).unwrap();
    let now = Utc
        .with_ymd_and_hms(2026, 7, 31, 4, 30, 0)
        .single()
        .unwrap()
        .timestamp();
    let plans = host.plan_at(now).unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].cycle, cycle("20260731", 4));
}

#[test]
fn every_ready_model_has_a_valid_cadence_profile_and_remote_source() {
    use rustwx_models::{model_summary, supported_forecast_hours};
    use rw_ingest::{
        IngestSupportStatus, model_ingest_capabilities, model_source_ingest_supported,
    };

    let mut ready = 0;
    for capability in model_ingest_capabilities()
        .into_iter()
        .filter(|capability| capability.status == IngestSupportStatus::Ready)
    {
        ready += 1;
        let model = capability.model;
        let summary = model_summary(model);
        let cycle_hour = *summary
            .cycle_hours_utc
            .first()
            .expect("a ready model must publish a cycle");
        let cycle = cycle("20260731", cycle_hour);
        let config = scheduler_config(&format!("ready-{model}"), &[model.as_str()]);
        let profile = config.profile_for(model).unwrap();
        let supported_sources = summary
            .sources
            .iter()
            .filter(|source| model_source_ingest_supported(model, source.id))
            .map(|source| source.id)
            .collect::<Vec<_>>();
        assert!(
            !supported_sources.is_empty(),
            "ready model '{model}' needs a remote scheduler source"
        );
        for source in summary.sources {
            let result = JobPlan::build_with_profile_and_source(
                model,
                cycle.clone(),
                &profile,
                Some(source.id),
            );
            assert_eq!(
                result.is_ok(),
                model_source_ingest_supported(model, source.id),
                "model '{model}' source '{}' scheduler compatibility drifted",
                source.id
            );
        }
        let plan = JobPlan::build_with_profile_and_source(
            model,
            cycle,
            &profile,
            Some(supported_sources[0]),
        )
        .unwrap();
        assert_eq!(
            plan.expected_valid_times
                .iter()
                .map(|expected| expected.forecast_hour)
                .collect::<Vec<_>>(),
            supported_forecast_hours(model, cycle_hour),
            "ready model '{model}' must preserve its native registry cadence"
        );
        plan.validate().unwrap();
    }
    assert_eq!(ready, 21);
}

#[test]
fn retry_jitter_is_stable_positive_and_capped() {
    let policy = RetryPolicy::new(8, 60, 300).unwrap();
    let first = deterministic_jittered_delay(policy, 3, "hrrr-20260731-00z", 20).unwrap();
    let again = deterministic_jittered_delay(policy, 3, "hrrr-20260731-00z", 20).unwrap();
    assert_eq!(first, again);
    assert!((192..=288).contains(&first));
    assert!(deterministic_jittered_delay(policy, 8, "same", 50).unwrap() <= 300);
    assert!(deterministic_jittered_delay(policy, 1, "same", 51).is_err());
}

#[test]
fn provider_discovery_honors_shutdown_before_any_network_probe() {
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let discovery = ProviderCycleDiscovery::with_cancellation(
        Duration::from_secs(30),
        Duration::from_secs(6),
        cancelled,
    )
    .unwrap();
    let started = Instant::now();
    let error = discovery
        .discover(ModelId::Hrrr, None, 1_775_000_000, 2)
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(started.elapsed() < Duration::from_secs(1));
}

struct FixedDiscovery;

impl CycleDiscovery for FixedDiscovery {
    fn discover(
        &self,
        model: ModelId,
        _source: Option<rustwx_core::SourceId>,
        _now_unix: i64,
        _rollback_days: u16,
    ) -> SchedulerResult<executor::DiscoveredCycle> {
        assert_eq!(model, ModelId::Rtma);
        Ok(executor::DiscoveredCycle {
            cycle: cycle("20260731", 12),
            source: rustwx_core::SourceId::Aws,
        })
    }
}

struct BlockingDiscovery {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl CycleDiscovery for BlockingDiscovery {
    fn discover(
        &self,
        _model: ModelId,
        _source: Option<SourceId>,
        _now_unix: i64,
        _rollback_days: u16,
    ) -> SchedulerResult<executor::DiscoveredCycle> {
        self.entered.wait();
        self.release.wait();
        Ok(executor::DiscoveredCycle {
            cycle: cycle("20260731", 12),
            source: SourceId::Aws,
        })
    }
}

#[test]
fn host_lease_rejects_a_second_scheduler_process() {
    let mut config = scheduler_config("host-lease", &["rtma"]);
    config.free_space_reserve_bytes = u64::MAX;
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let first = SchedulerHost::with_discovery(
        config.clone(),
        Arc::new(BlockingDiscovery {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
    )
    .unwrap();
    let worker = std::thread::spawn(move || first.run_once_at(1_775_000_000));
    entered.wait();
    let second = SchedulerHost::with_discovery(config, Arc::new(FixedDiscovery)).unwrap();
    assert!(matches!(
        second.run_once_at(1_775_000_000),
        Err(SchedulerError::Capacity(_))
    ));
    release.wait();
    worker.join().unwrap().unwrap();
}

#[test]
fn local_run_once_persists_retry_without_touching_the_network() {
    let mut config = scheduler_config("local-run-once", &["rtma"]);
    config.free_space_reserve_bytes = u64::MAX;
    let state_root = config.state_root.clone();
    let host = SchedulerHost::with_discovery(config, Arc::new(FixedDiscovery)).unwrap();
    let report = host.run_once_at(1_775_000_000).unwrap();
    assert_eq!(report.admitted, vec!["rtma-20260731-12z"]);
    assert_eq!(report.retrying, vec!["rtma-20260731-12z"]);
    let records = JobStateStore::new(state_root).load_all().unwrap();
    assert_eq!(records.len(), 1);
    assert!(matches!(records[0].state, JobState::RetryBackoff { .. }));
    assert_eq!(records[0].attempts, 1);
}

#[test]
fn retention_executes_only_marked_scheduler_owned_runs() {
    let dir = test_dir("retention-execute");
    let store_root = dir.join("store");
    fs::create_dir_all(&store_root).unwrap();
    let mut records = Vec::new();
    for (date, hour) in [("20260730", 18), ("20260731", 0)] {
        let plan = JobPlan::build(ModelId::Rtma, cycle(date, hour)).unwrap();
        let mut record = JobRecord::new(plan, 1).unwrap();
        record.state = JobState::Failed { finished_unix: 2 };
        record.attempts = 1;
        record.last_error = Some("test terminal state".to_string());
        let run_dir = store_root
            .join(record.plan.model.as_str())
            .join(&record.plan.run_id);
        crate::retention::ensure_owner_marker(&run_dir, &record).unwrap();
        records.push(record);
    }
    let plan = plan_owned_retention(&records, &BTreeSet::new(), 1).unwrap();
    assert_eq!(plan.delete.len(), 1);
    let old = store_root.join("rtma").join("20260730_18z");
    let dry = execute_retention(&store_root, &records, &plan, true).unwrap();
    assert!(dry.deleted.is_empty());
    assert!(old.is_dir());
    let applied = execute_retention(&store_root, &records, &plan, false).unwrap();
    assert_eq!(applied.state_prunable, vec!["rtma:20260730_18z"]);
    if cfg!(windows) {
        assert_eq!(applied.purged_shells, vec!["rtma:20260730_18z"]);
        assert!(old.join(".rw-scheduler-purged.json").is_file());
    } else {
        assert_eq!(applied.deleted, vec!["rtma:20260730_18z"]);
        assert!(!old.exists());
    }
    assert!(store_root.join("rtma").join("20260731_00z").is_dir());
}

#[test]
fn applied_retention_can_prune_state_for_an_already_absent_run() {
    let dir = test_dir("retention-absent-state");
    let store_root = dir.join("store");
    fs::create_dir_all(&store_root).unwrap();
    let plan = JobPlan::build(ModelId::Rtma, cycle("20260730", 18)).unwrap();
    let mut record = JobRecord::new(plan, 1).unwrap();
    record.state = JobState::Failed { finished_unix: 2 };
    record.attempts = 1;
    record.last_error = Some("terminal".to_string());
    let retention = plan_owned_retention(&[record.clone()], &BTreeSet::new(), 0).unwrap();
    let report = execute_retention(&store_root, &[record], &retention, false).unwrap();
    assert_eq!(report.state_prunable, vec!["rtma:20260730_18z"]);
    assert_eq!(report.skipped, vec!["rtma:20260730_18z: absent"]);
    let _ = fs::remove_dir_all(dir);
}
