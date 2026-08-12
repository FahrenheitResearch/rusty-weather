use serde::{Deserialize, Serialize};

use crate::error::{SchedulerError, SchedulerResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerLimits {
    pub max_concurrent_jobs: usize,
    pub max_queued_jobs: usize,
}
impl SchedulerLimits {
    pub fn new(max_concurrent_jobs: usize, max_queued_jobs: usize) -> SchedulerResult<Self> {
        let limits = Self {
            max_concurrent_jobs,
            max_queued_jobs,
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(&self) -> SchedulerResult<()> {
        if self.max_concurrent_jobs == 0 {
            return Err(SchedulerError::InvalidConfig(
                "max_concurrent_jobs must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }

    pub fn admit(&self, running: usize, queued: usize) -> SchedulerResult<AdmissionDecision> {
        self.validate()?;
        if running > self.max_concurrent_jobs {
            return Err(SchedulerError::Capacity(format!(
                "{running} running jobs exceeds configured maximum {}",
                self.max_concurrent_jobs
            )));
        }
        if queued > self.max_queued_jobs {
            return Err(SchedulerError::Capacity(format!(
                "{queued} queued jobs exceeds configured maximum {}",
                self.max_queued_jobs
            )));
        }
        if running < self.max_concurrent_jobs && queued == 0 {
            Ok(AdmissionDecision::StartNow)
        } else if queued < self.max_queued_jobs {
            Ok(AdmissionDecision::Queue)
        } else {
            Ok(AdmissionDecision::AtCapacity)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    StartNow,
    Queue,
    AtCapacity,
}
