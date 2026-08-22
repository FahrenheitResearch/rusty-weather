use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rw_store::format::RwsVariableMeta;
use rw_store::grid::{GridFile, GridLocator};
use rw_store::reader::HourReader;
use rw_store::run::{MAX_RUN_MANIFEST_BYTES, RwsRunManifest, validate_store_component};
use same_file::Handle;
use sha2::{Digest, Sha256};

use crate::{
    GridPoint, QueryError, QueryLimits, QueryResult, RunDescriptor, SourceProvenance,
    TemporalReducer, TimePoint, TimeRange, VariableCapability,
    parse_legacy_observation_day_origin_unix, parse_legacy_observation_hhmm_slot,
    parse_legacy_run_origin_unix, provider_attributions_for_provenance,
    variable_temporal_capabilities,
};

pub(crate) const DEFAULT_READER_POOL_BYTES: u64 = 64 * 1024 * 1024;
const READER_TILE_CACHE_BYTES: usize = 1024 * 1024;
const MAX_READER_POOL_ENTRIES: usize = 1024;

struct ReaderPoolEntry {
    path: PathBuf,
    reader: Arc<HourReader>,
}

/// Clone-shared, generation-validated reader reuse for point/profile traffic.
/// Temporal grid reducers deliberately bypass this pool because they hold all
/// selected hours while walking tiles and need zero per-reader tile caches.
pub(crate) struct ReaderPool {
    entries: Mutex<VecDeque<ReaderPoolEntry>>,
    max_entries: usize,
    tile_cache_bytes: usize,
}

impl std::fmt::Debug for ReaderPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReaderPool")
            .field("max_entries", &self.max_entries)
            .field("tile_cache_bytes", &self.tile_cache_bytes)
            .finish_non_exhaustive()
    }
}

impl ReaderPool {
    pub(crate) fn new(total_tile_cache_bytes: u64) -> Self {
        let total = usize::try_from(total_tile_cache_bytes).unwrap_or(usize::MAX);
        let (max_entries, tile_cache_bytes) = if total == 0 {
            (0, 0)
        } else if total < READER_TILE_CACHE_BYTES {
            (1, total)
        } else {
            (
                (total / READER_TILE_CACHE_BYTES).min(MAX_READER_POOL_ENTRIES),
                READER_TILE_CACHE_BYTES,
            )
        };
        Self {
            entries: Mutex::new(VecDeque::new()),
            max_entries,
            tile_cache_bytes,
        }
    }

    fn open(&self, path: &Path) -> QueryResult<Arc<HourReader>> {
        if self.max_entries == 0 {
            return Ok(Arc::new(HourReader::open_with_tile_cache_bytes(path, 0)?));
        }

        // Retry a small bounded number of times if an atomic publisher swaps
        // the pathname while we are validating/opening it.
        for _ in 0..4 {
            let cached = {
                let mut entries = self.entries.lock().map_err(|_| {
                    QueryError::InvalidRequest("reader pool lock was poisoned".to_string())
                })?;
                entries
                    .iter()
                    .position(|entry| entry.path == path)
                    .and_then(|index| entries.remove(index))
                    .map(|entry| {
                        let reader = entry.reader.clone();
                        entries.push_back(entry);
                        reader
                    })
            };
            if let Some(reader) = cached {
                if reader.source_matches_path(path).unwrap_or(false) {
                    return Ok(reader);
                }
                self.invalidate_if_same(path, &reader)?;
                continue;
            }

            let candidate = Arc::new(HourReader::open_with_tile_cache_bytes(
                path,
                self.tile_cache_bytes,
            )?);
            if !candidate.source_matches_path(path).unwrap_or(false) {
                continue;
            }

            let winner = {
                let mut entries = self.entries.lock().map_err(|_| {
                    QueryError::InvalidRequest("reader pool lock was poisoned".to_string())
                })?;
                if let Some(index) = entries.iter().position(|entry| entry.path == path) {
                    let entry = entries.remove(index).expect("reader pool index exists");
                    let winner = entry.reader.clone();
                    entries.push_back(entry);
                    winner
                } else {
                    entries.push_back(ReaderPoolEntry {
                        path: path.to_path_buf(),
                        reader: candidate.clone(),
                    });
                    while entries.len() > self.max_entries {
                        entries.pop_front();
                    }
                    candidate
                }
            };
            if winner.source_matches_path(path).unwrap_or(false) {
                return Ok(winner);
            }
            self.invalidate_if_same(path, &winner)?;
        }

        Err(QueryError::InvalidRequest(format!(
            "hour file {} changed repeatedly while opening",
            path.display()
        )))
    }

    fn invalidate_if_same(&self, path: &Path, reader: &Arc<HourReader>) -> QueryResult<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| QueryError::InvalidRequest("reader pool lock was poisoned".to_string()))?;
        if let Some(index) = entries
            .iter()
            .position(|entry| entry.path == path && Arc::ptr_eq(&entry.reader, reader))
        {
            entries.remove(index);
        }
        Ok(())
    }
}

pub struct RunSnapshot {
    store_root: PathBuf,
    run_dir: PathBuf,
    manifest: Arc<RwsRunManifest>,
    grid: Arc<GridFile>,
    locator: GridLocator,
    axis: Arc<Vec<TimePoint>>,
    manifest_digest: [u8; 32],
    manifest_source: Handle,
    descriptor: RunDescriptor,
    limits: QueryLimits,
    reader_pool: Arc<ReaderPool>,
}

impl RunSnapshot {
    pub fn open(store_root: impl AsRef<Path>, model: &str, run: &str) -> QueryResult<Self> {
        Self::open_with_limits(store_root, model, run, QueryLimits::default())
    }

    pub fn open_with_limits(
        store_root: impl AsRef<Path>,
        model: &str,
        run: &str,
        limits: QueryLimits,
    ) -> QueryResult<Self> {
        Self::open_with_pool(
            store_root,
            model,
            run,
            limits,
            Arc::new(ReaderPool::new(DEFAULT_READER_POOL_BYTES)),
        )
    }

    pub(crate) fn open_with_pool(
        store_root: impl AsRef<Path>,
        model: &str,
        run: &str,
        limits: QueryLimits,
        reader_pool: Arc<ReaderPool>,
    ) -> QueryResult<Self> {
        validate_store_component("model", model)?;
        validate_store_component("run", run)?;
        let store_root = store_root.as_ref().to_path_buf();
        let model_dir = store_root.join(model);
        require_real_directory(&model_dir, "model")?;
        let run_dir = model_dir.join(run);
        require_real_directory(&run_dir, "run")?;
        let manifest_path = run_dir.join("run.json");
        require_regular_file(&manifest_path, "run manifest")?;
        let (manifest, manifest_source, manifest_generation) =
            load_manifest_snapshot(&manifest_path, model, run)?;

        if manifest.hours.len() > limits.max_time_points {
            return Err(QueryError::LimitExceeded {
                what: "time points",
                requested: manifest.hours.len(),
                limit: limits.max_time_points,
            });
        }

        let grid_path = run_dir.join("grid.rwg");
        require_regular_file(&grid_path, "grid")?;
        let grid = GridFile::open(&grid_path)?;
        manifest.validate_grid(&grid.hash, grid.nx, grid.ny)?;

        let (axis, origin_unix) = build_time_axis(&manifest)?;
        let manifest_digest = digest_manifest(&manifest)?;
        let mut digest = Sha256::new();
        digest.update(b"rw-query.snapshot.v1\0");
        digest.update(manifest_digest);
        digest.update(manifest_generation);
        digest.update(grid.hash.as_bytes());
        let snapshot_id = format!("{:x}", digest.finalize());
        let source_provenance: Vec<SourceProvenance> = manifest
            .source_provenance()?
            .into_iter()
            .map(SourceProvenance::from)
            .collect();
        let provider_attributions = provider_attributions_for_provenance(&source_provenance);
        let descriptor = RunDescriptor {
            model: manifest.model.clone(),
            run: manifest.run.clone(),
            schema: manifest.schema.clone(),
            snapshot_id,
            grid_hash: grid.hash.clone(),
            nx: grid.nx,
            ny: grid.ny,
            exact_time_axis: manifest.is_exact_time_axis(),
            origin_unix,
            sample_count: axis.len(),
            first_valid_unix: axis.first().map(|time| time.valid_unix),
            last_valid_unix: axis.last().map(|time| time.valid_unix),
            source_provenance,
            provider_attributions,
        };
        let locator = GridLocator::build(&grid);

        Ok(Self {
            store_root,
            run_dir,
            manifest: Arc::new(manifest),
            grid: Arc::new(grid),
            locator,
            axis: Arc::new(axis),
            manifest_digest,
            manifest_source,
            descriptor,
            limits,
            reader_pool,
        })
    }

    pub fn descriptor(&self) -> &RunDescriptor {
        &self.descriptor
    }

    pub fn limits(&self) -> &QueryLimits {
        &self.limits
    }

    pub fn manifest(&self) -> &RwsRunManifest {
        &self.manifest
    }

    pub fn grid(&self) -> &GridFile {
        &self.grid
    }

    pub fn time_axis(&self) -> &[TimePoint] {
        &self.axis
    }

    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    pub fn timepoint(&self, storage_slot: u16) -> QueryResult<TimePoint> {
        self.axis
            .iter()
            .find(|time| time.storage_slot == storage_slot)
            .cloned()
            .ok_or(QueryError::UnknownStorageSlot(storage_slot))
    }

    pub fn select_timepoints(&self, range: TimeRange) -> QueryResult<Vec<TimePoint>> {
        if range
            .start_unix
            .zip(range.end_unix)
            .is_some_and(|(start, end)| start >= end)
        {
            return Err(QueryError::InvalidTimeRange {
                start: range.start_unix,
                end: range.end_unix,
            });
        }
        let selected: Vec<_> = self
            .axis
            .iter()
            .filter(|time| {
                range
                    .start_unix
                    .is_none_or(|start| time.valid_unix >= start)
                    && range.end_unix.is_none_or(|end| time.valid_unix < end)
            })
            .cloned()
            .collect();
        if selected.is_empty() {
            return Err(QueryError::EmptyTimeSelection);
        }
        if selected.len() > self.limits.max_selected_time_points {
            return Err(QueryError::LimitExceeded {
                what: "selected time points",
                requested: selected.len(),
                limit: self.limits.max_selected_time_points,
            });
        }
        Ok(selected)
    }

    pub fn locate_point(&self, latitude: f64, longitude: f64) -> QueryResult<GridPoint> {
        validate_coordinates(latitude, longitude)?;
        let (fx, fy) =
            self.locator
                .locate(latitude, longitude)
                .ok_or(QueryError::PointOutsideGrid {
                    lat: latitude,
                    lon: longitude,
                })?;
        Ok(self.grid_point_from_fractional(latitude, longitude, fx, fy))
    }

    pub fn variable_capabilities(&self) -> QueryResult<Vec<VariableCapability>> {
        struct Building {
            meta: RwsVariableMeta,
            slots: Vec<u16>,
        }

        let mut variables: BTreeMap<String, Building> = BTreeMap::new();
        for time in self.axis.iter() {
            let (reader, path) = self.open_reader(time)?;
            for meta in &reader.meta().variables {
                if let Some(existing) = variables.get_mut(&meta.name) {
                    ensure_compatible(&existing.meta, meta)?;
                    existing.slots.push(time.storage_slot);
                } else {
                    if variables.len() >= self.limits.max_catalog_entries {
                        return Err(QueryError::LimitExceeded {
                            what: "catalog variables",
                            requested: variables.len() + 1,
                            limit: self.limits.max_catalog_entries,
                        });
                    }
                    variables.insert(
                        meta.name.clone(),
                        Building {
                            meta: meta.clone(),
                            slots: vec![time.storage_slot],
                        },
                    );
                }
            }
            self.ensure_source(&reader, &path, time.storage_slot)?;
        }

        let expected = self.axis.len();
        let temporal = variable_temporal_capabilities(
            &variables
                .values()
                .map(|building| building.meta.clone())
                .collect::<Vec<_>>(),
        );
        let capabilities = variables
            .into_values()
            .map(|building| {
                let available = building.slots.len();
                let kind = building.meta.kind.clone();
                let temporal = temporal
                    .get(&building.meta.name)
                    .cloned()
                    .expect("every inventoried variable has a temporal capability");
                VariableCapability {
                    name: building.meta.name,
                    units: building.meta.units,
                    kind: kind.clone(),
                    codec: building.meta.codec,
                    levels_hpa: building.meta.levels_hpa,
                    selector: building.meta.selector,
                    available_slots: building.slots,
                    available_samples: available,
                    expected_samples: expected,
                    coverage: ratio(available, expected),
                    point_series: kind == "surface2d",
                    pressure_profile: kind == "pressure3d",
                    profile_cycle: kind == "pressure3d",
                    geographic_window: matches!(kind.as_str(), "surface2d" | "pressure3d"),
                    scalar_temporal_reduction: temporal
                        .supported_reducers
                        .contains(&TemporalReducer::ScalarSummary),
                    temporal,
                }
            })
            .collect();
        self.ensure_manifest_current()?;
        Ok(capabilities)
    }

    pub(crate) fn locate_fractional(
        &self,
        latitude: f64,
        longitude: f64,
    ) -> QueryResult<(GridPoint, f64, f64)> {
        validate_coordinates(latitude, longitude)?;
        let (fx, fy) =
            self.locator
                .locate(latitude, longitude)
                .ok_or(QueryError::PointOutsideGrid {
                    lat: latitude,
                    lon: longitude,
                })?;
        Ok((
            self.grid_point_from_fractional(latitude, longitude, fx, fy),
            fx,
            fy,
        ))
    }

    pub(crate) fn open_reader(&self, time: &TimePoint) -> QueryResult<(Arc<HourReader>, PathBuf)> {
        let path = self.validated_hour_path(time)?;
        let reader = self.reader_pool.open(&path)?;
        self.validate_reader(time, &reader)?;
        Ok((reader, path))
    }

    /// Open a validated hour with no decoded-tile cache. Temporal grid
    /// reducers keep all selected readers open while walking one spatial tile
    /// across time, so disabling each reader's cache keeps memory bounded by
    /// the current tile rather than `selected_hours * cache_capacity`.
    pub(crate) fn open_reader_uncached(
        &self,
        time: &TimePoint,
    ) -> QueryResult<(HourReader, PathBuf)> {
        let path = self.validated_hour_path(time)?;
        let reader = HourReader::open_with_tile_cache_bytes(&path, 0)?;
        self.validate_reader(time, &reader)?;
        Ok((reader, path))
    }

    fn validated_hour_path(&self, time: &TimePoint) -> QueryResult<PathBuf> {
        let entry = self
            .manifest
            .hours
            .get(&time.storage_slot)
            .ok_or(QueryError::UnknownStorageSlot(time.storage_slot))?;
        if entry.file != time.file {
            return Err(QueryError::SnapshotInvalidated {
                slot: time.storage_slot,
            });
        }
        let path = self.run_dir.join(&time.file);
        require_regular_file(&path, "hour")?;
        Ok(path)
    }

    fn validate_reader(&self, time: &TimePoint, reader: &HourReader) -> QueryResult<()> {
        let expected_entry = self
            .manifest
            .validate_hour_meta(time.storage_slot, reader.meta())?;
        let actual_variables: Vec<_> = reader
            .meta()
            .variables
            .iter()
            .map(|variable| variable.name.clone())
            .collect();
        if expected_entry.variables != actual_variables {
            return Err(QueryError::VariableInventoryMismatch {
                slot: time.storage_slot,
            });
        }
        Ok(())
    }

    pub(crate) fn ensure_source(
        &self,
        reader: &HourReader,
        path: &Path,
        slot: u16,
    ) -> QueryResult<()> {
        if !reader.source_matches_path(path)? {
            return Err(QueryError::SnapshotInvalidated { slot });
        }
        Ok(())
    }

    pub(crate) fn ensure_manifest_current(&self) -> QueryResult<()> {
        let path = self.run_dir.join("run.json");
        require_regular_file(&path, "run manifest").map_err(|_| QueryError::ManifestInvalidated)?;
        let (current, current_source, _) =
            load_manifest_snapshot(&path, &self.descriptor.model, &self.descriptor.run)
                .map_err(|_| QueryError::ManifestInvalidated)?;
        if current_source != self.manifest_source {
            return Err(QueryError::ManifestInvalidated);
        }
        let current_digest =
            digest_manifest(&current).map_err(|_| QueryError::ManifestInvalidated)?;
        if current_digest != self.manifest_digest {
            return Err(QueryError::ManifestInvalidated);
        }
        Ok(())
    }

    fn grid_point_from_fractional(
        &self,
        latitude: f64,
        longitude: f64,
        fx: f64,
        fy: f64,
    ) -> GridPoint {
        let x = (fx.round() as usize).min(self.grid.nx - 1);
        let y = (fy.round() as usize).min(self.grid.ny - 1);
        let index = y * self.grid.nx + x;
        GridPoint {
            requested_latitude: latitude,
            requested_longitude: longitude,
            x,
            y,
            grid_latitude: self.grid.lat[index],
            grid_longitude: self.grid.lon[index],
        }
    }
}

/// Open, parse, and validate the exact manifest file object used to define a
/// snapshot. Atomic publishers may replace run.json under the same pathname;
/// retaining and hashing its file identity makes response-cache keys change
/// even when the replacement serializes to identical JSON.
fn load_manifest_snapshot(
    path: &Path,
    model: &str,
    run: &str,
) -> QueryResult<(RwsRunManifest, Handle, [u8; 32])> {
    for _ in 0..4 {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(QueryError::InvalidRequest(format!(
                "run manifest {} must be a regular file",
                path.display()
            )));
        }
        if metadata.len() > MAX_RUN_MANIFEST_BYTES {
            return Err(QueryError::InvalidRequest(format!(
                "run manifest {} is {} bytes; limit is {MAX_RUN_MANIFEST_BYTES} bytes",
                path.display(),
                metadata.len()
            )));
        }

        let source = Handle::from_file(file.try_clone()?)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.by_ref()
            .take(MAX_RUN_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RUN_MANIFEST_BYTES {
            return Err(QueryError::InvalidRequest(format!(
                "run manifest {} grew beyond the {MAX_RUN_MANIFEST_BYTES}-byte limit while reading",
                path.display()
            )));
        }

        let manifest: RwsRunManifest = serde_json::from_slice(&bytes)?;
        manifest.validate_contents()?;
        manifest.validate_identity(model, run)?;
        let current = match Handle::from_path(path) {
            Ok(current) => current,
            Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound) => continue,
            Err(error) => return Err(error.into()),
        };
        if current == source {
            let generation = digest_file_identity(&source);
            return Ok((manifest, source, generation));
        }
    }

    Err(QueryError::InvalidRequest(format!(
        "run manifest {} changed repeatedly while opening",
        path.display()
    )))
}

/// Feed the platform file identity into SHA-256 without relying on the
/// standard library's intentionally unspecified DefaultHasher algorithm.
fn digest_file_identity(source: &Handle) -> [u8; 32] {
    struct IdentityHasher(Sha256);

    impl Hasher for IdentityHasher {
        fn finish(&self) -> u64 {
            let digest = self.0.clone().finalize();
            u64::from_le_bytes(
                digest[..8]
                    .try_into()
                    .expect("SHA-256 prefix is eight bytes"),
            )
        }

        fn write(&mut self, bytes: &[u8]) {
            self.0.update((bytes.len() as u64).to_le_bytes());
            self.0.update(bytes);
        }
    }

    let mut hasher = IdentityHasher(Sha256::new());
    source.hash(&mut hasher);
    hasher.0.finalize().into()
}

fn digest_manifest(manifest: &RwsRunManifest) -> QueryResult<[u8; 32]> {
    let bytes = serde_json::to_vec(manifest)?;
    let mut digest = Sha256::new();
    digest.update(b"rw-query.manifest.v1\0");
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
    Ok(digest.finalize().into())
}

pub(crate) fn ensure_compatible(
    expected: &RwsVariableMeta,
    actual: &RwsVariableMeta,
) -> QueryResult<()> {
    if expected.name != actual.name
        || expected.units != actual.units
        || expected.kind != actual.kind
        || expected.codec != actual.codec
        || expected.levels_hpa != actual.levels_hpa
        || expected.selector != actual.selector
    {
        return Err(QueryError::InconsistentVariable {
            variable: expected.name.clone(),
            detail: format!("expected {expected:?}, found {actual:?}"),
        });
    }
    Ok(())
}

pub(crate) fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn build_time_axis(manifest: &RwsRunManifest) -> QueryResult<(Vec<TimePoint>, Option<i64>)> {
    if manifest.is_exact_time_axis() {
        let mut origin = None;
        let axis = manifest
            .hours
            .iter()
            .map(|(&storage_slot, entry)| {
                let exact = entry.exact_time().ok_or_else(|| {
                    QueryError::InvalidRequest(format!(
                        "exact-time slot {storage_slot} lacks physical timing"
                    ))
                })?;
                origin.get_or_insert_with(|| {
                    exact
                        .origin_unix()
                        .expect("validated manifest has representable exact-time origin")
                });
                Ok(TimePoint {
                    storage_slot,
                    lead_seconds: exact.lead_seconds,
                    valid_unix: exact.valid_unix,
                    file: entry.file.clone(),
                })
            })
            .collect::<QueryResult<Vec<_>>>()?;
        return Ok((axis, origin));
    }

    if let Ok(origin) = parse_legacy_run_origin_unix(&manifest.run) {
        let axis = manifest
            .hours
            .iter()
            .map(|(&storage_slot, entry)| {
                let lead_seconds = u64::from(storage_slot) * 3_600;
                let valid_unix = origin.checked_add(lead_seconds as i64).ok_or_else(|| {
                    QueryError::InvalidLegacyRunSlug {
                        run: manifest.run.clone(),
                        reason: format!("slot {storage_slot} valid time overflows i64"),
                    }
                })?;
                Ok(TimePoint {
                    storage_slot,
                    lead_seconds,
                    valid_unix,
                    file: entry.file.clone(),
                })
            })
            .collect::<QueryResult<Vec<_>>>()?;
        return Ok((axis, Some(origin)));
    }

    // Legacy rw-sat and SimSat stores predate exact-time v2. They encode one
    // real UTC day in the run name and HHMM in both the map key and tHHMM.rws
    // filename. Require the complete shape before interpreting a key as time;
    // arbitrary v1 model runs still fail rather than receiving a guessed axis.
    let origin = parse_legacy_observation_day_origin_unix(&manifest.run)?;
    let axis = manifest
        .hours
        .iter()
        .map(|(&storage_slot, entry)| {
            let expected_file = format!("t{storage_slot:04}.rws");
            if entry.file != expected_file {
                return Err(QueryError::InvalidLegacyRunSlug {
                    run: manifest.run.clone(),
                    reason: format!(
                        "observation slot {storage_slot} file '{}' must be '{expected_file}'",
                        entry.file
                    ),
                });
            }
            let lead_seconds = parse_legacy_observation_hhmm_slot(&manifest.run, storage_slot)?;
            let valid_unix = origin.checked_add(lead_seconds as i64).ok_or_else(|| {
                QueryError::InvalidLegacyRunSlug {
                    run: manifest.run.clone(),
                    reason: format!("slot {storage_slot} valid time overflows i64"),
                }
            })?;
            Ok(TimePoint {
                storage_slot,
                lead_seconds,
                valid_unix,
                file: entry.file.clone(),
            })
        })
        .collect::<QueryResult<Vec<_>>>()?;
    Ok((axis, Some(origin)))
}

fn require_real_directory(path: &Path, label: &str) -> QueryResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(QueryError::InvalidRequest(format!(
            "{label} path {} must be a real directory, not a symlink",
            path.display()
        )));
    }
    Ok(())
}

fn require_regular_file(path: &Path, label: &str) -> QueryResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(QueryError::InvalidRequest(format!(
            "{label} path {} must be a regular file, not a symlink",
            path.display()
        )));
    }
    Ok(())
}

fn validate_coordinates(latitude: f64, longitude: f64) -> QueryResult<()> {
    if !latitude.is_finite() || !longitude.is_finite() {
        return Err(QueryError::InvalidRequest(
            "latitude and longitude must be finite".to_string(),
        ));
    }
    if !(-90.0..=90.0).contains(&latitude) {
        return Err(QueryError::InvalidRequest(format!(
            "latitude {latitude} is outside -90..=90 degrees"
        )));
    }
    if !(-180.0..=180.0).contains(&longitude) {
        return Err(QueryError::InvalidRequest(format!(
            "longitude {longitude} is outside -180..=180 degrees"
        )));
    }
    Ok(())
}
