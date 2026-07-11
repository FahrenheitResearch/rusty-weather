//! Safe adapters between `wrf-formula`, raw WRF files, and `rw-store` runs.
//!
//! Raw WRF evaluation delegates to `CompiledFormula::evaluate_wrf`, retaining
//! Formula Lab's projection, map-factor, physical-height, and WRF-time
//! semantics. The store adapter is intentionally narrower: rw-store v1 does
//! not retain DX/DY, MAPFAC_M, a physical-height volume, vector basis, or an
//! exact valid-time axis. It therefore supports unit-checked pointwise algebra
//! over surface and pressure-volume fields. Explicit vertical operations can
//! use a stored physical-height volume (for example `height_iso`), but no
//! implicit/default height is invented. Horizontal calculus is advertised as
//! unsupported, and time is supplied only when the caller provides a verified
//! exact time for every participating store hour.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustwx_core::{GridProjection, GridShape};
use rw_store::grid::GridFile;
use rw_store::reader::HourReader;
use rw_store::run::{RwsRunManifest, SCHEMA_RUN, SCHEMA_RUN_V2, validate_store_component};
use sha2::{Digest, Sha256};
use thiserror::Error;
use wrf_core::WrfFile;
pub use wrf_formula::{
    Axis, BoundaryPolicy, CompileOptions, CompiledFormula, ENGINE_VERSION, ErrorKind,
    EvaluationOptions, ExecutionPlan, FieldRequest, FieldResolver, FormulaError, FormulaOutput,
    FormulaProvenance, FormulaResult, GridConvention, GridLocation, GridMetadata, HeightDatum,
    MissingPolicy, NonFinitePolicy, ParameterSpec, ParameterValues, Recipe, RecipeReference,
    RecipeRequirements, Requirement, ResolvedField, ResourceLimits, Span, compile,
    compile_with_options,
};

const MAX_STORE_FIELD_ELEMENTS: usize = 128 * 1024 * 1024;

/// Result type for bridge construction, raw-file opening, and display output.
pub type BridgeResult<T> = Result<T, BridgeError>;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error(transparent)]
    Formula(#[from] FormulaError),
    #[error("store Formula Lab source is invalid: {0}")]
    Store(String),
    #[error("raw WRF Formula Lab source is invalid: {0}")]
    Wrf(String),
    #[error("Formula Lab output cannot be displayed: {0}")]
    Output(String),
}

/// A caller-verified valid time for one rw-store timestep.
///
/// For a legacy v1 run, `seconds` may use any epoch as long as every entry uses
/// the same one. For an exact-time v2 run it must equal the persisted lead time
/// in seconds. Formula Lab only consumes differences. `label` is copied into
/// result provenance when present.
#[derive(Debug, Clone, PartialEq)]
pub struct ExactStoreTime {
    pub seconds: f64,
    pub label: Option<String>,
}

impl ExactStoreTime {
    pub fn new(seconds: f64, label: Option<String>) -> Self {
        Self { seconds, label }
    }
}

/// Display-safe scalar 2-D Formula Lab result.
#[derive(Debug, Clone)]
pub struct EvaluatedField2D {
    pub nx: usize,
    pub ny: usize,
    pub values: Vec<f32>,
    pub units: String,
    pub description: String,
    pub provenance: FormulaProvenance,
    /// Adapter warnings in addition to `provenance.warnings`.
    pub warnings: Vec<String>,
}

/// Formula resolver over one sorted rw-store run.
///
/// Hour files are opened lazily and retained for the lifetime of one
/// evaluation. The manifest's BTreeMap order defines relative time offsets;
/// an offset of +1 means the next stored valid hour, not necessarily f+1.
pub struct StoreRunResolver {
    run_dir: PathBuf,
    manifest: RwsRunManifest,
    grid: Arc<GridFile>,
    hours: Vec<StoreHour>,
    base_index: usize,
    exact_times: BTreeMap<u16, ExactStoreTime>,
    readers: Mutex<BTreeMap<usize, Arc<HourReader>>>,
    pressure_levels: Mutex<Option<Vec<u16>>>,
    allocation_budget: Mutex<StoreAllocationBudget>,
    limits: ResourceLimits,
    identity: String,
}

#[derive(Debug, Clone)]
struct StoreHour {
    forecast_hour: u16,
    path: PathBuf,
}

#[derive(Debug, Default)]
struct StoreAllocationBudget {
    resident_promoted_bytes: u64,
    reserved_transient_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct StoreFieldAllocation {
    raw_bytes: u64,
    promoted_bytes: u64,
}

struct StoreAllocationReservation<'a> {
    budget: &'a Mutex<StoreAllocationBudget>,
    transient_bytes: u64,
    promoted_bytes: u64,
    committed: bool,
}

impl StoreAllocationReservation<'_> {
    fn commit(mut self) -> FormulaResult<()> {
        let mut budget = self.budget.lock().map_err(|_| {
            FormulaError::new(
                ErrorKind::Internal,
                "rw-store Formula Lab allocation budget was poisoned",
            )
        })?;
        let reserved_transient_bytes = budget
            .reserved_transient_bytes
            .checked_sub(self.transient_bytes)
            .ok_or_else(|| {
                FormulaError::new(
                    ErrorKind::Internal,
                    "rw-store Formula Lab allocation reservation underflowed",
                )
            })?;
        let resident_promoted_bytes = budget
            .resident_promoted_bytes
            .checked_add(self.promoted_bytes)
            .ok_or_else(|| {
                FormulaError::new(
                    ErrorKind::Limit,
                    "rw-store Formula Lab resident allocation accounting overflowed",
                )
            })?;
        budget.reserved_transient_bytes = reserved_transient_bytes;
        budget.resident_promoted_bytes = resident_promoted_bytes;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StoreAllocationReservation<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(mut budget) = self.budget.lock() {
            budget.reserved_transient_bytes = budget
                .reserved_transient_bytes
                .saturating_sub(self.transient_bytes);
        }
    }
}

impl StoreRunResolver {
    /// Open a run using persisted exact timing when it is a v2 run. Legacy v1
    /// runs still leave `dt` disabled instead of treating forecast-hour labels
    /// or file write timestamps as verified valid times.
    pub fn open(
        store_root: impl AsRef<Path>,
        model: impl Into<String>,
        run: impl Into<String>,
        base_hour: u16,
    ) -> BridgeResult<Self> {
        Self::open_with_exact_times(store_root, model, run, base_hour, BTreeMap::new())
    }

    /// Open a run with caller-verified exact times. If any times are supplied,
    /// every manifest hour must have one and they must be finite and strictly
    /// increasing in manifest order. This makes temporal stencils fail closed.
    pub fn open_with_exact_times(
        store_root: impl AsRef<Path>,
        model: impl Into<String>,
        run: impl Into<String>,
        base_hour: u16,
        exact_times: BTreeMap<u16, ExactStoreTime>,
    ) -> BridgeResult<Self> {
        Self::open_with_exact_times_and_limits(
            store_root,
            model,
            run,
            base_hour,
            exact_times,
            ResourceLimits::default(),
        )
    }

    /// Open a run with caller-verified exact times and the resource limits
    /// compiled into the formula that will consume this resolver. The limits
    /// are enforced before any field payload is decoded or promoted to f64.
    pub fn open_with_exact_times_and_limits(
        store_root: impl AsRef<Path>,
        model: impl Into<String>,
        run: impl Into<String>,
        base_hour: u16,
        exact_times: BTreeMap<u16, ExactStoreTime>,
        limits: ResourceLimits,
    ) -> BridgeResult<Self> {
        let model = model.into();
        let run = run.into();
        validate_store_segment("model", &model)?;
        validate_store_segment("run", &run)?;

        let root = fs::canonicalize(store_root.as_ref()).map_err(|error| {
            BridgeError::Store(format!(
                "cannot resolve store root '{}': {error}",
                store_root.as_ref().display()
            ))
        })?;
        let requested_run_dir = root.join(&model).join(&run);
        let run_dir = fs::canonicalize(&requested_run_dir).map_err(|error| {
            BridgeError::Store(format!(
                "cannot resolve run directory '{}': {error}",
                requested_run_dir.display()
            ))
        })?;
        if !run_dir.starts_with(&root) {
            return Err(BridgeError::Store(
                "resolved run directory escapes the store root".to_string(),
            ));
        }

        let manifest_path = run_dir.join("run.json");
        let manifest =
            RwsRunManifest::load_for_run(&manifest_path, &model, &run).map_err(|error| {
                BridgeError::Store(format!(
                    "cannot load run manifest '{}': {error}",
                    manifest_path.display()
                ))
            })?;
        validate_manifest(&manifest, &model, &run)?;
        GridShape::new(manifest.nx, manifest.ny).map_err(|error| {
            BridgeError::Store(format!(
                "run manifest grid is outside desktop limits: {error}"
            ))
        })?;

        let grid_path = run_dir.join("grid.rwg");
        let grid = Arc::new(GridFile::open(&grid_path).map_err(|error| {
            BridgeError::Store(format!(
                "cannot open grid '{}': {error}",
                grid_path.display()
            ))
        })?);
        if grid.hash != manifest.grid_hash || grid.nx != manifest.nx || grid.ny != manifest.ny {
            return Err(BridgeError::Store(format!(
                "run/grid identity mismatch: manifest {}x{} hash {}, grid {}x{} hash {}",
                manifest.nx, manifest.ny, manifest.grid_hash, grid.nx, grid.ny, grid.hash
            )));
        }

        let mut hours = Vec::with_capacity(manifest.hours.len());
        let mut hour_paths = BTreeSet::new();
        for (&forecast_hour, entry) in &manifest.hours {
            let timestep_label = manifest_timestep_label(&manifest, forecast_hour);
            let relative = Path::new(&entry.file);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(BridgeError::Store(format!(
                    "timestep {timestep_label} has unsafe file path '{}'",
                    entry.file
                )));
            }
            let requested = run_dir.join(relative);
            if !hour_paths.insert(requested.clone()) {
                return Err(BridgeError::Store(format!(
                    "multiple manifest timesteps resolve to the same file '{}'",
                    requested.display()
                )));
            }
            hours.push(StoreHour {
                forecast_hour,
                path: requested,
            });
        }
        if hours.is_empty() {
            return Err(BridgeError::Store("run contains no timesteps".to_string()));
        }
        let base_index = hours
            .iter()
            .position(|hour| hour.forecast_hour == base_hour)
            .ok_or_else(|| {
                BridgeError::Store(format!(
                    "run has no timestep {}",
                    manifest_timestep_label(&manifest, base_hour)
                ))
            })?;
        let exact_times = reconcile_exact_times(&manifest, exact_times)?;
        validate_exact_times(&hours, &exact_times)?;
        let time_axis_hash = time_axis_hash(&manifest, &exact_times);

        let identity = format!(
            "rw-store:{};model={model};run={run};grid={};time_axis={time_axis_hash}",
            run_dir.display(),
            grid.hash
        );
        Ok(Self {
            run_dir,
            manifest,
            grid,
            hours,
            base_index,
            exact_times,
            readers: Mutex::new(BTreeMap::new()),
            pressure_levels: Mutex::new(None),
            allocation_budget: Mutex::new(StoreAllocationBudget::default()),
            limits,
            identity,
        })
    }

    pub fn grid(&self) -> Arc<GridFile> {
        self.grid.clone()
    }

    pub fn base_hour(&self) -> u16 {
        self.hours[self.base_index].forecast_hour
    }

    pub fn available_hours(&self) -> impl Iterator<Item = u16> + '_ {
        self.hours.iter().map(|hour| hour.forecast_hour)
    }

    pub fn has_exact_time_axis(&self) -> bool {
        !self.exact_times.is_empty()
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    fn index_for_offset(&self, offset: isize) -> FormulaResult<usize> {
        let index = self.base_index.checked_add_signed(offset).ok_or_else(|| {
            FormulaError::new(
                ErrorKind::Time,
                format!("store time offset {offset} is before the first available hour"),
            )
        })?;
        if index >= self.hours.len() {
            return Err(FormulaError::new(
                ErrorKind::Time,
                format!(
                    "store time offset {offset} is outside the run after the last available hour"
                ),
            ));
        }
        Ok(index)
    }

    fn reader(&self, index: usize) -> FormulaResult<Arc<HourReader>> {
        let mut readers = self.readers.lock().map_err(|_| {
            FormulaError::new(
                ErrorKind::Internal,
                "rw-store Formula Lab reader cache was poisoned",
            )
        })?;
        if let Some(reader) = readers.get(&index) {
            return Ok(reader.clone());
        }
        let hour = self.hours.get(index).ok_or_else(|| {
            FormulaError::new(
                ErrorKind::Time,
                format!("store hour index {index} is out of range"),
            )
        })?;
        let canonical_path = fs::canonicalize(&hour.path).map_err(|error| {
            FormulaError::new(
                ErrorKind::Resolver,
                format!(
                    "cannot resolve store timestep {} file '{}': {error}",
                    self.time_label(index),
                    hour.path.display()
                ),
            )
        })?;
        if !canonical_path.starts_with(&self.run_dir) {
            return Err(FormulaError::new(
                ErrorKind::Resolver,
                format!(
                    "store timestep {} file escapes its run directory",
                    self.time_label(index)
                ),
            ));
        }
        let reader = Arc::new(HourReader::open(&canonical_path).map_err(|error| {
            FormulaError::new(
                ErrorKind::Resolver,
                format!(
                    "cannot open store timestep {}: {error}",
                    self.time_label(index)
                ),
            )
        })?);
        self.manifest
            .validate_hour_meta(hour.forecast_hour, reader.meta())
            .map_err(|error| {
                FormulaError::new(
                    ErrorKind::Resolver,
                    format!(
                        "store timestep {} metadata does not match its manifest: {error}",
                        self.time_label(index)
                    ),
                )
            })?;
        readers.insert(index, reader.clone());
        Ok(reader)
    }

    fn time_label(&self, index: usize) -> String {
        let Some(hour) = self.hours.get(index) else {
            return format!("index {index}");
        };
        self.exact_times
            .get(&hour.forecast_hour)
            .and_then(|time| time.label.clone())
            .unwrap_or_else(|| manifest_timestep_label(&self.manifest, hour.forecast_hour))
    }
}

impl FieldResolver for StoreRunResolver {
    fn resolve(&self, request: &FieldRequest) -> FormulaResult<ResolvedField> {
        let index = self.index_for_offset(request.time_offset)?;
        let reader = self.reader(index)?;
        let matches = reader
            .meta()
            .variables
            .iter()
            .filter(|variable| variable.name.eq_ignore_ascii_case(&request.name))
            .collect::<Vec<_>>();
        let variable = match matches.as_slice() {
            [] => {
                return Err(FormulaError::new(
                    ErrorKind::Resolver,
                    format!(
                        "store timestep {} has no field '{}'",
                        self.time_label(index),
                        request.name
                    ),
                ));
            }
            [variable] => *variable,
            _ => {
                return Err(FormulaError::new(
                    ErrorKind::Resolver,
                    format!(
                        "store contains case-colliding fields for '{}'",
                        request.name
                    ),
                ));
            }
        };

        let (raw, shape, axes, reservation) = match variable.kind.as_str() {
            "surface2d" => {
                let allocation = checked_store_elements(
                    self.grid.nx,
                    self.grid.ny,
                    1,
                    &variable.name,
                    &self.limits,
                )?;
                let reservation = reserve_store_allocation(
                    &self.allocation_budget,
                    &self.limits,
                    allocation,
                    &variable.name,
                )?;
                let values = reader.read_full_2d(&variable.name).map_err(|error| {
                    FormulaError::new(
                        ErrorKind::Resolver,
                        format!("cannot read store field '{}': {error}", variable.name),
                    )
                })?;
                (
                    values,
                    vec![self.grid.ny, self.grid.nx],
                    vec![Axis::Y, Axis::X],
                    reservation,
                )
            }
            "pressure3d" => {
                if variable.levels_hpa.is_empty() {
                    return Err(FormulaError::new(
                        ErrorKind::Shape,
                        format!("pressure field '{}' has no levels", variable.name),
                    ));
                }
                let allocation = checked_store_elements(
                    self.grid.nx,
                    self.grid.ny,
                    variable.levels_hpa.len(),
                    &variable.name,
                    &self.limits,
                )?;
                let reservation = reserve_store_allocation(
                    &self.allocation_budget,
                    &self.limits,
                    allocation,
                    &variable.name,
                )?;
                let mut pressure_levels = self.pressure_levels.lock().map_err(|_| {
                    FormulaError::new(
                        ErrorKind::Internal,
                        "rw-store Formula Lab pressure-coordinate cache was poisoned",
                    )
                })?;
                match pressure_levels.as_ref() {
                    Some(levels) if levels != &variable.levels_hpa => {
                        return Err(FormulaError::new(
                            ErrorKind::Shape,
                            format!(
                                "pressure field '{}' uses levels {:?}, but another formula input uses {:?}",
                                variable.name, variable.levels_hpa, levels
                            ),
                        ));
                    }
                    None => *pressure_levels = Some(variable.levels_hpa.clone()),
                    Some(_) => {}
                }
                drop(pressure_levels);
                let values = reader.read_full_3d(&variable.name).map_err(|error| {
                    FormulaError::new(
                        ErrorKind::Resolver,
                        format!(
                            "cannot read store pressure field '{}': {error}",
                            variable.name
                        ),
                    )
                })?;
                (
                    values,
                    vec![variable.levels_hpa.len(), self.grid.ny, self.grid.nx],
                    vec![Axis::Z, Axis::Y, Axis::X],
                    reservation,
                )
            }
            other => {
                return Err(FormulaError::new(
                    ErrorKind::Unsupported,
                    format!(
                        "store field '{}' has unsupported kind '{other}'",
                        variable.name
                    ),
                ));
            }
        };
        let expected = shape
            .iter()
            .try_fold(1usize, |count, &dim| count.checked_mul(dim))
            .ok_or_else(|| {
                FormulaError::new(ErrorKind::Limit, "store field shape overflows usize")
            })?;
        if raw.len() != expected {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                format!(
                    "store field '{}' has {} values but shape {shape:?} requires {expected}",
                    variable.name,
                    raw.len()
                ),
            ));
        }
        let mut promoted = Vec::new();
        promoted.try_reserve_exact(raw.len()).map_err(|error| {
            FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "cannot reserve f64 working storage for store field '{}': {error}",
                    variable.name
                ),
            )
        })?;
        promoted.extend(raw.into_iter().map(f64::from));
        let data: Arc<[f64]> = promoted.into();
        reservation.commit()?;
        Ok(ResolvedField {
            resolved_name: variable.name.clone(),
            data,
            shape,
            axes,
            units: (!variable.units.trim().is_empty()).then(|| variable.units.clone()),
            grid_location: GridLocation::Mass,
            vector_basis: None,
            description: format!("rw-store field {}", variable.name),
        })
    }

    fn grid_metadata(&self, time_offset: isize) -> FormulaResult<GridMetadata> {
        let _ = self.index_for_offset(time_offset)?;
        Ok(GridMetadata {
            nx: self.grid.nx,
            ny: self.grid.ny,
            // Pressure volumes may use different level sets, so there is no
            // honest run-wide nz for recipe preflight.
            nz: None,
            dx_m: f64::NAN,
            dy_m: f64::NAN,
            convention: match self.grid.projection.as_ref() {
                Some(projection) if projection.is_projected() => {
                    GridConvention::WrfMassPointProjected
                }
                _ => GridConvention::Cartesian,
            },
            horizontal_calculus_supported: false,
            mass_map_factor: None,
            default_vertical_coordinate: None,
            default_height_datum: None,
        })
    }

    fn time_seconds(&self, time_offset: isize) -> FormulaResult<f64> {
        let index = self.index_for_offset(time_offset)?;
        let forecast_hour = self.hours[index].forecast_hour;
        let Some(time) = self.exact_times.get(&forecast_hour) else {
            return Err(FormulaError::new(
                ErrorKind::Time,
                format!(
                    "store valid time for {} is not verified; dt is disabled for this imported run",
                    self.time_label(index)
                ),
            )
            .note("supply a complete caller-verified ExactStoreTime map to enable temporal derivatives"));
        };
        Ok(time.seconds)
    }

    fn base_time_index(&self) -> Option<usize> {
        Some(self.base_index)
    }

    fn valid_time(&self, time_offset: isize) -> Option<String> {
        let index = self.index_for_offset(time_offset).ok()?;
        let time = self.exact_times.get(&self.hours[index].forecast_hour)?;
        time.label
            .clone()
            .or_else(|| Some(format!("exact-seconds:{:.6}", time.seconds)))
    }

    fn input_identity(&self) -> Option<String> {
        Some(self.identity.clone())
    }
}

/// Evaluate against any resolver and narrow only a scalar `[Y, X]` result for
/// display. Spatial/vector/3-D outputs are rejected rather than sliced.
pub fn evaluate_resolver_2d<R: FieldResolver>(
    formula: &CompiledFormula,
    resolver: &R,
    parameters: &ParameterValues,
    options: &EvaluationOptions,
) -> BridgeResult<EvaluatedField2D> {
    let output = formula.evaluate(resolver, parameters, options)?;
    let field = output_to_field_2d(output)?;
    let grid = resolver.grid_metadata(0)?;
    if field.nx != grid.nx || field.ny != grid.ny {
        return Err(BridgeError::Output(format!(
            "formula result {}x{} does not match resolver grid {}x{}",
            field.nx, field.ny, grid.nx, grid.ny
        )));
    }
    Ok(field)
}

/// Full-fidelity direct WRF evaluation. Formula Lab obtains map factors,
/// physical height, and exact WRF Times through its native WRF resolver.
pub fn evaluate_wrf_2d(
    formula: &CompiledFormula,
    file: &WrfFile,
    time_index: usize,
    parameters: &ParameterValues,
    options: &EvaluationOptions,
) -> BridgeResult<EvaluatedField2D> {
    let output = formula.evaluate_wrf(file, time_index, parameters, options)?;
    let field = output_to_field_2d(output)?;
    if field.nx != file.nx || field.ny != file.ny {
        return Err(BridgeError::Output(format!(
            "formula result {}x{} does not match WRF grid {}x{}",
            field.nx, field.ny, file.nx, file.ny
        )));
    }
    Ok(field)
}

/// Open a raw WRF path, evaluate it, and construct the exact display grid for
/// the same time index. Opening occurs in the caller's thread; the egui bridge
/// invokes this only from its background worker.
pub fn evaluate_wrf_path_2d(
    formula: &CompiledFormula,
    path: impl AsRef<Path>,
    time_index: usize,
    parameters: &ParameterValues,
    options: &EvaluationOptions,
) -> BridgeResult<(EvaluatedField2D, Arc<GridFile>)> {
    evaluate_wrf_path_2d_with_limits(
        formula,
        path,
        time_index,
        parameters,
        options,
        &ResourceLimits::default(),
    )
}

/// Raw-WRF path evaluation with an explicit host resource policy. A cheap
/// horizontal-grid preflight runs immediately after open, then wrf-formula can
/// reject oversized 3-D dependencies before XLAT/XLONG are decoded. The
/// display-grid allocation is checked again before either coordinate is read.
pub fn evaluate_wrf_path_2d_with_limits(
    formula: &CompiledFormula,
    path: impl AsRef<Path>,
    time_index: usize,
    parameters: &ParameterValues,
    options: &EvaluationOptions,
    limits: &ResourceLimits,
) -> BridgeResult<(EvaluatedField2D, Arc<GridFile>)> {
    let path = path.as_ref();
    let file = WrfFile::open(path)
        .map_err(|error| BridgeError::Wrf(format!("cannot open '{}': {error}", path.display())))?;
    // Reject a desktop-ineligible horizontal shape before even a 2-D formula
    // dependency can allocate. Formula evaluation then performs its stricter
    // dependency-aware 3-D preflight before the display coordinates are read.
    checked_wrf_grid_elements(file.nx, file.ny, limits)?;
    let output = evaluate_wrf_2d(formula, &file, time_index, parameters, options)?;
    let grid = Arc::new(grid_from_wrf(&file, path, time_index, limits)?);
    if output.nx != grid.nx || output.ny != grid.ny {
        return Err(BridgeError::Output(format!(
            "formula result {}x{} does not match WRF grid {}x{}",
            output.nx, output.ny, grid.nx, grid.ny
        )));
    }
    Ok((output, grid))
}

fn output_to_field_2d(output: FormulaOutput) -> BridgeResult<EvaluatedField2D> {
    if output.axes != [Axis::Y, Axis::X] || output.shape.len() != 2 {
        return Err(BridgeError::Output(format!(
            "expected one scalar field with axes [Y, X], got axes {:?} and shape {:?}",
            output.axes, output.shape
        )));
    }
    let ny = output.shape[0];
    let nx = output.shape[1];
    if nx == 0 || ny == 0 {
        return Err(BridgeError::Output(
            "formula returned a degenerate 2-D field".to_string(),
        ));
    }
    let expected = nx
        .checked_mul(ny)
        .ok_or_else(|| BridgeError::Output("2-D output shape overflows usize".to_string()))?;
    if output.data.len() != expected {
        return Err(BridgeError::Output(format!(
            "shape [{ny}, {nx}] requires {expected} values, got {}",
            output.data.len()
        )));
    }

    let mut replaced_infinite = 0usize;
    let mut replaced_overflow = 0usize;
    let mut values = Vec::new();
    values.try_reserve_exact(expected).map_err(|error| {
        BridgeError::Output(format!(
            "cannot reserve f32 display storage for {expected} formula values: {error}"
        ))
    })?;
    values.extend(output.data.into_iter().map(|value| {
        if value.is_nan() {
            f32::NAN
        } else if !value.is_finite() {
            replaced_infinite += 1;
            f32::NAN
        } else if value > f32::MAX as f64 || value < f32::MIN as f64 {
            replaced_overflow += 1;
            f32::NAN
        } else {
            value as f32
        }
    }));
    let mut warnings = Vec::new();
    if replaced_infinite > 0 {
        warnings.push(format!(
            "{replaced_infinite} infinite output value(s) were converted to display-missing NaN"
        ));
    }
    if replaced_overflow > 0 {
        warnings.push(format!(
            "{replaced_overflow} finite value(s) outside the f32 display range were converted to NaN"
        ));
    }
    Ok(EvaluatedField2D {
        nx,
        ny,
        values,
        units: output.units,
        description: output.description,
        provenance: output.provenance,
        warnings,
    })
}

fn grid_from_wrf(
    file: &WrfFile,
    path: &Path,
    time_index: usize,
    limits: &ResourceLimits,
) -> BridgeResult<GridFile> {
    if time_index >= file.nt {
        return Err(BridgeError::Wrf(format!(
            "time index {time_index} is outside file with {} times",
            file.nt
        )));
    }
    let expected = checked_wrf_grid_elements(file.nx, file.ny, limits)?;
    let lat = file
        .xlat(time_index)
        .map_err(|error| BridgeError::Wrf(format!("cannot read XLAT: {error}")))?;
    let lon = file
        .xlong(time_index)
        .map_err(|error| BridgeError::Wrf(format!("cannot read XLONG: {error}")))?;
    if lat.len() != expected || lon.len() != expected {
        return Err(BridgeError::Wrf(format!(
            "WRF grid expects {expected} coordinates, got lat {} and lon {}",
            lat.len(),
            lon.len()
        )));
    }
    let projection = match file.global_attr_i32("MAP_PROJ").ok() {
        Some(1) => {
            let truelat1 = file.global_attr_f64("TRUELAT1").ok();
            let stand_lon = file
                .global_attr_f64("STAND_LON")
                .ok()
                .or_else(|| file.global_attr_f64("CEN_LON").ok());
            truelat1
                .zip(stand_lon)
                .map(|(truelat1, stand_lon)| GridProjection::LambertConformal {
                    standard_parallel_1_deg: truelat1,
                    standard_parallel_2_deg: normalized_lambert_second_parallel(
                        truelat1,
                        file.global_attr_f64("TRUELAT2").ok(),
                    ),
                    central_meridian_deg: stand_lon,
                })
        }
        Some(2) => {
            let truelat1 = file.global_attr_f64("TRUELAT1").ok();
            let stand_lon = file
                .global_attr_f64("STAND_LON")
                .ok()
                .or_else(|| file.global_attr_f64("CEN_LON").ok());
            truelat1.zip(stand_lon).map(|(truelat1, stand_lon)| {
                GridProjection::PolarStereographic {
                    true_latitude_deg: truelat1,
                    central_meridian_deg: stand_lon,
                    // WRF/wrf-python choose the stereographic pole from
                    // TRUELAT1, not the nested-domain CEN_LAT.
                    south_pole_on_projection_plane: polar_projection_uses_south_pole(truelat1),
                }
            })
        }
        Some(3) => Some(GridProjection::Mercator {
            latitude_of_true_scale_deg: file.global_attr_f64("TRUELAT1").unwrap_or(0.0),
            // wrf-python uses STAND_LON here and defaults a missing value to
            // zero; CEN_LON is a domain center, not the central meridian.
            central_meridian_deg: file.global_attr_f64("STAND_LON").unwrap_or(0.0),
        }),
        Some(6) => {
            let pole_lat = file.global_attr_f64("POLE_LAT").ok();
            let pole_lon = file.global_attr_f64("POLE_LON").ok();
            // GridProjection has no rotated-pole representation. Returning no
            // native projection is honest and keeps the actual curvilinear
            // XLAT/XLONG grid in control instead of claiming a regular axis.
            is_default_wrf_latlon_pole(pole_lat, pole_lon).then_some(GridProjection::Geographic)
        }
        _ => None,
    };
    let identity = fs::metadata(path)
        .map(|metadata| {
            format!(
                "bytes={};modified={:?}",
                metadata.len(),
                metadata.modified().ok()
            )
        })
        .unwrap_or_else(|_| "metadata-unavailable".to_string());
    let lat = narrow_coordinates(&lat, expected, "XLAT")?;
    let lon = narrow_coordinates(&lon, expected, "XLONG")?;
    Ok(GridFile {
        nx: file.nx,
        ny: file.ny,
        lat,
        lon,
        projection,
        // This grid is ephemeral and not a serialized .rwg. The identity is
        // still deterministic enough for viewer cache invalidation.
        hash: format!("wrf-formula:{};t={time_index};{identity}", path.display()),
    })
}

fn checked_wrf_grid_elements(nx: usize, ny: usize, limits: &ResourceLimits) -> BridgeResult<usize> {
    let shape = GridShape::new(nx, ny)
        .map_err(|error| BridgeError::Wrf(format!("WRF horizontal grid is invalid: {error}")))?;
    let elements = shape
        .checked_len()
        .map_err(|error| BridgeError::Wrf(format!("WRF horizontal grid is invalid: {error}")))?;
    if elements > limits.max_output_elements {
        return Err(BridgeError::Wrf(format!(
            "WRF grid has {elements} cells; active formula ceiling is {}",
            limits.max_output_elements
        )));
    }
    let decoded_bytes = elements
        .checked_mul(std::mem::size_of::<f64>())
        .ok_or_else(|| BridgeError::Wrf("WRF coordinate byte size overflows usize".to_string()))?;
    let display_bytes = elements
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| {
            BridgeError::Wrf("WRF display-coordinate byte size overflows usize".to_string())
        })?;
    if decoded_bytes > limits.max_working_bytes || display_bytes > limits.max_working_bytes {
        return Err(BridgeError::Wrf(format!(
            "one WRF coordinate needs up to {decoded_bytes} decoded bytes and {display_bytes} display bytes; per-allocation limit is {}",
            limits.max_working_bytes
        )));
    }
    let concurrent_bytes = u64::try_from(decoded_bytes)
        .ok()
        .and_then(|decoded| decoded.checked_mul(2))
        .and_then(|decoded| {
            u64::try_from(display_bytes)
                .ok()
                .and_then(|display| display.checked_mul(2))
                .and_then(|display| decoded.checked_add(display))
        })
        .ok_or_else(|| {
            BridgeError::Wrf("WRF coordinate allocation estimate overflows u64".to_string())
        })?;
    if concurrent_bytes > limits.max_total_allocated_bytes {
        return Err(BridgeError::Wrf(format!(
            "WRF latitude/longitude decode and display buffers need up to {concurrent_bytes} concurrent bytes; total allocation limit is {}",
            limits.max_total_allocated_bytes
        )));
    }
    Ok(elements)
}

fn narrow_coordinates(values: &[f64], expected: usize, name: &str) -> BridgeResult<Vec<f32>> {
    let mut narrowed = Vec::new();
    narrowed.try_reserve_exact(expected).map_err(|error| {
        BridgeError::Wrf(format!(
            "cannot reserve {expected} display coordinates for {name}: {error}"
        ))
    })?;
    narrowed.extend(values.iter().copied().map(narrow_coordinate));
    Ok(narrowed)
}

fn polar_projection_uses_south_pole(true_scale_latitude_deg: f64) -> bool {
    true_scale_latitude_deg < 0.0
}

fn normalized_lambert_second_parallel(first: f64, second: Option<f64>) -> f64 {
    if second.is_some_and(|value| value.is_finite() && value.abs() <= 90.0) {
        second.unwrap_or(first)
    } else {
        first
    }
}

fn is_default_wrf_latlon_pole(pole_lat: Option<f64>, pole_lon: Option<f64>) -> bool {
    matches!((pole_lat, pole_lon), (None, None))
        || matches!((pole_lat, pole_lon), (Some(lat), Some(lon)) if lat == 90.0 && lon == 0.0)
}

fn narrow_coordinate(value: f64) -> f32 {
    if value.is_finite() && value <= f32::MAX as f64 && value >= f32::MIN as f64 {
        value as f32
    } else {
        f32::NAN
    }
}

fn validate_store_segment(label: &str, value: &str) -> BridgeResult<()> {
    validate_store_component(label, value).map_err(|error| BridgeError::Store(error.to_string()))
}

fn checked_store_elements(
    nx: usize,
    ny: usize,
    levels: usize,
    name: &str,
    limits: &ResourceLimits,
) -> FormulaResult<StoreFieldAllocation> {
    let elements = nx
        .checked_mul(ny)
        .and_then(|count| count.checked_mul(levels))
        .ok_or_else(|| FormulaError::new(ErrorKind::Limit, "store field shape overflows usize"))?;
    let element_ceiling = MAX_STORE_FIELD_ELEMENTS.min(limits.max_output_elements);
    if elements > element_ceiling {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!(
                "store field '{name}' has {elements} elements; active formula ceiling is {element_ceiling}"
            ),
        ));
    }
    let raw_bytes = elements
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| {
            FormulaError::new(ErrorKind::Limit, "store f32 byte size overflows usize")
        })?;
    let promoted_bytes = elements
        .checked_mul(std::mem::size_of::<f64>())
        .ok_or_else(|| {
            FormulaError::new(ErrorKind::Limit, "store f64 byte size overflows usize")
        })?;
    if raw_bytes > limits.max_working_bytes {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!(
                "store field '{name}' needs a {raw_bytes}-byte f32 decode buffer; per-allocation limit is {}",
                limits.max_working_bytes
            ),
        ));
    }
    if promoted_bytes > limits.max_working_bytes {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!(
                "store field '{name}' needs a {promoted_bytes}-byte f64 resolver buffer; per-allocation limit is {}",
                limits.max_working_bytes
            ),
        ));
    }
    let raw_bytes_u64 = u64::try_from(raw_bytes)
        .map_err(|_| FormulaError::new(ErrorKind::Limit, "store f32 byte size does not fit u64"))?;
    let promoted_bytes_u64 = u64::try_from(promoted_bytes)
        .map_err(|_| FormulaError::new(ErrorKind::Limit, "store f64 byte size does not fit u64"))?;
    let concurrent_bytes = raw_bytes_u64
        .checked_add(promoted_bytes_u64)
        .ok_or_else(|| {
            FormulaError::new(
                ErrorKind::Limit,
                "store decode plus promotion byte estimate overflows u64",
            )
        })?;
    if concurrent_bytes > limits.max_total_allocated_bytes {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!(
                "store field '{name}' needs {concurrent_bytes} concurrent bytes while f32 decode and f64 promotion coexist; total allocation limit is {}",
                limits.max_total_allocated_bytes
            ),
        ));
    }
    Ok(StoreFieldAllocation {
        raw_bytes: raw_bytes_u64,
        promoted_bytes: promoted_bytes_u64,
    })
}

fn reserve_store_allocation<'a>(
    budget: &'a Mutex<StoreAllocationBudget>,
    limits: &ResourceLimits,
    allocation: StoreFieldAllocation,
    name: &str,
) -> FormulaResult<StoreAllocationReservation<'a>> {
    let transient_bytes = allocation
        .raw_bytes
        .checked_add(allocation.promoted_bytes)
        .ok_or_else(|| {
            FormulaError::new(
                ErrorKind::Limit,
                "store decode plus promotion byte estimate overflows u64",
            )
        })?;
    let mut state = budget.lock().map_err(|_| {
        FormulaError::new(
            ErrorKind::Internal,
            "rw-store Formula Lab allocation budget was poisoned",
        )
    })?;
    let projected_bytes = state
        .resident_promoted_bytes
        .checked_add(state.reserved_transient_bytes)
        .and_then(|bytes| bytes.checked_add(transient_bytes))
        .ok_or_else(|| {
            FormulaError::new(
                ErrorKind::Limit,
                "rw-store Formula Lab cumulative allocation estimate overflows u64",
            )
        })?;
    if projected_bytes > limits.max_total_allocated_bytes {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!(
                "store field '{name}' would raise resident plus in-flight resolver storage to {projected_bytes} bytes; total allocation limit is {}",
                limits.max_total_allocated_bytes
            ),
        ));
    }
    state.reserved_transient_bytes = state
        .reserved_transient_bytes
        .checked_add(transient_bytes)
        .ok_or_else(|| {
            FormulaError::new(
                ErrorKind::Limit,
                "rw-store Formula Lab allocation reservation overflows u64",
            )
        })?;
    drop(state);
    Ok(StoreAllocationReservation {
        budget,
        transient_bytes,
        promoted_bytes: allocation.promoted_bytes,
        committed: false,
    })
}

fn validate_manifest(manifest: &RwsRunManifest, model: &str, run: &str) -> BridgeResult<()> {
    if !matches!(manifest.schema.as_str(), SCHEMA_RUN | SCHEMA_RUN_V2) {
        return Err(BridgeError::Store(format!(
            "unsupported run schema '{}' (expected '{SCHEMA_RUN}' or '{SCHEMA_RUN_V2}')",
            manifest.schema,
        )));
    }
    if manifest.model != model || manifest.run != run {
        return Err(BridgeError::Store(format!(
            "run manifest identifies {}/{}, requested {model}/{run}",
            manifest.model, manifest.run
        )));
    }
    if manifest.nx == 0 || manifest.ny == 0 || manifest.grid_hash.trim().is_empty() {
        return Err(BridgeError::Store(
            "run manifest has a degenerate grid identity".to_string(),
        ));
    }
    Ok(())
}

fn reconcile_exact_times(
    manifest: &RwsRunManifest,
    mut supplied: BTreeMap<u16, ExactStoreTime>,
) -> BridgeResult<BTreeMap<u16, ExactStoreTime>> {
    if !manifest.is_exact_time_axis() {
        return Ok(supplied);
    }
    let persisted = manifest.exact_times().collect::<BTreeMap<_, _>>();
    if supplied.is_empty() {
        return persisted
            .into_iter()
            .map(|(slot, time)| {
                Ok((
                    slot,
                    ExactStoreTime::new(
                        exact_lead_seconds_f64(slot, time.lead_seconds)?,
                        Some(manifest_timestep_label(manifest, slot)),
                    ),
                ))
            })
            .collect();
    }
    if supplied.len() != persisted.len() {
        return Err(BridgeError::Store(format!(
            "supplied exact time axis has {} entries, but v2 manifest has {}",
            supplied.len(),
            persisted.len()
        )));
    }
    for (slot, persisted_time) in persisted {
        let supplied_time = supplied.get_mut(&slot).ok_or_else(|| {
            BridgeError::Store(format!(
                "supplied exact time axis is missing storage slot {slot}"
            ))
        })?;
        let persisted_seconds = exact_lead_seconds_f64(slot, persisted_time.lead_seconds)?;
        if supplied_time.seconds != persisted_seconds {
            return Err(BridgeError::Store(format!(
                "supplied exact time for storage slot {slot} ({}) differs from persisted lead time {} seconds",
                supplied_time.seconds, persisted_time.lead_seconds
            )));
        }
    }
    Ok(supplied)
}

fn manifest_timestep_label(manifest: &RwsRunManifest, storage_slot: u16) -> String {
    if let Some(time) = manifest
        .hours
        .get(&storage_slot)
        .and_then(|entry| entry.exact_time())
    {
        return format!(
            "+{}s · Unix {} [storage slot {}]",
            time.lead_seconds, time.valid_unix, storage_slot
        );
    }
    if manifest.is_exact_time_axis() {
        format!("storage slot {storage_slot}")
    } else {
        format!("f{storage_slot:03}")
    }
}

fn time_axis_hash(
    manifest: &RwsRunManifest,
    exact_times: &BTreeMap<u16, ExactStoreTime>,
) -> String {
    let mut hash = Sha256::new();
    hash.update(manifest.schema.as_bytes());
    for (&slot, entry) in &manifest.hours {
        hash.update(slot.to_le_bytes());
        if let Some(time) = entry.exact_time() {
            hash.update([1]);
            hash.update(time.lead_seconds.to_le_bytes());
            hash.update(time.valid_unix.to_le_bytes());
        } else {
            hash.update([0]);
        }
        if let Some(time) = exact_times.get(&slot) {
            hash.update([1]);
            hash.update(time.seconds.to_bits().to_le_bytes());
        } else {
            hash.update([0]);
        }
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn exact_lead_seconds_f64(storage_slot: u16, lead_seconds: u64) -> BridgeResult<f64> {
    if lead_seconds != 0 {
        let significant_bits =
            u64::BITS - lead_seconds.leading_zeros() - lead_seconds.trailing_zeros();
        if significant_bits > f64::MANTISSA_DIGITS {
            return Err(BridgeError::Store(format!(
                "persisted lead time {lead_seconds} seconds for storage slot {storage_slot} cannot be represented exactly for Formula Lab timing"
            )));
        }
    }
    Ok(lead_seconds as f64)
}

fn validate_exact_times(
    hours: &[StoreHour],
    exact_times: &BTreeMap<u16, ExactStoreTime>,
) -> BridgeResult<()> {
    if exact_times.is_empty() {
        return Ok(());
    }
    if exact_times.len() != hours.len() {
        return Err(BridgeError::Store(format!(
            "exact time axis has {} entries, but run has {} hours",
            exact_times.len(),
            hours.len()
        )));
    }
    let mut previous = None;
    for hour in hours {
        let time = exact_times.get(&hour.forecast_hour).ok_or_else(|| {
            BridgeError::Store(format!(
                "exact time axis is missing storage slot {}",
                hour.forecast_hour
            ))
        })?;
        if !time.seconds.is_finite() {
            return Err(BridgeError::Store(format!(
                "exact time for storage slot {} is not finite",
                hour.forecast_hour
            )));
        }
        if let Some(previous) = previous {
            if time.seconds <= previous {
                return Err(BridgeError::Store(format!(
                    "exact times are not strictly increasing at storage slot {}",
                    hour.forecast_hour
                )));
            }
        }
        previous = Some(time.seconds);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_manifest(times: &[(u16, u64, i64)]) -> RwsRunManifest {
        RwsRunManifest {
            schema: SCHEMA_RUN_V2.to_string(),
            model: "wrf".to_string(),
            run: "minute_loop".to_string(),
            grid_hash: "grid".to_string(),
            nx: 2,
            ny: 2,
            hours: times
                .iter()
                .map(|&(slot, lead_seconds, valid_unix)| {
                    (
                        slot,
                        rw_store::run::RwsHourEntry {
                            file: format!("f{slot:03}.rws"),
                            lead_seconds: Some(lead_seconds),
                            valid_unix: Some(valid_unix),
                            written_unix: 0,
                            encode_ms: 0,
                            variables: Vec::new(),
                        },
                    )
                })
                .collect(),
            writer: rw_store::format::RwsWriterInfo {
                name: "test".to_string(),
                version: "0".to_string(),
                build: "test".to_string(),
            },
        }
    }

    #[test]
    fn polar_projection_hemisphere_follows_wrf_true_scale_latitude() {
        assert!(polar_projection_uses_south_pole(-45.0));
        assert!(!polar_projection_uses_south_pole(45.0));
        assert!(!polar_projection_uses_south_pole(0.0));
        assert!(!polar_projection_uses_south_pole(f64::NAN));
    }

    #[test]
    fn projection_compatibility_helpers_fail_closed() {
        assert_eq!(normalized_lambert_second_parallel(30.0, Some(60.0)), 60.0);
        assert_eq!(normalized_lambert_second_parallel(30.0, Some(120.0)), 30.0);
        assert_eq!(
            normalized_lambert_second_parallel(30.0, Some(f64::NAN)),
            30.0
        );
        assert_eq!(normalized_lambert_second_parallel(30.0, None), 30.0);
        assert!(is_default_wrf_latlon_pole(None, None));
        assert!(is_default_wrf_latlon_pole(Some(90.0), Some(0.0)));
        assert!(!is_default_wrf_latlon_pole(Some(45.0), Some(180.0)));
        assert!(!is_default_wrf_latlon_pole(Some(90.0), None));
    }

    #[test]
    fn exact_times_must_be_complete_and_increasing() {
        let hours = vec![
            StoreHour {
                forecast_hour: 0,
                path: PathBuf::from("f000.rws"),
            },
            StoreHour {
                forecast_hour: 3,
                path: PathBuf::from("f003.rws"),
            },
        ];
        let mut times = BTreeMap::new();
        times.insert(0, ExactStoreTime::new(100.0, None));
        assert!(validate_exact_times(&hours, &times).is_err());
        times.insert(3, ExactStoreTime::new(100.0, None));
        assert!(validate_exact_times(&hours, &times).is_err());
        times.insert(3, ExactStoreTime::new(10_900.0, None));
        assert!(validate_exact_times(&hours, &times).is_ok());
    }

    #[test]
    fn persisted_lead_seconds_must_be_exact_in_formula_timing() {
        assert_eq!(exact_lead_seconds_f64(0, 31_680).unwrap(), 31_680.0);
        assert!(exact_lead_seconds_f64(1, (1_u64 << 53) + 1).is_err());
        assert!(exact_lead_seconds_f64(2, (1_u64 << 53) + 2).is_ok());
    }

    #[test]
    fn exact_manifest_reconciliation_uses_leads_and_preserves_ui_labels() {
        let manifest = exact_manifest(&[(0, 31_680, 134_000_000), (1, 31_740, 134_000_060)]);
        let derived = reconcile_exact_times(&manifest, BTreeMap::new()).unwrap();
        assert_eq!(derived[&1].seconds - derived[&0].seconds, 60.0);
        assert!(derived[&0].label.as_deref().unwrap().contains("31680"));

        let mut supplied = BTreeMap::new();
        supplied.insert(
            0,
            ExactStoreTime::new(31_680.0, Some("first label".to_string())),
        );
        supplied.insert(
            1,
            ExactStoreTime::new(31_740.0, Some("second label".to_string())),
        );
        let reconciled = reconcile_exact_times(&manifest, supplied).unwrap();
        assert_eq!(reconciled[&0].label.as_deref(), Some("first label"));

        let mut unix_seconds = reconciled.clone();
        unix_seconds.get_mut(&0).unwrap().seconds = 134_000_000.0;
        assert!(reconcile_exact_times(&manifest, unix_seconds).is_err());

        let mut incomplete = reconciled.clone();
        incomplete.remove(&1);
        assert!(reconcile_exact_times(&manifest, incomplete).is_err());

        let base_hash = time_axis_hash(&manifest, &reconciled);
        let changed_manifest =
            exact_manifest(&[(0, 31_680, 134_000_000), (1, 31_800, 134_000_120)]);
        let changed = reconcile_exact_times(&changed_manifest, BTreeMap::new()).unwrap();
        assert_ne!(base_hash, time_axis_hash(&changed_manifest, &changed));
    }

    #[test]
    fn raw_grid_preflight_rejects_invalid_and_over_limit_shapes() {
        let limits = ResourceLimits::default();
        assert!(checked_wrf_grid_elements(0, 10, &limits).is_err());
        assert!(checked_wrf_grid_elements(usize::MAX, 2, &limits).is_err());
        assert!(checked_wrf_grid_elements(rustwx_core::MAX_GRID_CELLS + 1, 1, &limits).is_err());

        let mut narrow = limits;
        narrow.max_output_elements = 99;
        assert!(
            checked_wrf_grid_elements(10, 10, &narrow)
                .unwrap_err()
                .to_string()
                .contains("active formula ceiling")
        );
    }

    #[test]
    fn store_preflight_accounts_for_decode_and_promotion_before_read() {
        let mut standard = ResourceLimits::default();
        standard.max_output_elements = 64 * 1024 * 1024;
        standard.max_working_bytes = 512 * 1024 * 1024;
        standard.max_total_allocated_bytes = 2 * 1024 * 1024 * 1024;
        let standard_elements = standard.max_output_elements;
        assert!(checked_store_elements(standard_elements, 1, 1, "field", &standard).is_ok());
        assert!(checked_store_elements(standard_elements + 1, 1, 1, "field", &standard).is_err());

        let elements = 1_000usize;
        let mut working = ResourceLimits::default();
        working.max_working_bytes = elements * std::mem::size_of::<f64>() - 1;
        assert!(checked_store_elements(elements, 1, 1, "field", &working).is_err());

        let mut total = ResourceLimits::default();
        total.max_total_allocated_bytes =
            (elements * (std::mem::size_of::<f32>() + std::mem::size_of::<f64>()) - 1) as u64;
        assert!(checked_store_elements(elements, 1, 1, "field", &total).is_err());

        let large = ResourceLimits::default();
        assert!(checked_store_elements(MAX_STORE_FIELD_ELEMENTS, 1, 1, "field", &large).is_ok());
    }

    #[test]
    fn store_resolver_budget_rejects_a_field_that_exceeds_cumulative_residency() {
        let mut limits = ResourceLimits::default();
        limits.max_total_allocated_bytes = 199;
        let allocation = checked_store_elements(10, 1, 1, "first", &limits).unwrap();
        let budget = Mutex::new(StoreAllocationBudget::default());

        reserve_store_allocation(&budget, &limits, allocation, "first")
            .unwrap()
            .commit()
            .unwrap();
        let error = reserve_store_allocation(&budget, &limits, allocation, "second")
            .err()
            .expect("resident f64 bytes plus the next f32/f64 pair must exceed the limit");
        assert!(error.to_string().contains("resident plus in-flight"));
    }
}
