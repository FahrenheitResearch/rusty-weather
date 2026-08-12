use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rustwx_core::{CycleSpec, ModelId};
use rw_store::RunLock;
use rw_store::run::validate_store_component;
use serde::{Deserialize, Serialize};

use crate::durable::durable_atomic_write;
use crate::error::{SchedulerError, SchedulerResult};
use crate::plan::{canonical_run_id, cycle_origin_unix, revalidate_cycle};
use crate::state::JobRecord;

pub(crate) const OWNER_FILE: &str = ".rw-scheduler-owner.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OwnerMarker {
    schema: String,
    job_id: String,
    model: ModelId,
    run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunKey {
    model: ModelId,
    run_id: String,
}
impl RunKey {
    pub fn new(model: ModelId, run_id: impl Into<String>) -> SchedulerResult<Self> {
        let run_id = run_id.into();
        validate_store_component("retention run id", &run_id)?;
        Ok(Self { model, run_id })
    }

    pub fn model(&self) -> ModelId {
        self.model
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionRun {
    key: RunKey,
    cycle: CycleSpec,
    cycle_origin_unix: i64,
    active: bool,
}

impl RetentionRun {
    pub fn new(
        model: ModelId,
        cycle: CycleSpec,
        run_id: impl Into<String>,
        active: bool,
    ) -> SchedulerResult<Self> {
        let cycle = revalidate_cycle(&cycle)?;
        let run_id = run_id.into();
        if run_id != canonical_run_id(&cycle) {
            return Err(SchedulerError::InvalidConfig(format!(
                "retention run '{run_id}' does not match cycle {} {:02}z",
                cycle.date_yyyymmdd, cycle.hour_utc
            )));
        }
        let key = RunKey::new(model, run_id)?;
        let cycle_origin_unix = cycle_origin_unix(&cycle)?;
        Ok(Self {
            key,
            cycle,
            cycle_origin_unix,
            active,
        })
    }

    pub fn key(&self) -> &RunKey {
        &self.key
    }

    pub fn cycle(&self) -> &CycleSpec {
        &self.cycle
    }

    pub fn is_active(&self) -> bool {
        self.active
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPlan {
    pub keep: BTreeSet<RunKey>,
    pub delete: BTreeSet<RunKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetentionExecution {
    pub dry_run: bool,
    pub candidates: Vec<String>,
    pub deleted: Vec<String>,
    /// Windows can deny renaming a directory while its advisory lock file is
    /// open. In that case payload files are removed under the held lock and a
    /// tiny, non-queryable ownership shell remains.
    pub purged_shells: Vec<String>,
    /// Applied candidates whose payload is gone (or was already absent).
    /// The scheduler host uses this exact inventory to prune terminal durable
    /// state without parsing human-readable diagnostics.
    pub state_prunable: Vec<String>,
    pub skipped: Vec<String>,
}

/// Plan retention for scheduler-owned runs. This function never mutates the
/// filesystem; callers must independently revalidate ownership and locks just
/// before executing a returned deletion candidate.
pub fn plan_retention(
    runs: &[RetentionRun],
    aliased: &BTreeSet<RunKey>,
    keep_latest_per_model: usize,
) -> SchedulerResult<RetentionPlan> {
    let mut seen = BTreeSet::new();
    let mut by_model = BTreeMap::<ModelId, Vec<&RetentionRun>>::new();
    for run in runs {
        if !seen.insert(run.key.clone()) {
            return Err(SchedulerError::InvalidConfig(format!(
                "duplicate retention run '{}:{}'",
                run.key.model, run.key.run_id
            )));
        }
        by_model.entry(run.key.model).or_default().push(run);
    }

    let mut keep = BTreeSet::new();
    for run in runs {
        if run.active || aliased.contains(&run.key) {
            keep.insert(run.key.clone());
        }
    }
    for model_runs in by_model.values_mut() {
        model_runs.sort_by(|left, right| {
            (right.cycle_origin_unix, right.key.run_id.as_str())
                .cmp(&(left.cycle_origin_unix, left.key.run_id.as_str()))
        });
        keep.extend(
            model_runs
                .iter()
                .take(keep_latest_per_model)
                .map(|run| run.key.clone()),
        );
    }

    let delete = seen.difference(&keep).cloned().collect();
    Ok(RetentionPlan { keep, delete })
}

/// Build retention candidates exclusively from durable scheduler state. A
/// directory that merely resembles a model/run layout is never adopted.
pub fn plan_owned_retention(
    records: &[JobRecord],
    aliased: &BTreeSet<RunKey>,
    keep_latest_per_model: usize,
) -> SchedulerResult<RetentionPlan> {
    let mut runs = Vec::with_capacity(records.len());
    for record in records {
        runs.push(RetentionRun::new(
            record.plan.model,
            record.plan.cycle.clone(),
            record.plan.run_id.clone(),
            record.state.is_active(),
        )?);
    }
    plan_retention(&runs, aliased, keep_latest_per_model)
}

/// Execute a retention plan using scheduler state as the ownership boundary.
/// The safe default is dry-run. Every real deletion revalidates the root,
/// model/run components, ownership marker, and advisory writer lock before an
/// atomic same-parent rename to a tombstone directory.
pub fn execute_retention(
    store_root: &Path,
    records: &[JobRecord],
    plan: &RetentionPlan,
    dry_run: bool,
) -> SchedulerResult<RetentionExecution> {
    let root = real_directory(store_root, "store root")?;
    let by_key = records
        .iter()
        .map(|record| {
            (
                RunKey::new(record.plan.model, record.plan.run_id.clone()),
                record,
            )
        })
        .map(|(key, record)| key.map(|key| (key, record)))
        .collect::<SchedulerResult<BTreeMap<_, _>>>()?;
    let mut report = RetentionExecution {
        dry_run,
        candidates: Vec::new(),
        deleted: Vec::new(),
        purged_shells: Vec::new(),
        state_prunable: Vec::new(),
        skipped: Vec::new(),
    };
    for key in &plan.delete {
        let label = format!("{}:{}", key.model(), key.run_id());
        report.candidates.push(label.clone());
        let Some(record) = by_key.get(key) else {
            return Err(SchedulerError::InvalidState(format!(
                "retention candidate '{label}' has no scheduler state"
            )));
        };
        if record.state.is_active() {
            return Err(SchedulerError::InvalidState(format!(
                "retention candidate '{label}' became active"
            )));
        }
        let model_dir = root.join(key.model().as_str());
        let run_dir = model_dir.join(key.run_id());
        if !run_dir.exists() {
            report.skipped.push(format!("{label}: absent"));
            if !dry_run {
                report.state_prunable.push(label);
            }
            continue;
        }
        revalidate_owned_run(&root, &run_dir, record)?;
        if run_dir.join(".rw-scheduler-purged.json").is_file() {
            report.skipped.push(format!("{label}: already purged"));
            if !dry_run {
                report.state_prunable.push(label);
            }
            continue;
        }
        if dry_run {
            continue;
        }
        let Some(lock) = RunLock::try_acquire(&run_dir)? else {
            report.skipped.push(format!("{label}: writer lock held"));
            continue;
        };
        // Close the check/use gap as much as a cooperative filesystem
        // protocol permits: identity and marker are checked again while the
        // exclusive writer lock is held.
        revalidate_owned_run(&root, &run_dir, record)?;
        let tombstone = model_dir.join(format!(
            ".rw-scheduler-delete-{}-{}",
            key.run_id(),
            std::process::id()
        ));
        if fs::symlink_metadata(&tombstone).is_ok() {
            return Err(SchedulerError::InvalidState(format!(
                "retention tombstone '{}' already exists",
                tombstone.display()
            )));
        }
        if let Err(error) = fs::rename(&run_dir, &tombstone) {
            #[cfg(windows)]
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                purge_payload_under_lock(&run_dir, record)?;
                drop(lock);
                report.purged_shells.push(label.clone());
                report.state_prunable.push(label);
                continue;
            }
            return Err(error.into());
        }
        drop(lock);
        let tombstone_parent = tombstone.parent().ok_or_else(|| {
            SchedulerError::InvalidState("retention tombstone has no parent".to_string())
        })?;
        if fs::canonicalize(tombstone_parent)? != fs::canonicalize(&model_dir)? {
            return Err(SchedulerError::InvalidState(
                "retention tombstone escaped its model directory".to_string(),
            ));
        }
        fs::remove_dir_all(&tombstone)?;
        report.deleted.push(label.clone());
        report.state_prunable.push(label);
    }
    Ok(report)
}

#[cfg(windows)]
fn purge_payload_under_lock(run_dir: &Path, record: &JobRecord) -> SchedulerResult<()> {
    for entry in fs::read_dir(run_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == OWNER_FILE || name == rw_store::LOCK_FILE_NAME {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(SchedulerError::InvalidState(format!(
                "refusing to purge symlink '{}'",
                entry.path().display()
            )));
        }
        if metadata.is_dir() {
            remove_tree_no_symlinks(&entry.path())?;
        } else if metadata.is_file() {
            fs::remove_file(entry.path())?;
        } else {
            return Err(SchedulerError::InvalidState(format!(
                "refusing to purge special entry '{}'",
                entry.path().display()
            )));
        }
    }
    let marker = serde_json::json!({
        "schema": "rw-scheduler.purged.v1",
        "job_id": record.plan.job_id,
        "model": record.plan.model,
        "run_id": record.plan.run_id,
    });
    durable_atomic_write(
        &run_dir.join(".rw-scheduler-purged.json"),
        &serde_json::to_vec_pretty(&marker)?,
    )
}

#[cfg(windows)]
fn remove_tree_no_symlinks(path: &Path) -> SchedulerResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SchedulerError::InvalidState(format!(
            "purge subtree '{}' must be a real directory",
            path.display()
        )));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(SchedulerError::InvalidState(format!(
                "refusing to purge nested symlink '{}'",
                entry.path().display()
            )));
        }
        if metadata.is_dir() {
            remove_tree_no_symlinks(&entry.path())?;
        } else if metadata.is_file() {
            fs::remove_file(entry.path())?;
        } else {
            return Err(SchedulerError::InvalidState(format!(
                "refusing to purge nested special entry '{}'",
                entry.path().display()
            )));
        }
    }
    fs::remove_dir(path)?;
    Ok(())
}

pub(crate) fn ensure_owner_marker(run_dir: &Path, record: &JobRecord) -> SchedulerResult<()> {
    fs::create_dir_all(run_dir)?;
    let metadata = fs::symlink_metadata(run_dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SchedulerError::InvalidState(format!(
            "run '{}' must be a real directory",
            run_dir.display()
        )));
    }
    let expected = OwnerMarker {
        schema: "rw-scheduler.owner.v1".to_string(),
        job_id: record.plan.job_id.clone(),
        model: record.plan.model,
        run_id: record.plan.run_id.clone(),
    };
    let path = run_dir.join(OWNER_FILE);
    if path.exists() {
        let actual = load_owner(&path)?;
        if actual != expected {
            return Err(SchedulerError::InvalidState(format!(
                "run '{}' has a different scheduler owner",
                run_dir.display()
            )));
        }
        return Ok(());
    }
    durable_atomic_write(&path, &serde_json::to_vec_pretty(&expected)?)
}

fn revalidate_owned_run(root: &Path, run_dir: &Path, record: &JobRecord) -> SchedulerResult<()> {
    validate_store_component("retention model", record.plan.model.as_str())?;
    validate_store_component("retention run", &record.plan.run_id)?;
    let metadata = fs::symlink_metadata(run_dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SchedulerError::InvalidState(format!(
            "retention target '{}' must be a real directory",
            run_dir.display()
        )));
    }
    let canonical = fs::canonicalize(run_dir)?;
    let expected_parent = root.join(record.plan.model.as_str());
    let canonical_parent = fs::canonicalize(&expected_parent)?;
    if canonical.parent() != Some(canonical_parent.as_path()) || !canonical.starts_with(root) {
        return Err(SchedulerError::InvalidState(format!(
            "retention target '{}' escaped its expected root",
            run_dir.display()
        )));
    }
    let owner = load_owner(&run_dir.join(OWNER_FILE))?;
    if owner.job_id != record.plan.job_id
        || owner.model != record.plan.model
        || owner.run_id != record.plan.run_id
    {
        return Err(SchedulerError::InvalidState(format!(
            "retention target '{}' is not owned by job '{}'",
            run_dir.display(),
            record.plan.job_id
        )));
    }
    Ok(())
}

fn load_owner(path: &Path) -> SchedulerResult<OwnerMarker> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 16 * 1024 {
        return Err(SchedulerError::InvalidState(format!(
            "owner marker '{}' is not a bounded regular file",
            path.display()
        )));
    }
    let owner: OwnerMarker = serde_json::from_slice(&fs::read(path)?)?;
    if owner.schema != "rw-scheduler.owner.v1" {
        return Err(SchedulerError::InvalidState(format!(
            "owner marker '{}' has an unsupported schema",
            path.display()
        )));
    }
    Ok(owner)
}

fn real_directory(path: &Path, label: &str) -> SchedulerResult<PathBuf> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SchedulerError::InvalidState(format!(
            "{label} '{}' must be a real directory",
            path.display()
        )));
    }
    Ok(fs::canonicalize(path)?)
}
