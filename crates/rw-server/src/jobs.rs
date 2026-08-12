use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use utoipa::ToSchema;
use uuid::Uuid;

const JOB_SCHEMA: &str = "rw-server.job.v1";
const MAX_JOB_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_JOB_RECORDS: usize = 100_000;

#[derive(Debug, Error)]
pub enum JobError {
    #[error("asynchronous job capacity is full")]
    Capacity,
    #[error("job '{0}' was not found")]
    NotFound(Uuid),
    #[error("artifact was not found")]
    ArtifactNotFound,
    #[error("job state transition is invalid")]
    InvalidTransition,
    #[error("job result exceeds the configured byte limit")]
    ResultTooLarge,
    #[error("invalid job or artifact metadata: {0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] rw_store::RwStoreError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ArtifactRef {
    pub sha256: String,
    pub file: String,
    pub content_type: String,
    pub bytes: u64,
    pub download_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct JobView {
    pub schema: String,
    pub id: Uuid,
    pub kind: String,
    pub request_fingerprint: String,
    pub status: JobStatus,
    pub created_unix: i64,
    pub updated_unix: i64,
    pub artifact: Option<ArtifactRef>,
    pub error_code: Option<String>,
}

#[derive(Clone)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Signal an in-process worker without changing durable job state. The
    /// manager still owns the public state transition through `cancel` or
    /// `fail`; this is used when an execution deadline expires while a
    /// blocking reducer is still unwinding.
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

struct Entry {
    view: JobView,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    entries: BTreeMap<Uuid, Entry>,
}

#[derive(Clone)]
pub struct JobManager {
    root: Arc<PathBuf>,
    max_active: usize,
    max_result_bytes: u64,
    max_records: usize,
    retention_seconds: u64,
    inner: Arc<Mutex<Inner>>,
    objects_guard: Arc<Mutex<()>>,
}

impl std::fmt::Debug for JobManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JobManager")
            .field("max_active", &self.max_active)
            .field("max_result_bytes", &self.max_result_bytes)
            .field("max_records", &self.max_records)
            .field("retention_seconds", &self.retention_seconds)
            .finish_non_exhaustive()
    }
}

impl JobManager {
    pub fn open(
        root: impl Into<PathBuf>,
        max_active: usize,
        max_result_bytes: u64,
        max_records: usize,
        retention_seconds: u64,
    ) -> Result<Self, JobError> {
        if max_active == 0
            || max_result_bytes == 0
            || max_records == 0
            || max_records > MAX_JOB_RECORDS
            || retention_seconds == 0
        {
            return Err(JobError::Invalid(
                "job limits must be greater than zero".to_string(),
            ));
        }
        let root = root.into();
        let jobs_root = root.join("jobs");
        let objects_root = root.join("objects");
        create_real_directory(&root)?;
        create_real_directory(&jobs_root)?;
        create_real_directory(&objects_root)?;

        let mut entries = BTreeMap::new();
        let mut inspected = 0usize;
        for entry in fs::read_dir(&jobs_root)? {
            let entry = entry?;
            inspected += 1;
            if inspected > MAX_JOB_RECORDS {
                return Err(JobError::Invalid(format!(
                    "job directory exceeds {MAX_JOB_RECORDS} entries"
                )));
            }
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || metadata.len() > MAX_JOB_RECORD_BYTES {
                return Err(JobError::Invalid(
                    "job record must be a bounded regular file".to_string(),
                ));
            }
            let mut view: JobView = serde_json::from_slice(&fs::read(&path)?)?;
            validate_view(&view)?;
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    JobError::Invalid("job record name is not valid UTF-8".to_string())
                })?;
            if stem != view.id.to_string() {
                return Err(JobError::Invalid(
                    "job record filename does not match its id".to_string(),
                ));
            }
            if matches!(view.status, JobStatus::Queued | JobStatus::Running) {
                view.status = JobStatus::Failed;
                view.updated_unix = now_unix();
                view.error_code = Some("INTERRUPTED".to_string());
                persist_view(&jobs_root, &view)?;
            }
            entries.insert(
                view.id,
                Entry {
                    view,
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        let manager = Self {
            root: Arc::new(root),
            max_active,
            max_result_bytes,
            max_records,
            retention_seconds,
            inner: Arc::new(Mutex::new(Inner { entries })),
            objects_guard: Arc::new(Mutex::new(())),
        };
        manager.prune()?;
        Ok(manager)
    }

    pub fn create(
        &self,
        kind: impl Into<String>,
        request_fingerprint: impl Into<String>,
    ) -> Result<(JobView, CancellationToken), JobError> {
        let kind = kind.into();
        let request_fingerprint = request_fingerprint.into();
        if kind.is_empty() || kind.len() > 128 || !is_lower_hex(&request_fingerprint, 64) {
            return Err(JobError::Invalid(
                "job kind or request fingerprint is invalid".to_string(),
            ));
        }
        self.prune_to_limit(self.max_records.saturating_sub(1))?;
        let mut inner = self.inner.lock().expect("job manager mutex poisoned");
        if inner.entries.len() >= self.max_records {
            return Err(JobError::Capacity);
        }
        let active = inner
            .entries
            .values()
            .filter(|entry| matches!(entry.view.status, JobStatus::Queued | JobStatus::Running))
            .count();
        if active >= self.max_active {
            return Err(JobError::Capacity);
        }
        let now = now_unix();
        let id = Uuid::new_v4();
        let view = JobView {
            schema: JOB_SCHEMA.to_string(),
            id,
            kind,
            request_fingerprint,
            status: JobStatus::Queued,
            created_unix: now,
            updated_unix: now,
            artifact: None,
            error_code: None,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        persist_view(&self.jobs_root(), &view)?;
        inner.entries.insert(
            id,
            Entry {
                view: view.clone(),
                cancel: cancel.clone(),
            },
        );
        Ok((view, CancellationToken(cancel)))
    }

    pub fn get(&self, id: Uuid) -> Result<JobView, JobError> {
        self.inner
            .lock()
            .expect("job manager mutex poisoned")
            .entries
            .get(&id)
            .map(|entry| entry.view.clone())
            .ok_or(JobError::NotFound(id))
    }

    pub fn mark_running(&self, id: Uuid) -> Result<bool, JobError> {
        self.update(id, |view, cancelled| {
            if cancelled || matches!(view.status, JobStatus::Cancelled) {
                return Ok((false, cancelled));
            }
            if !matches!(view.status, JobStatus::Queued) {
                return Err(JobError::InvalidTransition);
            }
            view.status = JobStatus::Running;
            view.updated_unix = now_unix();
            Ok((true, cancelled))
        })
    }

    pub fn cancel(&self, id: Uuid) -> Result<JobView, JobError> {
        self.update(id, |view, cancelled| {
            let cancel_after = match view.status {
                JobStatus::Queued | JobStatus::Running => {
                    view.status = JobStatus::Cancelled;
                    view.updated_unix = now_unix();
                    view.error_code = Some("CANCELLED".to_string());
                    true
                }
                JobStatus::Cancelled => true,
                JobStatus::Succeeded | JobStatus::Failed => cancelled,
            };
            Ok((view.clone(), cancel_after))
        })
    }

    pub fn fail(&self, id: Uuid, error_code: &str) -> Result<JobView, JobError> {
        if error_code.is_empty()
            || error_code.len() > 64
            || !error_code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(JobError::Invalid(
                "invalid public job error code".to_string(),
            ));
        }
        self.update(id, |view, cancelled| {
            if matches!(view.status, JobStatus::Cancelled) {
                return Ok((view.clone(), cancelled));
            }
            if !matches!(view.status, JobStatus::Running | JobStatus::Queued) {
                return Err(JobError::InvalidTransition);
            }
            view.status = JobStatus::Failed;
            view.updated_unix = now_unix();
            view.error_code = Some(error_code.to_string());
            Ok((view.clone(), cancelled))
        })
    }

    pub fn succeed(
        &self,
        id: Uuid,
        file: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<JobView, JobError> {
        self.succeed_with_hook(id, file, content_type, bytes, || {})
    }

    fn succeed_with_hook(
        &self,
        id: Uuid,
        file: &str,
        content_type: &str,
        bytes: &[u8],
        before_record: impl FnOnce(),
    ) -> Result<JobView, JobError> {
        validate_artifact_file(file)?;
        if content_type.is_empty()
            || content_type.len() > 128
            || content_type.contains(['\r', '\n'])
        {
            return Err(JobError::Invalid(
                "invalid artifact content type".to_string(),
            ));
        }
        if bytes.len() as u64 > self.max_result_bytes {
            return Err(JobError::ResultTooLarge);
        }
        {
            let inner = self.inner.lock().expect("job manager mutex poisoned");
            let entry = inner.entries.get(&id).ok_or(JobError::NotFound(id))?;
            if entry.cancel.load(Ordering::Acquire)
                || matches!(entry.view.status, JobStatus::Cancelled)
            {
                return Ok(entry.view.clone());
            }
            if !matches!(entry.view.status, JobStatus::Running) {
                return Err(JobError::InvalidTransition);
            }
        }
        // Serialize publication with object GC. The guard stays held from
        // before the object becomes visible until the durable job record
        // references it, so pruning can observe only neither or both.
        let _objects_guard = self
            .objects_guard
            .lock()
            .expect("job object mutex poisoned");
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let object_root = self.objects_root().join(&sha256);
        create_real_directory(&object_root)?;
        let artifact_path = object_root.join(file);
        if artifact_path.exists() {
            let existing = fs::read(&artifact_path)?;
            if Sha256::digest(&existing).as_slice() != Sha256::digest(bytes).as_slice() {
                return Err(JobError::Invalid(
                    "content-addressed artifact collision".to_string(),
                ));
            }
        } else {
            rw_store::atomic::atomic_write_bytes(&artifact_path, bytes)?;
        }
        let artifact = ArtifactRef {
            sha256: sha256.clone(),
            file: file.to_string(),
            content_type: content_type.to_string(),
            bytes: bytes.len() as u64,
            download_path: format!("/v1/artifacts/{sha256}/{file}"),
        };
        before_record();
        self.update(id, |view, cancelled| {
            if cancelled || matches!(view.status, JobStatus::Cancelled) {
                return Ok((view.clone(), cancelled));
            }
            if !matches!(view.status, JobStatus::Running) {
                return Err(JobError::InvalidTransition);
            }
            view.status = JobStatus::Succeeded;
            view.updated_unix = now_unix();
            view.artifact = Some(artifact);
            view.error_code = None;
            Ok((view.clone(), cancelled))
        })
    }

    pub fn artifact_path(&self, hash: &str, file: &str) -> Result<PathBuf, JobError> {
        if !is_lower_hex(hash, 64) {
            return Err(JobError::Invalid("invalid artifact hash".to_string()));
        }
        validate_artifact_file(file)?;
        let path = self.objects_root().join(hash).join(file);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                JobError::ArtifactNotFound
            } else {
                JobError::Io(error)
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(JobError::Invalid(
                "artifact is not a regular file".to_string(),
            ));
        }
        Ok(path)
    }

    /// Remove expired terminal job records and unreferenced immutable
    /// artifacts. Active jobs are never evicted. This is also called during
    /// startup and before admitting a new job so a long-running service cannot
    /// grow its durable control plane without bound.
    pub fn prune(&self) -> Result<usize, JobError> {
        self.prune_to_limit(self.max_records)
    }

    fn prune_to_limit(&self, target_records: usize) -> Result<usize, JobError> {
        // Lock order is always objects_guard, then inner. This prevents GC
        // from deleting an object between its atomic write and the durable
        // job-record transition that begins referencing it.
        let _objects_guard = self
            .objects_guard
            .lock()
            .expect("job object mutex poisoned");
        let now = now_unix();
        let cutoff = now.saturating_sub(i64::try_from(self.retention_seconds).unwrap_or(i64::MAX));
        let mut inner = self.inner.lock().expect("job manager mutex poisoned");
        let mut removable = inner
            .entries
            .iter()
            .filter(|(_, entry)| is_terminal(&entry.view.status))
            .map(|(id, entry)| (*id, entry.view.updated_unix))
            .collect::<Vec<_>>();
        removable.sort_by_key(|(id, updated)| (*updated, *id));

        let mut selected = BTreeSet::new();
        for (id, updated) in &removable {
            if *updated < cutoff {
                selected.insert(*id);
            }
        }
        let mut remaining = inner.entries.len().saturating_sub(selected.len());
        for (id, _) in removable {
            if remaining <= target_records {
                break;
            }
            if selected.insert(id) {
                remaining = remaining.saturating_sub(1);
            }
        }

        for id in &selected {
            let path = self.jobs_root().join(format!("{id}.json"));
            match fs::remove_file(&path) {
                Ok(()) => {
                    inner.entries.remove(id);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    inner.entries.remove(id);
                }
                Err(error) => return Err(error.into()),
            }
        }
        let referenced = inner
            .entries
            .values()
            .filter_map(|entry| {
                entry
                    .view
                    .artifact
                    .as_ref()
                    .map(|artifact| artifact.sha256.clone())
            })
            .collect::<BTreeSet<_>>();
        drop(inner);
        self.gc_unreferenced_objects(&referenced)?;
        Ok(selected.len())
    }

    fn gc_unreferenced_objects(&self, referenced: &BTreeSet<String>) -> Result<(), JobError> {
        let root = self.objects_root();
        let mut inspected = 0usize;
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            inspected = inspected.saturating_add(1);
            if inspected > MAX_JOB_RECORDS {
                return Err(JobError::Invalid(format!(
                    "artifact object directory exceeds {MAX_JOB_RECORDS} entries"
                )));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(JobError::Invalid(
                    "artifact object entry must be a real directory".to_string(),
                ));
            }
            let hash = entry.file_name().into_string().map_err(|_| {
                JobError::Invalid("artifact object name is not valid UTF-8".to_string())
            })?;
            if !is_lower_hex(&hash, 64) {
                return Err(JobError::Invalid(
                    "artifact object directory name is invalid".to_string(),
                ));
            }
            if !referenced.contains(&hash) {
                fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }

    fn update<T>(
        &self,
        id: Uuid,
        update: impl FnOnce(&mut JobView, bool) -> Result<(T, bool), JobError>,
    ) -> Result<T, JobError> {
        let mut inner = self.inner.lock().expect("job manager mutex poisoned");
        let entry = inner.entries.get_mut(&id).ok_or(JobError::NotFound(id))?;
        let cancelled = entry.cancel.load(Ordering::Acquire);
        let mut candidate = entry.view.clone();
        let (output, cancel_after) = update(&mut candidate, cancelled)?;
        persist_view(&self.jobs_root(), &candidate)?;
        entry.view = candidate;
        entry.cancel.store(cancel_after, Ordering::Release);
        Ok(output)
    }

    fn jobs_root(&self) -> PathBuf {
        self.root.join("jobs")
    }

    fn objects_root(&self) -> PathBuf {
        self.root.join("objects")
    }
}

fn persist_view(root: &Path, view: &JobView) -> Result<(), JobError> {
    validate_view(view)?;
    let bytes = serde_json::to_vec_pretty(view)?;
    if bytes.len() as u64 > MAX_JOB_RECORD_BYTES {
        return Err(JobError::Invalid("job record exceeds 1 MiB".to_string()));
    }
    rw_store::atomic::atomic_write_bytes(&root.join(format!("{}.json", view.id)), &bytes)?;
    Ok(())
}

fn validate_view(view: &JobView) -> Result<(), JobError> {
    if view.schema != JOB_SCHEMA
        || view.kind.is_empty()
        || view.kind.len() > 128
        || !is_lower_hex(&view.request_fingerprint, 64)
        || view.updated_unix < view.created_unix
    {
        return Err(JobError::Invalid("job record invariant failed".to_string()));
    }
    if let Some(artifact) = &view.artifact {
        if !is_lower_hex(&artifact.sha256, 64) {
            return Err(JobError::Invalid("artifact hash is invalid".to_string()));
        }
        validate_artifact_file(&artifact.file)?;
    }
    Ok(())
}

fn create_real_directory(path: &Path) -> Result<(), JobError> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(JobError::Invalid(format!(
            "'{}' must be a real directory",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("directory")
        )));
    }
    Ok(())
}

fn validate_artifact_file(file: &str) -> Result<(), JobError> {
    if file.is_empty()
        || file.len() > 128
        || file == "."
        || file == ".."
        || !file
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(JobError::Invalid("invalid artifact filename".to_string()));
    }
    Ok(())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn is_terminal(status: &JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Succeeded | JobStatus::Failed | JobStatus::Cancelled
    )
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_are_bounded_cancelled_durable_and_content_addressed() {
        let directory = tempfile::tempdir().unwrap();
        let manager = JobManager::open(directory.path(), 1, 1024, 100, 86_400).unwrap();
        let fingerprint = "a".repeat(64);
        let (first, token) = manager.create("temporal_grid", &fingerprint).unwrap();
        assert!(matches!(first.status, JobStatus::Queued));
        assert!(matches!(
            manager.create("temporal_grid", &fingerprint),
            Err(JobError::Capacity)
        ));
        assert!(manager.mark_running(first.id).unwrap());
        let finished = manager
            .succeed(
                first.id,
                "result.json",
                "application/json",
                br#"{"ok":true}"#,
            )
            .unwrap();
        assert!(matches!(finished.status, JobStatus::Succeeded));
        let artifact = finished.artifact.unwrap();
        assert_eq!(
            fs::read(
                manager
                    .artifact_path(&artifact.sha256, &artifact.file)
                    .unwrap()
            )
            .unwrap(),
            br#"{"ok":true}"#
        );
        assert!(!token.is_cancelled());

        let (second, token) = manager.create("temporal_grid", fingerprint).unwrap();
        manager.cancel(second.id).unwrap();
        assert!(token.is_cancelled());

        let reopened = JobManager::open(directory.path(), 1, 1024, 100, 86_400).unwrap();
        assert!(matches!(
            reopened.get(first.id).unwrap().status,
            JobStatus::Succeeded
        ));
        assert!(matches!(
            reopened.get(second.id).unwrap().status,
            JobStatus::Cancelled
        ));
    }

    #[test]
    fn active_jobs_become_interrupted_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let manager = JobManager::open(directory.path(), 1, 1024, 100, 86_400).unwrap();
        let (job, _) = manager.create("temporal_grid", "b".repeat(64)).unwrap();
        manager.mark_running(job.id).unwrap();
        drop(manager);
        let reopened = JobManager::open(directory.path(), 1, 1024, 100, 86_400).unwrap();
        let recovered = reopened.get(job.id).unwrap();
        assert!(matches!(recovered.status, JobStatus::Failed));
        assert_eq!(recovered.error_code.as_deref(), Some("INTERRUPTED"));
    }

    #[test]
    fn failed_persistence_does_not_mutate_memory_or_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let manager = JobManager::open(directory.path(), 1, 1024, 100, 86_400).unwrap();
        let (job, token) = manager.create("temporal_grid", "d".repeat(64)).unwrap();
        let jobs_root = directory.path().join("jobs");
        let displaced = directory.path().join("jobs-displaced");
        fs::rename(&jobs_root, &displaced).unwrap();
        fs::write(&jobs_root, b"not a directory").unwrap();

        assert!(manager.cancel(job.id).is_err());
        let in_memory = manager.get(job.id).unwrap();
        assert!(matches!(in_memory.status, JobStatus::Queued));
        assert!(!token.is_cancelled());

        fs::remove_file(&jobs_root).unwrap();
        fs::rename(&displaced, &jobs_root).unwrap();
        let persisted: JobView =
            serde_json::from_slice(&fs::read(jobs_root.join(format!("{}.json", job.id))).unwrap())
                .unwrap();
        assert!(matches!(persisted.status, JobStatus::Queued));
    }

    #[test]
    fn prune_cannot_delete_an_artifact_during_publication() {
        use std::sync::{Barrier, mpsc};
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let manager = JobManager::open(directory.path(), 2, 1024, 100, 86_400).unwrap();
        let (job, _) = manager.create("temporal_grid", "e".repeat(64)).unwrap();
        manager.mark_running(job.id).unwrap();

        let object_written = Arc::new(Barrier::new(2));
        let release_publication = Arc::new(Barrier::new(2));
        let succeed_manager = manager.clone();
        let succeed_written = object_written.clone();
        let succeed_release = release_publication.clone();
        let succeed = std::thread::spawn(move || {
            succeed_manager.succeed_with_hook(
                job.id,
                "result.json",
                "application/json",
                br#"{"ok":true}"#,
                || {
                    succeed_written.wait();
                    succeed_release.wait();
                },
            )
        });
        object_written.wait();

        let prune_start = Arc::new(Barrier::new(2));
        let prune_manager = manager.clone();
        let prune_started = prune_start.clone();
        let (prune_done_tx, prune_done_rx) = mpsc::sync_channel(1);
        let prune = std::thread::spawn(move || {
            prune_started.wait();
            let result = prune_manager.prune();
            let _ = prune_done_tx.send(result);
        });
        prune_start.wait();
        assert!(
            prune_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "prune must wait while an object is published but not yet referenced"
        );

        release_publication.wait();
        let finished = succeed.join().unwrap().unwrap();
        assert!(matches!(finished.status, JobStatus::Succeeded));
        prune_done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        prune.join().unwrap();
        let artifact = finished.artifact.unwrap();
        assert_eq!(
            fs::read(
                manager
                    .artifact_path(&artifact.sha256, &artifact.file)
                    .unwrap()
            )
            .unwrap(),
            br#"{"ok":true}"#
        );
    }

    #[test]
    fn expired_jobs_and_unreferenced_objects_are_pruned() {
        let directory = tempfile::tempdir().unwrap();
        let manager = JobManager::open(directory.path(), 1, 1024, 100, 1).unwrap();
        let (job, _) = manager.create("temporal_grid", "c".repeat(64)).unwrap();
        manager.mark_running(job.id).unwrap();
        let finished = manager
            .succeed(job.id, "result.json", "application/json", b"result")
            .unwrap();
        let artifact = finished.artifact.unwrap();
        let record_path = directory
            .path()
            .join("jobs")
            .join(format!("{}.json", job.id));
        let mut record: JobView = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        record.created_unix = 0;
        record.updated_unix = 0;
        rw_store::atomic::atomic_write_bytes(
            &record_path,
            &serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
        drop(manager);

        let reopened = JobManager::open(directory.path(), 1, 1024, 100, 1).unwrap();
        assert!(matches!(reopened.get(job.id), Err(JobError::NotFound(_))));
        assert!(
            !directory
                .path()
                .join("objects")
                .join(artifact.sha256)
                .exists()
        );
    }
}
