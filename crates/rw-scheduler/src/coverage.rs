use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rw_store::grid::GridFile;
use rw_store::reader::HourReader;
use rw_store::run::RwsRunManifest;
use serde::{Deserialize, Serialize};

use crate::error::{SchedulerError, SchedulerResult};
use crate::plan::{ExpectedValidTime, JobPlan};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ValidTime {
    pub lead_seconds: u64,
    pub valid_unix: i64,
}

impl From<ExpectedValidTime> for ValidTime {
    fn from(value: ExpectedValidTime) -> Self {
        Self {
            lead_seconds: value.lead_seconds,
            valid_unix: value.valid_unix,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotMismatch {
    pub storage_slot: u16,
    pub expected: ValidTime,
    pub available: ValidTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCoverage {
    pub expected: BTreeSet<ValidTime>,
    pub available: BTreeSet<ValidTime>,
    pub present: BTreeSet<ValidTime>,
    pub missing: BTreeSet<ValidTime>,
    pub unexpected: BTreeSet<ValidTime>,
    /// Exact-time v2 files are ordinal. A physically present timestamp under
    /// the wrong slot is not complete even if set comparison alone would pass.
    pub slot_mismatches: Vec<SlotMismatch>,
    pub missing_slots: BTreeSet<u16>,
    /// True only when the grid and every manifest-referenced hour were opened,
    /// validated against the manifest, and rechecked against their paths.
    pub storage_validated: bool,
    pub validated_slots: BTreeSet<u16>,
    pub variable_slots: BTreeMap<String, BTreeSet<u16>>,
}

impl RunCoverage {
    pub fn temporal_inventory_complete(&self) -> bool {
        self.missing.is_empty()
            && self.unexpected.is_empty()
            && self.slot_mismatches.is_empty()
            && self.missing_slots.is_empty()
    }

    pub fn is_complete(&self) -> bool {
        self.temporal_inventory_complete()
            && self.storage_validated
            && self.validated_slots.len() == self.expected.len()
    }

    pub fn available_valid_unix(&self) -> BTreeSet<i64> {
        self.present.iter().map(|time| time.valid_unix).collect()
    }

    pub fn matches_plan(&self, plan: &JobPlan) -> bool {
        self.expected
            == plan
                .expected_valid_times
                .iter()
                .copied()
                .map(ValidTime::from)
                .collect()
    }
}

pub fn verify_run_json(plan: &JobPlan, path: &Path) -> SchedulerResult<RunCoverage> {
    plan.validate()?;
    let manifest = RwsRunManifest::load_for_run(path, plan.model.as_str(), &plan.run_id)?;
    let mut coverage = verify_manifest(plan, &manifest)?;
    let run_dir = path.parent().ok_or_else(|| {
        SchedulerError::InvalidCoverage("run manifest has no parent directory".to_string())
    })?;
    require_regular_file(path, "run manifest")?;
    let grid_path = run_dir.join("grid.rwg");
    require_regular_file(&grid_path, "run grid")?;
    let grid = GridFile::open(&grid_path)?;
    manifest.validate_grid(&grid.hash, grid.nx, grid.ny)?;

    for (&storage_slot, entry) in &manifest.hours {
        rw_store::run::validate_store_component("hour filename", &entry.file)?;
        let hour_path = run_dir.join(&entry.file);
        require_regular_file(&hour_path, "run hour")?;
        let reader = HourReader::open_with_tile_cache_bytes(&hour_path, 0)?;
        manifest.validate_hour_meta(storage_slot, reader.meta())?;
        if !reader.source_matches_path(&hour_path)? {
            return Err(SchedulerError::InvalidCoverage(format!(
                "storage slot {storage_slot} changed while it was validated"
            )));
        }
        coverage.validated_slots.insert(storage_slot);
        for variable in &reader.meta().variables {
            coverage
                .variable_slots
                .entry(variable.name.clone())
                .or_default()
                .insert(storage_slot);
        }
    }
    let current = RwsRunManifest::load_for_run(path, plan.model.as_str(), &plan.run_id)?;
    if current != manifest {
        return Err(SchedulerError::InvalidCoverage(
            "run manifest changed during storage validation".to_string(),
        ));
    }
    coverage.storage_validated = true;
    Ok(coverage)
}

pub fn verify_manifest(plan: &JobPlan, manifest: &RwsRunManifest) -> SchedulerResult<RunCoverage> {
    plan.validate()?;
    manifest.validate_contents()?;
    manifest.validate_identity(plan.model.as_str(), &plan.run_id)?;

    let expected = plan
        .expected_valid_times
        .iter()
        .copied()
        .map(ValidTime::from)
        .collect::<BTreeSet<_>>();
    let mut available = BTreeSet::new();
    let mut slot_mismatches = Vec::new();
    let mut missing_slots = BTreeSet::new();

    if manifest.is_exact_time_axis() {
        for (storage_slot, exact) in manifest.exact_times() {
            let actual = ValidTime {
                lead_seconds: exact.lead_seconds,
                valid_unix: exact.valid_unix,
            };
            available.insert(actual);
            if let Some(expected_slot) = plan
                .expected_valid_times
                .iter()
                .find(|expected| expected.storage_slot == storage_slot)
            {
                let expected_time = ValidTime::from(*expected_slot);
                if actual != expected_time {
                    slot_mismatches.push(SlotMismatch {
                        storage_slot,
                        expected: expected_time,
                        available: actual,
                    });
                }
            }
        }
        missing_slots.extend(
            plan.expected_valid_times
                .iter()
                .map(|expected| expected.storage_slot)
                .filter(|storage_slot| !manifest.hours.contains_key(storage_slot)),
        );
    } else {
        let origin_unix = plan.origin_unix()?;
        for forecast_hour in manifest.hours.keys().copied() {
            let lead_seconds = u64::from(forecast_hour).checked_mul(3_600).ok_or_else(|| {
                SchedulerError::InvalidCoverage(format!(
                    "forecast hour {forecast_hour} cannot be represented as seconds"
                ))
            })?;
            let valid_unix = origin_unix
                .checked_add(i64::try_from(lead_seconds).map_err(|_| {
                    SchedulerError::InvalidCoverage(format!(
                        "forecast hour {forecast_hour} exceeds timestamp range"
                    ))
                })?)
                .ok_or_else(|| {
                    SchedulerError::InvalidCoverage(format!(
                        "forecast hour {forecast_hour} overflows its valid timestamp"
                    ))
                })?;
            available.insert(ValidTime {
                lead_seconds,
                valid_unix,
            });
        }
    }

    let present = expected
        .intersection(&available)
        .copied()
        .collect::<BTreeSet<_>>();
    let missing = expected
        .difference(&available)
        .copied()
        .collect::<BTreeSet<_>>();
    let unexpected = available
        .difference(&expected)
        .copied()
        .collect::<BTreeSet<_>>();
    Ok(RunCoverage {
        expected,
        available,
        present,
        missing,
        unexpected,
        slot_mismatches,
        missing_slots,
        storage_validated: false,
        validated_slots: BTreeSet::new(),
        variable_slots: BTreeMap::new(),
    })
}

fn require_regular_file(path: &Path, label: &str) -> SchedulerResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(SchedulerError::InvalidCoverage(format!(
            "{label} '{}' must be a real regular file",
            display_path(path)
        )));
    }
    Ok(())
}

fn display_path(path: &Path) -> String {
    path.file_name()
        .map(|name| PathBuf::from(name).display().to_string())
        .unwrap_or_else(|| "<unnamed>".to_string())
}
