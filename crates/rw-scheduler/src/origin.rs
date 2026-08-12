//! Planning and durable alias publication for a small public HTTPS origin
//! catalog.
//!
//! The scheduler executor supplies only runs which passed its exact rw-store
//! validation. This module selects the active and one previous generation for
//! each configured lane and publishes that inventory with a same-directory
//! atomic replacement. The origin is a trusted conventional HTTPS
//! server/signer, never a relay; object-tier promotion and relay-mediated
//! transport are outside this scheduler policy.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;

use rustwx_core::ModelId;
use rustwx_models::{model_summary, supported_forecast_hours};
use rw_ingest::ingest_profile::IngestProfile;
use rw_ingest::{IngestSupportStatus, model_ingest_capability};
use serde::{Deserialize, Serialize};

use crate::alias::RunCandidate;
use crate::durable::durable_atomic_write;
use crate::error::{SchedulerError, SchedulerResult};
use crate::plan::{canonical_run_id, cycle_origin_unix};
use crate::retention::RunKey;

pub const ORIGIN_CATALOG_STATE_SCHEMA: &str = "rw-scheduler.origin-catalog.v1";
const MAX_ORIGIN_CATALOG_STATE_BYTES: u64 = 1024 * 1024;
pub const ORIGIN_CATALOG_FILE: &str = ".rw-origin-catalog.json";

/// The initial, deliberately bounded catalog shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OriginCatalogPreset {
    #[default]
    InitialPublicSubset,
}

/// Capacity values are not deployment defaults. They become valid only after
/// an operator records that the target host has been audited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapacityAuditStatus {
    #[default]
    Pending,
    Complete,
}

/// Scheduler configuration for the public origin catalog.
///
/// While `capacity_audit` is `pending`, disk and concurrency values must stay
/// unset. This prevents illustrative values from becoming accidental
/// production commitments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OriginCatalogPlanConfig {
    pub preset: OriginCatalogPreset,
    pub capacity_audit: CapacityAuditStatus,
    pub disk_budget_bytes: Option<u64>,
    pub max_concurrent_jobs: Option<usize>,
    /// Exactly one prior lane generation is retained for atomic replacement.
    pub previous_generations: u8,
}

impl Default for OriginCatalogPlanConfig {
    fn default() -> Self {
        Self {
            preset: OriginCatalogPreset::InitialPublicSubset,
            capacity_audit: CapacityAuditStatus::Pending,
            disk_budget_bytes: None,
            max_concurrent_jobs: None,
            previous_generations: 1,
        }
    }
}

impl OriginCatalogPlanConfig {
    /// Validate the audit values and ensure every lane is backed by a
    /// configured, ready ingest capability. A pending audit is valid for
    /// offline planning, but the executor refuses discovery or mutation until
    /// it is complete.
    pub fn validate_for_models(&self, configured: &BTreeSet<ModelId>) -> SchedulerResult<()> {
        match self.capacity_audit {
            CapacityAuditStatus::Pending => {
                if self.disk_budget_bytes.is_some() || self.max_concurrent_jobs.is_some() {
                    return Err(SchedulerError::InvalidConfig(
                        "origin_catalog_plan capacity values must remain unset while the capacity audit is pending"
                            .to_string(),
                    ));
                }
            }
            CapacityAuditStatus::Complete => {
                if self.disk_budget_bytes == Some(0) || self.disk_budget_bytes.is_none() {
                    return Err(SchedulerError::InvalidConfig(
                        "origin_catalog_plan.disk_budget_bytes must be a nonzero audited value when the capacity audit is complete"
                            .to_string(),
                    ));
                }
                if self.max_concurrent_jobs == Some(0) || self.max_concurrent_jobs.is_none() {
                    return Err(SchedulerError::InvalidConfig(
                        "origin_catalog_plan.max_concurrent_jobs must be a nonzero audited value when the capacity audit is complete"
                            .to_string(),
                    ));
                }
            }
        }
        if self.previous_generations != 1 {
            return Err(SchedulerError::InvalidConfig(
                "origin_catalog_plan.previous_generations must be exactly 1 for atomic replacement"
                    .to_string(),
            ));
        }

        for lane in self.lanes() {
            if !configured.contains(&lane.model) {
                return Err(SchedulerError::InvalidConfig(format!(
                    "origin catalog lane '{}' requires model '{}' in the scheduler allowlist",
                    lane.id, lane.model
                )));
            }
            let capability = model_ingest_capability(lane.model);
            if capability.status != IngestSupportStatus::Ready || capability.products.is_empty() {
                return Err(SchedulerError::UnsupportedModel(lane.model));
            }
        }
        Ok(())
    }

    pub fn lanes(&self) -> [OriginLane; 4] {
        match self.preset {
            OriginCatalogPreset::InitialPublicSubset => INITIAL_PUBLIC_LANES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginLaneSelector {
    /// Newest run with at least one validated valid time. A caller must only
    /// supply candidates whose referenced storage passed its queryability
    /// checks; the planner does not weaken rw-store validation.
    NewestAvailable,
    /// Newest complete run on a cycle whose capability-declared forecast
    /// horizon is the model's longest.
    NewestCompleteLongestHorizon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginProfileRequirement {
    ConfiguredCapabilitySafe,
    CompleteSurface,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginLane {
    pub id: &'static str,
    pub model: ModelId,
    pub selector: OriginLaneSelector,
    pub profile: OriginProfileRequirement,
}

impl OriginLane {
    pub fn validate_profile(self, profile: &IngestProfile) -> SchedulerResult<()> {
        if self.profile == OriginProfileRequirement::CompleteSurface
            && profile != &IngestProfile::surface()
        {
            return Err(SchedulerError::InvalidConfig(format!(
                "origin catalog lane '{}' requires the complete surface ingest profile for '{}'",
                self.id, self.model
            )));
        }
        Ok(())
    }
}

const INITIAL_PUBLIC_LANES: [OriginLane; 4] = [
    OriginLane {
        id: "hrrr-hourly",
        model: ModelId::Hrrr,
        selector: OriginLaneSelector::NewestAvailable,
        profile: OriginProfileRequirement::ConfiguredCapabilitySafe,
    },
    OriginLane {
        id: "hrrr-extended",
        model: ModelId::Hrrr,
        selector: OriginLaneSelector::NewestCompleteLongestHorizon,
        profile: OriginProfileRequirement::ConfiguredCapabilitySafe,
    },
    OriginLane {
        id: "gfs",
        model: ModelId::Gfs,
        selector: OriginLaneSelector::NewestAvailable,
        profile: OriginProfileRequirement::ConfiguredCapabilitySafe,
    },
    OriginLane {
        id: "nbm-surface",
        model: ModelId::Nbm,
        selector: OriginLaneSelector::NewestAvailable,
        profile: OriginProfileRequirement::CompleteSurface,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginLanePlan {
    pub lane: OriginLane,
    pub active: Option<RunKey>,
    pub previous: Option<RunKey>,
}

/// Immutable planning result. `protected` can be supplied as the alias set to
/// the existing retention planner; this module never deletes on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginPublicationPlan {
    pub lanes: Vec<OriginLanePlan>,
    pub protected: BTreeSet<RunKey>,
}

impl OriginPublicationPlan {
    pub fn build(
        config: &OriginCatalogPlanConfig,
        candidates: &[RunCandidate],
    ) -> SchedulerResult<Self> {
        if config.previous_generations != 1 {
            return Err(SchedulerError::InvalidConfig(
                "origin publication planning supports exactly one previous generation".to_string(),
            ));
        }

        let mut unique = BTreeSet::new();
        for candidate in candidates {
            let key = RunKey::new(candidate.model(), candidate.run_id())?;
            if !unique.insert(key) {
                return Err(SchedulerError::InvalidConfig(format!(
                    "duplicate origin candidate '{}:{}'",
                    candidate.model(),
                    candidate.run_id()
                )));
            }
        }

        let mut protected = BTreeSet::new();
        let mut lanes = Vec::with_capacity(INITIAL_PUBLIC_LANES.len());
        for lane in config.lanes() {
            let mut matching = candidates
                .iter()
                .filter(|candidate| candidate_matches_lane(candidate, lane))
                .map(|candidate| {
                    Ok((
                        cycle_origin_unix(candidate.cycle())?,
                        candidate.run_id(),
                        candidate,
                    ))
                })
                .collect::<SchedulerResult<Vec<_>>>()?;
            matching.sort_by(|left, right| (right.0, right.1).cmp(&(left.0, left.1)));

            let active = matching
                .first()
                .map(|(_, _, candidate)| RunKey::new(candidate.model(), candidate.run_id()))
                .transpose()?;
            let previous = matching
                .get(1)
                .map(|(_, _, candidate)| RunKey::new(candidate.model(), candidate.run_id()))
                .transpose()?;
            protected.extend(active.iter().cloned());
            protected.extend(previous.iter().cloned());
            lanes.push(OriginLanePlan {
                lane,
                active,
                previous,
            });
        }
        Ok(Self { lanes, protected })
    }
}

/// A generation exposed by a mutable origin lane alias. This is deliberately
/// small: immutable run metadata remains in rw-store and signed object
/// manifests remain the responsibility of the HTTPS origin service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginPublishedGeneration {
    pub model: ModelId,
    pub cycle: rustwx_core::CycleSpec,
    pub run_id: String,
    pub coverage_complete: bool,
    pub available_valid_unix: BTreeSet<i64>,
}

impl OriginPublishedGeneration {
    fn from_candidate(candidate: &RunCandidate) -> Self {
        Self {
            model: candidate.model(),
            cycle: candidate.cycle().clone(),
            run_id: candidate.run_id().to_string(),
            coverage_complete: candidate.coverage_complete(),
            available_valid_unix: candidate.available_valid_unix().clone(),
        }
    }

    fn key(&self) -> SchedulerResult<RunKey> {
        RunKey::new(self.model, self.run_id.clone())
    }

    fn validate_for_lane(&self, lane: OriginLane) -> SchedulerResult<()> {
        if self.model != lane.model {
            return Err(SchedulerError::InvalidState(format!(
                "origin lane '{}' contains model '{}' instead of '{}'",
                lane.id, self.model, lane.model
            )));
        }
        if canonical_run_id(&self.cycle) != self.run_id {
            return Err(SchedulerError::InvalidState(format!(
                "origin lane '{}' run '{}' does not match its cycle",
                lane.id, self.run_id
            )));
        }
        let candidate = RunCandidate::new(
            self.model,
            self.cycle.clone(),
            self.run_id.clone(),
            self.coverage_complete,
            self.available_valid_unix.clone(),
        )?;
        let origin_unix = cycle_origin_unix(&self.cycle)?;
        let expected = supported_forecast_hours(self.model, self.cycle.hour_utc)
            .into_iter()
            .map(|forecast_hour| {
                origin_unix
                    .checked_add(i64::from(forecast_hour) * 3_600)
                    .ok_or_else(|| {
                        SchedulerError::InvalidState(format!(
                            "origin lane '{}' forecast timestamp overflowed",
                            lane.id
                        ))
                    })
            })
            .collect::<SchedulerResult<BTreeSet<_>>>()?;
        if !self.available_valid_unix.is_subset(&expected)
            || self.coverage_complete != (self.available_valid_unix == expected)
        {
            return Err(SchedulerError::InvalidState(format!(
                "origin lane '{}' generation '{}:{}' has an inconsistent valid-time inventory",
                lane.id, self.model, self.run_id
            )));
        }
        if !candidate_matches_lane(&candidate, lane) {
            return Err(SchedulerError::InvalidState(format!(
                "origin lane '{}' generation '{}:{}' does not satisfy its selector",
                lane.id, self.model, self.run_id
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginPublishedLane {
    pub id: String,
    pub active: Option<OriginPublishedGeneration>,
    pub previous: Option<OriginPublishedGeneration>,
}

/// Durable alias catalog consumed by the conventional HTTPS origin. It is
/// written only after exact validation and is also the retention protection
/// source, so an active or rollback generation survives process restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginCatalogState {
    pub schema: String,
    pub updated_unix: i64,
    pub lanes: Vec<OriginPublishedLane>,
}

impl OriginCatalogState {
    pub fn empty(config: &OriginCatalogPlanConfig) -> Self {
        Self {
            schema: ORIGIN_CATALOG_STATE_SCHEMA.to_string(),
            updated_unix: 0,
            lanes: config
                .lanes()
                .into_iter()
                .map(|lane| OriginPublishedLane {
                    id: lane.id.to_string(),
                    active: None,
                    previous: None,
                })
                .collect(),
        }
    }

    pub fn from_candidates(
        config: &OriginCatalogPlanConfig,
        candidates: &[RunCandidate],
        updated_unix: i64,
    ) -> SchedulerResult<Self> {
        let plan = OriginPublicationPlan::build(config, candidates)?;
        let lanes = plan
            .lanes
            .iter()
            .map(|lane_plan| {
                Ok(OriginPublishedLane {
                    id: lane_plan.lane.id.to_string(),
                    active: generation_for_key(candidates, lane_plan.active.as_ref())?,
                    previous: generation_for_key(candidates, lane_plan.previous.as_ref())?,
                })
            })
            .collect::<SchedulerResult<Vec<_>>>()?;
        let state = Self {
            schema: ORIGIN_CATALOG_STATE_SCHEMA.to_string(),
            updated_unix,
            lanes,
        };
        state.validate(config)?;
        Ok(state)
    }

    pub fn validate(&self, config: &OriginCatalogPlanConfig) -> SchedulerResult<()> {
        if self.schema != ORIGIN_CATALOG_STATE_SCHEMA {
            return Err(SchedulerError::InvalidState(format!(
                "unexpected origin catalog schema '{}'",
                self.schema
            )));
        }
        if self.updated_unix < 0 {
            return Err(SchedulerError::InvalidState(
                "origin catalog timestamp cannot be negative".to_string(),
            ));
        }
        let expected = config.lanes();
        if self.lanes.len() != expected.len() {
            return Err(SchedulerError::InvalidState(format!(
                "origin catalog has {} lanes; expected {}",
                self.lanes.len(),
                expected.len()
            )));
        }
        for (published, lane) in self.lanes.iter().zip(expected) {
            if published.id != lane.id {
                return Err(SchedulerError::InvalidState(format!(
                    "origin catalog lane '{}' appeared where '{}' was expected",
                    published.id, lane.id
                )));
            }
            if let Some(active) = &published.active {
                active.validate_for_lane(lane)?;
            }
            if let Some(previous) = &published.previous {
                previous.validate_for_lane(lane)?;
            }
            if published.previous.is_some() && published.active.is_none() {
                return Err(SchedulerError::InvalidState(format!(
                    "origin lane '{}' has a previous generation without an active generation",
                    lane.id
                )));
            }
            if let (Some(active), Some(previous)) = (&published.active, &published.previous) {
                if active.key()? == previous.key()? {
                    return Err(SchedulerError::InvalidState(format!(
                        "origin lane '{}' repeats its active generation as previous",
                        lane.id
                    )));
                }
                if cycle_origin_unix(&active.cycle)? <= cycle_origin_unix(&previous.cycle)? {
                    return Err(SchedulerError::InvalidState(format!(
                        "origin lane '{}' previous generation is not older than active",
                        lane.id
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn protected(&self) -> SchedulerResult<BTreeSet<RunKey>> {
        let mut protected = BTreeSet::new();
        for lane in &self.lanes {
            if let Some(active) = &lane.active {
                protected.insert(active.key()?);
            }
            if let Some(previous) = &lane.previous {
                protected.insert(previous.key()?);
            }
        }
        Ok(protected)
    }
}

fn generation_for_key(
    candidates: &[RunCandidate],
    key: Option<&RunKey>,
) -> SchedulerResult<Option<OriginPublishedGeneration>> {
    let Some(key) = key else { return Ok(None) };
    let candidate = candidates
        .iter()
        .find(|candidate| candidate.model() == key.model() && candidate.run_id() == key.run_id())
        .ok_or_else(|| {
            SchedulerError::InvalidState(format!(
                "origin publication selected missing candidate '{}:{}'",
                key.model(),
                key.run_id()
            ))
        })?;
    Ok(Some(OriginPublishedGeneration::from_candidate(candidate)))
}

#[derive(Debug, Clone)]
pub struct OriginCatalogStateStore {
    store_root: PathBuf,
}

impl OriginCatalogStateStore {
    pub fn new(store_root: impl Into<PathBuf>) -> Self {
        Self {
            store_root: store_root.into(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.store_root.join(ORIGIN_CATALOG_FILE)
    }

    pub fn load_or_empty(
        &self,
        config: &OriginCatalogPlanConfig,
    ) -> SchedulerResult<OriginCatalogState> {
        let path = self.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(OriginCatalogState::empty(config));
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SchedulerError::InvalidState(format!(
                "origin catalog '{}' must be a real regular file",
                path.display()
            )));
        }
        if metadata.len() > MAX_ORIGIN_CATALOG_STATE_BYTES {
            return Err(SchedulerError::InvalidState(format!(
                "origin catalog exceeds {MAX_ORIGIN_CATALOG_STATE_BYTES} bytes"
            )));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&path)?
            .take(MAX_ORIGIN_CATALOG_STATE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ORIGIN_CATALOG_STATE_BYTES {
            return Err(SchedulerError::InvalidState(
                "origin catalog grew beyond its size limit while reading".to_string(),
            ));
        }
        let state: OriginCatalogState = serde_json::from_slice(&bytes)?;
        state.validate(config)?;
        Ok(state)
    }

    pub fn save(
        &self,
        config: &OriginCatalogPlanConfig,
        state: &OriginCatalogState,
    ) -> SchedulerResult<()> {
        state.validate(config)?;
        fs::create_dir_all(&self.store_root)?;
        let metadata = fs::symlink_metadata(&self.store_root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SchedulerError::InvalidState(format!(
                "origin store root '{}' must be a real directory",
                self.store_root.display()
            )));
        }
        let bytes = serde_json::to_vec_pretty(state)?;
        if bytes.len() as u64 > MAX_ORIGIN_CATALOG_STATE_BYTES {
            return Err(SchedulerError::InvalidState(format!(
                "serialized origin catalog exceeds {MAX_ORIGIN_CATALOG_STATE_BYTES} bytes"
            )));
        }
        durable_atomic_write(&self.path(), &bytes)
    }
}

fn candidate_matches_lane(candidate: &RunCandidate, lane: OriginLane) -> bool {
    if candidate.model() != lane.model {
        return false;
    }
    match lane.selector {
        OriginLaneSelector::NewestAvailable => !candidate.available_valid_unix().is_empty(),
        OriginLaneSelector::NewestCompleteLongestHorizon => {
            candidate.coverage_complete()
                && is_longest_horizon_cycle(candidate.model(), candidate.cycle().hour_utc)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::CycleSpec;

    fn candidate(model: ModelId, date: &str, hour: u8, complete: bool) -> RunCandidate {
        let cycle = CycleSpec::new(date, hour).unwrap();
        let available = if complete {
            supported_forecast_hours(model, hour)
                .into_iter()
                .map(|forecast_hour| {
                    cycle_origin_unix(&cycle).unwrap() + i64::from(forecast_hour) * 3_600
                })
                .collect()
        } else {
            BTreeSet::from([cycle_origin_unix(&cycle).unwrap()])
        };
        RunCandidate::new(
            model,
            cycle,
            format!("{date}_{hour:02}z"),
            complete,
            available,
        )
        .unwrap()
    }

    #[test]
    fn pending_capacity_audit_rejects_guessed_values() {
        let mut config = OriginCatalogPlanConfig::default();
        let models = BTreeSet::from([ModelId::Hrrr, ModelId::Gfs, ModelId::Nbm]);
        assert!(config.validate_for_models(&models).is_ok());
        config.disk_budget_bytes = Some(1);
        assert!(config.validate_for_models(&models).is_err());
        config.disk_budget_bytes = None;
        config.capacity_audit = CapacityAuditStatus::Complete;
        assert!(config.validate_for_models(&models).is_err());
        config.disk_budget_bytes = Some(1);
        config.max_concurrent_jobs = Some(1);
        assert!(config.validate_for_models(&models).is_ok());
    }

    #[test]
    fn preset_is_exact_and_requires_the_surface_profile_for_nbm() {
        let config = OriginCatalogPlanConfig::default();
        let lanes = config.lanes();
        assert_eq!(
            lanes.map(|lane| (lane.id, lane.model)),
            [
                ("hrrr-hourly", ModelId::Hrrr),
                ("hrrr-extended", ModelId::Hrrr),
                ("gfs", ModelId::Gfs),
                ("nbm-surface", ModelId::Nbm),
            ]
        );
        assert!(lanes[3].validate_profile(&IngestProfile::surface()).is_ok());
        assert!(lanes[3].validate_profile(&IngestProfile::view()).is_err());
    }

    #[test]
    fn planner_keeps_independent_hourly_and_extended_hrrr_generations() {
        let candidates = vec![
            candidate(ModelId::Hrrr, "20260812", 18, false),
            candidate(ModelId::Hrrr, "20260812", 13, false),
            candidate(ModelId::Hrrr, "20260812", 12, true),
            candidate(ModelId::Hrrr, "20260812", 11, true),
            candidate(ModelId::Hrrr, "20260812", 6, true),
            candidate(ModelId::Hrrr, "20260812", 5, true),
            candidate(ModelId::Gfs, "20260812", 12, false),
            candidate(ModelId::Gfs, "20260812", 6, true),
            candidate(ModelId::Nbm, "20260812", 12, false),
            candidate(ModelId::Nbm, "20260812", 6, true),
        ];
        let plan =
            OriginPublicationPlan::build(&OriginCatalogPlanConfig::default(), &candidates).unwrap();

        assert_eq!(
            plan.lanes[0].active.as_ref().unwrap().run_id(),
            "20260812_18z"
        );
        assert_eq!(
            plan.lanes[0].previous.as_ref().unwrap().run_id(),
            "20260812_13z"
        );
        assert_eq!(
            plan.lanes[1].active.as_ref().unwrap().run_id(),
            "20260812_12z"
        );
        assert_eq!(
            plan.lanes[1].previous.as_ref().unwrap().run_id(),
            "20260812_06z"
        );
        assert_eq!(
            plan.lanes[2].active.as_ref().unwrap().run_id(),
            "20260812_12z"
        );
        assert_eq!(
            plan.lanes[3].active.as_ref().unwrap().run_id(),
            "20260812_12z"
        );
        assert_eq!(
            plan.protected.len(),
            8,
            "overlapping HRRR lanes deduplicate"
        );
        assert!(
            !plan
                .protected
                .iter()
                .any(|key| key.model() == ModelId::Hrrr && key.run_id() == "20260812_11z")
        );
    }

    #[test]
    fn durable_catalog_rejects_tampered_valid_time_inventory() {
        let config = OriginCatalogPlanConfig::default();
        let candidates = vec![candidate(ModelId::Hrrr, "20260812", 12, true)];
        let mut state = OriginCatalogState::from_candidates(&config, &candidates, 1).unwrap();
        let hourly = state
            .lanes
            .iter_mut()
            .find(|lane| lane.id == "hrrr-hourly")
            .unwrap();
        hourly
            .active
            .as_mut()
            .unwrap()
            .available_valid_unix
            .insert(1);
        assert!(matches!(
            state.validate(&config),
            Err(SchedulerError::InvalidState(_))
        ));
    }
}
