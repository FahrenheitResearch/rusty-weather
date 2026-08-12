use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::coverage::RunCoverage;
use crate::durable::durable_atomic_write;
use crate::error::{SchedulerError, SchedulerResult};
use crate::plan::JobPlan;

pub const JOB_STATE_SCHEMA: &str = "rw-scheduler.job-state.v1";
pub const MAX_JOB_STATE_BYTES: u64 = 1024 * 1024;
pub const MAX_STATE_DIRECTORY_ENTRIES: usize = 100_000;
pub const MAX_LAST_ERROR_BYTES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff_seconds: u64,
    pub max_backoff_seconds: u64,
}

impl RetryPolicy {
    pub fn new(
        max_attempts: u32,
        initial_backoff_seconds: u64,
        max_backoff_seconds: u64,
    ) -> SchedulerResult<Self> {
        let policy = Self {
            max_attempts,
            initial_backoff_seconds,
            max_backoff_seconds,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> SchedulerResult<()> {
        if self.max_attempts == 0 {
            return Err(SchedulerError::InvalidConfig(
                "retry max_attempts must be greater than zero".to_string(),
            ));
        }
        if self.initial_backoff_seconds == 0 {
            return Err(SchedulerError::InvalidConfig(
                "retry initial_backoff_seconds must be greater than zero".to_string(),
            ));
        }
        if self.max_backoff_seconds < self.initial_backoff_seconds {
            return Err(SchedulerError::InvalidConfig(
                "retry max_backoff_seconds must be at least the initial backoff".to_string(),
            ));
        }
        Ok(())
    }

    pub fn delay_after_attempt(&self, attempt: u32) -> SchedulerResult<u64> {
        self.validate()?;
        if attempt == 0 {
            return Err(SchedulerError::InvalidState(
                "cannot back off an attempt numbered zero".to_string(),
            ));
        }
        let factor = 2_u64.checked_pow(attempt - 1).unwrap_or(u64::MAX);
        Ok(self
            .initial_backoff_seconds
            .saturating_mul(factor)
            .min(self.max_backoff_seconds))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running { started_unix: i64 },
    RetryBackoff { retry_at_unix: i64 },
    Succeeded { finished_unix: i64 },
    Failed { finished_unix: i64 },
}

impl JobState {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Running { .. } | Self::RetryBackoff { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub schema: String,
    pub plan: JobPlan,
    pub state: JobState,
    pub attempts: u32,
    pub recovery_count: u32,
    pub created_unix: i64,
    pub updated_unix: i64,
    pub last_error: Option<String>,
}

impl JobRecord {
    pub fn new(plan: JobPlan, now_unix: i64) -> SchedulerResult<Self> {
        plan.validate()?;
        Ok(Self {
            schema: JOB_STATE_SCHEMA.to_string(),
            plan,
            state: JobState::Queued,
            attempts: 0,
            recovery_count: 0,
            created_unix: now_unix,
            updated_unix: now_unix,
            last_error: None,
        })
    }

    pub fn validate(&self) -> SchedulerResult<()> {
        if self.schema != JOB_STATE_SCHEMA {
            return Err(SchedulerError::InvalidState(format!(
                "unexpected state schema '{}'",
                self.schema
            )));
        }
        self.plan.validate()?;
        if self
            .last_error
            .as_ref()
            .is_some_and(|error| error.len() > MAX_LAST_ERROR_BYTES)
        {
            return Err(SchedulerError::InvalidState(format!(
                "last_error exceeds {MAX_LAST_ERROR_BYTES} bytes"
            )));
        }
        if !matches!(&self.state, JobState::Queued) && self.attempts == 0 {
            return Err(SchedulerError::InvalidState(
                "a started or terminal job must have at least one attempt".to_string(),
            ));
        }
        if matches!(
            &self.state,
            JobState::RetryBackoff { .. } | JobState::Failed { .. }
        ) && self.last_error.is_none()
        {
            return Err(SchedulerError::InvalidState(
                "retrying or failed jobs must retain a diagnostic".to_string(),
            ));
        }
        Ok(())
    }

    pub fn start(&mut self, now_unix: i64, policy: RetryPolicy) -> SchedulerResult<()> {
        policy.validate()?;
        match &self.state {
            JobState::Queued => {}
            JobState::RetryBackoff { retry_at_unix } if now_unix >= *retry_at_unix => {}
            JobState::RetryBackoff { retry_at_unix } => {
                return Err(SchedulerError::InvalidState(format!(
                    "job is in backoff until {retry_at_unix}"
                )));
            }
            _ => {
                return Err(SchedulerError::InvalidState(
                    "only a queued or due retry job can start".to_string(),
                ));
            }
        }
        if self.attempts >= policy.max_attempts {
            return Err(SchedulerError::InvalidState(format!(
                "job exhausted its {} allowed attempts",
                policy.max_attempts
            )));
        }
        self.attempts = self.attempts.checked_add(1).ok_or_else(|| {
            SchedulerError::InvalidState("job attempt counter overflow".to_string())
        })?;
        self.state = JobState::Running {
            started_unix: now_unix,
        };
        self.updated_unix = now_unix;
        Ok(())
    }

    pub fn finish_success(&mut self, now_unix: i64, coverage: &RunCoverage) -> SchedulerResult<()> {
        if !matches!(&self.state, JobState::Running { .. }) {
            return Err(SchedulerError::InvalidState(
                "only a running job can succeed".to_string(),
            ));
        }
        if !coverage.is_complete() {
            return Err(SchedulerError::InvalidCoverage(format!(
                "cannot complete job with {} missing valid times",
                coverage.missing.len()
            )));
        }
        if !coverage.matches_plan(&self.plan) {
            return Err(SchedulerError::InvalidCoverage(format!(
                "coverage does not belong to job '{}'",
                self.plan.job_id
            )));
        }
        self.state = JobState::Succeeded {
            finished_unix: now_unix,
        };
        self.updated_unix = now_unix;
        self.last_error = None;
        Ok(())
    }

    pub fn finish_failure(
        &mut self,
        now_unix: i64,
        error: &str,
        policy: RetryPolicy,
    ) -> SchedulerResult<()> {
        policy.validate()?;
        if !matches!(&self.state, JobState::Running { .. }) {
            return Err(SchedulerError::InvalidState(
                "only a running job can fail".to_string(),
            ));
        }
        let diagnostic = truncate_utf8(error, MAX_LAST_ERROR_BYTES);
        self.last_error = Some(diagnostic);
        self.updated_unix = now_unix;
        if self.attempts >= policy.max_attempts {
            self.state = JobState::Failed {
                finished_unix: now_unix,
            };
        } else {
            let delay = policy.delay_after_attempt(self.attempts)?;
            let retry_at_unix = now_unix
                .checked_add(i64::try_from(delay).map_err(|_| {
                    SchedulerError::InvalidState("retry delay exceeds timestamp range".to_string())
                })?)
                .ok_or_else(|| {
                    SchedulerError::InvalidState("retry timestamp overflow".to_string())
                })?;
            self.state = JobState::RetryBackoff { retry_at_unix };
        }
        Ok(())
    }

    /// Record a failure using a host-computed deterministic jittered delay.
    /// The retry ceiling still comes from `policy`; terminal attempts ignore
    /// the supplied delay.
    pub fn finish_failure_with_delay(
        &mut self,
        now_unix: i64,
        error: &str,
        policy: RetryPolicy,
        retry_delay_seconds: u64,
    ) -> SchedulerResult<()> {
        if retry_delay_seconds == 0 || retry_delay_seconds > policy.max_backoff_seconds {
            return Err(SchedulerError::InvalidState(format!(
                "retry delay must be in 1..={} seconds",
                policy.max_backoff_seconds
            )));
        }
        self.finish_failure(now_unix, error, policy)?;
        if matches!(self.state, JobState::RetryBackoff { .. }) {
            let retry_at_unix = now_unix
                .checked_add(i64::try_from(retry_delay_seconds).map_err(|_| {
                    SchedulerError::InvalidState("retry delay exceeds timestamp range".to_string())
                })?)
                .ok_or_else(|| {
                    SchedulerError::InvalidState("retry timestamp overflow".to_string())
                })?;
            self.state = JobState::RetryBackoff { retry_at_unix };
        }
        Ok(())
    }

    /// Re-admit a terminal job whose deep storage verification no longer
    /// passes (for example an operator removed a published hour). The repair
    /// gets a fresh bounded retry budget and preserves the diagnostic.
    pub fn requeue_for_repair(&mut self, now_unix: i64, reason: &str) -> SchedulerResult<()> {
        if !matches!(
            self.state,
            JobState::Succeeded { .. } | JobState::Failed { .. }
        ) {
            return Err(SchedulerError::InvalidState(
                "only a terminal job can be requeued for repair".to_string(),
            ));
        }
        self.state = JobState::Queued;
        self.attempts = 0;
        self.updated_unix = now_unix;
        self.last_error = Some(truncate_utf8(reason, MAX_LAST_ERROR_BYTES));
        Ok(())
    }

    pub fn release_retry(&mut self, now_unix: i64) -> SchedulerResult<()> {
        let retry_at_unix = match &self.state {
            JobState::RetryBackoff { retry_at_unix } => *retry_at_unix,
            _ => {
                return Err(SchedulerError::InvalidState(
                    "only a retry-backoff job can be released".to_string(),
                ));
            }
        };
        if now_unix < retry_at_unix {
            return Err(SchedulerError::InvalidState(format!(
                "job is in backoff until {retry_at_unix}"
            )));
        }
        self.state = JobState::Queued;
        self.updated_unix = now_unix;
        Ok(())
    }

    /// Recover an interrupted in-flight attempt. The unfinished attempt is
    /// rolled back so restart alone cannot consume the retry budget; the
    /// separate recovery counter preserves that diagnostic fact.
    pub fn recover_after_restart(&mut self, now_unix: i64) -> SchedulerResult<bool> {
        if !matches!(&self.state, JobState::Running { .. }) {
            return Ok(false);
        }
        if self.attempts == 0 {
            return Err(SchedulerError::InvalidState(
                "running job has no in-flight attempt".to_string(),
            ));
        }
        self.attempts -= 1;
        self.recovery_count = self.recovery_count.checked_add(1).ok_or_else(|| {
            SchedulerError::InvalidState("job recovery counter overflow".to_string())
        })?;
        self.state = JobState::Queued;
        self.updated_unix = now_unix;
        Ok(true)
    }
}
#[derive(Debug, Clone)]
pub struct JobStateStore {
    root: PathBuf,
}

impl JobStateStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn save(&self, record: &JobRecord) -> SchedulerResult<()> {
        record.validate()?;
        fs::create_dir_all(&self.root)?;
        let root_metadata = fs::symlink_metadata(&self.root)?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(SchedulerError::InvalidState(format!(
                "state root '{}' must be a real directory",
                self.root.display()
            )));
        }
        let bytes = serde_json::to_vec_pretty(record)?;
        if bytes.len() as u64 > MAX_JOB_STATE_BYTES {
            return Err(SchedulerError::InvalidState(format!(
                "serialized job state is {} bytes; limit is {MAX_JOB_STATE_BYTES}",
                bytes.len()
            )));
        }
        durable_atomic_write(&self.path_for(&record.plan.job_id)?, &bytes)
    }

    pub fn load(&self, job_id: &str) -> SchedulerResult<JobRecord> {
        self.validate_root()?;
        let path = self.path_for(job_id)?;
        let record = read_record(&path)?;
        if record.plan.job_id != job_id {
            return Err(SchedulerError::InvalidState(format!(
                "state file '{}' contains job '{}'",
                path.display(),
                record.plan.job_id
            )));
        }
        record.validate()?;
        Ok(record)
    }

    pub fn load_all(&self) -> SchedulerResult<Vec<JobRecord>> {
        match fs::symlink_metadata(&self.root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(SchedulerError::InvalidState(format!(
                    "state root '{}' must be a real directory",
                    self.root.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }

        let mut records = Vec::new();
        let mut entries_seen = 0_usize;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            entries_seen = entries_seen.checked_add(1).ok_or_else(|| {
                SchedulerError::InvalidState("state directory entry count overflow".to_string())
            })?;
            if entries_seen > MAX_STATE_DIRECTORY_ENTRIES {
                return Err(SchedulerError::InvalidState(format!(
                    "state directory exceeds {MAX_STATE_DIRECTORY_ENTRIES} entries"
                )));
            }
            let path = entry.path();
            if path.extension() != Some(OsStr::new("json")) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_file() {
                return Err(SchedulerError::InvalidState(format!(
                    "state entry '{}' must be a real regular file",
                    path.display()
                )));
            }
            let job_id = path.file_stem().and_then(OsStr::to_str).ok_or_else(|| {
                SchedulerError::InvalidState(format!(
                    "state filename '{}' is not valid UTF-8",
                    path.display()
                ))
            })?;
            let record = self.load(job_id)?;
            records.push(record);
        }
        records.sort_by(|left, right| left.plan.job_id.cmp(&right.plan.job_id));
        Ok(records)
    }

    pub fn recover_running(&self, now_unix: i64) -> SchedulerResult<Vec<JobRecord>> {
        let mut records = self.load_all()?;
        for record in &mut records {
            if record.recover_after_restart(now_unix)? {
                self.save(record)?;
            }
        }
        Ok(records)
    }

    /// Remove terminal state records whose corresponding run was already
    /// removed by retention. Active records and any record whose run still
    /// exists are never selected. The caller supplies the exact job IDs from
    /// a successfully executed retention plan.
    pub fn remove_terminal(&self, job_id: &str) -> SchedulerResult<()> {
        let record = self.load(job_id)?;
        if record.state.is_active() {
            return Err(SchedulerError::InvalidState(format!(
                "refusing to remove active scheduler state '{job_id}'"
            )));
        }
        let path = self.path_for(job_id)?;
        fs::remove_file(&path)?;
        if let Ok(parent) = File::open(&self.root) {
            let _ = parent.sync_all();
        }
        Ok(())
    }

    fn path_for(&self, job_id: &str) -> SchedulerResult<PathBuf> {
        rw_store::run::validate_store_component("scheduler job id", job_id)?;
        Ok(self.root.join(format!("{job_id}.json")))
    }

    fn validate_root(&self) -> SchedulerResult<()> {
        let metadata = fs::symlink_metadata(&self.root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SchedulerError::InvalidState(format!(
                "state root '{}' must be a real directory",
                self.root.display()
            )));
        }
        Ok(())
    }
}

fn read_record(path: &Path) -> SchedulerResult<JobRecord> {
    let initial_metadata = fs::symlink_metadata(path)?;
    if initial_metadata.file_type().is_symlink() || !initial_metadata.is_file() {
        return Err(SchedulerError::InvalidState(format!(
            "state file '{}' must be a real regular file",
            path.display()
        )));
    }
    let file = File::open(path)?;
    let open_metadata = file.metadata()?;
    if !open_metadata.is_file() {
        return Err(SchedulerError::InvalidState(format!(
            "state file '{}' changed to a non-file before opening",
            path.display()
        )));
    }
    if open_metadata.len() > MAX_JOB_STATE_BYTES {
        return Err(SchedulerError::InvalidState(format!(
            "state file '{}' is {} bytes; limit is {MAX_JOB_STATE_BYTES}",
            path.display(),
            open_metadata.len()
        )));
    }
    let mut bytes = Vec::with_capacity(open_metadata.len() as usize);
    file.take(MAX_JOB_STATE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JOB_STATE_BYTES {
        return Err(SchedulerError::InvalidState(format!(
            "state file '{}' grew beyond {MAX_JOB_STATE_BYTES} bytes while reading",
            path.display()
        )));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}
