use rustwx_core::{CycleSpec, ModelId, SourceId};
use rustwx_models::{model_summary, supported_forecast_hours};
use rw_ingest::ingest_profile::{FieldSet, IngestProfile, VolumeChoice};
use rw_ingest::{
    IngestCapabilityLimitation, IngestSupportStatus, model_ingest_capability,
    model_source_ingest_supported,
};
use rw_store::run::validate_store_component;
use serde::{Deserialize, Serialize};

use crate::error::{SchedulerError, SchedulerResult};

pub const JOB_PLAN_SCHEMA: &str = "rw-scheduler.job-plan.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestProductPlan {
    pub product: String,
    pub surface_source: bool,
    pub pressure_source: bool,
    pub idx_patterns: Vec<String>,
}

/// The exact ingest shape attached to a durable job. Configuration is only
/// consulted when a job is first admitted; restarts reconstruct this value so
/// a config edit cannot produce a run whose hours contain different fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedIngestProfile {
    pub volumes: Vec<String>,
    pub level_step_hpa: u16,
    /// `None` is the full 2-D set; `Some` is an explicit named surface set.
    pub surface_fields: Option<Vec<String>>,
    pub derived: bool,
    pub heavy: bool,
}

impl PersistedIngestProfile {
    pub fn from_profile(profile: &IngestProfile) -> SchedulerResult<Self> {
        profile.validate().map_err(SchedulerError::InvalidPlan)?;
        Ok(Self {
            volumes: profile
                .volumes
                .iter()
                .map(|choice| choice.store_name().to_string())
                .collect(),
            level_step_hpa: profile.level_step_hpa,
            surface_fields: match &profile.surface_fields {
                FieldSet::All => None,
                FieldSet::Named(names) => Some(names.clone()),
            },
            derived: profile.derived,
            heavy: profile.heavy,
        })
    }

    pub fn to_profile(&self) -> SchedulerResult<IngestProfile> {
        let volumes = self
            .volumes
            .iter()
            .map(|name| match name.as_str() {
                "temperature_iso" => Ok(VolumeChoice::Temperature),
                "dewpoint_iso" => Ok(VolumeChoice::Dewpoint),
                "u_iso" => Ok(VolumeChoice::UWind),
                "v_iso" => Ok(VolumeChoice::VWind),
                "height_iso" => Ok(VolumeChoice::GeopotentialHeight),
                _ => Err(SchedulerError::InvalidPlan(format!(
                    "unknown persisted volume '{name}'"
                ))),
            })
            .collect::<SchedulerResult<Vec<_>>>()?;
        let profile = IngestProfile {
            volumes,
            level_step_hpa: self.level_step_hpa,
            surface_fields: self
                .surface_fields
                .clone()
                .map(FieldSet::Named)
                .unwrap_or(FieldSet::All),
            derived: self.derived,
            heavy: self.heavy,
        };
        profile.validate().map_err(SchedulerError::InvalidPlan)?;
        Ok(profile)
    }
}

impl Default for PersistedIngestProfile {
    fn default() -> Self {
        Self::from_profile(&IngestProfile::full()).expect("built-in full profile is valid")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExpectedValidTime {
    pub storage_slot: u16,
    pub forecast_hour: u16,
    pub lead_seconds: u64,
    pub valid_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobPlan {
    pub schema: String,
    pub job_id: String,
    pub model: ModelId,
    pub cycle: CycleSpec,
    pub run_id: String,
    pub expected_valid_times: Vec<ExpectedValidTime>,
    pub ingest_products: Vec<IngestProductPlan>,
    /// Typed capability restrictions snapshotted as stable slugs at
    /// admission. Validation never consults a future registry revision.
    #[serde(default)]
    pub capability_limitations: Vec<String>,
    #[serde(default)]
    pub ingest_profile: PersistedIngestProfile,
    /// Provider chosen during cycle discovery. Persisted so retries do not
    /// silently move between providers after a configuration edit.
    #[serde(default)]
    pub source_override: Option<SourceId>,
}

impl JobPlan {
    pub fn build(model: ModelId, cycle: CycleSpec) -> SchedulerResult<Self> {
        let capability = model_ingest_capability(model);
        let profile = if capability
            .limitations
            .contains(&IngestCapabilityLimitation::AnalysisOnly)
        {
            IngestProfile::analysis()
        } else if capability
            .limitations
            .contains(&IngestCapabilityLimitation::SurfaceOnly)
        {
            IngestProfile::surface()
        } else if capability
            .limitations
            .contains(&IngestCapabilityLimitation::DerivedProductsDisabled)
        {
            let mut profile = IngestProfile::full();
            profile.derived = false;
            profile.heavy = false;
            profile
        } else {
            IngestProfile::full()
        };
        Self::build_with_profile(model, cycle, &profile)
    }

    pub fn build_with_profile(
        model: ModelId,
        cycle: CycleSpec,
        profile: &IngestProfile,
    ) -> SchedulerResult<Self> {
        Self::build_with_profile_and_source(model, cycle, profile, None)
    }

    pub fn build_with_profile_and_source(
        model: ModelId,
        cycle: CycleSpec,
        profile: &IngestProfile,
        source_override: Option<SourceId>,
    ) -> SchedulerResult<Self> {
        let cycle = revalidate_cycle(&cycle)?;
        validate_model_cycle(model, &cycle)?;
        validate_source_for_model(model, source_override)?;

        let capability = model_ingest_capability(model);
        if capability.status != IngestSupportStatus::Ready || capability.products.is_empty() {
            return Err(SchedulerError::UnsupportedModel(model));
        }
        validate_profile_for_capability(model, profile, &capability.limitations)?;
        let ingest_profile = PersistedIngestProfile::from_profile(profile)?;
        let capability_limitations = capability
            .limitations
            .iter()
            .map(|limitation| limitation_slug(*limitation).to_string())
            .collect();

        let origin_unix = cycle_origin_unix(&cycle)?;
        let forecast_hours = supported_forecast_hours(model, cycle.hour_utc);
        if forecast_hours.is_empty() {
            return Err(SchedulerError::InvalidPlan(format!(
                "model '{model}' cycle {:02}z has no expected forecast times",
                cycle.hour_utc
            )));
        }
        let expected_valid_times = forecast_hours
            .into_iter()
            .enumerate()
            .map(|(storage_slot, forecast_hour)| {
                let storage_slot = u16::try_from(storage_slot).map_err(|_| {
                    SchedulerError::InvalidPlan(format!(
                        "model '{model}' has more than {} schedulable files",
                        u16::MAX
                    ))
                })?;
                let lead_seconds =
                    u64::from(forecast_hour).checked_mul(3_600).ok_or_else(|| {
                        SchedulerError::InvalidPlan(format!(
                            "forecast hour {forecast_hour} cannot be represented as seconds"
                        ))
                    })?;
                let valid_unix = origin_unix
                    .checked_add(i64::try_from(lead_seconds).map_err(|_| {
                        SchedulerError::InvalidPlan(format!(
                            "forecast hour {forecast_hour} exceeds the timestamp range"
                        ))
                    })?)
                    .ok_or_else(|| {
                        SchedulerError::InvalidPlan(format!(
                            "forecast hour {forecast_hour} overflows the valid timestamp"
                        ))
                    })?;
                Ok(ExpectedValidTime {
                    storage_slot,
                    forecast_hour,
                    lead_seconds,
                    valid_unix,
                })
            })
            .collect::<SchedulerResult<Vec<_>>>()?;

        let run_id = canonical_run_id(&cycle);
        let job_id = canonical_job_id(model, &cycle);
        validate_store_component("scheduler run id", &run_id)?;
        validate_store_component("scheduler job id", &job_id)?;

        let ingest_products = capability
            .products
            .into_iter()
            .map(|product| IngestProductPlan {
                product: product.product.to_string(),
                surface_source: product.surface_source,
                pressure_source: product.pressure_source,
                idx_patterns: product
                    .idx_patterns
                    .iter()
                    .map(|pattern| (*pattern).to_string())
                    .collect(),
            })
            .collect();

        Ok(Self {
            schema: JOB_PLAN_SCHEMA.to_string(),
            job_id,
            model,
            cycle,
            run_id,
            expected_valid_times,
            ingest_products,
            capability_limitations,
            ingest_profile,
            source_override,
        })
    }

    /// Validate the persisted plan against its own immutable contract.
    ///
    /// Deliberately do not rebuild it from the current model registry here:
    /// provider products and cadence knowledge can expand after a deployment,
    /// while an already admitted job must remain restartable with the exact
    /// expectations it was created with.
    pub fn validate(&self) -> SchedulerResult<()> {
        if self.schema != JOB_PLAN_SCHEMA {
            return Err(SchedulerError::InvalidPlan(format!(
                "unexpected job-plan schema '{}'",
                self.schema
            )));
        }
        let cycle = revalidate_cycle(&self.cycle)?;
        validate_source_for_model(self.model, self.source_override)?;
        let expected_run_id = canonical_run_id(&cycle);
        let expected_job_id = canonical_job_id(self.model, &cycle);
        if self.run_id != expected_run_id || self.job_id != expected_job_id {
            return Err(SchedulerError::InvalidPlan(
                "job or run identifier does not match its model cycle".to_string(),
            ));
        }
        validate_store_component("scheduler run id", &self.run_id)?;
        validate_store_component("scheduler job id", &self.job_id)?;
        if self.expected_valid_times.is_empty() {
            return Err(SchedulerError::InvalidPlan(
                "job has no expected valid times".to_string(),
            ));
        }
        let origin_unix = cycle_origin_unix(&cycle)?;
        let mut prior_forecast_hour = None;
        for (index, expected) in self.expected_valid_times.iter().enumerate() {
            let storage_slot = u16::try_from(index).map_err(|_| {
                SchedulerError::InvalidPlan("job has too many expected valid times".to_string())
            })?;
            if expected.storage_slot != storage_slot {
                return Err(SchedulerError::InvalidPlan(format!(
                    "expected time {} uses storage slot {} instead of {storage_slot}",
                    index, expected.storage_slot
                )));
            }
            if prior_forecast_hour.is_some_and(|prior| expected.forecast_hour <= prior) {
                return Err(SchedulerError::InvalidPlan(
                    "forecast hours must be strictly increasing".to_string(),
                ));
            }
            prior_forecast_hour = Some(expected.forecast_hour);
            let lead_seconds = u64::from(expected.forecast_hour)
                .checked_mul(3_600)
                .ok_or_else(|| SchedulerError::InvalidPlan("forecast lead overflow".to_string()))?;
            let valid_unix = origin_unix
                .checked_add(i64::try_from(lead_seconds).map_err(|_| {
                    SchedulerError::InvalidPlan("forecast lead exceeds timestamp range".to_string())
                })?)
                .ok_or_else(|| {
                    SchedulerError::InvalidPlan("valid timestamp overflow".to_string())
                })?;
            if expected.lead_seconds != lead_seconds || expected.valid_unix != valid_unix {
                return Err(SchedulerError::InvalidPlan(format!(
                    "expected time at storage slot {storage_slot} is inconsistent"
                )));
            }
        }
        if self.ingest_products.is_empty() {
            return Err(SchedulerError::InvalidPlan(
                "job has no persisted ingest products".to_string(),
            ));
        }
        let profile = self.ingest_profile.to_profile()?;
        let mut limitations = std::collections::BTreeSet::new();
        for limitation in &self.capability_limitations {
            if !KNOWN_LIMITATION_SLUGS.contains(&limitation.as_str())
                || !limitations.insert(limitation.as_str())
            {
                return Err(SchedulerError::InvalidPlan(
                    "persisted capability limitations are invalid".to_string(),
                ));
            }
        }
        if profile.needs_prs()
            && !self
                .ingest_products
                .iter()
                .any(|product| product.pressure_source)
        {
            return Err(SchedulerError::InvalidPlan(
                "persisted profile requires pressure data absent from its product plan".to_string(),
            ));
        }
        if limitations.contains("derived_products_disabled") && (profile.derived || profile.heavy) {
            return Err(SchedulerError::InvalidPlan(
                "persisted profile violates its derived-products limitation".to_string(),
            ));
        }
        let mut product_names = std::collections::BTreeSet::new();
        for product in &self.ingest_products {
            if product.product.trim().is_empty()
                || product.product.trim() != product.product
                || (!product.surface_source && !product.pressure_source)
                || !product_names.insert(product.product.as_str())
                || product
                    .idx_patterns
                    .iter()
                    .any(|pattern| pattern.is_empty() || pattern.trim() != pattern)
            {
                return Err(SchedulerError::InvalidPlan(
                    "persisted ingest product contract is invalid".to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn origin_unix(&self) -> SchedulerResult<i64> {
        cycle_origin_unix(&self.cycle)
    }
}

const KNOWN_LIMITATION_SLUGS: &[&str] = &[
    "analysis_only",
    "surface_only",
    "ensemble_mean_only",
    "ensemble_control_member_only",
    "sparse_pressure_levels",
    "derived_products_disabled",
    "conus_only",
    "pre_operational_feed",
];

fn limitation_slug(limitation: IngestCapabilityLimitation) -> &'static str {
    match limitation {
        IngestCapabilityLimitation::AnalysisOnly => "analysis_only",
        IngestCapabilityLimitation::SurfaceOnly => "surface_only",
        IngestCapabilityLimitation::EnsembleMeanOnly => "ensemble_mean_only",
        IngestCapabilityLimitation::EnsembleControlMemberOnly => "ensemble_control_member_only",
        IngestCapabilityLimitation::SparsePressureLevels => "sparse_pressure_levels",
        IngestCapabilityLimitation::DerivedProductsDisabled => "derived_products_disabled",
        IngestCapabilityLimitation::ConusOnly => "conus_only",
        IngestCapabilityLimitation::PreOperationalFeed => "pre_operational_feed",
    }
}

fn validate_source_for_model(model: ModelId, source: Option<SourceId>) -> SchedulerResult<()> {
    if let Some(source) = source
        && !model_source_ingest_supported(model, source)
    {
        return Err(SchedulerError::InvalidPlan(format!(
            "source '{source}' is not supported by remote ingest for model '{model}'"
        )));
    }
    Ok(())
}

fn validate_profile_for_capability(
    model: ModelId,
    profile: &IngestProfile,
    limitations: &[IngestCapabilityLimitation],
) -> SchedulerResult<()> {
    profile.validate().map_err(SchedulerError::InvalidPlan)?;
    let capability = model_ingest_capability(model);
    let has_pressure = capability
        .products
        .iter()
        .any(|product| product.pressure_source);
    if profile.needs_prs() && !has_pressure {
        return Err(SchedulerError::InvalidPlan(format!(
            "model '{model}' is surface-only but the selected profile requires pressure data"
        )));
    }
    if limitations.contains(&IngestCapabilityLimitation::DerivedProductsDisabled)
        && (profile.derived || profile.heavy)
    {
        return Err(SchedulerError::InvalidPlan(format!(
            "model '{model}' forbids derived/heavy diagnostics for its ingest product"
        )));
    }
    Ok(())
}

pub fn canonical_run_id(cycle: &CycleSpec) -> String {
    format!("{}_{:02}z", cycle.date_yyyymmdd, cycle.hour_utc)
}

pub fn canonical_job_id(model: ModelId, cycle: &CycleSpec) -> String {
    format!(
        "{}-{}-{:02}z",
        model.as_str(),
        cycle.date_yyyymmdd,
        cycle.hour_utc
    )
}

pub(crate) fn revalidate_cycle(cycle: &CycleSpec) -> SchedulerResult<CycleSpec> {
    Ok(CycleSpec::new(cycle.date_yyyymmdd.clone(), cycle.hour_utc)?)
}

pub(crate) fn validate_model_cycle(model: ModelId, cycle: &CycleSpec) -> SchedulerResult<()> {
    if !model_summary(model)
        .cycle_hours_utc
        .contains(&cycle.hour_utc)
    {
        return Err(SchedulerError::UnsupportedCycle {
            model,
            cycle_hour: cycle.hour_utc,
        });
    }
    Ok(())
}

pub fn cycle_origin_unix(cycle: &CycleSpec) -> SchedulerResult<i64> {
    let cycle = revalidate_cycle(cycle)?;
    let year = cycle.date_yyyymmdd[0..4]
        .parse::<i64>()
        .map_err(|_| SchedulerError::InvalidPlan("cycle year is not numeric".to_string()))?;
    let month = cycle.date_yyyymmdd[4..6]
        .parse::<i64>()
        .map_err(|_| SchedulerError::InvalidPlan("cycle month is not numeric".to_string()))?;
    let day = cycle.date_yyyymmdd[6..8]
        .parse::<i64>()
        .map_err(|_| SchedulerError::InvalidPlan("cycle day is not numeric".to_string()))?;

    // Howard Hinnant's proleptic-Gregorian days-from-civil transform, with
    // 1970-01-01 as day zero. CycleSpec validation has already bounded the
    // components to a real Gregorian date.
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    days_since_epoch
        .checked_mul(86_400)
        .and_then(|seconds| seconds.checked_add(i64::from(cycle.hour_utc) * 3_600))
        .ok_or_else(|| SchedulerError::InvalidPlan("cycle timestamp overflow".to_string()))
}
