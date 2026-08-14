use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use rustwx_core::{CycleSpec, GridShape, LatLonGrid, ModelId, SourceId};
use rw_ingest::ingest_profile::{FieldSet, IngestProfile};
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
fn noaa_wave1_plans_pin_ingest_products_and_native_cadence() {
    let cases = [
        (ModelId::HrrrAk, 0, 49, 48, vec!["prs", "sfc"]),
        (ModelId::HrrrAk, 1, 19, 18, vec!["prs", "sfc"]),
        (ModelId::Rap, 0, 22, 21, vec!["awp130pgrb"]),
        (ModelId::Rap, 3, 52, 51, vec!["awp130pgrb"]),
        (ModelId::Nam, 0, 53, 84, vec!["awip3d"]),
        (ModelId::Gdas, 0, 10, 9, vec!["pgrb2.0p25"]),
    ];
    for (model, cycle_hour, expected_count, expected_last, products) in cases {
        let plan = JobPlan::build(model, cycle("20260812", cycle_hour)).unwrap();
        assert_eq!(plan.expected_valid_times.len(), expected_count, "{model}");
        assert_eq!(
            plan.expected_valid_times.last().unwrap().forecast_hour,
            expected_last,
            "{model} {cycle_hour:02}z horizon"
        );
        assert_eq!(
            plan.ingest_products
                .iter()
                .map(|product| product.product.as_str())
                .collect::<Vec<_>>(),
            products,
            "{model} scheduler product contract"
        );
        for (slot, expected) in plan.expected_valid_times.iter().enumerate() {
            assert_eq!(usize::from(expected.storage_slot), slot);
            assert_eq!(
                expected.lead_seconds,
                u64::from(expected.forecast_hour) * 3_600
            );
        }
        plan.validate().unwrap();
    }
}

#[test]
fn eccc_regional_plans_pin_logical_products_and_hourly_native_cadence() {
    for (model, expected_count, expected_last) in
        [(ModelId::Rdps, 85, 84), (ModelId::Hrdps, 49, 48)]
    {
        for cycle_hour in [0, 6, 12, 18] {
            let plan = JobPlan::build(model, cycle("20260814", cycle_hour)).unwrap();
            assert_eq!(plan.expected_valid_times.len(), expected_count, "{model}");
            assert_eq!(
                plan.expected_valid_times.last().unwrap().forecast_hour,
                expected_last,
                "{model} {cycle_hour:02}z horizon"
            );
            assert_eq!(
                plan.ingest_products
                    .iter()
                    .map(|product| product.product.as_str())
                    .collect::<Vec<_>>(),
                vec!["rws-pressure", "rws-surface"]
            );
            assert!(!plan.ingest_profile.derived);
            assert!(!plan.ingest_profile.heavy);
            assert_eq!(
                plan.capability_limitations,
                vec![
                    "sparse_pressure_levels".to_string(),
                    "derived_products_disabled".to_string(),
                ]
            );
            plan.validate().unwrap();
        }
        assert!(JobPlan::build(model, cycle("20260814", 3)).is_err());
    }
}

#[test]
fn dwd_icon_plans_pin_component_bundles_native_cadence_and_source() {
    let cases = [
        (ModelId::IconEu, 0, 93, 120),
        (ModelId::IconEu, 3, 34, 48),
        (ModelId::IconD2, 0, 49, 48),
        (ModelId::IconD2, 21, 49, 48),
    ];
    for (model, cycle_hour, expected_count, expected_last) in cases {
        let profile = IngestProfile::sounding();
        let plan = JobPlan::build_with_profile_and_source(
            model,
            cycle("20260814", cycle_hour),
            &profile,
            Some(SourceId::Dwd),
        )
        .unwrap();
        assert_eq!(plan.expected_valid_times.len(), expected_count, "{model}");
        assert_eq!(
            plan.expected_valid_times.last().unwrap().forecast_hour,
            expected_last,
            "{model} {cycle_hour:02}z horizon"
        );
        assert_eq!(
            plan.ingest_products
                .iter()
                .map(|product| product.product.as_str())
                .collect::<Vec<_>>(),
            vec!["rws-pressure", "rws-surface"]
        );
        assert_eq!(plan.source_override, Some(SourceId::Dwd));
        plan.validate().unwrap();

        assert!(
            JobPlan::build_with_profile_and_source(
                model,
                cycle("20260814", cycle_hour),
                &profile,
                Some(SourceId::Nomads),
            )
            .is_err(),
            "{model} must not silently substitute a non-DWD transport"
        );
    }
}

#[test]
fn geps_plan_pins_published_statistics_profile_and_invariant_cadence() {
    let plan = JobPlan::build_with_profile_and_source(
        ModelId::Geps,
        cycle("20260814", 0),
        &rw_ingest::ingest_profile::IngestProfile::surface(),
        Some(SourceId::Eccc),
    )
    .unwrap();
    let hours = plan
        .expected_valid_times
        .iter()
        .map(|time| time.forecast_hour)
        .collect::<Vec<_>>();
    assert_eq!(hours.len(), 96);
    assert_eq!(hours.first(), Some(&3));
    assert_eq!(hours.last(), Some(&384));
    assert!(hours.contains(&192));
    assert!(hours.contains(&198));
    assert!(!hours.contains(&195));
    assert!(!hours.contains(&201));
    assert_eq!(plan.source_override, Some(SourceId::Eccc));
    assert_eq!(plan.ingest_products.len(), 1);
    assert_eq!(plan.ingest_products[0].product, "rws-published-statistics");
    assert!(plan.ingest_products[0].surface_source);
    assert!(!plan.ingest_products[0].pressure_source);
    assert_eq!(
        plan.capability_limitations,
        vec![
            "provider_statistics_only".to_string(),
            "sparse_pressure_levels".to_string(),
            "two_dimensional_statistics_only".to_string(),
            "derived_products_disabled".to_string(),
            "extended_range_not_scheduled".to_string(),
        ]
    );
    assert_eq!(
        plan.ingest_profile.to_profile().unwrap(),
        rw_ingest::ingest_profile::IngestProfile::surface()
    );
    plan.validate().unwrap();

    assert!(JobPlan::build(ModelId::Geps, cycle("20260814", 6)).is_err());
    assert!(
        JobPlan::build_with_profile(
            ModelId::Geps,
            cycle("20260814", 0),
            &rw_ingest::ingest_profile::IngestProfile::full(),
        )
        .is_err()
    );
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

fn completed_origin_config(label: &str) -> config::SchedulerConfig {
    let mut config = scheduler_config(label, &["hrrr", "gfs", "nbm"]);
    config.max_concurrent_jobs = 4;
    config.retention.enabled = true;
    config.retention.dry_run = true;
    config.retention.keep_latest_per_model = 1;
    config.origin_catalog_plan = Some(origin::OriginCatalogPlanConfig {
        capacity_audit: origin::CapacityAuditStatus::Complete,
        disk_budget_bytes: Some(u64::MAX),
        max_concurrent_jobs: Some(2),
        ..origin::OriginCatalogPlanConfig::default()
    });
    config
}

#[test]
fn config_requires_an_allowlist_and_selects_limitation_safe_profiles() {
    let empty = scheduler_config("empty-config", &[]);
    assert!(empty.validate().is_err());

    let all = scheduler_config("all-ready", &["all_ready"]);
    let expanded = all.expanded_models().unwrap();
    assert!(expanded.contains(&ModelId::Hrrr));
    assert!(expanded.contains(&ModelId::IconRu));
    assert!(expanded.contains(&ModelId::Rtma));
    assert!(expanded.contains(&ModelId::CmaGeps));

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

    let cma_geps = scheduler_config("cma-geps-statistics-profile", &["cma-geps"]);
    let profile = cma_geps.profile_for(ModelId::CmaGeps).unwrap();
    assert!(!profile.needs_prs());
    assert!(!profile.derived && !profile.heavy);
    assert!(matches!(
        profile.surface_fields,
        FieldSet::Named(ref names) if names.len() == 57
    ));
    assert!(profile.includes_surface_field("temperature_2m_p50"));
    assert!(profile.includes_surface_field("wind_speed_10m_probability_gt_15ms"));
    let plan = JobPlan::build_with_profile_and_source(
        ModelId::CmaGeps,
        cycle("20260731", 0),
        &profile,
        Some(SourceId::Cma),
    )
    .expect("CMA provider-statistics lane is schedulable");
    assert_eq!(
        plan.ingest_profile.to_profile().unwrap().surface_fields,
        profile.surface_fields
    );
    assert_eq!(plan.expected_valid_times.len(), 74);
    assert_eq!(
        plan.expected_valid_times
            .last()
            .map(|time| time.forecast_hour),
        Some(360)
    );
    assert!(
        plan.capability_limitations
            .contains(&"provider_statistics_only".to_string())
    );

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

    for model in [
        ModelId::Aigfs,
        ModelId::Aigefs,
        ModelId::Hgefs,
        ModelId::EcmwfOpenData,
        ModelId::Rdps,
        ModelId::Hrdps,
        ModelId::IconEu,
        ModelId::IconD2,
    ] {
        let config = scheduler_config(&format!("ensemble-profile-{model}"), &[model.as_str()]);
        let profile = config.profile_for(model).unwrap();
        assert!(!profile.derived && !profile.heavy);
    }

    let gefs = scheduler_config("gefs-control-profile", &["gefs"]);
    let profile = gefs.profile_for(ModelId::Gefs).unwrap();
    assert!(!profile.derived && !profile.heavy);
    let plan = JobPlan::build_with_profile(ModelId::Gefs, cycle("20260731", 0), &profile)
        .expect("GEFS control-member lane is schedulable");
    assert!(
        plan.capability_limitations
            .contains(&"ensemble_control_member_only".to_string())
    );
    assert_eq!(
        plan.expected_valid_times
            .last()
            .map(|time| time.forecast_hour),
        Some(840)
    );

    let geps = scheduler_config("geps-statistics-profile", &["geps"]);
    let profile = geps.profile_for(ModelId::Geps).unwrap();
    assert_eq!(profile, rw_ingest::ingest_profile::IngestProfile::surface());
    let mut unsafe_geps = geps;
    unsafe_geps
        .model_profiles
        .insert("geps".to_string(), "analysis".to_string());
    assert!(unsafe_geps.validate().is_err());
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
fn scheduler_config_validates_the_origin_subset() {
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

    let mut undersized = scheduler_config("origin-plan-state-capacity", &["hrrr", "gfs", "nbm"]);
    undersized.max_concurrent_jobs = 2;
    undersized.max_queued_jobs = 5;
    undersized.origin_catalog_plan = Some(crate::origin::OriginCatalogPlanConfig::default());
    assert!(undersized.validate().is_err());
    undersized.max_queued_jobs = 6;
    assert!(undersized.validate().is_ok());
}

#[derive(Default)]
struct MemoryRuns {
    coverage: Mutex<BTreeMap<String, RunCoverage>>,
    rejected: Mutex<BTreeSet<String>>,
    repairable: Mutex<BTreeSet<String>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl MemoryRuns {
    fn reject(&self, job_id: &str) {
        self.rejected.lock().unwrap().insert(job_id.to_string());
    }

    fn corrupt_until_executed(&self, job_id: &str) {
        self.reject(job_id);
        self.repairable.lock().unwrap().insert(job_id.to_string());
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }
}

impl JobExecution for MemoryRuns {
    fn execute(&self, plan: &JobPlan) -> SchedulerResult<RunCoverage> {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(active, Ordering::AcqRel);
        std::thread::sleep(Duration::from_millis(10));
        let expected = plan
            .expected_valid_times
            .iter()
            .copied()
            .map(ValidTime::from)
            .collect::<BTreeSet<_>>();
        let coverage = RunCoverage {
            expected: expected.clone(),
            available: expected.clone(),
            present: expected.clone(),
            missing: BTreeSet::new(),
            unexpected: BTreeSet::new(),
            slot_mismatches: Vec::new(),
            missing_slots: BTreeSet::new(),
            storage_validated: true,
            validated_slots: plan
                .expected_valid_times
                .iter()
                .map(|expected| expected.storage_slot)
                .collect(),
            variable_slots: BTreeMap::new(),
        };
        self.coverage
            .lock()
            .unwrap()
            .insert(plan.job_id.clone(), coverage.clone());
        if self.repairable.lock().unwrap().remove(&plan.job_id) {
            self.rejected.lock().unwrap().remove(&plan.job_id);
        }
        self.active.fetch_sub(1, Ordering::AcqRel);
        Ok(coverage)
    }
}

impl RunValidation for MemoryRuns {
    fn verify(&self, plan: &JobPlan) -> SchedulerResult<RunCoverage> {
        if self.rejected.lock().unwrap().contains(&plan.job_id) {
            return Err(SchedulerError::InvalidCoverage(format!(
                "test rejection for '{}'",
                plan.job_id
            )));
        }
        self.coverage
            .lock()
            .unwrap()
            .get(&plan.job_id)
            .cloned()
            .ok_or_else(|| {
                SchedulerError::InvalidCoverage(format!(
                    "test has no validated coverage for '{}'",
                    plan.job_id
                ))
            })
    }
}

struct LaneDiscovery {
    generation: AtomicUsize,
}

impl LaneDiscovery {
    fn set_generation(&self, generation: usize) {
        self.generation.store(generation, Ordering::Release);
    }

    fn cycle_for(&self, lane: OriginLane) -> CycleSpec {
        match (self.generation.load(Ordering::Acquire), lane.id) {
            (0, "hrrr-hourly") => cycle("20260812", 13),
            (0, "hrrr-extended") => cycle("20260812", 12),
            (0, "gfs" | "nbm-surface") => cycle("20260812", 12),
            (1, "hrrr-hourly") => cycle("20260813", 1),
            (1, "hrrr-extended" | "gfs" | "nbm-surface") => cycle("20260813", 0),
            (3, "hrrr-hourly" | "hrrr-extended") => cycle("20260812", 12),
            (3, "gfs" | "nbm-surface") => cycle("20260812", 12),
            (_, "hrrr-hourly") => cycle("20260814", 1),
            (_, "hrrr-extended" | "gfs" | "nbm-surface") => cycle("20260814", 0),
            (_, other) => panic!("unexpected origin lane '{other}'"),
        }
    }
}

impl CycleDiscovery for LaneDiscovery {
    fn discover(
        &self,
        _model: ModelId,
        _source: Option<SourceId>,
        _now_unix: i64,
        _rollback_days: u16,
    ) -> SchedulerResult<DiscoveredCycle> {
        Err(SchedulerError::InvalidState(
            "origin test requires lane discovery".to_string(),
        ))
    }

    fn discover_origin_lane(
        &self,
        lane: OriginLane,
        _source: Option<SourceId>,
        _now_unix: i64,
        _rollback_days: u16,
    ) -> SchedulerResult<DiscoveredCycle> {
        Ok(DiscoveredCycle {
            cycle: self.cycle_for(lane),
            source: SourceId::Nomads,
        })
    }
}

fn injected_origin_host(
    config: config::SchedulerConfig,
    discovery: Arc<LaneDiscovery>,
    runs: Arc<MemoryRuns>,
) -> SchedulerHost {
    let execution: Arc<dyn JobExecution> = runs.clone();
    let validation: Arc<dyn RunValidation> = runs;
    SchedulerHost::with_components(config, discovery, execution, validation).unwrap()
}

#[test]
fn origin_execution_fails_closed_before_mutation_while_audit_is_pending() {
    let mut config = scheduler_config("origin-pending-runtime", &["hrrr", "gfs", "nbm"]);
    config.origin_catalog_plan = Some(origin::OriginCatalogPlanConfig::default());
    let store_root = config.store_root.clone();
    let discovery = Arc::new(LaneDiscovery {
        generation: AtomicUsize::new(0),
    });
    let host = injected_origin_host(config, discovery, Arc::new(MemoryRuns::default()));
    assert!(matches!(
        host.discover_at(1_786_579_200),
        Err(SchedulerError::Capacity(_))
    ));
    assert!(matches!(
        host.run_once_at(1_786_579_200),
        Err(SchedulerError::Capacity(_))
    ));
    assert!(!store_root.exists());
}

#[test]
fn production_origin_discovery_shape_separates_queryable_and_extended_hrrr() {
    let (first, hourly_cycles) =
        executor::discovery_shape_for_selector(ModelId::Hrrr, OriginLaneSelector::NewestAvailable)
            .unwrap();
    let (terminal, extended_cycles) = executor::discovery_shape_for_selector(
        ModelId::Hrrr,
        OriginLaneSelector::NewestCompleteLongestHorizon,
    )
    .unwrap();
    assert_eq!(first, 0);
    assert_eq!(terminal, 48);
    assert!(hourly_cycles.len() > extended_cycles.len());
    assert!(!extended_cycles.is_empty());
    assert!(extended_cycles.iter().all(|hour| {
        rustwx_models::supported_forecast_hours(ModelId::Hrrr, *hour)
            .into_iter()
            .max()
            == Some(terminal)
    }));
}

#[test]
fn origin_execution_obeys_the_audited_store_budget() {
    let mut config = completed_origin_config("origin-disk-budget");
    config
        .origin_catalog_plan
        .as_mut()
        .unwrap()
        .disk_budget_bytes = Some(1);
    fs::create_dir_all(&config.store_root).unwrap();
    fs::write(config.store_root.join("accounted.bin"), [0_u8, 1]).unwrap();
    let discovery = Arc::new(LaneDiscovery {
        generation: AtomicUsize::new(0),
    });
    let runs = Arc::new(MemoryRuns::default());
    let host = injected_origin_host(config, discovery, runs.clone());
    let error = host.run_once_at(1_786_579_200).unwrap_err();
    assert!(matches!(error, SchedulerError::Capacity(_)));
    assert_eq!(runs.peak(), 0, "no job started above the audited budget");
}

#[test]
fn overlapping_origin_lanes_admit_a_shared_run_only_once() {
    let config = completed_origin_config("origin-overlap");
    let discovery = Arc::new(LaneDiscovery {
        generation: AtomicUsize::new(3),
    });
    let runs = Arc::new(MemoryRuns::default());
    let host = injected_origin_host(config, discovery, runs);
    let report = host.run_once_at(1_786_579_200).unwrap();
    assert_eq!(report.admitted.len(), 3);
    assert_eq!(
        report.admitted.iter().collect::<BTreeSet<_>>().len(),
        report.admitted.len()
    );
}

#[test]
fn origin_lanes_publish_validated_active_and_one_previous_across_restart() {
    let config = completed_origin_config("origin-runtime");
    let store_root = config.store_root.clone();
    let discovery = Arc::new(LaneDiscovery {
        generation: AtomicUsize::new(0),
    });
    let runs = Arc::new(MemoryRuns::default());
    let host = injected_origin_host(config.clone(), discovery.clone(), runs.clone());

    let first = host.run_once_at(1_786_579_200).unwrap();
    assert_eq!(runs.peak(), 2, "the audited two-job ceiling is enforced");
    let first_catalog = first.origin_catalog.unwrap();
    assert_eq!(first_catalog.lanes.len(), 4);
    assert!(first_catalog.lanes.iter().all(|lane| lane.active.is_some()));
    assert!(
        first_catalog
            .lanes
            .iter()
            .find(|lane| lane.id == "hrrr-hourly")
            .unwrap()
            .previous
            .is_some(),
        "the preceding queryable HRRR cycle is the complete-cycle generation"
    );
    assert!(
        first_catalog
            .lanes
            .iter()
            .filter(|lane| lane.id != "hrrr-hourly")
            .all(|lane| lane.previous.is_none())
    );

    discovery.set_generation(1);
    let second = host.run_once_at(1_786_665_600).unwrap();
    let second_catalog = second.origin_catalog.unwrap();
    assert!(
        second_catalog
            .lanes
            .iter()
            .all(|lane| lane.active.is_some() && lane.previous.is_some())
    );
    assert_eq!(
        second_catalog.protected().unwrap().len(),
        7,
        "the hourly rollback generation overlaps the active extended run"
    );

    let restarted = injected_origin_host(config.clone(), discovery.clone(), runs.clone());
    let after_restart = restarted.run_once_at(1_786_665_601).unwrap();
    assert_eq!(after_restart.origin_catalog.unwrap(), second_catalog);
    let persisted = OriginCatalogStateStore::new(&store_root)
        .load_or_empty(config.origin_catalog_plan.as_ref().unwrap())
        .unwrap();
    assert_eq!(persisted, second_catalog);
    assert_eq!(
        restarted.status().unwrap().origin_catalog.unwrap(),
        second_catalog
    );

    runs.corrupt_until_executed("hrrr-20260812-12z");
    let repaired = restarted.run_once_at(1_786_665_602).unwrap();
    assert!(
        repaired
            .succeeded
            .contains(&"hrrr-20260812-12z".to_string()),
        "restart inventory keeps a published rollback generation repairable"
    );
    assert_eq!(repaired.origin_catalog.unwrap(), second_catalog);

    runs.reject("gfs-20260814-00z");
    discovery.set_generation(2);
    let third = restarted.run_once_at(1_786_752_000).unwrap();
    let third_catalog = third.origin_catalog.unwrap();
    let gfs = third_catalog
        .lanes
        .iter()
        .find(|lane| lane.id == "gfs")
        .unwrap();
    assert_eq!(gfs.active.as_ref().unwrap().run_id, "20260813_00z");
    assert_eq!(gfs.previous.as_ref().unwrap().run_id, "20260812_12z");
    let lane = |id: &str| {
        third_catalog
            .lanes
            .iter()
            .find(|lane| lane.id == id)
            .unwrap()
    };
    assert_eq!(
        lane("hrrr-hourly").active.as_ref().unwrap().run_id,
        "20260814_01z"
    );
    assert_eq!(
        lane("hrrr-hourly").previous.as_ref().unwrap().run_id,
        "20260814_00z"
    );
    for id in ["hrrr-extended", "nbm-surface"] {
        assert_eq!(lane(id).active.as_ref().unwrap().run_id, "20260814_00z");
        assert_eq!(lane(id).previous.as_ref().unwrap().run_id, "20260813_00z");
    }
    let retention = third.retention.unwrap();
    assert!(
        retention
            .candidates
            .contains(&"hrrr:20260812_12z".to_string())
    );
    assert!(
        retention
            .candidates
            .contains(&"hrrr:20260812_13z".to_string())
    );
    assert!(
        !retention
            .candidates
            .contains(&"gfs:20260812_12z".to_string())
    );
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
    assert_eq!(ready, 30);
}

#[test]
fn reps_plan_schedules_only_provider_statistics_from_f003_through_f072() {
    let plan = JobPlan::build(ModelId::Reps, cycle("20260814", 0)).unwrap();
    assert_eq!(plan.expected_valid_times.len(), 24);
    assert_eq!(plan.expected_valid_times.first().unwrap().forecast_hour, 3);
    assert_eq!(plan.expected_valid_times.last().unwrap().forecast_hour, 72);
    assert!(
        plan.expected_valid_times
            .iter()
            .all(|valid| valid.forecast_hour % 3 == 0)
    );
    assert_eq!(plan.ingest_products.len(), 1);
    assert_eq!(
        plan.ingest_products[0].product,
        "rws-reps-provider-statistics"
    );
    assert!(plan.ingest_products[0].surface_source);
    assert!(!plan.ingest_products[0].pressure_source);
    assert_eq!(
        plan.capability_limitations,
        vec![
            "provider_statistics_only",
            "surface_only",
            "derived_products_disabled",
        ]
    );
    assert!(plan.ingest_profile.volumes.is_empty());
    assert_eq!(
        plan.ingest_profile
            .surface_fields
            .as_ref()
            .expect("REPS persists an exact named statistics set")
            .len(),
        37
    );
    assert!(!plan.ingest_profile.derived && !plan.ingest_profile.heavy);
    plan.validate().unwrap();
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
fn scheduler_never_claims_or_retains_an_unowned_generation_directory() {
    let dir = test_dir("retention-unowned-generation");
    let store_root = dir.join("store");
    let plan = JobPlan::build(ModelId::Rtma, cycle("20260730", 18)).unwrap();
    let mut record = JobRecord::new(plan, 1).unwrap();
    record.state = JobState::Failed { finished_unix: 2 };
    record.attempts = 1;
    record.last_error = Some("terminal".to_string());
    let run_dir = store_root
        .join(record.plan.model.as_str())
        .join(&record.plan.run_id);
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(run_dir.join("run.json"), b"replication-owned-placeholder").unwrap();

    assert!(matches!(
        crate::retention::ensure_owner_marker(&run_dir, &record),
        Err(SchedulerError::InvalidState(detail)) if detail.contains("without scheduler ownership")
    ));
    assert!(!run_dir.join(crate::retention::OWNER_FILE).exists());

    let retention = plan_owned_retention(&[record.clone()], &BTreeSet::new(), 0).unwrap();
    assert!(execute_retention(&store_root, &[record], &retention, false).is_err());
    assert!(run_dir.join("run.json").is_file());
    let _ = fs::remove_dir_all(dir);
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
