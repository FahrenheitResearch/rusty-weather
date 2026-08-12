//! Explicit, tile-first temporal analytics over one immutable run snapshot.
//!
//! Callers declare both the physical temporal meaning of the input and the
//! compatible reducer. The query layer never guesses semantics from a field
//! name or unit string.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{Duration, LocalResult, NaiveDate, TimeZone};
use chrono_tz::Tz;
use rw_store::format::RwsVariableMeta;
use rw_store::reader::HourReader;
use serde::{Deserialize, Serialize};

use crate::capability::{TemporalValueClass, variable_temporal_capabilities};
use crate::point::validate_variable_names;
use crate::snapshot::{ensure_compatible, ratio};
use crate::{
    MissingPolicy, QueryError, QueryResult, RunDescriptor, RunSnapshot, TimePoint, TimeRange,
};

const MAX_CATEGORIES_PER_CELL: usize = 256;

/// A physical half-open time window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TemporalWindow {
    Utc {
        start_unix: i64,
        end_unix: i64,
    },
    /// One civil date in an IANA timezone. `date` is exactly YYYY-MM-DD.
    LocalDay {
        date: String,
        timezone: String,
    },
}

/// The UTC interval actually used after resolving a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTemporalWindow {
    pub start_unix: i64,
    pub end_unix: i64,
    pub duration_seconds: u64,
    pub requested_local_date: Option<String>,
    pub timezone: Option<String>,
}

/// How expected valid times are established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum TimeExpectation {
    /// Only times declared by the immutable run manifest are expected. This
    /// cannot detect a timestep that was never inventoried by the producer.
    ManifestAxis,
    /// A caller-declared cadence. If `anchor_unix` is absent, the resolved
    /// window start is the phase anchor.
    FixedCadence {
        step_seconds: u64,
        anchor_unix: Option<i64>,
    },
}

impl Default for TimeExpectation {
    fn default() -> Self {
        Self::ManifestAxis
    }
}

/// Where an interval-valued sample sits relative to its valid timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntervalSupport {
    StartsAtValidTime { seconds: u64 },
    EndsAtValidTime { seconds: u64 },
    UntilNextExpectedTime,
    SincePreviousExpectedTime,
}

/// Physical temporal meaning declared by the caller or a trusted registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TemporalSemantics {
    InstantaneousScalar,
    IntervalAccumulation {
        support: IntervalSupport,
    },
    /// Each sample is the maximum observed over its declared interval. The
    /// reducer compares those interval maxima; it never treats them as
    /// instantaneous values or sums them.
    IntervalMaximum {
        support: IntervalSupport,
    },
    CumulativeFromOrigin {
        #[serde(default)]
        include_first_value: bool,
        #[serde(default)]
        reset_tolerance: f64,
    },
    IntervalRate {
        support: IntervalSupport,
        /// Numeric divisor that converts value * seconds to `integral_units`.
        /// Use 3600 for a quantity expressed per hour, for example.
        seconds_per_rate_unit: f64,
        integral_units: String,
    },
    /// Two variables ordered as eastward (u), then northward (v).
    VectorComponents,
    /// Angles in degrees. The result uses the same zero/direction convention.
    CircularDegrees,
    /// Finite, exactly integral i32 category codes.
    Categorical,
    /// Raw sampling is allowed elsewhere, but no reduction is guessed.
    Unknown,
}

/// Named reducer family. It must match `semantics` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalReducer {
    ScalarSummary,
    IntervalSummary,
    IntervalMaximumSummary,
    CumulativeSummary,
    RateSummary,
    VectorSummary,
    CircularMean,
    CategoricalSummary,
}

/// Optional vertical axis for a temporal-grid reduction. Omitting this field
/// retains the original surface-only request contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TemporalVerticalSelection {
    /// Exact pressure levels, in caller-defined output order.
    PressureLevels { levels_hpa: Vec<u16> },
}

/// Transport-independent request suitable for a temporal-grid HTTP endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalGridRequest {
    pub variables: Vec<String>,
    pub semantics: TemporalSemantics,
    pub reducer: TemporalReducer,
    pub window: TemporalWindow,
    #[serde(default)]
    pub expectation: TimeExpectation,
    #[serde(default)]
    pub missing_policy: MissingPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical: Option<TemporalVerticalSelection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalCompleteness {
    pub expectation: TimeExpectation,
    pub expected_samples: usize,
    pub available_samples: usize,
    pub missing_samples: usize,
    pub missing_valid_unix: Vec<i64>,
    pub expected_duration_seconds: u64,
    pub covered_duration_seconds: u64,
    pub duration_coverage: f64,
    pub largest_gap_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalGridMetadata {
    pub run: RunDescriptor,
    pub variables: Vec<String>,
    pub units: Vec<String>,
    pub semantics: TemporalSemantics,
    pub reducer: TemporalReducer,
    pub nx: usize,
    pub ny: usize,
    /// Present only for pressure-volume reductions. Output arrays are flat
    /// in this exact caller-requested level order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels_hpa: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<TemporalGridLayout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<[usize; 3]>,
    /// Available samples only, in exact chronological order. Every arg index
    /// in a result addresses this axis.
    pub axis: Vec<TimePoint>,
    pub window: ResolvedTemporalWindow,
    pub completeness: TemporalCompleteness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemporalGridLayout {
    #[serde(rename = "level_y_x")]
    LevelYX,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScalarSummaryGrid {
    pub metadata: TemporalGridMetadata,
    pub minimum: Vec<Option<f64>>,
    pub maximum: Vec<Option<f64>>,
    pub range: Vec<Option<f64>>,
    pub time_weighted_mean: Vec<Option<f64>>,
    pub argmin_time_index: Vec<Option<u32>>,
    pub argmax_time_index: Vec<Option<u32>>,
    pub finite_count: Vec<u32>,
    pub covered_duration_seconds: Vec<u64>,
    pub duration_coverage: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntervalSummaryGrid {
    pub metadata: TemporalGridMetadata,
    pub total: Vec<Option<f64>>,
    pub minimum_interval: Vec<Option<f64>>,
    pub maximum_interval: Vec<Option<f64>>,
    /// Maximum finite interval amount minus minimum finite interval amount.
    pub range_interval: Vec<Option<f64>>,
    pub argmin_time_index: Vec<Option<u32>>,
    pub argmax_time_index: Vec<Option<u32>>,
    pub finite_count: Vec<u32>,
    pub covered_duration_seconds: Vec<u64>,
    pub duration_coverage: Vec<f64>,
}

/// Extrema across a sequence whose individual samples are interval maxima.
/// Field names deliberately preserve that physical distinction in JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntervalMaximumSummaryGrid {
    pub metadata: TemporalGridMetadata,
    pub minimum_of_interval_maxima: Vec<Option<f64>>,
    pub maximum_of_interval_maxima: Vec<Option<f64>>,
    pub range_of_interval_maxima: Vec<Option<f64>>,
    pub argmin_interval_maximum_time_index: Vec<Option<u32>>,
    pub argmax_interval_maximum_time_index: Vec<Option<u32>>,
    pub finite_interval_maximum_count: Vec<u32>,
    pub covered_duration_seconds: Vec<u64>,
    pub duration_coverage: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CumulativeSummaryGrid {
    pub metadata: TemporalGridMetadata,
    pub total_increment: Vec<Option<f64>>,
    pub minimum_increment: Vec<Option<f64>>,
    pub maximum_increment: Vec<Option<f64>>,
    /// Maximum finite reset-aware increment minus minimum finite increment.
    pub range_increment: Vec<Option<f64>>,
    pub argmin_time_index: Vec<Option<u32>>,
    pub argmax_time_index: Vec<Option<u32>>,
    pub finite_increment_count: Vec<u32>,
    pub reset_count: Vec<u32>,
    pub covered_duration_seconds: Vec<u64>,
    pub duration_coverage: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateSummaryGrid {
    pub metadata: TemporalGridMetadata,
    pub integral_units: String,
    pub minimum_rate: Vec<Option<f64>>,
    pub maximum_rate: Vec<Option<f64>>,
    /// Maximum finite rate minus minimum finite rate.
    pub range_rate: Vec<Option<f64>>,
    pub duration_weighted_mean: Vec<Option<f64>>,
    pub integral: Vec<Option<f64>>,
    pub argmin_time_index: Vec<Option<u32>>,
    pub argmax_time_index: Vec<Option<u32>>,
    pub finite_count: Vec<u32>,
    pub covered_duration_seconds: Vec<u64>,
    pub duration_coverage: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorSummaryGrid {
    pub metadata: TemporalGridMetadata,
    pub minimum_speed: Vec<Option<f64>>,
    pub maximum_speed: Vec<Option<f64>>,
    /// Maximum finite vector speed minus minimum finite vector speed.
    pub range_speed: Vec<Option<f64>>,
    pub time_weighted_mean_speed: Vec<Option<f64>>,
    pub vector_mean_u: Vec<Option<f64>>,
    pub vector_mean_v: Vec<Option<f64>>,
    pub vector_mean_speed: Vec<Option<f64>>,
    /// Direction toward, clockwise from north, in [0, 360).
    pub vector_mean_direction_toward_degrees: Vec<Option<f64>>,
    pub argmin_time_index: Vec<Option<u32>>,
    pub argmax_time_index: Vec<Option<u32>>,
    pub finite_count: Vec<u32>,
    pub covered_duration_seconds: Vec<u64>,
    pub duration_coverage: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CircularMeanGrid {
    pub metadata: TemporalGridMetadata,
    pub mean_degrees: Vec<Option<f64>>,
    pub resultant_length: Vec<Option<f64>>,
    pub finite_count: Vec<u32>,
    pub covered_duration_seconds: Vec<u64>,
    pub duration_coverage: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryDuration {
    pub category: i32,
    pub duration_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoricalSummaryGrid {
    pub metadata: TemporalGridMetadata,
    pub mode: Vec<Option<i32>>,
    pub mode_duration_seconds: Vec<u64>,
    pub category_durations: Vec<Vec<CategoryDuration>>,
    pub transitions: Vec<u32>,
    pub finite_count: Vec<u32>,
    pub covered_duration_seconds: Vec<u64>,
    pub duration_coverage: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum TemporalGridResult {
    Scalar(ScalarSummaryGrid),
    Interval(IntervalSummaryGrid),
    IntervalMaximum(IntervalMaximumSummaryGrid),
    Cumulative(CumulativeSummaryGrid),
    Rate(RateSummaryGrid),
    Vector(VectorSummaryGrid),
    Circular(CircularMeanGrid),
    Categorical(CategoricalSummaryGrid),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalSemanticsCapability {
    pub semantics: TemporalSemantics,
    pub supported_reducers: Vec<TemporalReducer>,
    pub reducible: bool,
}

/// Allocation budgets for one temporal-grid execution. Callers serving both
/// synchronous JSON and asynchronous artifacts can apply different budgets
/// without weakening the point/profile limits stored on the run snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalReductionLimits {
    pub max_reduction_cells: usize,
    pub max_output_values: usize,
}

impl TemporalReductionLimits {
    fn from_snapshot(snapshot: &RunSnapshot) -> Self {
        Self {
            max_reduction_cells: snapshot.limits().max_temporal_reduction_cells,
            max_output_values: snapshot.limits().max_temporal_output_values,
        }
    }
}

/// Report the reducer contract for an explicitly declared semantics value.
pub fn temporal_semantics_capability(semantics: TemporalSemantics) -> TemporalSemanticsCapability {
    let reducer = match &semantics {
        TemporalSemantics::InstantaneousScalar => Some(TemporalReducer::ScalarSummary),
        TemporalSemantics::IntervalAccumulation { .. } => Some(TemporalReducer::IntervalSummary),
        TemporalSemantics::IntervalMaximum { .. } => Some(TemporalReducer::IntervalMaximumSummary),
        TemporalSemantics::CumulativeFromOrigin { .. } => Some(TemporalReducer::CumulativeSummary),
        TemporalSemantics::IntervalRate { .. } => Some(TemporalReducer::RateSummary),
        TemporalSemantics::VectorComponents => Some(TemporalReducer::VectorSummary),
        TemporalSemantics::CircularDegrees => Some(TemporalReducer::CircularMean),
        TemporalSemantics::Categorical => Some(TemporalReducer::CategoricalSummary),
        TemporalSemantics::Unknown => None,
    };
    TemporalSemanticsCapability {
        semantics,
        supported_reducers: reducer.into_iter().collect(),
        reducible: reducer.is_some(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialStatsSeriesRequest {
    pub variable: String,
    #[serde(default)]
    pub time: TimeRange,
    #[serde(default)]
    pub missing_policy: MissingPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialStatsSample {
    pub time: TimePoint,
    pub variable_available: bool,
    pub minimum: Option<f32>,
    pub maximum: Option<f32>,
    pub finite_count: u64,
    pub missing_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialStatsSeriesResult {
    pub run: RunDescriptor,
    pub variable: String,
    pub units: String,
    pub samples: Vec<SpatialStatsSample>,
    pub expected_samples: usize,
    pub available_samples: usize,
    pub coverage: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexWindow2DRequest {
    pub storage_slot: u16,
    pub variable: String,
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexWindow2DResult {
    pub run: RunDescriptor,
    pub time: TimePoint,
    pub variable: String,
    pub units: String,
    pub x0: usize,
    pub y0: usize,
    pub nx: usize,
    pub ny: usize,
    /// Row-major native-grid values. Non-finite stored cells are represented
    /// as `None` so JSON transports emit an explicit `null` instead of
    /// relying on a serializer-specific non-finite float encoding.
    pub values: Vec<Option<f32>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexWindow3DRequest {
    pub storage_slot: u16,
    pub variable: String,
    /// Explicit caller order, preserved in the returned level axis.
    pub levels_hpa: Vec<u16>,
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexWindow3DResult {
    pub run: RunDescriptor,
    pub time: TimePoint,
    pub variable: String,
    pub units: String,
    pub levels_hpa: Vec<u16>,
    pub x0: usize,
    pub y0: usize,
    pub nx: usize,
    pub ny: usize,
    /// Flat `[level][y][x]` values in the explicit requested level order.
    pub values: Vec<Option<f32>>,
}

/// Resolve a UTC interval or IANA local civil day without assuming that every
/// day is 24 hours long.
pub fn resolve_temporal_window(request: &TemporalWindow) -> QueryResult<ResolvedTemporalWindow> {
    let (start_unix, end_unix, requested_local_date, timezone) = match request {
        TemporalWindow::Utc {
            start_unix,
            end_unix,
        } => (*start_unix, *end_unix, None, None),
        TemporalWindow::LocalDay { date, timezone } => {
            if date.len() != 10 {
                return Err(QueryError::InvalidRequest(
                    "local-day date must be exactly YYYY-MM-DD".to_string(),
                ));
            }
            let date_value = NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|error| {
                QueryError::InvalidRequest(format!("invalid local-day date '{date}': {error}"))
            })?;
            let next_date = date_value.succ_opt().ok_or_else(|| {
                QueryError::InvalidRequest(format!("local-day date '{date}' has no next day"))
            })?;
            let zone = Tz::from_str(timezone).map_err(|_| {
                QueryError::InvalidRequest(format!("unknown IANA timezone '{timezone}'"))
            })?;
            let start = resolve_local_boundary(zone, date_value)?;
            let end = resolve_local_boundary(zone, next_date)?;
            (start, end, Some(date.clone()), Some(timezone.clone()))
        }
    };
    if start_unix >= end_unix {
        return Err(QueryError::InvalidTimeRange {
            start: Some(start_unix),
            end: Some(end_unix),
        });
    }
    let duration_seconds = u64::try_from(end_unix - start_unix).map_err(|_| {
        QueryError::InvalidRequest("resolved temporal duration is not representable".to_string())
    })?;
    Ok(ResolvedTemporalWindow {
        start_unix,
        end_unix,
        duration_seconds,
        requested_local_date,
        timezone,
    })
}

fn resolve_local_boundary(zone: Tz, date: NaiveDate) -> QueryResult<i64> {
    let midnight = date.and_hms_opt(0, 0, 0).ok_or_else(|| {
        QueryError::InvalidRequest(format!("cannot construct midnight for {date}"))
    })?;
    // A few IANA zones move clocks at midnight. Select the earliest instant
    // belonging to the date; if midnight is skipped, walk to its first valid
    // local second. A completely skipped civil date is rejected.
    for offset in 0..=86_400i64 {
        let local = midnight
            .checked_add_signed(Duration::seconds(offset))
            .ok_or_else(|| QueryError::InvalidRequest("local-day boundary overflow".to_string()))?;
        if local.date() != date {
            break;
        }
        match zone.from_local_datetime(&local) {
            LocalResult::Single(value) => return Ok(value.timestamp()),
            LocalResult::Ambiguous(first, second) => {
                return Ok(first.timestamp().min(second.timestamp()));
            }
            LocalResult::None => {}
        }
    }
    Err(QueryError::InvalidRequest(format!(
        "civil date {date} does not exist in timezone {zone}"
    )))
}

struct PreparedSample {
    time: TimePoint,
    axis_index: u32,
    reader: HourReader,
    path: PathBuf,
}

struct PreparedBaseline {
    time: TimePoint,
    reader: HourReader,
    path: PathBuf,
}

struct PreparedQuery {
    metadata: TemporalGridMetadata,
    samples: Vec<Option<PreparedSample>>,
    supports: Vec<Option<(i64, i64)>>,
    cumulative_baseline: Option<PreparedBaseline>,
    max_dynamic_output_values: usize,
}

impl PreparedQuery {
    fn duration_seconds(&self, index: usize) -> u64 {
        self.supports[index]
            .map(|(start, end)| u64::try_from(end - start).expect("ordered support"))
            .unwrap_or(0)
    }

    fn first_reader(&self) -> &PreparedSample {
        self.samples
            .iter()
            .flatten()
            .next()
            .expect("preparation requires an available sample")
    }
}

fn prepare_temporal_query<F: FnMut() -> bool>(
    snapshot: &RunSnapshot,
    request: &TemporalGridRequest,
    limits: TemporalReductionLimits,
    is_cancelled: &mut F,
) -> QueryResult<PreparedQuery> {
    validate_temporal_request(request, snapshot.limits().max_variables)?;
    let levels_hpa = match &request.vertical {
        None => Vec::new(),
        Some(TemporalVerticalSelection::PressureLevels { levels_hpa }) => levels_hpa.clone(),
    };
    let expected_kind = if levels_hpa.is_empty() {
        "surface2d"
    } else {
        "pressure3d"
    };
    let window = resolve_temporal_window(&request.window)?;
    let ExpectedSchedule {
        expected_times,
        time_by_valid,
        preceding_expected_unix,
        following_expected_unix,
    } = expected_schedule(snapshot, &window, &request.expectation, &request.semantics)?;

    let mut metadata_by_variable: Vec<Option<RwsVariableMeta>> =
        (0..request.variables.len()).map(|_| None).collect();
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(expected_times.len())
        .map_err(|error| QueryError::Allocation {
            what: "temporal samples",
            detail: error.to_string(),
        })?;
    let mut axis = Vec::new();
    axis.try_reserve_exact(expected_times.len())
        .map_err(|error| QueryError::Allocation {
            what: "temporal result axis",
            detail: error.to_string(),
        })?;

    for &valid_unix in &expected_times {
        check_cancelled(is_cancelled)?;
        let Some(time) = time_by_valid.get(&valid_unix).cloned() else {
            if request.missing_policy == MissingPolicy::Strict {
                return Err(QueryError::MissingExpectedTime { valid_unix });
            }
            samples.push(None);
            continue;
        };
        let (reader, path) = snapshot.open_reader_uncached(&time)?;
        let mut complete = true;
        for (variable_index, variable) in request.variables.iter().enumerate() {
            let Some(meta) = reader.variable(variable) else {
                complete = false;
                if request.missing_policy == MissingPolicy::Strict {
                    return Err(QueryError::MissingVariable {
                        variable: variable.clone(),
                        slot: time.storage_slot,
                    });
                }
                continue;
            };
            if meta.kind != expected_kind {
                return Err(QueryError::WrongVariableKind {
                    variable: variable.clone(),
                    expected: expected_kind,
                    actual: meta.kind.clone(),
                });
            }
            for &level_hpa in &levels_hpa {
                if !meta.levels_hpa.contains(&level_hpa) {
                    return Err(QueryError::InvalidRequest(format!(
                        "pressure level {level_hpa} hPa is not available for variable '{variable}'"
                    )));
                }
            }
            if let Some(expected) = &metadata_by_variable[variable_index] {
                ensure_compatible(expected, meta)?;
            } else {
                metadata_by_variable[variable_index] = Some(meta.clone());
            }
        }
        if !complete {
            snapshot.ensure_source(&reader, &path, time.storage_slot)?;
            samples.push(None);
            continue;
        }
        let axis_index = u32::try_from(axis.len()).map_err(|_| QueryError::LimitExceeded {
            what: "temporal argument index",
            requested: axis.len() + 1,
            limit: u32::MAX as usize,
        })?;
        axis.push(time.clone());
        samples.push(Some(PreparedSample {
            time,
            axis_index,
            reader,
            path,
        }));
    }

    let metas = request
        .variables
        .iter()
        .zip(metadata_by_variable)
        .map(|(name, meta)| meta.ok_or_else(|| QueryError::UnknownVariable(name.clone())))
        .collect::<QueryResult<Vec<_>>>()?;
    if matches!(request.semantics, TemporalSemantics::VectorComponents)
        && metas[0].units != metas[1].units
    {
        return Err(QueryError::InvalidRequest(format!(
            "vector component units differ: '{}' versus '{}'",
            metas[0].units, metas[1].units
        )));
    }
    if matches!(request.semantics, TemporalSemantics::VectorComponents)
        && metas[0].levels_hpa != metas[1].levels_hpa
    {
        return Err(QueryError::InvalidRequest(format!(
            "vector component pressure axes differ for '{}' and '{}'",
            request.variables[0], request.variables[1]
        )));
    }
    validate_metadata_semantics(&metas, &request.variables, &request.semantics)?;

    let cumulative_baseline = if matches!(
        request.semantics,
        TemporalSemantics::CumulativeFromOrigin { .. }
    ) {
        match preceding_expected_unix {
            Some(valid_unix) => match time_by_valid.get(&valid_unix).cloned() {
                Some(time) => {
                    let (reader, path) = snapshot.open_reader_uncached(&time)?;
                    let mut complete = true;
                    for (variable, expected) in request.variables.iter().zip(&metas) {
                        let Some(meta) = reader.variable(variable) else {
                            complete = false;
                            if request.missing_policy == MissingPolicy::Strict {
                                return Err(QueryError::MissingVariable {
                                    variable: variable.clone(),
                                    slot: time.storage_slot,
                                });
                            }
                            continue;
                        };
                        if meta.kind != expected_kind {
                            return Err(QueryError::WrongVariableKind {
                                variable: variable.clone(),
                                expected: expected_kind,
                                actual: meta.kind.clone(),
                            });
                        }
                        ensure_compatible(expected, meta)?;
                    }
                    if complete {
                        Some(PreparedBaseline { time, reader, path })
                    } else {
                        snapshot.ensure_source(&reader, &path, time.storage_slot)?;
                        None
                    }
                }
                None if request.missing_policy == MissingPolicy::Strict => {
                    return Err(QueryError::MissingExpectedTime { valid_unix });
                }
                None => None,
            },
            None => None,
        }
    } else {
        None
    };

    let availability: Vec<_> = samples.iter().map(Option::is_some).collect();
    let supports = build_supports(
        &expected_times,
        &availability,
        preceding_expected_unix,
        following_expected_unix,
        cumulative_baseline.is_some(),
        &window,
        &request.semantics,
    )?;
    let completeness = build_completeness(
        request.expectation.clone(),
        &expected_times,
        &availability,
        &supports,
        &window,
    );
    let horizontal_cells =
        snapshot
            .grid()
            .nx
            .checked_mul(snapshot.grid().ny)
            .ok_or(QueryError::LimitExceeded {
                what: "temporal reduction cells",
                requested: usize::MAX,
                limit: limits.max_reduction_cells,
            })?;
    let vertical_cells = levels_hpa.len().max(1);
    let cells = horizontal_cells
        .checked_mul(vertical_cells)
        .ok_or(QueryError::LimitExceeded {
            what: "temporal reduction cells",
            requested: usize::MAX,
            limit: limits.max_reduction_cells,
        })?;
    if cells > limits.max_reduction_cells {
        return Err(QueryError::LimitExceeded {
            what: "temporal reduction cells",
            requested: cells,
            limit: limits.max_reduction_cells,
        });
    }
    let output_limit = limits.max_output_values;
    let fixed_output_values = cells
        .checked_mul(temporal_result_values_per_cell(request.reducer))
        .ok_or(QueryError::LimitExceeded {
            what: "temporal output values",
            requested: usize::MAX,
            limit: output_limit,
        })?;
    if fixed_output_values > output_limit {
        return Err(QueryError::LimitExceeded {
            what: "temporal output values",
            requested: fixed_output_values,
            limit: output_limit,
        });
    }
    let max_dynamic_output_values = output_limit - fixed_output_values;

    Ok(PreparedQuery {
        metadata: TemporalGridMetadata {
            run: snapshot.descriptor().clone(),
            variables: request.variables.clone(),
            units: metas.into_iter().map(|meta| meta.units).collect(),
            semantics: request.semantics.clone(),
            reducer: request.reducer,
            nx: snapshot.grid().nx,
            ny: snapshot.grid().ny,
            levels_hpa: levels_hpa.clone(),
            layout: (!levels_hpa.is_empty()).then_some(TemporalGridLayout::LevelYX),
            shape: (!levels_hpa.is_empty()).then_some([
                levels_hpa.len(),
                snapshot.grid().ny,
                snapshot.grid().nx,
            ]),
            axis,
            window,
            completeness,
        },
        samples,
        supports,
        cumulative_baseline,
        max_dynamic_output_values,
    })
}

fn validate_temporal_request(
    request: &TemporalGridRequest,
    variable_limit: usize,
) -> QueryResult<()> {
    validate_variable_names(&request.variables, variable_limit)?;
    if let Some(TemporalVerticalSelection::PressureLevels { levels_hpa }) = &request.vertical {
        if levels_hpa.is_empty() {
            return Err(QueryError::InvalidRequest(
                "pressure_levels requires a nonempty levels_hpa list".to_string(),
            ));
        }
        let unique = levels_hpa.iter().copied().collect::<BTreeSet<_>>();
        if unique.len() != levels_hpa.len() {
            return Err(QueryError::InvalidRequest(
                "pressure_levels levels_hpa must be unique".to_string(),
            ));
        }
        if !matches!(
            request.semantics,
            TemporalSemantics::InstantaneousScalar | TemporalSemantics::VectorComponents
        ) {
            return Err(QueryError::InvalidRequest(
                "pressure-level temporal reduction supports only instantaneous_scalar and vector_components semantics"
                    .to_string(),
            ));
        }
    }
    let compatible = matches!(
        (&request.semantics, request.reducer),
        (
            TemporalSemantics::InstantaneousScalar,
            TemporalReducer::ScalarSummary
        ) | (
            TemporalSemantics::IntervalAccumulation { .. },
            TemporalReducer::IntervalSummary
        ) | (
            TemporalSemantics::IntervalMaximum { .. },
            TemporalReducer::IntervalMaximumSummary
        ) | (
            TemporalSemantics::CumulativeFromOrigin { .. },
            TemporalReducer::CumulativeSummary
        ) | (
            TemporalSemantics::IntervalRate { .. },
            TemporalReducer::RateSummary
        ) | (
            TemporalSemantics::VectorComponents,
            TemporalReducer::VectorSummary
        ) | (
            TemporalSemantics::CircularDegrees,
            TemporalReducer::CircularMean
        ) | (
            TemporalSemantics::Categorical,
            TemporalReducer::CategoricalSummary
        )
    );
    if !compatible {
        return Err(QueryError::InvalidRequest(format!(
            "reducer {:?} is incompatible with semantics {:?}",
            request.reducer, request.semantics
        )));
    }
    let expected_variables = if matches!(request.semantics, TemporalSemantics::VectorComponents) {
        2
    } else {
        1
    };
    if request.variables.len() != expected_variables {
        return Err(QueryError::InvalidRequest(format!(
            "semantics {:?} requires {expected_variables} variable(s), got {}",
            request.semantics,
            request.variables.len()
        )));
    }
    match &request.semantics {
        TemporalSemantics::IntervalAccumulation { support }
        | TemporalSemantics::IntervalMaximum { support }
        | TemporalSemantics::IntervalRate { support, .. } => validate_interval_support(*support)?,
        _ => {}
    }
    if let TemporalSemantics::CumulativeFromOrigin {
        reset_tolerance, ..
    } = request.semantics
    {
        if !reset_tolerance.is_finite() || reset_tolerance < 0.0 {
            return Err(QueryError::InvalidRequest(
                "cumulative reset_tolerance must be finite and non-negative".to_string(),
            ));
        }
    }
    if let TemporalSemantics::IntervalRate {
        seconds_per_rate_unit,
        integral_units,
        ..
    } = &request.semantics
    {
        if !seconds_per_rate_unit.is_finite() || *seconds_per_rate_unit <= 0.0 {
            return Err(QueryError::InvalidRequest(
                "seconds_per_rate_unit must be finite and positive".to_string(),
            ));
        }
        if integral_units.trim().is_empty() {
            return Err(QueryError::InvalidRequest(
                "integral_units must not be empty".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_metadata_semantics(
    metas: &[RwsVariableMeta],
    variables: &[String],
    semantics: &TemporalSemantics,
) -> QueryResult<()> {
    let capabilities = variable_temporal_capabilities(metas);
    for variable in variables {
        let capability = capabilities
            .get(variable)
            .expect("every request metadata record has a capability");
        let compatible = match capability.value_class {
            TemporalValueClass::InstantaneousScalar => {
                matches!(semantics, TemporalSemantics::InstantaneousScalar)
            }
            TemporalValueClass::IntervalAccumulation | TemporalValueClass::Rate => {
                capability.recommended_semantics.as_ref() == Some(semantics)
            }
            TemporalValueClass::IntervalExtremum => {
                capability.recommended_semantics.as_ref() == Some(semantics)
            }
            TemporalValueClass::CumulativeAccumulation => {
                matches!(semantics, TemporalSemantics::CumulativeFromOrigin { .. })
            }
            TemporalValueClass::VectorComponent => {
                matches!(semantics, TemporalSemantics::VectorComponents)
                    && capability.required_variables.as_slice() == variables
            }
            TemporalValueClass::CircularDirection => {
                matches!(semantics, TemporalSemantics::CircularDegrees)
            }
            TemporalValueClass::Categorical => {
                matches!(semantics, TemporalSemantics::Categorical)
            }
            // Unknown fields are deliberately manual: a caller with trusted
            // external metadata may still declare explicit semantics.
            TemporalValueClass::Unknown => true,
        };
        if !compatible {
            return Err(QueryError::InvalidRequest(format!(
                "declared temporal semantics {semantics:?} conflict with the trusted {:?} capability for variable '{variable}'",
                capability.value_class
            )));
        }
    }
    Ok(())
}

fn temporal_result_values_per_cell(reducer: TemporalReducer) -> usize {
    match reducer {
        TemporalReducer::ScalarSummary => 9,
        TemporalReducer::IntervalSummary
        | TemporalReducer::CumulativeSummary
        | TemporalReducer::RateSummary => 10,
        TemporalReducer::IntervalMaximumSummary => 8,
        TemporalReducer::VectorSummary => 13,
        TemporalReducer::CircularMean => 5,
        // Includes the outer category_durations vector; individual category
        // entries consume the remaining dynamic output budget.
        TemporalReducer::CategoricalSummary => 7,
    }
}
fn validate_interval_support(support: IntervalSupport) -> QueryResult<()> {
    match support {
        IntervalSupport::StartsAtValidTime { seconds }
        | IntervalSupport::EndsAtValidTime { seconds }
            if seconds == 0 =>
        {
            Err(QueryError::InvalidRequest(
                "fixed interval support must be positive".to_string(),
            ))
        }
        _ => Ok(()),
    }
}

struct ExpectedSchedule {
    expected_times: Vec<i64>,
    time_by_valid: BTreeMap<i64, TimePoint>,
    preceding_expected_unix: Option<i64>,
    following_expected_unix: Option<i64>,
}

fn expected_schedule(
    snapshot: &RunSnapshot,
    window: &ResolvedTemporalWindow,
    expectation: &TimeExpectation,
    semantics: &TemporalSemantics,
) -> QueryResult<ExpectedSchedule> {
    let mut time_by_valid = BTreeMap::new();
    for time in snapshot.time_axis() {
        if time_by_valid
            .insert(time.valid_unix, time.clone())
            .is_some()
        {
            return Err(QueryError::InvalidRequest(format!(
                "run snapshot contains duplicate valid time {}",
                time.valid_unix
            )));
        }
    }

    let end_stamped = uses_end_stamped_samples(semantics);
    let expected_times = match expectation {
        TimeExpectation::ManifestAxis => time_by_valid
            .keys()
            .copied()
            .filter(|valid_unix| {
                if end_stamped {
                    *valid_unix > window.start_unix && *valid_unix <= window.end_unix
                } else {
                    *valid_unix >= window.start_unix && *valid_unix < window.end_unix
                }
            })
            .collect::<Vec<_>>(),
        TimeExpectation::FixedCadence {
            step_seconds,
            anchor_unix,
        } => {
            if *step_seconds == 0 {
                return Err(QueryError::InvalidRequest(
                    "fixed cadence must be positive".to_string(),
                ));
            }
            let step = i128::from(*step_seconds);
            let start = i128::from(window.start_unix);
            let end = i128::from(window.end_unix);
            let anchor = i128::from(anchor_unix.unwrap_or(window.start_unix));
            let phase = (start - anchor).rem_euclid(step);
            let mut current = if phase == 0 {
                start
            } else {
                start + (step - phase)
            };
            if end_stamped && current == start {
                current = current.checked_add(step).ok_or_else(|| {
                    QueryError::InvalidRequest("fixed-cadence axis overflow".to_string())
                })?;
            }
            let mut expected = Vec::new();
            while if end_stamped {
                current <= end
            } else {
                current < end
            } {
                if expected.len() >= snapshot.limits().max_selected_time_points {
                    return Err(QueryError::LimitExceeded {
                        what: "expected time points",
                        requested: expected.len() + 1,
                        limit: snapshot.limits().max_selected_time_points,
                    });
                }
                expected.push(i64::try_from(current).map_err(|_| {
                    QueryError::InvalidRequest(
                        "fixed-cadence valid time does not fit i64".to_string(),
                    )
                })?);
                current = current.checked_add(step).ok_or_else(|| {
                    QueryError::InvalidRequest("fixed-cadence axis overflow".to_string())
                })?;
            }
            expected
        }
    };
    if expected_times.is_empty() {
        return Err(QueryError::EmptyTimeSelection);
    }
    if expected_times.len() > snapshot.limits().max_selected_time_points {
        return Err(QueryError::LimitExceeded {
            what: "expected time points",
            requested: expected_times.len(),
            limit: snapshot.limits().max_selected_time_points,
        });
    }

    let preceding_expected_unix = match expectation {
        TimeExpectation::ManifestAxis => expected_times.first().and_then(|first| {
            time_by_valid
                .range(..*first)
                .next_back()
                .map(|(&valid_unix, _)| valid_unix)
        }),
        TimeExpectation::FixedCadence { step_seconds, .. } => expected_times
            .first()
            .and_then(|first| first.checked_sub(i64::try_from(*step_seconds).ok()?)),
    };
    let following_expected_unix = match expectation {
        TimeExpectation::ManifestAxis => expected_times.last().and_then(|last| {
            time_by_valid
                .range((std::ops::Bound::Excluded(*last), std::ops::Bound::Unbounded))
                .next()
                .map(|(&valid_unix, _)| valid_unix)
        }),
        TimeExpectation::FixedCadence { step_seconds, .. } => expected_times
            .last()
            .and_then(|last| last.checked_add(i64::try_from(*step_seconds).ok()?)),
    };
    Ok(ExpectedSchedule {
        expected_times,
        time_by_valid,
        preceding_expected_unix,
        following_expected_unix,
    })
}

fn uses_end_stamped_samples(semantics: &TemporalSemantics) -> bool {
    matches!(
        semantics,
        TemporalSemantics::CumulativeFromOrigin { .. }
            | TemporalSemantics::IntervalAccumulation {
                support: IntervalSupport::EndsAtValidTime { .. }
                    | IntervalSupport::SincePreviousExpectedTime,
            }
            | TemporalSemantics::IntervalMaximum {
                support: IntervalSupport::EndsAtValidTime { .. }
                    | IntervalSupport::SincePreviousExpectedTime,
            }
            | TemporalSemantics::IntervalRate {
                support: IntervalSupport::EndsAtValidTime { .. }
                    | IntervalSupport::SincePreviousExpectedTime,
                ..
            }
    )
}

fn build_supports(
    expected_times: &[i64],
    availability: &[bool],
    preceding_expected_unix: Option<i64>,
    following_expected_unix: Option<i64>,
    cumulative_baseline_available: bool,
    window: &ResolvedTemporalWindow,
    semantics: &TemporalSemantics,
) -> QueryResult<Vec<Option<(i64, i64)>>> {
    let supports = expected_times
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if !availability[index] {
                return Ok(None);
            }
            let support = match semantics {
                TemporalSemantics::InstantaneousScalar
                | TemporalSemantics::VectorComponents
                | TemporalSemantics::CircularDegrees
                | TemporalSemantics::Categorical => {
                    let start = expected_times[index].max(window.start_unix);
                    let end = expected_times
                        .get(index + 1)
                        .copied()
                        .unwrap_or(window.end_unix)
                        .min(window.end_unix);
                    ordered_interval(start, end)
                }
                TemporalSemantics::IntervalAccumulation { support } => {
                    let (start, end) = interval_bounds(
                        index,
                        expected_times,
                        preceding_expected_unix,
                        following_expected_unix,
                        window,
                        *support,
                    )?;
                    // An amount covering time outside the query cannot be
                    // apportioned honestly, so only wholly contained intervals
                    // contribute to an accumulation summary.
                    if start >= window.start_unix && end <= window.end_unix {
                        ordered_interval(start, end)
                    } else {
                        None
                    }
                }
                TemporalSemantics::IntervalMaximum { support } => {
                    let (start, end) = interval_bounds(
                        index,
                        expected_times,
                        preceding_expected_unix,
                        following_expected_unix,
                        window,
                        *support,
                    )?;
                    // A fixed-window maximum that crosses the requested
                    // boundary describes values outside the request. Do not
                    // silently relabel or prorate that sample.
                    if start >= window.start_unix && end <= window.end_unix {
                        ordered_interval(start, end)
                    } else {
                        None
                    }
                }
                TemporalSemantics::IntervalRate { support, .. } => {
                    let (start, end) = interval_bounds(
                        index,
                        expected_times,
                        preceding_expected_unix,
                        following_expected_unix,
                        window,
                        *support,
                    )?;
                    ordered_interval(start.max(window.start_unix), end.min(window.end_unix))
                }
                TemporalSemantics::CumulativeFromOrigin { .. } => {
                    let previous_available = if index == 0 {
                        cumulative_baseline_available
                    } else {
                        availability[index - 1]
                    };
                    let previous_time = if index == 0 {
                        preceding_expected_unix
                    } else {
                        Some(expected_times[index - 1])
                    };
                    if !previous_available {
                        None
                    } else if let Some(start) = previous_time {
                        let end = expected_times[index];
                        if start >= window.start_unix && end <= window.end_unix {
                            ordered_interval(start, end)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                TemporalSemantics::Unknown => None,
            };
            Ok(support)
        })
        .collect::<QueryResult<Vec<_>>>()?;
    if matches!(
        semantics,
        TemporalSemantics::IntervalAccumulation { .. } | TemporalSemantics::IntervalRate { .. }
    ) {
        let mut previous_end = None;
        for (start, end) in supports.iter().flatten().copied() {
            if previous_end.is_some_and(|previous| start < previous) {
                return Err(QueryError::InvalidRequest(
                    "interval supports overlap; totals and duration weighting would double-count time"
                        .to_string(),
                ));
            }
            previous_end = Some(end);
        }
    }
    Ok(supports)
}

fn interval_bounds(
    index: usize,
    expected_times: &[i64],
    preceding_expected_unix: Option<i64>,
    following_expected_unix: Option<i64>,
    window: &ResolvedTemporalWindow,
    support: IntervalSupport,
) -> QueryResult<(i64, i64)> {
    let time = expected_times[index];
    match support {
        IntervalSupport::StartsAtValidTime { seconds } => {
            let seconds = i64::try_from(seconds).map_err(|_| {
                QueryError::InvalidRequest("interval seconds exceed i64".to_string())
            })?;
            Ok((
                time,
                time.checked_add(seconds).ok_or_else(|| {
                    QueryError::InvalidRequest("interval end overflow".to_string())
                })?,
            ))
        }
        IntervalSupport::EndsAtValidTime { seconds } => {
            let seconds = i64::try_from(seconds).map_err(|_| {
                QueryError::InvalidRequest("interval seconds exceed i64".to_string())
            })?;
            Ok((
                time.checked_sub(seconds).ok_or_else(|| {
                    QueryError::InvalidRequest("interval start overflow".to_string())
                })?,
                time,
            ))
        }
        IntervalSupport::UntilNextExpectedTime => Ok((
            time,
            expected_times
                .get(index + 1)
                .copied()
                .or(following_expected_unix)
                .unwrap_or(window.end_unix),
        )),
        IntervalSupport::SincePreviousExpectedTime => Ok((
            index
                .checked_sub(1)
                .and_then(|previous| expected_times.get(previous).copied())
                .or(preceding_expected_unix)
                .unwrap_or(window.start_unix),
            time,
        )),
    }
}

fn ordered_interval(start: i64, end: i64) -> Option<(i64, i64)> {
    (start < end).then_some((start, end))
}

fn build_completeness(
    expectation: TimeExpectation,
    expected_times: &[i64],
    availability: &[bool],
    supports: &[Option<(i64, i64)>],
    window: &ResolvedTemporalWindow,
) -> TemporalCompleteness {
    let missing_valid_unix = expected_times
        .iter()
        .zip(availability)
        .filter_map(|(&time, &available)| (!available).then_some(time))
        .collect::<Vec<_>>();
    let available_samples = availability.iter().filter(|&&available| available).count();
    let (covered_duration_seconds, largest_gap_seconds) =
        duration_union_and_largest_gap(supports, window.start_unix, window.end_unix);
    TemporalCompleteness {
        expectation,
        expected_samples: expected_times.len(),
        available_samples,
        missing_samples: expected_times.len() - available_samples,
        missing_valid_unix,
        expected_duration_seconds: window.duration_seconds,
        covered_duration_seconds,
        duration_coverage: if window.duration_seconds == 0 {
            0.0
        } else {
            covered_duration_seconds as f64 / window.duration_seconds as f64
        },
        largest_gap_seconds,
    }
}

fn duration_union_and_largest_gap(
    supports: &[Option<(i64, i64)>],
    window_start: i64,
    window_end: i64,
) -> (u64, u64) {
    let mut intervals = supports.iter().flatten().copied().collect::<Vec<_>>();
    intervals.sort_unstable();
    let mut merged = Vec::<(i64, i64)>::new();
    for (start, end) in intervals {
        let start = start.max(window_start);
        let end = end.min(window_end);
        if start >= end {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    let covered = merged
        .iter()
        .map(|(start, end)| u64::try_from(end - start).expect("merged interval is ordered"))
        .sum();
    let mut cursor = window_start;
    let mut largest_gap = 0u64;
    for (start, end) in merged {
        if start > cursor {
            largest_gap = largest_gap.max((start - cursor) as u64);
        }
        cursor = cursor.max(end);
    }
    if cursor < window_end {
        largest_gap = largest_gap.max((window_end - cursor) as u64);
    }
    (covered, largest_gap)
}

/// Run a temporal grid query without cancellation.
pub fn reduce_temporal_grid(
    snapshot: &RunSnapshot,
    request: &TemporalGridRequest,
) -> QueryResult<TemporalGridResult> {
    reduce_temporal_grid_with_cancel(snapshot, request, || false)
}

/// Run a temporal grid query while checking `is_cancelled` between tiles and
/// timesteps. This seam is synchronous and runtime-agnostic; an HTTP job can
/// capture an `AtomicBool` without introducing tokio into `rw-query`.
pub fn reduce_temporal_grid_with_cancel<F>(
    snapshot: &RunSnapshot,
    request: &TemporalGridRequest,
    is_cancelled: F,
) -> QueryResult<TemporalGridResult>
where
    F: FnMut() -> bool,
{
    reduce_temporal_grid_with_cancel_and_limits(
        snapshot,
        request,
        TemporalReductionLimits::from_snapshot(snapshot),
        is_cancelled,
    )
}

/// Run a temporal grid query under request-specific allocation limits. This
/// lets an HTTP service keep synchronous JSON small while allowing bounded
/// full-domain asynchronous artifacts from the same immutable snapshot.
pub fn reduce_temporal_grid_with_cancel_and_limits<F>(
    snapshot: &RunSnapshot,
    request: &TemporalGridRequest,
    limits: TemporalReductionLimits,
    mut is_cancelled: F,
) -> QueryResult<TemporalGridResult>
where
    F: FnMut() -> bool,
{
    if limits.max_reduction_cells == 0 || limits.max_output_values == 0 {
        return Err(QueryError::InvalidRequest(
            "temporal reduction limits must be positive".to_string(),
        ));
    }
    check_cancelled(&mut is_cancelled)?;
    let prepared = prepare_temporal_query(snapshot, request, limits, &mut is_cancelled)?;
    check_cancelled(&mut is_cancelled)?;
    let result =
        match &request.semantics {
            TemporalSemantics::InstantaneousScalar => TemporalGridResult::Scalar(
                reduce_scalar_grid(&prepared, request.missing_policy, &mut is_cancelled)?,
            ),
            TemporalSemantics::IntervalAccumulation { .. } => TemporalGridResult::Interval(
                reduce_interval_grid(&prepared, request.missing_policy, &mut is_cancelled)?,
            ),
            TemporalSemantics::IntervalMaximum { .. } => TemporalGridResult::IntervalMaximum(
                reduce_interval_maximum_grid(&prepared, request.missing_policy, &mut is_cancelled)?,
            ),
            TemporalSemantics::CumulativeFromOrigin {
                include_first_value,
                reset_tolerance,
            } => TemporalGridResult::Cumulative(reduce_cumulative_grid(
                &prepared,
                request.missing_policy,
                *include_first_value,
                *reset_tolerance,
                &mut is_cancelled,
            )?),
            TemporalSemantics::IntervalRate {
                seconds_per_rate_unit,
                integral_units,
                ..
            } => TemporalGridResult::Rate(reduce_rate_grid(
                &prepared,
                request.missing_policy,
                *seconds_per_rate_unit,
                integral_units,
                &mut is_cancelled,
            )?),
            TemporalSemantics::VectorComponents => TemporalGridResult::Vector(reduce_vector_grid(
                &prepared,
                request.missing_policy,
                &mut is_cancelled,
            )?),
            TemporalSemantics::CircularDegrees => TemporalGridResult::Circular(
                reduce_circular_grid(&prepared, request.missing_policy, &mut is_cancelled)?,
            ),
            TemporalSemantics::Categorical => TemporalGridResult::Categorical(
                reduce_categorical_grid(&prepared, request.missing_policy, &mut is_cancelled)?,
            ),
            TemporalSemantics::Unknown => {
                return Err(QueryError::InvalidRequest(
                    "unknown temporal semantics permit raw sampling only".to_string(),
                ));
            }
        };
    check_cancelled(&mut is_cancelled)?;
    if let Some(baseline) = &prepared.cumulative_baseline {
        snapshot.ensure_source(&baseline.reader, &baseline.path, baseline.time.storage_slot)?;
    }
    for sample in prepared.samples.iter().flatten() {
        snapshot.ensure_source(&sample.reader, &sample.path, sample.time.storage_slot)?;
    }
    snapshot.ensure_manifest_current()?;
    Ok(result)
}

fn check_cancelled<F: FnMut() -> bool>(is_cancelled: &mut F) -> QueryResult<()> {
    if is_cancelled() {
        Err(QueryError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct NumericAccumulator {
    minimum: Option<f64>,
    maximum: Option<f64>,
    total: f64,
    weighted_total: f64,
    weight_seconds: u64,
    count: u32,
    argmin: Option<u32>,
    argmax: Option<u32>,
}

impl NumericAccumulator {
    fn update(&mut self, value: f64, time_index: u32, weight_seconds: u64) {
        if self.minimum.is_none_or(|minimum| value < minimum) {
            self.minimum = Some(value);
            self.argmin = Some(time_index);
        }
        if self.maximum.is_none_or(|maximum| value > maximum) {
            self.maximum = Some(value);
            self.argmax = Some(time_index);
        }
        self.total += value;
        self.weighted_total += value * weight_seconds as f64;
        self.weight_seconds += weight_seconds;
        self.count += 1;
    }
}

fn reduce_scalar_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    is_cancelled: &mut F,
) -> QueryResult<ScalarSummaryGrid> {
    if !prepared.metadata.levels_hpa.is_empty() {
        return reduce_pressure_scalar_grid(prepared, missing_policy, is_cancelled);
    }
    let cells = prepared.metadata.nx * prepared.metadata.ny;
    let mut minimum = output(cells, None, "scalar minimum")?;
    let mut maximum = output(cells, None, "scalar maximum")?;
    let mut range = output(cells, None, "scalar range")?;
    let mut time_weighted_mean = output(cells, None, "scalar weighted mean")?;
    let mut argmin_time_index = output(cells, None, "scalar argmin")?;
    let mut argmax_time_index = output(cells, None, "scalar argmax")?;
    let mut finite_count = output(cells, 0u32, "scalar count")?;
    let mut covered_duration_seconds = output(cells, 0u64, "scalar duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "scalar coverage")?;
    let variable = &prepared.metadata.variables[0];
    let first = prepared.first_reader();

    for geometry in first.reader.tiles_2d(variable)? {
        check_cancelled(is_cancelled)?;
        let mut accumulators = output(
            geometry.cell_count(),
            NumericAccumulator::default(),
            "scalar tile accumulators",
        )?;
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let Some(sample) = sample else { continue };
            let tile =
                sample
                    .reader
                    .read_tile_2d(variable, geometry.tile_y(), geometry.tile_x())?;
            ensure_tile_geometry(&tile, geometry, variable)?;
            let weight = prepared.duration_seconds(sample_index);
            for (local, accumulator) in accumulators.iter_mut().enumerate() {
                let value = tile_value(&tile, geometry, local);
                if value.is_finite() {
                    accumulator.update(f64::from(value), sample.axis_index, weight);
                } else if missing_policy == MissingPolicy::Strict {
                    return Err(missing_value_error(
                        variable,
                        sample.time.storage_slot,
                        geometry,
                        local,
                    ));
                }
            }
        }
        for (local, accumulator) in accumulators.into_iter().enumerate() {
            let global = global_index(prepared.metadata.nx, geometry, local);
            minimum[global] = accumulator.minimum;
            maximum[global] = accumulator.maximum;
            range[global] = finite_range(accumulator.minimum, accumulator.maximum);
            time_weighted_mean[global] = (accumulator.weight_seconds > 0)
                .then(|| accumulator.weighted_total / accumulator.weight_seconds as f64);
            argmin_time_index[global] = accumulator.argmin;
            argmax_time_index[global] = accumulator.argmax;
            finite_count[global] = accumulator.count;
            covered_duration_seconds[global] = accumulator.weight_seconds;
            duration_coverage[global] = duration_ratio(
                accumulator.weight_seconds,
                prepared.metadata.window.duration_seconds,
            );
        }
    }
    Ok(ScalarSummaryGrid {
        metadata: prepared.metadata.clone(),
        minimum,
        maximum,
        range,
        time_weighted_mean,
        argmin_time_index,
        argmax_time_index,
        finite_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

fn reduce_pressure_scalar_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    is_cancelled: &mut F,
) -> QueryResult<ScalarSummaryGrid> {
    let horizontal_cells = prepared.metadata.nx * prepared.metadata.ny;
    let cells = horizontal_cells * prepared.metadata.levels_hpa.len();
    let mut minimum = output(cells, None, "pressure scalar minimum")?;
    let mut maximum = output(cells, None, "pressure scalar maximum")?;
    let mut range = output(cells, None, "pressure scalar range")?;
    let mut time_weighted_mean = output(cells, None, "pressure scalar weighted mean")?;
    let mut argmin_time_index = output(cells, None, "pressure scalar argmin")?;
    let mut argmax_time_index = output(cells, None, "pressure scalar argmax")?;
    let mut finite_count = output(cells, 0u32, "pressure scalar count")?;
    let mut covered_duration_seconds = output(cells, 0u64, "pressure scalar duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "pressure scalar coverage")?;
    let variable = &prepared.metadata.variables[0];
    let levels_hpa = &prepared.metadata.levels_hpa;
    let first = prepared.first_reader();

    for geometry in first
        .reader
        .selected_pressure_level_chunks_3d(variable, levels_hpa)?
    {
        check_cancelled(is_cancelled)?;
        let chunk_cells = geometry.cell_count();
        let mut accumulators = output(
            chunk_cells * levels_hpa.len(),
            NumericAccumulator::default(),
            "pressure scalar chunk accumulators",
        )?;
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let Some(sample) = sample else { continue };
            let chunk = sample.reader.read_selected_pressure_level_chunk_3d(
                variable,
                levels_hpa,
                geometry.chunk_y(),
                geometry.chunk_x(),
            )?;
            ensure_pressure_chunk_geometry(&chunk, geometry, variable)?;
            let weight = prepared.duration_seconds(sample_index);
            for level_index in 0..levels_hpa.len() {
                for local in 0..chunk_cells {
                    let accumulator = &mut accumulators[level_index * chunk_cells + local];
                    let value = pressure_chunk_value(&chunk, level_index, geometry, local);
                    if value.is_finite() {
                        accumulator.update(f64::from(value), sample.axis_index, weight);
                    } else if missing_policy == MissingPolicy::Strict {
                        return Err(pressure_missing_value_error(
                            variable,
                            sample.time.storage_slot,
                            geometry,
                            local,
                        ));
                    }
                }
            }
        }
        for level_index in 0..levels_hpa.len() {
            for local in 0..chunk_cells {
                let accumulator = &accumulators[level_index * chunk_cells + local];
                let global = level_index * horizontal_cells
                    + pressure_global_index(prepared.metadata.nx, geometry, local);
                minimum[global] = accumulator.minimum;
                maximum[global] = accumulator.maximum;
                range[global] = finite_range(accumulator.minimum, accumulator.maximum);
                time_weighted_mean[global] = (accumulator.weight_seconds > 0)
                    .then(|| accumulator.weighted_total / accumulator.weight_seconds as f64);
                argmin_time_index[global] = accumulator.argmin;
                argmax_time_index[global] = accumulator.argmax;
                finite_count[global] = accumulator.count;
                covered_duration_seconds[global] = accumulator.weight_seconds;
                duration_coverage[global] = duration_ratio(
                    accumulator.weight_seconds,
                    prepared.metadata.window.duration_seconds,
                );
            }
        }
    }
    Ok(ScalarSummaryGrid {
        metadata: prepared.metadata.clone(),
        minimum,
        maximum,
        range,
        time_weighted_mean,
        argmin_time_index,
        argmax_time_index,
        finite_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

fn output<T: Clone>(cells: usize, value: T, what: &'static str) -> QueryResult<Vec<T>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(cells)
        .map_err(|error| QueryError::Allocation {
            what,
            detail: error.to_string(),
        })?;
    output.resize(cells, value);
    Ok(output)
}

fn ensure_tile_geometry(
    tile: &rw_store::Tile2D,
    expected: rw_store::TileGeometry2D,
    variable: &str,
) -> QueryResult<()> {
    let actual = tile.geometry();
    if actual.x0() != expected.x0()
        || actual.y0() != expected.y0()
        || actual.nx() != expected.nx()
        || actual.ny() != expected.ny()
    {
        return Err(QueryError::InconsistentVariable {
            variable: variable.to_string(),
            detail: "tile geometry changed between hours".to_string(),
        });
    }
    Ok(())
}

fn tile_value(tile: &rw_store::Tile2D, geometry: rw_store::TileGeometry2D, local: usize) -> f32 {
    tile.get(local / geometry.nx(), local % geometry.nx())
        .expect("local index is bounded by geometry.cell_count")
}

fn global_index(grid_nx: usize, geometry: rw_store::TileGeometry2D, local: usize) -> usize {
    let row = local / geometry.nx();
    let column = local % geometry.nx();
    (geometry.y0() + row) * grid_nx + geometry.x0() + column
}

fn ensure_pressure_chunk_geometry(
    chunk: &rw_store::SelectedPressureLevelChunk3D,
    expected: rw_store::SelectedPressureLevelChunkGeometry3D,
    variable: &str,
) -> QueryResult<()> {
    let actual = chunk.geometry();
    if actual.x0() != expected.x0()
        || actual.y0() != expected.y0()
        || actual.width() != expected.width()
        || actual.height() != expected.height()
    {
        return Err(QueryError::InconsistentVariable {
            variable: variable.to_string(),
            detail: "pressure column-chunk geometry changed between hours".to_string(),
        });
    }
    Ok(())
}

fn pressure_chunk_value(
    chunk: &rw_store::SelectedPressureLevelChunk3D,
    level_index: usize,
    geometry: rw_store::SelectedPressureLevelChunkGeometry3D,
    local: usize,
) -> f32 {
    chunk
        .get(
            level_index,
            local / geometry.width(),
            local % geometry.width(),
        )
        .expect("local pressure index is bounded by geometry.cell_count")
}

fn pressure_global_index(
    grid_nx: usize,
    geometry: rw_store::SelectedPressureLevelChunkGeometry3D,
    local: usize,
) -> usize {
    let row = local / geometry.width();
    let column = local % geometry.width();
    (geometry.y0() + row) * grid_nx + geometry.x0() + column
}

fn pressure_missing_value_error(
    variable: &str,
    slot: u16,
    geometry: rw_store::SelectedPressureLevelChunkGeometry3D,
    local: usize,
) -> QueryError {
    QueryError::MissingValue {
        variable: variable.to_string(),
        slot,
        x: geometry.x0() + local % geometry.width(),
        y: geometry.y0() + local / geometry.width(),
    }
}

fn missing_value_error(
    variable: &str,
    slot: u16,
    geometry: rw_store::TileGeometry2D,
    local: usize,
) -> QueryError {
    QueryError::MissingValue {
        variable: variable.to_string(),
        slot,
        x: geometry.x0() + local % geometry.nx(),
        y: geometry.y0() + local / geometry.nx(),
    }
}

fn duration_ratio(covered: u64, expected: u64) -> f64 {
    if expected == 0 {
        0.0
    } else {
        covered as f64 / expected as f64
    }
}

fn finite_range(minimum: Option<f64>, maximum: Option<f64>) -> Option<f64> {
    match (minimum, maximum) {
        (Some(low), Some(high)) if low.is_finite() && high.is_finite() => Some(high - low),
        _ => None,
    }
}

#[cfg(test)]
mod finite_range_tests {
    use super::finite_range;

    #[test]
    fn range_requires_two_finite_extrema() {
        assert_eq!(finite_range(Some(-2.0), Some(5.0)), Some(7.0));
        assert_eq!(finite_range(None, Some(5.0)), None);
        assert_eq!(finite_range(Some(-2.0), None), None);
        assert_eq!(finite_range(Some(f64::NAN), Some(5.0)), None);
        assert_eq!(finite_range(Some(-2.0), Some(f64::INFINITY)), None);
    }
}

fn reduce_interval_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    is_cancelled: &mut F,
) -> QueryResult<IntervalSummaryGrid> {
    let cells = prepared.metadata.nx * prepared.metadata.ny;
    let mut total = output(cells, None, "interval total")?;
    let mut minimum_interval = output(cells, None, "interval minimum")?;
    let mut maximum_interval = output(cells, None, "interval maximum")?;
    let mut range_interval = output(cells, None, "interval range")?;
    let mut argmin_time_index = output(cells, None, "interval argmin")?;
    let mut argmax_time_index = output(cells, None, "interval argmax")?;
    let mut finite_count = output(cells, 0u32, "interval count")?;
    let mut covered_duration_seconds = output(cells, 0u64, "interval duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "interval coverage")?;
    let variable = &prepared.metadata.variables[0];
    let first = prepared.first_reader();

    for geometry in first.reader.tiles_2d(variable)? {
        check_cancelled(is_cancelled)?;
        let mut accumulators = output(
            geometry.cell_count(),
            NumericAccumulator::default(),
            "interval tile accumulators",
        )?;
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let Some(sample) = sample else { continue };
            let tile =
                sample
                    .reader
                    .read_tile_2d(variable, geometry.tile_y(), geometry.tile_x())?;
            ensure_tile_geometry(&tile, geometry, variable)?;
            let weight = prepared.duration_seconds(sample_index);
            for (local, accumulator) in accumulators.iter_mut().enumerate() {
                let value = tile_value(&tile, geometry, local);
                if !value.is_finite() {
                    if missing_policy == MissingPolicy::Strict {
                        return Err(missing_value_error(
                            variable,
                            sample.time.storage_slot,
                            geometry,
                            local,
                        ));
                    }
                } else if weight > 0 {
                    accumulator.update(f64::from(value), sample.axis_index, weight);
                }
            }
        }
        for (local, accumulator) in accumulators.into_iter().enumerate() {
            let global = global_index(prepared.metadata.nx, geometry, local);
            total[global] = (accumulator.count > 0).then_some(accumulator.total);
            minimum_interval[global] = accumulator.minimum;
            maximum_interval[global] = accumulator.maximum;
            range_interval[global] = finite_range(accumulator.minimum, accumulator.maximum);
            argmin_time_index[global] = accumulator.argmin;
            argmax_time_index[global] = accumulator.argmax;
            finite_count[global] = accumulator.count;
            covered_duration_seconds[global] = accumulator.weight_seconds;
            duration_coverage[global] = duration_ratio(
                accumulator.weight_seconds,
                prepared.metadata.window.duration_seconds,
            );
        }
    }
    Ok(IntervalSummaryGrid {
        metadata: prepared.metadata.clone(),
        total,
        minimum_interval,
        maximum_interval,
        range_interval,
        argmin_time_index,
        argmax_time_index,
        finite_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

#[derive(Clone, Default)]
struct IntervalMaximumAccumulator {
    minimum: Option<f64>,
    maximum: Option<f64>,
    argmin: Option<u32>,
    argmax: Option<u32>,
    count: u32,
    covered_duration_seconds: u64,
    covered_until: Option<i64>,
}

impl IntervalMaximumAccumulator {
    fn update(
        &mut self,
        value: f64,
        time_index: u32,
        support_start: i64,
        support_end: i64,
    ) -> QueryResult<()> {
        if self.minimum.is_none_or(|minimum| value < minimum) {
            self.minimum = Some(value);
            self.argmin = Some(time_index);
        }
        if self.maximum.is_none_or(|maximum| value > maximum) {
            self.maximum = Some(value);
            self.argmax = Some(time_index);
        }
        self.count += 1;

        // Fixed-window extrema may overlap when output cadence is shorter
        // than the extremum window. Coverage is their union, not the sum of
        // interval lengths, so it can never exceed the requested duration.
        let uncovered_start = self.covered_until.map_or(support_start, |covered_until| {
            support_start.max(covered_until)
        });
        if support_end > uncovered_start {
            let seconds = u64::try_from(support_end - uncovered_start).map_err(|_| {
                QueryError::InvalidRequest("interval-maximum coverage is not representable".into())
            })?;
            self.covered_duration_seconds = self
                .covered_duration_seconds
                .checked_add(seconds)
                .ok_or_else(|| {
                    QueryError::InvalidRequest("interval-maximum coverage overflow".into())
                })?;
        }
        self.covered_until = Some(
            self.covered_until
                .map_or(support_end, |covered_until| covered_until.max(support_end)),
        );
        Ok(())
    }
}

fn reduce_interval_maximum_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    is_cancelled: &mut F,
) -> QueryResult<IntervalMaximumSummaryGrid> {
    let cells = prepared.metadata.nx * prepared.metadata.ny;
    let mut minimum_of_interval_maxima = output(cells, None, "minimum of interval maxima")?;
    let mut maximum_of_interval_maxima = output(cells, None, "maximum of interval maxima")?;
    let mut range_of_interval_maxima = output(cells, None, "range of interval maxima")?;
    let mut argmin_interval_maximum_time_index = output(cells, None, "interval-maximum argmin")?;
    let mut argmax_interval_maximum_time_index = output(cells, None, "interval-maximum argmax")?;
    let mut finite_interval_maximum_count = output(cells, 0u32, "interval-maximum count")?;
    let mut covered_duration_seconds = output(cells, 0u64, "interval-maximum duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "interval-maximum coverage")?;
    let variable = &prepared.metadata.variables[0];
    let first = prepared.first_reader();

    for geometry in first.reader.tiles_2d(variable)? {
        check_cancelled(is_cancelled)?;
        let mut accumulators = output(
            geometry.cell_count(),
            IntervalMaximumAccumulator::default(),
            "interval-maximum tile accumulators",
        )?;
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let (Some(sample), Some((support_start, support_end))) =
                (sample, prepared.supports[sample_index])
            else {
                continue;
            };
            let tile =
                sample
                    .reader
                    .read_tile_2d(variable, geometry.tile_y(), geometry.tile_x())?;
            ensure_tile_geometry(&tile, geometry, variable)?;
            for (local, accumulator) in accumulators.iter_mut().enumerate() {
                let value = tile_value(&tile, geometry, local);
                if value.is_finite() {
                    accumulator.update(
                        f64::from(value),
                        sample.axis_index,
                        support_start,
                        support_end,
                    )?;
                } else if missing_policy == MissingPolicy::Strict {
                    return Err(missing_value_error(
                        variable,
                        sample.time.storage_slot,
                        geometry,
                        local,
                    ));
                }
            }
        }
        for (local, accumulator) in accumulators.into_iter().enumerate() {
            let global = global_index(prepared.metadata.nx, geometry, local);
            minimum_of_interval_maxima[global] = accumulator.minimum;
            maximum_of_interval_maxima[global] = accumulator.maximum;
            range_of_interval_maxima[global] =
                finite_range(accumulator.minimum, accumulator.maximum);
            argmin_interval_maximum_time_index[global] = accumulator.argmin;
            argmax_interval_maximum_time_index[global] = accumulator.argmax;
            finite_interval_maximum_count[global] = accumulator.count;
            covered_duration_seconds[global] = accumulator.covered_duration_seconds;
            duration_coverage[global] = duration_ratio(
                accumulator.covered_duration_seconds,
                prepared.metadata.window.duration_seconds,
            );
        }
    }

    Ok(IntervalMaximumSummaryGrid {
        metadata: prepared.metadata.clone(),
        minimum_of_interval_maxima,
        maximum_of_interval_maxima,
        range_of_interval_maxima,
        argmin_interval_maximum_time_index,
        argmax_interval_maximum_time_index,
        finite_interval_maximum_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

fn reduce_rate_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    seconds_per_rate_unit: f64,
    integral_units: &str,
    is_cancelled: &mut F,
) -> QueryResult<RateSummaryGrid> {
    let cells = prepared.metadata.nx * prepared.metadata.ny;
    let mut minimum_rate = output(cells, None, "rate minimum")?;
    let mut maximum_rate = output(cells, None, "rate maximum")?;
    let mut range_rate = output(cells, None, "rate range")?;
    let mut duration_weighted_mean = output(cells, None, "rate weighted mean")?;
    let mut integral = output(cells, None, "rate integral")?;
    let mut argmin_time_index = output(cells, None, "rate argmin")?;
    let mut argmax_time_index = output(cells, None, "rate argmax")?;
    let mut finite_count = output(cells, 0u32, "rate count")?;
    let mut covered_duration_seconds = output(cells, 0u64, "rate duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "rate coverage")?;
    let variable = &prepared.metadata.variables[0];
    let first = prepared.first_reader();

    for geometry in first.reader.tiles_2d(variable)? {
        check_cancelled(is_cancelled)?;
        let mut accumulators = output(
            geometry.cell_count(),
            NumericAccumulator::default(),
            "rate tile accumulators",
        )?;
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let Some(sample) = sample else { continue };
            let tile =
                sample
                    .reader
                    .read_tile_2d(variable, geometry.tile_y(), geometry.tile_x())?;
            ensure_tile_geometry(&tile, geometry, variable)?;
            let weight = prepared.duration_seconds(sample_index);
            for (local, accumulator) in accumulators.iter_mut().enumerate() {
                let value = tile_value(&tile, geometry, local);
                if !value.is_finite() {
                    if missing_policy == MissingPolicy::Strict {
                        return Err(missing_value_error(
                            variable,
                            sample.time.storage_slot,
                            geometry,
                            local,
                        ));
                    }
                } else if weight > 0 {
                    accumulator.update(f64::from(value), sample.axis_index, weight);
                }
            }
        }
        for (local, accumulator) in accumulators.into_iter().enumerate() {
            let global = global_index(prepared.metadata.nx, geometry, local);
            minimum_rate[global] = accumulator.minimum;
            maximum_rate[global] = accumulator.maximum;
            range_rate[global] = finite_range(accumulator.minimum, accumulator.maximum);
            duration_weighted_mean[global] = (accumulator.weight_seconds > 0)
                .then(|| accumulator.weighted_total / accumulator.weight_seconds as f64);
            integral[global] = (accumulator.weight_seconds > 0)
                .then(|| accumulator.weighted_total / seconds_per_rate_unit);
            argmin_time_index[global] = accumulator.argmin;
            argmax_time_index[global] = accumulator.argmax;
            finite_count[global] = accumulator.count;
            covered_duration_seconds[global] = accumulator.weight_seconds;
            duration_coverage[global] = duration_ratio(
                accumulator.weight_seconds,
                prepared.metadata.window.duration_seconds,
            );
        }
    }
    Ok(RateSummaryGrid {
        metadata: prepared.metadata.clone(),
        integral_units: integral_units.to_string(),
        minimum_rate,
        maximum_rate,
        range_rate,
        duration_weighted_mean,
        integral,
        argmin_time_index,
        argmax_time_index,
        finite_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

#[derive(Clone, Default)]
struct CumulativeAccumulator {
    increments: NumericAccumulator,
    previous: Option<f64>,
    resets: u32,
}

fn reduce_cumulative_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    include_first_value: bool,
    reset_tolerance: f64,
    is_cancelled: &mut F,
) -> QueryResult<CumulativeSummaryGrid> {
    let cells = prepared.metadata.nx * prepared.metadata.ny;
    let mut total_increment = output(cells, None, "cumulative total")?;
    let mut minimum_increment = output(cells, None, "cumulative minimum")?;
    let mut maximum_increment = output(cells, None, "cumulative maximum")?;
    let mut range_increment = output(cells, None, "cumulative range")?;
    let mut argmin_time_index = output(cells, None, "cumulative argmin")?;
    let mut argmax_time_index = output(cells, None, "cumulative argmax")?;
    let mut finite_increment_count = output(cells, 0u32, "cumulative count")?;
    let mut reset_count = output(cells, 0u32, "cumulative resets")?;
    let mut covered_duration_seconds = output(cells, 0u64, "cumulative duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "cumulative coverage")?;
    let variable = &prepared.metadata.variables[0];
    let first = prepared.first_reader();

    for geometry in first.reader.tiles_2d(variable)? {
        check_cancelled(is_cancelled)?;
        let mut accumulators = output(
            geometry.cell_count(),
            CumulativeAccumulator::default(),
            "cumulative tile accumulators",
        )?;
        if let Some(baseline) = &prepared.cumulative_baseline {
            let tile =
                baseline
                    .reader
                    .read_tile_2d(variable, geometry.tile_y(), geometry.tile_x())?;
            ensure_tile_geometry(&tile, geometry, variable)?;
            for (local, accumulator) in accumulators.iter_mut().enumerate() {
                let value = tile_value(&tile, geometry, local);
                if value.is_finite() {
                    accumulator.previous = Some(f64::from(value));
                } else if missing_policy == MissingPolicy::Strict {
                    return Err(missing_value_error(
                        variable,
                        baseline.time.storage_slot,
                        geometry,
                        local,
                    ));
                }
            }
        }
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let Some(sample) = sample else {
                // Never difference across an unknown interval.
                for accumulator in &mut accumulators {
                    accumulator.previous = None;
                }
                continue;
            };
            let tile =
                sample
                    .reader
                    .read_tile_2d(variable, geometry.tile_y(), geometry.tile_x())?;
            ensure_tile_geometry(&tile, geometry, variable)?;
            let weight = prepared.duration_seconds(sample_index);
            for (local, accumulator) in accumulators.iter_mut().enumerate() {
                let value = tile_value(&tile, geometry, local);
                if !value.is_finite() {
                    accumulator.previous = None;
                    if missing_policy == MissingPolicy::Strict {
                        return Err(missing_value_error(
                            variable,
                            sample.time.storage_slot,
                            geometry,
                            local,
                        ));
                    }
                    continue;
                }
                let value = f64::from(value);
                if accumulator.previous.is_none() && sample_index == 0 && include_first_value {
                    // With no predecessor, an explicit first amount remains
                    // available as a zero-duration increment. A loaded
                    // baseline always takes precedence so a window does not
                    // double-count cumulative value from before its start.
                    accumulator.increments.update(value, sample.axis_index, 0);
                } else if let Some(previous) = accumulator.previous
                    && prepared.supports[sample_index].is_some()
                {
                    let raw = value - previous;
                    let increment = if raw < -reset_tolerance {
                        accumulator.resets += 1;
                        value
                    } else {
                        // Small negative codec/noise excursions within the
                        // declared tolerance are treated as zero increase.
                        raw.max(0.0)
                    };
                    accumulator
                        .increments
                        .update(increment, sample.axis_index, weight);
                }
                accumulator.previous = Some(value);
            }
        }
        for (local, accumulator) in accumulators.into_iter().enumerate() {
            let global = global_index(prepared.metadata.nx, geometry, local);
            let increments = accumulator.increments;
            total_increment[global] = (increments.count > 0).then_some(increments.total);
            minimum_increment[global] = increments.minimum;
            maximum_increment[global] = increments.maximum;
            range_increment[global] = finite_range(increments.minimum, increments.maximum);
            argmin_time_index[global] = increments.argmin;
            argmax_time_index[global] = increments.argmax;
            finite_increment_count[global] = increments.count;
            reset_count[global] = accumulator.resets;
            covered_duration_seconds[global] = increments.weight_seconds;
            duration_coverage[global] = duration_ratio(
                increments.weight_seconds,
                prepared.metadata.window.duration_seconds,
            );
        }
    }
    Ok(CumulativeSummaryGrid {
        metadata: prepared.metadata.clone(),
        total_increment,
        minimum_increment,
        maximum_increment,
        range_increment,
        argmin_time_index,
        argmax_time_index,
        finite_increment_count,
        reset_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

#[derive(Clone, Default)]
struct VectorAccumulator {
    minimum_speed: Option<f64>,
    maximum_speed: Option<f64>,
    weighted_speed: f64,
    weighted_u: f64,
    weighted_v: f64,
    weight_seconds: u64,
    count: u32,
    argmin: Option<u32>,
    argmax: Option<u32>,
}

impl VectorAccumulator {
    fn update(&mut self, u: f64, v: f64, time_index: u32, weight: u64) {
        let speed = u.hypot(v);
        if self.minimum_speed.is_none_or(|minimum| speed < minimum) {
            self.minimum_speed = Some(speed);
            self.argmin = Some(time_index);
        }
        if self.maximum_speed.is_none_or(|maximum| speed > maximum) {
            self.maximum_speed = Some(speed);
            self.argmax = Some(time_index);
        }
        self.weighted_speed += speed * weight as f64;
        self.weighted_u += u * weight as f64;
        self.weighted_v += v * weight as f64;
        self.weight_seconds += weight;
        self.count += 1;
    }
}

fn reduce_vector_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    is_cancelled: &mut F,
) -> QueryResult<VectorSummaryGrid> {
    if !prepared.metadata.levels_hpa.is_empty() {
        return reduce_pressure_vector_grid(prepared, missing_policy, is_cancelled);
    }
    let cells = prepared.metadata.nx * prepared.metadata.ny;
    let mut minimum_speed = output(cells, None, "vector minimum speed")?;
    let mut maximum_speed = output(cells, None, "vector maximum speed")?;
    let mut range_speed = output(cells, None, "vector speed range")?;
    let mut time_weighted_mean_speed = output(cells, None, "vector mean speed")?;
    let mut vector_mean_u = output(cells, None, "vector mean u")?;
    let mut vector_mean_v = output(cells, None, "vector mean v")?;
    let mut vector_mean_speed = output(cells, None, "vector mean magnitude")?;
    let mut vector_mean_direction_toward_degrees = output(cells, None, "vector mean direction")?;
    let mut argmin_time_index = output(cells, None, "vector argmin")?;
    let mut argmax_time_index = output(cells, None, "vector argmax")?;
    let mut finite_count = output(cells, 0u32, "vector count")?;
    let mut covered_duration_seconds = output(cells, 0u64, "vector duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "vector coverage")?;
    let u_name = &prepared.metadata.variables[0];
    let v_name = &prepared.metadata.variables[1];
    let first = prepared.first_reader();

    for geometry in first.reader.tiles_2d(u_name)? {
        check_cancelled(is_cancelled)?;
        let mut accumulators = output(
            geometry.cell_count(),
            VectorAccumulator::default(),
            "vector tile accumulators",
        )?;
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let Some(sample) = sample else { continue };
            let u_tile =
                sample
                    .reader
                    .read_tile_2d(u_name, geometry.tile_y(), geometry.tile_x())?;
            let v_tile =
                sample
                    .reader
                    .read_tile_2d(v_name, geometry.tile_y(), geometry.tile_x())?;
            ensure_tile_geometry(&u_tile, geometry, u_name)?;
            ensure_tile_geometry(&v_tile, geometry, v_name)?;
            let weight = prepared.duration_seconds(sample_index);
            for (local, accumulator) in accumulators.iter_mut().enumerate() {
                let u = tile_value(&u_tile, geometry, local);
                let v = tile_value(&v_tile, geometry, local);
                if u.is_finite() && v.is_finite() {
                    accumulator.update(f64::from(u), f64::from(v), sample.axis_index, weight);
                } else if missing_policy == MissingPolicy::Strict {
                    let missing = if !u.is_finite() { u_name } else { v_name };
                    return Err(missing_value_error(
                        missing,
                        sample.time.storage_slot,
                        geometry,
                        local,
                    ));
                }
            }
        }
        for (local, accumulator) in accumulators.into_iter().enumerate() {
            let global = global_index(prepared.metadata.nx, geometry, local);
            minimum_speed[global] = accumulator.minimum_speed;
            maximum_speed[global] = accumulator.maximum_speed;
            range_speed[global] =
                finite_range(accumulator.minimum_speed, accumulator.maximum_speed);
            argmin_time_index[global] = accumulator.argmin;
            argmax_time_index[global] = accumulator.argmax;
            finite_count[global] = accumulator.count;
            covered_duration_seconds[global] = accumulator.weight_seconds;
            duration_coverage[global] = duration_ratio(
                accumulator.weight_seconds,
                prepared.metadata.window.duration_seconds,
            );
            if accumulator.weight_seconds > 0 {
                let weight = accumulator.weight_seconds as f64;
                let mean_u = accumulator.weighted_u / weight;
                let mean_v = accumulator.weighted_v / weight;
                let mean_speed = mean_u.hypot(mean_v);
                time_weighted_mean_speed[global] = Some(accumulator.weighted_speed / weight);
                vector_mean_u[global] = Some(mean_u);
                vector_mean_v[global] = Some(mean_v);
                vector_mean_speed[global] = Some(mean_speed);
                if mean_speed > 1.0e-12 {
                    vector_mean_direction_toward_degrees[global] =
                        Some(mean_u.atan2(mean_v).to_degrees().rem_euclid(360.0));
                }
            }
        }
    }
    Ok(VectorSummaryGrid {
        metadata: prepared.metadata.clone(),
        minimum_speed,
        maximum_speed,
        range_speed,
        time_weighted_mean_speed,
        vector_mean_u,
        vector_mean_v,
        vector_mean_speed,
        vector_mean_direction_toward_degrees,
        argmin_time_index,
        argmax_time_index,
        finite_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

fn reduce_pressure_vector_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    is_cancelled: &mut F,
) -> QueryResult<VectorSummaryGrid> {
    let horizontal_cells = prepared.metadata.nx * prepared.metadata.ny;
    let cells = horizontal_cells * prepared.metadata.levels_hpa.len();
    let mut minimum_speed = output(cells, None, "pressure vector minimum speed")?;
    let mut maximum_speed = output(cells, None, "pressure vector maximum speed")?;
    let mut range_speed = output(cells, None, "pressure vector speed range")?;
    let mut time_weighted_mean_speed = output(cells, None, "pressure vector mean speed")?;
    let mut vector_mean_u = output(cells, None, "pressure vector mean u")?;
    let mut vector_mean_v = output(cells, None, "pressure vector mean v")?;
    let mut vector_mean_speed = output(cells, None, "pressure vector mean magnitude")?;
    let mut vector_mean_direction_toward_degrees =
        output(cells, None, "pressure vector mean direction")?;
    let mut argmin_time_index = output(cells, None, "pressure vector argmin")?;
    let mut argmax_time_index = output(cells, None, "pressure vector argmax")?;
    let mut finite_count = output(cells, 0u32, "pressure vector count")?;
    let mut covered_duration_seconds = output(cells, 0u64, "pressure vector duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "pressure vector coverage")?;
    let u_name = &prepared.metadata.variables[0];
    let v_name = &prepared.metadata.variables[1];
    let levels_hpa = &prepared.metadata.levels_hpa;
    let first = prepared.first_reader();

    for geometry in first
        .reader
        .selected_pressure_level_chunks_3d(u_name, levels_hpa)?
    {
        check_cancelled(is_cancelled)?;
        let chunk_cells = geometry.cell_count();
        let mut accumulators = output(
            chunk_cells * levels_hpa.len(),
            VectorAccumulator::default(),
            "pressure vector chunk accumulators",
        )?;
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let Some(sample) = sample else { continue };
            let u_chunk = sample.reader.read_selected_pressure_level_chunk_3d(
                u_name,
                levels_hpa,
                geometry.chunk_y(),
                geometry.chunk_x(),
            )?;
            let v_chunk = sample.reader.read_selected_pressure_level_chunk_3d(
                v_name,
                levels_hpa,
                geometry.chunk_y(),
                geometry.chunk_x(),
            )?;
            ensure_pressure_chunk_geometry(&u_chunk, geometry, u_name)?;
            ensure_pressure_chunk_geometry(&v_chunk, geometry, v_name)?;
            let weight = prepared.duration_seconds(sample_index);
            for level_index in 0..levels_hpa.len() {
                for local in 0..chunk_cells {
                    let accumulator = &mut accumulators[level_index * chunk_cells + local];
                    let u = pressure_chunk_value(&u_chunk, level_index, geometry, local);
                    let v = pressure_chunk_value(&v_chunk, level_index, geometry, local);
                    if u.is_finite() && v.is_finite() {
                        accumulator.update(f64::from(u), f64::from(v), sample.axis_index, weight);
                    } else if missing_policy == MissingPolicy::Strict {
                        let missing = if !u.is_finite() { u_name } else { v_name };
                        return Err(pressure_missing_value_error(
                            missing,
                            sample.time.storage_slot,
                            geometry,
                            local,
                        ));
                    }
                }
            }
        }
        for level_index in 0..levels_hpa.len() {
            for local in 0..chunk_cells {
                let accumulator = &accumulators[level_index * chunk_cells + local];
                let global = level_index * horizontal_cells
                    + pressure_global_index(prepared.metadata.nx, geometry, local);
                minimum_speed[global] = accumulator.minimum_speed;
                maximum_speed[global] = accumulator.maximum_speed;
                range_speed[global] =
                    finite_range(accumulator.minimum_speed, accumulator.maximum_speed);
                argmin_time_index[global] = accumulator.argmin;
                argmax_time_index[global] = accumulator.argmax;
                finite_count[global] = accumulator.count;
                covered_duration_seconds[global] = accumulator.weight_seconds;
                duration_coverage[global] = duration_ratio(
                    accumulator.weight_seconds,
                    prepared.metadata.window.duration_seconds,
                );
                if accumulator.weight_seconds > 0 {
                    let weight = accumulator.weight_seconds as f64;
                    let mean_u = accumulator.weighted_u / weight;
                    let mean_v = accumulator.weighted_v / weight;
                    let mean_speed = mean_u.hypot(mean_v);
                    time_weighted_mean_speed[global] = Some(accumulator.weighted_speed / weight);
                    vector_mean_u[global] = Some(mean_u);
                    vector_mean_v[global] = Some(mean_v);
                    vector_mean_speed[global] = Some(mean_speed);
                    if mean_speed > 1.0e-12 {
                        vector_mean_direction_toward_degrees[global] =
                            Some(mean_u.atan2(mean_v).to_degrees().rem_euclid(360.0));
                    }
                }
            }
        }
    }
    Ok(VectorSummaryGrid {
        metadata: prepared.metadata.clone(),
        minimum_speed,
        maximum_speed,
        range_speed,
        time_weighted_mean_speed,
        vector_mean_u,
        vector_mean_v,
        vector_mean_speed,
        vector_mean_direction_toward_degrees,
        argmin_time_index,
        argmax_time_index,
        finite_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

#[derive(Clone, Default)]
struct CircularAccumulator {
    weighted_sine: f64,
    weighted_cosine: f64,
    weight_seconds: u64,
    count: u32,
}

fn reduce_circular_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    is_cancelled: &mut F,
) -> QueryResult<CircularMeanGrid> {
    let cells = prepared.metadata.nx * prepared.metadata.ny;
    let mut mean_degrees = output(cells, None, "circular mean")?;
    let mut resultant_length = output(cells, None, "circular resultant")?;
    let mut finite_count = output(cells, 0u32, "circular count")?;
    let mut covered_duration_seconds = output(cells, 0u64, "circular duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "circular coverage")?;
    let variable = &prepared.metadata.variables[0];
    let first = prepared.first_reader();

    for geometry in first.reader.tiles_2d(variable)? {
        check_cancelled(is_cancelled)?;
        let mut accumulators = output(
            geometry.cell_count(),
            CircularAccumulator::default(),
            "circular tile accumulators",
        )?;
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let Some(sample) = sample else { continue };
            let tile =
                sample
                    .reader
                    .read_tile_2d(variable, geometry.tile_y(), geometry.tile_x())?;
            ensure_tile_geometry(&tile, geometry, variable)?;
            let weight = prepared.duration_seconds(sample_index);
            for (local, accumulator) in accumulators.iter_mut().enumerate() {
                let value = tile_value(&tile, geometry, local);
                if value.is_finite() {
                    let radians = f64::from(value).to_radians();
                    accumulator.weighted_sine += radians.sin() * weight as f64;
                    accumulator.weighted_cosine += radians.cos() * weight as f64;
                    accumulator.weight_seconds += weight;
                    accumulator.count += 1;
                } else if missing_policy == MissingPolicy::Strict {
                    return Err(missing_value_error(
                        variable,
                        sample.time.storage_slot,
                        geometry,
                        local,
                    ));
                }
            }
        }
        for (local, accumulator) in accumulators.into_iter().enumerate() {
            let global = global_index(prepared.metadata.nx, geometry, local);
            finite_count[global] = accumulator.count;
            covered_duration_seconds[global] = accumulator.weight_seconds;
            duration_coverage[global] = duration_ratio(
                accumulator.weight_seconds,
                prepared.metadata.window.duration_seconds,
            );
            if accumulator.weight_seconds > 0 {
                let magnitude = accumulator.weighted_sine.hypot(accumulator.weighted_cosine);
                let weight = accumulator.weight_seconds as f64;
                resultant_length[global] = Some((magnitude / weight).clamp(0.0, 1.0));
                if magnitude > 1.0e-12 {
                    mean_degrees[global] = Some(
                        accumulator
                            .weighted_sine
                            .atan2(accumulator.weighted_cosine)
                            .to_degrees()
                            .rem_euclid(360.0),
                    );
                }
            }
        }
    }
    Ok(CircularMeanGrid {
        metadata: prepared.metadata.clone(),
        mean_degrees,
        resultant_length,
        finite_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

#[derive(Clone, Default)]
struct CategoryAccumulator {
    /// category -> (duration, first exact result-axis index)
    durations: BTreeMap<i32, (u64, u32)>,
    previous: Option<i32>,
    transitions: u32,
    count: u32,
    weight_seconds: u64,
}

fn reduce_categorical_grid<F: FnMut() -> bool>(
    prepared: &PreparedQuery,
    missing_policy: MissingPolicy,
    is_cancelled: &mut F,
) -> QueryResult<CategoricalSummaryGrid> {
    let cells = prepared.metadata.nx * prepared.metadata.ny;
    let mut mode = output(cells, None, "categorical mode")?;
    let mut mode_duration_seconds = output(cells, 0u64, "categorical mode duration")?;
    let mut category_durations = output(cells, Vec::new(), "categorical durations")?;
    let mut transitions = output(cells, 0u32, "categorical transitions")?;
    let mut finite_count = output(cells, 0u32, "categorical count")?;
    let mut covered_duration_seconds = output(cells, 0u64, "categorical duration")?;
    let mut duration_coverage = output(cells, 0.0f64, "categorical coverage")?;
    let variable = &prepared.metadata.variables[0];
    let first = prepared.first_reader();
    let mut total_category_entries = 0usize;

    for geometry in first.reader.tiles_2d(variable)? {
        check_cancelled(is_cancelled)?;
        let mut accumulators = output(
            geometry.cell_count(),
            CategoryAccumulator::default(),
            "categorical tile accumulators",
        )?;
        for (sample_index, sample) in prepared.samples.iter().enumerate() {
            check_cancelled(is_cancelled)?;
            let Some(sample) = sample else {
                for accumulator in &mut accumulators {
                    accumulator.previous = None;
                }
                continue;
            };
            let tile =
                sample
                    .reader
                    .read_tile_2d(variable, geometry.tile_y(), geometry.tile_x())?;
            ensure_tile_geometry(&tile, geometry, variable)?;
            let weight = prepared.duration_seconds(sample_index);
            for (local, accumulator) in accumulators.iter_mut().enumerate() {
                let value = tile_value(&tile, geometry, local);
                if !value.is_finite() {
                    accumulator.previous = None;
                    if missing_policy == MissingPolicy::Strict {
                        return Err(missing_value_error(
                            variable,
                            sample.time.storage_slot,
                            geometry,
                            local,
                        ));
                    }
                    continue;
                }
                let value_f64 = f64::from(value);
                if value_f64.fract() != 0.0
                    || value_f64 < f64::from(i32::MIN)
                    || value_f64 > f64::from(i32::MAX)
                {
                    return Err(QueryError::InvalidCategory {
                        variable: variable.clone(),
                        slot: sample.time.storage_slot,
                        x: geometry.x0() + local % geometry.nx(),
                        y: geometry.y0() + local / geometry.nx(),
                        value,
                    });
                }
                let category = value as i32;
                if !accumulator.durations.contains_key(&category) {
                    if accumulator.durations.len() >= MAX_CATEGORIES_PER_CELL {
                        return Err(QueryError::LimitExceeded {
                            what: "categories per cell",
                            requested: accumulator.durations.len() + 1,
                            limit: MAX_CATEGORIES_PER_CELL,
                        });
                    }
                    total_category_entries =
                        total_category_entries
                            .checked_add(1)
                            .ok_or(QueryError::LimitExceeded {
                                what: "categorical result entries",
                                requested: usize::MAX,
                                limit: prepared.max_dynamic_output_values,
                            })?;
                    if total_category_entries > prepared.max_dynamic_output_values {
                        return Err(QueryError::LimitExceeded {
                            what: "categorical result entries",
                            requested: total_category_entries,
                            limit: prepared.max_dynamic_output_values,
                        });
                    }
                }
                let entry = accumulator
                    .durations
                    .entry(category)
                    .or_insert((0, sample.axis_index));
                entry.0 = entry.0.checked_add(weight).ok_or_else(|| {
                    QueryError::InvalidRequest("category duration overflow".to_string())
                })?;
                if accumulator
                    .previous
                    .is_some_and(|previous| previous != category)
                {
                    accumulator.transitions += 1;
                }
                accumulator.previous = Some(category);
                accumulator.count += 1;
                accumulator.weight_seconds += weight;
            }
        }
        for (local, accumulator) in accumulators.into_iter().enumerate() {
            let global = global_index(prepared.metadata.nx, geometry, local);
            let selected_mode = accumulator
                .durations
                .iter()
                .filter(|(_, (duration, _))| *duration > 0)
                .max_by(
                    |(left_category, (left_duration, left_first)),
                     (right_category, (right_duration, right_first))| {
                        left_duration
                            .cmp(right_duration)
                            .then_with(|| right_first.cmp(left_first))
                            .then_with(|| right_category.cmp(left_category))
                    },
                )
                .map(|(&category, &(duration, _))| (category, duration));
            if let Some((category, duration)) = selected_mode {
                mode[global] = Some(category);
                mode_duration_seconds[global] = duration;
            }
            let durations = accumulator
                .durations
                .into_iter()
                .filter_map(|(category, (duration_seconds, _))| {
                    (duration_seconds > 0).then_some(CategoryDuration {
                        category,
                        duration_seconds,
                    })
                })
                .collect::<Vec<_>>();
            category_durations[global] = durations;
            transitions[global] = accumulator.transitions;
            finite_count[global] = accumulator.count;
            covered_duration_seconds[global] = accumulator.weight_seconds;
            duration_coverage[global] = duration_ratio(
                accumulator.weight_seconds,
                prepared.metadata.window.duration_seconds,
            );
        }
    }
    Ok(CategoricalSummaryGrid {
        metadata: prepared.metadata.clone(),
        mode,
        mode_duration_seconds,
        category_durations,
        transitions,
        finite_count,
        covered_duration_seconds,
        duration_coverage,
    })
}

/// Return exact full-domain min/max/count statistics for each selected time.
/// The store index supplies these values without decoding the field payload.
/// This intentionally does not pretend that full-tile index statistics are
/// valid for an arbitrary spatial window.
pub fn query_spatial_stats_series(
    snapshot: &RunSnapshot,
    request: &SpatialStatsSeriesRequest,
) -> QueryResult<SpatialStatsSeriesResult> {
    if request.variable.trim().is_empty() {
        return Err(QueryError::InvalidRequest(
            "a spatial-series variable is required".to_string(),
        ));
    }
    let axis = snapshot.select_timepoints(request.time)?;
    let expected_samples = axis.len();
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(expected_samples)
        .map_err(|error| QueryError::Allocation {
            what: "spatial stats samples",
            detail: error.to_string(),
        })?;
    let mut expected_meta: Option<RwsVariableMeta> = None;
    let mut available_samples = 0usize;
    for time in axis {
        let (reader, path) = snapshot.open_reader(&time)?;
        let Some(meta) = reader.variable(&request.variable) else {
            if request.missing_policy == MissingPolicy::Strict {
                return Err(QueryError::MissingVariable {
                    variable: request.variable.clone(),
                    slot: time.storage_slot,
                });
            }
            samples.push(SpatialStatsSample {
                time: time.clone(),
                variable_available: false,
                minimum: None,
                maximum: None,
                finite_count: 0,
                missing_count: 0,
            });
            snapshot.ensure_source(&reader, &path, time.storage_slot)?;
            continue;
        };
        if meta.kind != "surface2d" {
            return Err(QueryError::WrongVariableKind {
                variable: request.variable.clone(),
                expected: "surface2d",
                actual: meta.kind.clone(),
            });
        }
        if let Some(expected) = &expected_meta {
            ensure_compatible(expected, meta)?;
        } else {
            expected_meta = Some(meta.clone());
        }
        let stats = reader.stats_2d(&request.variable)?;
        samples.push(SpatialStatsSample {
            time: time.clone(),
            variable_available: true,
            minimum: stats.finite_min,
            maximum: stats.finite_max,
            finite_count: stats.finite_count,
            missing_count: stats.missing_count,
        });
        available_samples += 1;
        snapshot.ensure_source(&reader, &path, time.storage_slot)?;
    }
    let meta =
        expected_meta.ok_or_else(|| QueryError::UnknownVariable(request.variable.clone()))?;
    snapshot.ensure_manifest_current()?;
    Ok(SpatialStatsSeriesResult {
        run: snapshot.descriptor().clone(),
        variable: request.variable.clone(),
        units: meta.units,
        samples,
        expected_samples,
        available_samples,
        coverage: ratio(available_samples, expected_samples),
    })
}

/// Read one bounded half-open index rectangle from one exact storage slot.
/// Coordinates are native grid indices; no reprojection or geographic-box
/// approximation is performed.
pub fn query_window_2d(
    snapshot: &RunSnapshot,
    request: &IndexWindow2DRequest,
) -> QueryResult<IndexWindow2DResult> {
    if request.variable.trim().is_empty() {
        return Err(QueryError::InvalidRequest(
            "a window variable is required".to_string(),
        ));
    }
    if request.x0 >= request.x1 || request.y0 >= request.y1 {
        return Err(QueryError::InvalidRequest(
            "index window must be non-empty and half-open".to_string(),
        ));
    }
    if request.x1 > snapshot.grid().nx || request.y1 > snapshot.grid().ny {
        return Err(QueryError::InvalidRequest(format!(
            "index window [{},{}) x [{},{}) exceeds grid {} x {}",
            request.x0,
            request.x1,
            request.y0,
            request.y1,
            snapshot.grid().nx,
            snapshot.grid().ny
        )));
    }
    let cells = (request.x1 - request.x0)
        .checked_mul(request.y1 - request.y0)
        .ok_or(QueryError::LimitExceeded {
            what: "window cells",
            requested: usize::MAX,
            limit: snapshot.limits().max_reduction_cells,
        })?;
    let limit = snapshot
        .limits()
        .max_reduction_cells
        .min(snapshot.limits().max_point_values);
    if cells > limit {
        return Err(QueryError::LimitExceeded {
            what: "window cells",
            requested: cells,
            limit,
        });
    }
    let time = snapshot.timepoint(request.storage_slot)?;
    let (reader, path) = snapshot.open_reader(&time)?;
    let meta = reader
        .variable(&request.variable)
        .ok_or_else(|| QueryError::UnknownVariable(request.variable.clone()))?;
    if meta.kind != "surface2d" {
        return Err(QueryError::WrongVariableKind {
            variable: request.variable.clone(),
            expected: "surface2d",
            actual: meta.kind.clone(),
        });
    }
    let window = reader.read_window_2d(
        &request.variable,
        request.x0,
        request.y0,
        request.x1,
        request.y1,
    )?;
    if window.values.len() != cells || window.nx * window.ny != cells {
        return Err(QueryError::InconsistentVariable {
            variable: request.variable.clone(),
            detail: format!(
                "decoded {} cells for requested {cells}-cell window",
                window.values.len()
            ),
        });
    }
    snapshot.ensure_source(&reader, &path, time.storage_slot)?;
    snapshot.ensure_manifest_current()?;
    Ok(IndexWindow2DResult {
        run: snapshot.descriptor().clone(),
        time,
        variable: request.variable.clone(),
        units: meta.units.clone(),
        x0: window.x0,
        y0: window.y0,
        nx: window.nx,
        ny: window.ny,
        values: window
            .values
            .into_iter()
            .map(|value| value.is_finite().then_some(value))
            .collect(),
    })
}

/// Read an explicit pressure-level selection from one bounded half-open native
/// index rectangle. Only overlapping column chunks are decoded; a small tile
/// never materializes a complete pressure plane or volume.
pub fn query_window_3d(
    snapshot: &RunSnapshot,
    request: &IndexWindow3DRequest,
) -> QueryResult<IndexWindow3DResult> {
    if request.variable.trim().is_empty() || request.levels_hpa.is_empty() {
        return Err(QueryError::InvalidRequest(
            "a pressure-window variable and at least one pressure level are required".into(),
        ));
    }
    if request.x0 >= request.x1 || request.y0 >= request.y1 {
        return Err(QueryError::InvalidRequest(
            "index window must be non-empty and half-open".into(),
        ));
    }
    if request.x1 > snapshot.grid().nx || request.y1 > snapshot.grid().ny {
        return Err(QueryError::InvalidRequest(format!(
            "index window [{},{}) x [{},{}) exceeds grid {} x {}",
            request.x0,
            request.x1,
            request.y0,
            request.y1,
            snapshot.grid().nx,
            snapshot.grid().ny
        )));
    }
    let width = request.x1 - request.x0;
    let height = request.y1 - request.y0;
    let cells = width
        .checked_mul(height)
        .and_then(|cells| cells.checked_mul(request.levels_hpa.len()))
        .ok_or(QueryError::LimitExceeded {
            what: "pressure window values",
            requested: usize::MAX,
            limit: snapshot.limits().max_reduction_cells,
        })?;
    let limit = snapshot
        .limits()
        .max_reduction_cells
        .min(snapshot.limits().max_point_values);
    if cells > limit {
        return Err(QueryError::LimitExceeded {
            what: "pressure window values",
            requested: cells,
            limit,
        });
    }

    let time = snapshot.timepoint(request.storage_slot)?;
    let (reader, path) = snapshot.open_reader(&time)?;
    let meta = reader
        .variable(&request.variable)
        .ok_or_else(|| QueryError::UnknownVariable(request.variable.clone()))?;
    if meta.kind != "pressure3d" {
        return Err(QueryError::WrongVariableKind {
            variable: request.variable.clone(),
            expected: "pressure3d",
            actual: meta.kind.clone(),
        });
    }
    let mut unique = BTreeSet::new();
    if request
        .levels_hpa
        .iter()
        .any(|level| !unique.insert(*level))
    {
        return Err(QueryError::InvalidRequest(
            "pressure-window levels must be unique".into(),
        ));
    }
    let mut values = Vec::new();
    values
        .try_reserve_exact(cells)
        .map_err(|error| QueryError::Allocation {
            what: "pressure window values",
            detail: error.to_string(),
        })?;
    values.resize(cells, None);

    let chunk_width = reader.meta().chunking.col_x;
    let chunk_height = reader.meta().chunking.col_y;
    if chunk_width == 0 || chunk_height == 0 {
        return Err(QueryError::InconsistentVariable {
            variable: request.variable.clone(),
            detail: "pressure chunk dimensions are zero".into(),
        });
    }
    let first_chunk_x = request.x0 / chunk_width;
    let last_chunk_x = (request.x1 - 1) / chunk_width;
    let first_chunk_y = request.y0 / chunk_height;
    let last_chunk_y = (request.y1 - 1) / chunk_height;
    for chunk_y in first_chunk_y..=last_chunk_y {
        for chunk_x in first_chunk_x..=last_chunk_x {
            let chunk = reader.read_selected_pressure_level_chunk_3d(
                &request.variable,
                &request.levels_hpa,
                chunk_y,
                chunk_x,
            )?;
            let geometry = chunk.geometry();
            let overlap_x0 = request.x0.max(geometry.x0());
            let overlap_y0 = request.y0.max(geometry.y0());
            let overlap_x1 = request.x1.min(geometry.x0() + geometry.width());
            let overlap_y1 = request.y1.min(geometry.y0() + geometry.height());
            for level_index in 0..request.levels_hpa.len() {
                for y in overlap_y0..overlap_y1 {
                    for x in overlap_x0..overlap_x1 {
                        let value = chunk
                            .get(level_index, y - geometry.y0(), x - geometry.x0())
                            .ok_or_else(|| QueryError::InconsistentVariable {
                                variable: request.variable.clone(),
                                detail: "pressure chunk omitted an overlapping cell".into(),
                            })?;
                        let output = level_index * width * height
                            + (y - request.y0) * width
                            + (x - request.x0);
                        values[output] = value.is_finite().then_some(value);
                    }
                }
            }
        }
    }
    snapshot.ensure_source(&reader, &path, time.storage_slot)?;
    snapshot.ensure_manifest_current()?;
    Ok(IndexWindow3DResult {
        run: snapshot.descriptor().clone(),
        time,
        variable: request.variable.clone(),
        units: meta.units.clone(),
        levels_hpa: request.levels_hpa.clone(),
        x0: request.x0,
        y0: request.y0,
        nx: width,
        ny: height,
        values,
    })
}
