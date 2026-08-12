//! Read-only host capacity evidence for an operator-approved origin budget.
//!
//! The audit deliberately reports measurements and the exact in-flight
//! headroom formula; it never guesses production disk or concurrency values.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::SchedulerConfig;
use crate::error::{SchedulerError, SchedulerResult};
use crate::origin::CapacityAuditStatus;

pub const HOST_CAPACITY_AUDIT_SCHEMA: &str = "rw-scheduler.host-capacity-audit.v1";
const MAX_AUDIT_ENTRIES: u64 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapacityPathEvidence {
    pub path: PathBuf,
    pub exists: bool,
    pub is_real_directory: bool,
    pub filesystem_total_bytes: u64,
    pub filesystem_available_bytes: u64,
    pub observed_regular_file_bytes: u64,
    pub regular_file_count: u64,
    pub directory_count: u64,
    pub skipped_symlink_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorePayloadEvidence {
    pub rws_hour_count: u64,
    pub run_directories_with_hours: u64,
    pub largest_rws_hour_bytes: u64,
    pub largest_observed_run_bytes: u64,
    /// `largest_rws_hour_bytes * configured_max_concurrent_hours`.
    pub observed_inflight_hour_headroom_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfiguredOriginCapacity {
    pub status: CapacityAuditStatus,
    pub disk_budget_bytes: Option<u64>,
    pub max_concurrent_jobs: Option<usize>,
    pub disk_budget_fits_current_filesystem: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostCapacityAuditReport {
    pub schema: String,
    pub observed_unix: i64,
    pub logical_parallelism: usize,
    pub configured_max_concurrent_jobs: usize,
    pub configured_max_queued_jobs: usize,
    pub configured_max_concurrent_hours: usize,
    pub configured_free_space_reserve_bytes: u64,
    pub origin: Option<ConfiguredOriginCapacity>,
    pub store: CapacityPathEvidence,
    pub cache: CapacityPathEvidence,
    pub payloads: StorePayloadEvidence,
}

pub fn audit_host_capacity(
    config: &SchedulerConfig,
    observed_unix: i64,
) -> SchedulerResult<HostCapacityAuditReport> {
    if observed_unix < 0 {
        return Err(SchedulerError::InvalidConfig(
            "capacity-audit timestamp cannot be negative".to_string(),
        ));
    }
    let (store, payloads) = scan_path(&config.store_root, config.max_concurrent_hours, true)?;
    let (cache, _) = scan_path(&config.cache_root, config.max_concurrent_hours, false)?;
    let origin = config
        .origin_catalog_plan
        .as_ref()
        .map(|origin| ConfiguredOriginCapacity {
            status: origin.capacity_audit,
            disk_budget_bytes: origin.disk_budget_bytes,
            max_concurrent_jobs: origin.max_concurrent_jobs,
            disk_budget_fits_current_filesystem: origin
                .disk_budget_bytes
                .map(|budget| budget <= store.filesystem_total_bytes),
        });
    Ok(HostCapacityAuditReport {
        schema: HOST_CAPACITY_AUDIT_SCHEMA.to_string(),
        observed_unix,
        logical_parallelism: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        configured_max_concurrent_jobs: config.max_concurrent_jobs,
        configured_max_queued_jobs: config.max_queued_jobs,
        configured_max_concurrent_hours: config.max_concurrent_hours,
        configured_free_space_reserve_bytes: config.free_space_reserve_bytes,
        origin,
        store,
        cache,
        payloads,
    })
}

fn scan_path(
    path: &Path,
    max_concurrent_hours: usize,
    collect_payloads: bool,
) -> SchedulerResult<(CapacityPathEvidence, StorePayloadEvidence)> {
    let probe = nearest_existing_ancestor(path)?;
    let mut evidence = CapacityPathEvidence {
        path: path.to_path_buf(),
        exists: false,
        is_real_directory: false,
        filesystem_total_bytes: fs4::total_space(&probe)?,
        filesystem_available_bytes: fs4::available_space(&probe)?,
        observed_regular_file_bytes: 0,
        regular_file_count: 0,
        directory_count: 0,
        skipped_symlink_count: 0,
    };
    let mut payloads = StorePayloadEvidence {
        rws_hour_count: 0,
        run_directories_with_hours: 0,
        largest_rws_hour_bytes: 0,
        largest_observed_run_bytes: 0,
        observed_inflight_hour_headroom_bytes: 0,
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((evidence, payloads));
        }
        Err(error) => return Err(error.into()),
    };
    evidence.exists = true;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok((evidence, payloads));
    }
    evidence.is_real_directory = true;
    let mut stack = vec![path.to_path_buf()];
    let mut entries_seen = 0_u64;
    let mut run_bytes = BTreeMap::<PathBuf, u64>::new();
    while let Some(directory) = stack.pop() {
        evidence.directory_count = evidence
            .directory_count
            .checked_add(1)
            .ok_or_else(capacity_overflow)?;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            entries_seen = entries_seen.checked_add(1).ok_or_else(capacity_overflow)?;
            if entries_seen > MAX_AUDIT_ENTRIES {
                return Err(SchedulerError::Capacity(format!(
                    "capacity audit exceeded {MAX_AUDIT_ENTRIES} filesystem entries"
                )));
            }
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)?;
            if metadata.file_type().is_symlink() {
                evidence.skipped_symlink_count = evidence
                    .skipped_symlink_count
                    .checked_add(1)
                    .ok_or_else(capacity_overflow)?;
                continue;
            }
            if metadata.is_dir() {
                stack.push(entry_path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            evidence.regular_file_count = evidence
                .regular_file_count
                .checked_add(1)
                .ok_or_else(capacity_overflow)?;
            evidence.observed_regular_file_bytes = evidence
                .observed_regular_file_bytes
                .checked_add(metadata.len())
                .ok_or_else(capacity_overflow)?;
            if collect_payloads
                && entry_path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("rws"))
            {
                payloads.rws_hour_count = payloads
                    .rws_hour_count
                    .checked_add(1)
                    .ok_or_else(capacity_overflow)?;
                payloads.largest_rws_hour_bytes =
                    payloads.largest_rws_hour_bytes.max(metadata.len());
                if let Some(run_directory) = entry_path.parent() {
                    let total = run_bytes.entry(run_directory.to_path_buf()).or_default();
                    *total = total
                        .checked_add(metadata.len())
                        .ok_or_else(capacity_overflow)?;
                }
            }
        }
    }
    payloads.run_directories_with_hours = u64::try_from(run_bytes.len()).map_err(|_| {
        SchedulerError::Capacity("run-directory count does not fit u64".to_string())
    })?;
    payloads.largest_observed_run_bytes = run_bytes.values().copied().max().unwrap_or(0);
    payloads.observed_inflight_hour_headroom_bytes = payloads
        .largest_rws_hour_bytes
        .checked_mul(u64::try_from(max_concurrent_hours).map_err(|_| {
            SchedulerError::Capacity("max_concurrent_hours does not fit u64".to_string())
        })?)
        .ok_or_else(capacity_overflow)?;
    Ok((evidence, payloads))
}

fn nearest_existing_ancestor(path: &Path) -> SchedulerResult<PathBuf> {
    let mut candidate = path;
    loop {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                return Ok(candidate.to_path_buf());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        candidate = candidate.parent().ok_or_else(|| {
            SchedulerError::InvalidConfig(format!(
                "capacity-audit path '{}' has no existing real-directory ancestor",
                path.display()
            ))
        })?;
    }
}

fn capacity_overflow() -> SchedulerError {
    SchedulerError::Capacity("capacity-audit arithmetic overflowed".to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn test_dir(label: &str) -> PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rw-scheduler-capacity-{}-{label}-{}",
            process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn config(root: &Path) -> SchedulerConfig {
        SchedulerConfig {
            store_root: root.join("store"),
            cache_root: root.join("cache"),
            state_root: root.join("state"),
            max_concurrent_hours: 3,
            ..SchedulerConfig::default()
        }
    }

    #[test]
    fn capacity_audit_is_read_only_when_roots_are_absent() {
        let root = test_dir("absent");
        let config = config(&root);
        let report = audit_host_capacity(&config, 10).unwrap();
        assert!(!report.store.exists);
        assert!(!report.cache.exists);
        assert!(!config.store_root.exists());
        assert!(!config.cache_root.exists());
        assert!(report.store.filesystem_total_bytes > 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capacity_audit_measures_hours_runs_and_exact_headroom_formula() {
        let root = test_dir("measure");
        let config = config(&root);
        let run_a = config.store_root.join("hrrr").join("run-a");
        let run_b = config.store_root.join("gfs").join("run-b");
        fs::create_dir_all(&run_a).unwrap();
        fs::create_dir_all(&run_b).unwrap();
        fs::write(run_a.join("f000.rws"), vec![0_u8; 10]).unwrap();
        fs::write(run_a.join("f001.rws"), vec![0_u8; 20]).unwrap();
        fs::write(run_a.join("run.json"), vec![0_u8; 7]).unwrap();
        fs::write(run_b.join("f000.rws"), vec![0_u8; 15]).unwrap();
        let report = audit_host_capacity(&config, 10).unwrap();
        assert_eq!(report.payloads.rws_hour_count, 3);
        assert_eq!(report.payloads.run_directories_with_hours, 2);
        assert_eq!(report.payloads.largest_rws_hour_bytes, 20);
        assert_eq!(report.payloads.largest_observed_run_bytes, 30);
        assert_eq!(report.payloads.observed_inflight_hour_headroom_bytes, 60);
        assert_eq!(report.store.observed_regular_file_bytes, 52);
        fs::remove_dir_all(root).unwrap();
    }
}
