use std::collections::BTreeSet;

use rustwx_core::{CycleSpec, ModelId};
use rw_store::run::validate_store_component;

use crate::coverage::RunCoverage;
use crate::error::SchedulerResult;
use crate::plan::{JobPlan, cycle_origin_unix, revalidate_cycle, validate_model_cycle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCandidate {
    model: ModelId,
    cycle: CycleSpec,
    cycle_origin_unix: i64,
    run_id: String,
    coverage_complete: bool,
    available_valid_unix: BTreeSet<i64>,
}

impl RunCandidate {
    pub fn new(
        model: ModelId,
        cycle: CycleSpec,
        run_id: impl Into<String>,
        coverage_complete: bool,
        available_valid_unix: BTreeSet<i64>,
    ) -> SchedulerResult<Self> {
        let cycle = revalidate_cycle(&cycle)?;
        validate_model_cycle(model, &cycle)?;
        let run_id = run_id.into();
        validate_store_component("alias run id", &run_id)?;
        let cycle_origin_unix = cycle_origin_unix(&cycle)?;
        Ok(Self {
            model,
            cycle,
            cycle_origin_unix,
            run_id,
            coverage_complete,
            available_valid_unix,
        })
    }

    pub fn from_coverage(plan: &JobPlan, coverage: &RunCoverage) -> SchedulerResult<Self> {
        plan.validate()?;
        if !coverage.matches_plan(plan) {
            return Err(crate::error::SchedulerError::InvalidCoverage(format!(
                "coverage does not belong to job '{}'",
                plan.job_id
            )));
        }
        Self::new(
            plan.model,
            plan.cycle.clone(),
            plan.run_id.clone(),
            coverage.is_complete(),
            coverage.available_valid_unix(),
        )
    }

    pub fn model(&self) -> ModelId {
        self.model
    }

    pub fn cycle(&self) -> &CycleSpec {
        &self.cycle
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn coverage_complete(&self) -> bool {
        self.coverage_complete
    }

    pub fn available_valid_unix(&self) -> &BTreeSet<i64> {
        &self.available_valid_unix
    }
}

/// Resolve `latest` to the newest completely inventoried run.
pub fn select_latest(model: ModelId, candidates: &[RunCandidate]) -> Option<&RunCandidate> {
    newest_matching(candidates, |candidate| {
        candidate.model == model && candidate.coverage_complete
    })
}

/// Resolve `latest-available` to the newest run with at least one expected
/// valid timestamp present, whether or not the full run has arrived.
pub fn select_latest_available(
    model: ModelId,
    candidates: &[RunCandidate],
) -> Option<&RunCandidate> {
    newest_matching(candidates, |candidate| {
        candidate.model == model && !candidate.available_valid_unix.is_empty()
    })
}

/// Resolve `latest-covering` to the newest run containing every requested
/// valid timestamp. Coverage here is temporal; query layers remain responsible
/// for applying their requested-variable capability contract.
pub fn select_latest_covering<'a>(
    model: ModelId,
    candidates: &'a [RunCandidate],
    required_valid_unix: &BTreeSet<i64>,
) -> Option<&'a RunCandidate> {
    newest_matching(candidates, |candidate| {
        candidate.model == model
            && !candidate.available_valid_unix.is_empty()
            && required_valid_unix.is_subset(&candidate.available_valid_unix)
    })
}

fn newest_matching(
    candidates: &[RunCandidate],
    predicate: impl Fn(&RunCandidate) -> bool,
) -> Option<&RunCandidate> {
    candidates
        .iter()
        .filter(|candidate| predicate(candidate))
        .max_by(|left, right| {
            (left.cycle_origin_unix, left.run_id.as_str())
                .cmp(&(right.cycle_origin_unix, right.run_id.as_str()))
        })
}
