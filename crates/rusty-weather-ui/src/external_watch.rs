// SPDX-License-Identifier: Apache-2.0

//! Producer-independent live `wrfout` directory watching.
//!
//! This module deliberately owns no egui widgets and launches no model.  A UI
//! can keep an [`ExternalWatchSession`], call [`ExternalWatchSession::poll`] on
//! a timer, enqueue every returned [`SimulationOutput`], and mirror its public
//! settings/status into controls.  The first valid, one-record `wrfout` fixes
//! the run origin and grid contract.  Later files must match that contract.
//!
//! # Current scope
//!
//! Whatever writes the directory is out of scope here, on purpose.  Stock WRF
//! and any other model that emits `frames_per_outfile=1` WRF output are
//! equally valid producers.  This app contains no preprocessing frontend and
//! no forecast-runtime integration, so there is nothing to launch: the watched
//! run is always started by the user, outside the app.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rw_sim::StableWrfoutWatcher;
use rw_store::RwsExactTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wrf_core::{WrfError, WrfFile};

use crate::wrf_process::WrfProcessOptions;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(750);
const CASE_SCHEMA: &[u8] = b"rusty-weather-external-watch-case-v1";

/// One accepted publication, ready for the app's ordinary WRF processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationOutput {
    pub path: PathBuf,
    pub case_sha256: String,
    pub storage_slot: u16,
    pub exact_time: RwsExactTime,
}

/// How a stable, readable WRF file proves that its producer has closed it.
///
/// Stock WRF publishes no completion attribute, so the stability window plus a
/// complete metadata read is all the evidence that exists.  Some model runners
/// do write an integer "this file is finished" global attribute; when one
/// does, its name goes in [`ExternalWatchSettings::completion_attribute`] and
/// it becomes authoritative.  No attribute name is hardcoded here, because no
/// particular producer is assumed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCompletionMode {
    /// Honour the configured completion attribute when the producer writes
    /// one, and fall back to the stability window when it is absent.
    #[default]
    Auto,
    /// Rely on the stable-file window plus a complete WRF metadata read, and
    /// ignore any completion attribute the producer may write.
    StockWrf,
    /// Require the configured completion attribute to be present and equal
    /// one, in addition to the stability window.
    RequireMarker,
}

impl ExternalCompletionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::StockWrf => "Stock WRF",
            Self::RequireMarker => "Require completion attribute",
        }
    }

    /// Whether this mode ever reads the configured completion attribute.
    fn reads_attribute(self) -> bool {
        !matches!(self, Self::StockWrf)
    }
}

/// Processing work requested after a frame passes the watch contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalProcessingMode {
    /// Core map fields and isobaric sounding volumes, without the expensive
    /// severe-diagnostic and raw-extra suites.
    QuickLook,
    /// The normal complete WRF processing profile.
    Full,
}

impl Default for ExternalProcessingMode {
    fn default() -> Self {
        Self::QuickLook
    }
}

impl ExternalProcessingMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::QuickLook => "Quick look + soundings",
            Self::Full => "Full diagnostics",
        }
    }

    /// Translate the watch choice to the existing WRF processing engine.
    pub fn wrf_options(self) -> WrfProcessOptions {
        match self {
            Self::QuickLook => WrfProcessOptions {
                core_fields: true,
                diagnostics: false,
                heavy_ecape: false,
                raw_extras: false,
                only: Vec::new(),
                skip: Vec::new(),
            },
            Self::Full => WrfProcessOptions::default(),
        }
    }
}

/// Persistable controls for an externally launched simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalWatchSettings {
    pub root: PathBuf,
    /// Exact WRF domain token, such as `d01` or `d04`.
    pub domain: String,
    pub completion_mode: ExternalCompletionMode,
    /// Integer WRF global attribute a producer sets to one when it has closed
    /// a file, or empty when the producer publishes no such marker.  Only
    /// [`ExternalCompletionMode::Auto`] and
    /// [`ExternalCompletionMode::RequireMarker`] read it.
    pub completion_attribute: String,
    /// Expected output cadence.  Valid times off this cadence are rejected.
    pub cadence_seconds: u32,
    pub processing_mode: ExternalProcessingMode,
    /// Host hint: select each newly processed exact time while this is true.
    pub follow_newest: bool,
}

impl Default for ExternalWatchSettings {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            domain: "d01".to_string(),
            completion_mode: ExternalCompletionMode::Auto,
            completion_attribute: String::new(),
            cadence_seconds: 3_600,
            processing_mode: ExternalProcessingMode::QuickLook,
            follow_newest: true,
        }
    }
}

/// Immutable identity established by the first accepted frame.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalWatchCase {
    pub case_sha256: String,
    pub canonical_root: PathBuf,
    pub domain: String,
    pub origin_unix: i64,
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub dx_m: f64,
    pub dy_m: f64,
    pub grid_sha256: String,
    pub projection_sha256: String,
}

/// Small state surface intended for an egui status row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalWatchStatus {
    pub active: bool,
    pub message: String,
    /// Frames emitted by this session, including a changed path re-publication.
    pub emitted_frames: u64,
    /// Transient candidate publications withdrawn so they must pass stability again.
    pub retry_count: u64,
    /// Structurally invalid publications quarantined until their file signature changes.
    pub rejected_count: u64,
    /// Durable explanation for the most recently quarantined publication.
    pub last_rejection: Option<String>,
    /// Latest queue depth supplied by the host.
    pub backlog: usize,
    /// Latest active processor count supplied by the host.
    pub processing: usize,
    pub processed_frames: u64,
    pub processing_failures: u64,
}

impl ExternalWatchStatus {
    fn watching(root: &Path, domain: &str) -> Self {
        Self {
            active: true,
            message: format!("Watching {} for {domain} wrfout files", root.display()),
            emitted_frames: 0,
            retry_count: 0,
            rejected_count: 0,
            last_rejection: None,
            backlog: 0,
            processing: 0,
            processed_frames: 0,
            processing_failures: 0,
        }
    }

    fn record_rejection(&mut self, message: String) {
        self.rejected_count = self.rejected_count.saturating_add(1);
        self.last_rejection = Some(message);
    }

    fn watching_message(&self, root: &Path, domain: &str) -> String {
        format!(
            "Watching {} for {domain} output; {} queued, {} processing",
            root.display(),
            self.backlog,
            self.processing
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FrameMetadata {
    origin_unix: i64,
    valid_unix: i64,
    nx: usize,
    ny: usize,
    nz: usize,
    dx_m: f64,
    dy_m: f64,
    grid_sha256: [u8; 32],
    projection_sha256: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ProjectionFingerprint {
    None,
    Geographic,
    LambertConformal {
        standard_parallel_1_deg: f64,
        standard_parallel_2_deg: f64,
        central_meridian_deg: f64,
    },
    PolarStereographic {
        true_latitude_deg: f64,
        central_meridian_deg: f64,
        south_pole_on_projection_plane: bool,
    },
    Mercator {
        latitude_of_true_scale_deg: f64,
        central_meridian_deg: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublishedFrame {
    storage_slot: u16,
    valid_unix: i64,
}

#[derive(Debug)]
enum CandidateValidationError {
    Transient(String),
    Rejected(String),
}

impl CandidateValidationError {
    fn message(&self) -> &str {
        match self {
            Self::Transient(message) | Self::Rejected(message) => message,
        }
    }

    fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Stateful poller for a directory written by a process outside the app.
#[derive(Debug)]
pub struct ExternalWatchSession {
    settings: ExternalWatchSettings,
    canonical_root: PathBuf,
    watcher: StableWrfoutWatcher,
    case: Option<ExternalWatchCase>,
    published_paths: BTreeMap<PathBuf, PublishedFrame>,
    published_slots: BTreeMap<u16, PathBuf>,
    status: ExternalWatchStatus,
}

impl ExternalWatchSession {
    /// Resolve the watched root once and begin a new independent session.
    pub fn start(mut settings: ExternalWatchSettings) -> Result<Self, String> {
        settings.domain = normalize_domain(&settings.domain)?;
        settings.completion_attribute =
            normalize_completion_attribute(&settings.completion_attribute)?;
        if settings.completion_mode == ExternalCompletionMode::RequireMarker
            && settings.completion_attribute.is_empty()
        {
            return Err(
                "requiring a completion attribute needs the attribute name the producer writes"
                    .to_string(),
            );
        }
        if settings.cadence_seconds == 0 {
            return Err("watch output cadence must be greater than zero".to_string());
        }
        if settings.root.as_os_str().is_empty() {
            return Err("choose a simulation output directory to watch".to_string());
        }
        let canonical_root = fs::canonicalize(&settings.root).map_err(|error| {
            format!(
                "could not canonicalize watched output directory {}: {error}",
                settings.root.display()
            )
        })?;
        if !canonical_root.is_dir() {
            return Err(format!(
                "watched output path is not a directory: {}",
                canonical_root.display()
            ));
        }
        settings.root = canonical_root.clone();
        let status = ExternalWatchStatus::watching(&canonical_root, &settings.domain);
        Ok(Self {
            settings,
            canonical_root,
            watcher: StableWrfoutWatcher::default(),
            case: None,
            published_paths: BTreeMap::new(),
            published_slots: BTreeMap::new(),
            status,
        })
    }

    pub fn settings(&self) -> &ExternalWatchSettings {
        &self.settings
    }

    pub fn case(&self) -> Option<&ExternalWatchCase> {
        self.case.as_ref()
    }

    pub fn status(&self) -> &ExternalWatchStatus {
        &self.status
    }

    pub fn poll_interval(&self) -> Duration {
        DEFAULT_POLL_INTERVAL
    }

    /// The host owns the processing queue; reflect its current pressure here.
    pub fn set_backlog(&mut self, queued: usize, processing: usize) {
        self.status.backlog = queued;
        self.status.processing = processing;
    }

    pub fn record_processing_result(&mut self, success: bool) {
        if success {
            self.status.processed_frames = self.status.processed_frames.saturating_add(1);
        } else {
            self.status.processing_failures = self.status.processing_failures.saturating_add(1);
        }
    }

    pub fn stop(&mut self) {
        self.status.active = false;
        self.status.message = format!(
            "Stopped watching {} after {} frame(s)",
            self.canonical_root.display(),
            self.status.emitted_frames
        );
    }

    /// Poll once.  Only stable, readable, one-record files for the configured
    /// domain are returned.  A rejected publication is explicitly withdrawn
    /// from the lower-level watcher and must satisfy its stability window again.
    pub fn poll(&mut self) -> Result<Vec<SimulationOutput>, String> {
        if !self.status.active {
            return Ok(Vec::new());
        }
        let candidates = self.watcher.scan(&self.canonical_root).map_err(|error| {
            let message = format!(
                "could not scan watched output directory {}: {error}",
                self.canonical_root.display()
            );
            self.status.message = message.clone();
            message
        })?;
        let mut ready = Vec::new();
        let mut last_rejection = None;
        for path in candidates {
            if !path_matches_domain(&path, &self.settings.domain) {
                continue;
            }
            match self.validate_candidate(&path) {
                Ok(output) => {
                    self.status.emitted_frames = self.status.emitted_frames.saturating_add(1);
                    ready.push(output);
                }
                Err(error) => {
                    let disposition = if error.is_transient() {
                        self.watcher.retry(&path);
                        self.status.retry_count = self.status.retry_count.saturating_add(1);
                        "is not ready"
                    } else {
                        "was rejected until it changes"
                    };
                    let message = format!("{} {disposition}: {}", path.display(), error.message());
                    if !error.is_transient() {
                        self.status.record_rejection(message.clone());
                    }
                    last_rejection = Some(message);
                }
            }
        }
        self.status.message = if let Some(error) = last_rejection {
            error
        } else if ready.is_empty() {
            self.status
                .watching_message(&self.canonical_root, &self.settings.domain)
        } else {
            format!(
                "Accepted {} new {} frame(s); {} queued, {} processing",
                ready.len(),
                self.settings.domain,
                self.status.backlog.saturating_add(ready.len()),
                self.status.processing
            )
        };
        Ok(ready)
    }

    fn validate_candidate(
        &mut self,
        path: &Path,
    ) -> Result<SimulationOutput, CandidateValidationError> {
        let file = WrfFile::open(path).map_err(|error| {
            CandidateValidationError::Transient(format!("WRF-shaped NetCDF open failed: {error}"))
        })?;
        validate_completion_marker(
            &file,
            self.settings.completion_mode,
            &self.settings.completion_attribute,
        )
        .map_err(CandidateValidationError::Transient)?;
        let time_axis = crate::local_import::wrf_source_times(&file, path)
            .map_err(CandidateValidationError::Rejected)?;
        if time_axis.records.len() != 1 {
            return Err(CandidateValidationError::Rejected(format!(
                "watch-existing-run currently requires one time record per file, found {}",
                time_axis.records.len()
            )));
        }
        let origin_unix = time_axis.reference_unix.ok_or_else(|| {
            CandidateValidationError::Rejected(
                "WRF output has no model initialization time".to_string(),
            )
        })?;
        let grid_sha256 = coordinate_sha256(&file, file.nx, file.ny)?;
        let projection_sha256 = projection_sha256(projection_fingerprint(&file)?);
        let metadata = FrameMetadata {
            origin_unix,
            valid_unix: time_axis.records[0].valid_unix,
            nx: file.nx,
            ny: file.ny,
            nz: file.nz,
            dx_m: file.dx,
            dy_m: file.dy,
            grid_sha256,
            projection_sha256,
        };
        validate_metadata(metadata).map_err(CandidateValidationError::Rejected)?;

        let case = match self.case.as_ref() {
            Some(case) => {
                validate_case_consistency(case, metadata)
                    .map_err(CandidateValidationError::Rejected)?;
                case.clone()
            }
            None => ExternalWatchCase::new(&self.canonical_root, &self.settings.domain, metadata),
        };
        let (storage_slot, exact_time) = frame_target(metadata, self.settings.cadence_seconds)
            .map_err(CandidateValidationError::Rejected)?;

        if let Some(prior) = self.published_paths.get(path)
            && (prior.storage_slot != storage_slot || prior.valid_unix != metadata.valid_unix)
        {
            return Err(CandidateValidationError::Rejected(format!(
                "a previously published path changed physical time from slot {} / {} to slot {} / {}",
                prior.storage_slot, prior.valid_unix, storage_slot, metadata.valid_unix
            )));
        }
        if let Some(prior_path) = self.published_slots.get(&storage_slot)
            && prior_path != path
        {
            return Err(CandidateValidationError::Rejected(format!(
                "cadence slot {storage_slot} was already published by {}",
                prior_path.display()
            )));
        }

        if self.case.is_none() {
            self.case = Some(case.clone());
        }
        self.published_paths.insert(
            path.to_path_buf(),
            PublishedFrame {
                storage_slot,
                valid_unix: metadata.valid_unix,
            },
        );
        self.published_slots
            .insert(storage_slot, path.to_path_buf());
        Ok(SimulationOutput {
            path: path.to_path_buf(),
            case_sha256: case.case_sha256,
            storage_slot,
            exact_time,
        })
    }
}

impl ExternalWatchCase {
    fn new(canonical_root: &Path, domain: &str, metadata: FrameMetadata) -> Self {
        Self {
            case_sha256: case_sha256(canonical_root, domain, metadata),
            canonical_root: canonical_root.to_path_buf(),
            domain: domain.to_string(),
            origin_unix: metadata.origin_unix,
            nx: metadata.nx,
            ny: metadata.ny,
            nz: metadata.nz,
            dx_m: metadata.dx_m,
            dy_m: metadata.dy_m,
            grid_sha256: digest_hex(&metadata.grid_sha256),
            projection_sha256: digest_hex(&metadata.projection_sha256),
        }
    }
}

fn coordinate_sha256(
    file: &WrfFile,
    nx: usize,
    ny: usize,
) -> Result<[u8; 32], CandidateValidationError> {
    let lat = file.xlat(0).map_err(|error| {
        CandidateValidationError::Rejected(format!("XLAT could not be read safely: {error}"))
    })?;
    let lon = file.xlong(0).map_err(|error| {
        CandidateValidationError::Rejected(format!("XLONG could not be read safely: {error}"))
    })?;
    let cells = nx.checked_mul(ny).ok_or_else(|| {
        CandidateValidationError::Rejected("grid cell count overflows usize".to_string())
    })?;
    if lat.len() != cells || lon.len() != cells {
        return Err(CandidateValidationError::Rejected(format!(
            "coordinate grid has XLAT/XLONG lengths {}/{}, expected {cells}",
            lat.len(),
            lon.len()
        )));
    }
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"rusty-weather-wrf-coordinate-grid-v1");
    hash_part(&mut hasher, &(nx as u64).to_le_bytes());
    hash_part(&mut hasher, &(ny as u64).to_le_bytes());
    for values in [&lat, &lon] {
        for value in values.iter() {
            let canonical = *value as f32;
            if !canonical.is_finite() {
                return Err(CandidateValidationError::Rejected(
                    "coordinate grid contains a non-finite value".to_string(),
                ));
            }
            hasher.update(canonical.to_bits().to_le_bytes());
        }
    }
    Ok(hasher.finalize().into())
}

fn projection_fingerprint(
    file: &WrfFile,
) -> Result<ProjectionFingerprint, CandidateValidationError> {
    let Some(map_proj) = optional_attr_i32(file, "MAP_PROJ")? else {
        return Ok(ProjectionFingerprint::None);
    };
    let projection = match map_proj {
        1 => {
            let Some(truelat1) = optional_finite_attr_f64(file, "TRUELAT1")? else {
                return Ok(ProjectionFingerprint::None);
            };
            let truelat2 = crate::local_import::normalize_lambert_truelat2(
                truelat1,
                optional_finite_attr_f64(file, "TRUELAT2")?,
            );
            let stand_lon = match optional_finite_attr_f64(file, "STAND_LON")? {
                Some(value) => Some(value),
                None => optional_finite_attr_f64(file, "CEN_LON")?,
            };
            let Some(stand_lon) = stand_lon else {
                return Ok(ProjectionFingerprint::None);
            };
            ProjectionFingerprint::LambertConformal {
                standard_parallel_1_deg: truelat1,
                standard_parallel_2_deg: truelat2,
                central_meridian_deg: stand_lon,
            }
        }
        2 => {
            let Some(truelat1) = optional_finite_attr_f64(file, "TRUELAT1")? else {
                return Ok(ProjectionFingerprint::None);
            };
            let stand_lon = match optional_finite_attr_f64(file, "STAND_LON")? {
                Some(value) => Some(value),
                None => optional_finite_attr_f64(file, "CEN_LON")?,
            };
            let Some(stand_lon) = stand_lon else {
                return Ok(ProjectionFingerprint::None);
            };
            ProjectionFingerprint::PolarStereographic {
                true_latitude_deg: truelat1,
                central_meridian_deg: stand_lon,
                south_pole_on_projection_plane: crate::local_import::wrf_polar_uses_south_pole(
                    truelat1,
                ),
            }
        }
        3 => ProjectionFingerprint::Mercator {
            latitude_of_true_scale_deg: optional_finite_attr_f64(file, "TRUELAT1")?.unwrap_or(0.0),
            central_meridian_deg: crate::local_import::wrf_mercator_central_longitude(
                optional_finite_attr_f64(file, "STAND_LON")?,
            ),
        },
        6 if crate::local_import::wrf_latlon_is_unrotated(
            optional_finite_attr_f64(file, "POLE_LAT")?,
            optional_finite_attr_f64(file, "POLE_LON")?,
        ) =>
        {
            ProjectionFingerprint::Geographic
        }
        _ => ProjectionFingerprint::None,
    };
    Ok(projection)
}

fn optional_attr_i32(file: &WrfFile, name: &str) -> Result<Option<i32>, CandidateValidationError> {
    match file.global_attr_i32(name) {
        Ok(value) => Ok(Some(value)),
        Err(WrfError::AttrNotFound(_)) => Ok(None),
        Err(error) => Err(CandidateValidationError::Rejected(format!(
            "projection attribute {name} could not be read safely: {error}"
        ))),
    }
}

fn optional_finite_attr_f64(
    file: &WrfFile,
    name: &str,
) -> Result<Option<f64>, CandidateValidationError> {
    match file.global_attr_f64(name) {
        Ok(value) if value.is_finite() => Ok(Some(value)),
        Ok(value) => Err(CandidateValidationError::Rejected(format!(
            "projection attribute {name} must be finite, found {value}"
        ))),
        Err(WrfError::AttrNotFound(_)) => Ok(None),
        Err(error) => Err(CandidateValidationError::Rejected(format!(
            "projection attribute {name} could not be read safely: {error}"
        ))),
    }
}

fn projection_sha256(projection: ProjectionFingerprint) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"rusty-weather-wrf-projection-v1");
    match projection {
        ProjectionFingerprint::None => hash_part(&mut hasher, b"none"),
        ProjectionFingerprint::Geographic => hash_part(&mut hasher, b"geographic"),
        ProjectionFingerprint::LambertConformal {
            standard_parallel_1_deg,
            standard_parallel_2_deg,
            central_meridian_deg,
        } => {
            hash_part(&mut hasher, b"lambert_conformal");
            hash_part(
                &mut hasher,
                &standard_parallel_1_deg.to_bits().to_le_bytes(),
            );
            hash_part(
                &mut hasher,
                &standard_parallel_2_deg.to_bits().to_le_bytes(),
            );
            hash_part(&mut hasher, &central_meridian_deg.to_bits().to_le_bytes());
        }
        ProjectionFingerprint::PolarStereographic {
            true_latitude_deg,
            central_meridian_deg,
            south_pole_on_projection_plane,
        } => {
            hash_part(&mut hasher, b"polar_stereographic");
            hash_part(&mut hasher, &true_latitude_deg.to_bits().to_le_bytes());
            hash_part(&mut hasher, &central_meridian_deg.to_bits().to_le_bytes());
            hash_part(&mut hasher, &[u8::from(south_pole_on_projection_plane)]);
        }
        ProjectionFingerprint::Mercator {
            latitude_of_true_scale_deg,
            central_meridian_deg,
        } => {
            hash_part(&mut hasher, b"mercator");
            hash_part(
                &mut hasher,
                &latitude_of_true_scale_deg.to_bits().to_le_bytes(),
            );
            hash_part(&mut hasher, &central_meridian_deg.to_bits().to_le_bytes());
        }
    }
    hasher.finalize().into()
}

/// A completion attribute is any integer WRF global attribute a producer sets
/// to one once it has closed the file.  Restricting the name keeps a typo from
/// silently probing an unrelated attribute.
fn normalize_completion_attribute(value: &str) -> Result<String, String> {
    let normalized = value.trim();
    if normalized.is_empty() {
        return Ok(String::new());
    }
    if normalized.len() > 128
        || !normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(
            "completion attribute must be up to 128 ASCII letters, digits, or underscores"
                .to_string(),
        );
    }
    Ok(normalized.to_string())
}

fn validate_completion_marker(
    file: &WrfFile,
    mode: ExternalCompletionMode,
    attribute: &str,
) -> Result<(), String> {
    if !mode.reads_attribute() || attribute.is_empty() {
        return Ok(());
    }
    match file.global_attr_i32(attribute) {
        Ok(1) => Ok(()),
        Ok(value) => Err(format!("{attribute} is {value}, expected 1")),
        Err(WrfError::AttrNotFound(_)) if mode == ExternalCompletionMode::Auto => Ok(()),
        Err(WrfError::AttrNotFound(_)) => Err(format!(
            "{attribute} is required by the selected completion mode"
        )),
        Err(error) => Err(format!("{attribute} could not be read safely: {error}")),
    }
}

fn validate_metadata(metadata: FrameMetadata) -> Result<(), String> {
    if metadata.nx == 0 || metadata.ny == 0 || metadata.nz == 0 {
        return Err(format!(
            "grid dimensions must be nonzero, found {}x{}x{}",
            metadata.nx, metadata.ny, metadata.nz
        ));
    }
    if !metadata.dx_m.is_finite()
        || !metadata.dy_m.is_finite()
        || metadata.dx_m <= 0.0
        || metadata.dy_m <= 0.0
    {
        return Err(format!(
            "grid spacing must be finite and positive, found {}x{} m",
            metadata.dx_m, metadata.dy_m
        ));
    }
    Ok(())
}

fn validate_case_consistency(
    case: &ExternalWatchCase,
    metadata: FrameMetadata,
) -> Result<(), String> {
    if metadata.origin_unix != case.origin_unix {
        return Err(format!(
            "model initialization {} does not match watched case {}",
            metadata.origin_unix, case.origin_unix
        ));
    }
    if (metadata.nx, metadata.ny, metadata.nz) != (case.nx, case.ny, case.nz) {
        return Err(format!(
            "grid is {}x{}x{}, expected {}x{}x{}",
            metadata.nx, metadata.ny, metadata.nz, case.nx, case.ny, case.nz
        ));
    }
    let tolerance = case.dx_m.abs().max(case.dy_m.abs()).max(1.0) * 1.0e-9;
    if (metadata.dx_m - case.dx_m).abs() > tolerance
        || (metadata.dy_m - case.dy_m).abs() > tolerance
    {
        return Err(format!(
            "grid spacing is {:.9}x{:.9} m, expected {:.9}x{:.9} m",
            metadata.dx_m, metadata.dy_m, case.dx_m, case.dy_m
        ));
    }
    if digest_hex(&metadata.grid_sha256) != case.grid_sha256 {
        return Err("coordinate grid differs from the watched case".to_string());
    }
    if digest_hex(&metadata.projection_sha256) != case.projection_sha256 {
        return Err("projection metadata differs from the watched case".to_string());
    }
    Ok(())
}

fn frame_target(
    metadata: FrameMetadata,
    cadence_seconds: u32,
) -> Result<(u16, RwsExactTime), String> {
    if cadence_seconds == 0 {
        return Err("watch output cadence must be greater than zero".to_string());
    }
    let lead_seconds = metadata
        .valid_unix
        .checked_sub(metadata.origin_unix)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| "output valid time precedes its model initialization".to_string())?;
    let cadence = u64::from(cadence_seconds);
    if lead_seconds % cadence != 0 {
        return Err(format!(
            "output lead {lead_seconds} seconds is off the configured {cadence}-second cadence"
        ));
    }
    let slot = lead_seconds / cadence;
    let storage_slot = u16::try_from(slot)
        .map_err(|_| format!("output cadence slot {slot} exceeds rw-store capacity"))?;
    Ok((
        storage_slot,
        RwsExactTime::new(lead_seconds, metadata.valid_unix),
    ))
}

fn normalize_domain(value: &str) -> Result<String, String> {
    let normalized = value.trim().to_ascii_lowercase();
    let bytes = normalized.as_bytes();
    if bytes.len() != 3
        || bytes[0] != b'd'
        || !bytes[1].is_ascii_digit()
        || !bytes[2].is_ascii_digit()
        || normalized == "d00"
    {
        return Err("WRF domain filter must be d01 through d99".to_string());
    }
    Ok(normalized)
}

fn path_matches_domain(path: &Path, domain: &str) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some(remainder) = name.strip_prefix("wrfout_") else {
        return false;
    };
    remainder
        .strip_prefix(domain)
        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('_'))
}

fn case_sha256(canonical_root: &Path, domain: &str, metadata: FrameMetadata) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, CASE_SCHEMA);
    hash_part(&mut hasher, canonical_root.to_string_lossy().as_bytes());
    hash_part(&mut hasher, domain.as_bytes());
    hash_part(&mut hasher, &metadata.origin_unix.to_le_bytes());
    hash_part(&mut hasher, &(metadata.nx as u64).to_le_bytes());
    hash_part(&mut hasher, &(metadata.ny as u64).to_le_bytes());
    hash_part(&mut hasher, &(metadata.nz as u64).to_le_bytes());
    hash_part(&mut hasher, &metadata.dx_m.to_bits().to_le_bytes());
    hash_part(&mut hasher, &metadata.dy_m.to_bits().to_le_bytes());
    hash_part(&mut hasher, &metadata.grid_sha256);
    hash_part(&mut hasher, &metadata.projection_sha256);
    format!("{:x}", hasher.finalize())
}

fn digest_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hash_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> FrameMetadata {
        FrameMetadata {
            origin_unix: 1_700_000_000,
            valid_unix: 1_700_003_600,
            nx: 192,
            ny: 160,
            nz: 49,
            dx_m: 3_000.0,
            dy_m: 3_000.0,
            grid_sha256: [7; 32],
            projection_sha256: [9; 32],
        }
    }

    #[test]
    fn domain_filter_is_exact_and_normalized() {
        assert_eq!(normalize_domain(" D04 ").unwrap(), "d04");
        for invalid in ["", "d0", "d00", "d001", "x01", "d1a"] {
            assert!(normalize_domain(invalid).is_err(), "accepted {invalid}");
        }
        assert!(path_matches_domain(
            Path::new("wrfout_d04_2026-07-23_00:00:00"),
            "d04"
        ));
        assert!(path_matches_domain(Path::new("wrfout_d04"), "d04"));
        assert!(!path_matches_domain(
            Path::new("wrfout_d01_2026-07-23_00:00:00"),
            "d04"
        ));
        assert!(!path_matches_domain(Path::new("wrfout_d040_bad"), "d04"));
    }

    #[test]
    fn completion_attribute_is_producer_supplied_and_validated() {
        assert_eq!(
            normalize_completion_attribute("  MODEL_WRITE_COMPLETE ").unwrap(),
            "MODEL_WRITE_COMPLETE"
        );
        assert_eq!(normalize_completion_attribute("   ").unwrap(), "");
        for invalid in ["has space", "semi;colon", &"A".repeat(129)] {
            assert!(
                normalize_completion_attribute(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn requiring_a_marker_without_naming_it_is_refused() {
        let error = ExternalWatchSession::start(ExternalWatchSettings {
            root: PathBuf::from("C:/sim/output"),
            completion_mode: ExternalCompletionMode::RequireMarker,
            ..ExternalWatchSettings::default()
        })
        .unwrap_err();
        assert!(error.contains("attribute name"), "{error}");

        // Stock WRF publishes no marker, so the default mode must not need one.
        let error = ExternalWatchSession::start(ExternalWatchSettings {
            root: PathBuf::new(),
            ..ExternalWatchSettings::default()
        })
        .unwrap_err();
        assert!(error.contains("directory to watch"), "{error}");
    }

    #[test]
    fn case_digest_is_deterministic_and_load_bearing() {
        let root = Path::new("C:/sim/output");
        let baseline = case_sha256(root, "d01", metadata());
        assert_eq!(baseline, case_sha256(root, "d01", metadata()));
        assert_eq!(baseline.len(), 64);

        let mut changed_grid = metadata();
        changed_grid.nx += 1;
        assert_ne!(baseline, case_sha256(root, "d01", changed_grid));
        assert_ne!(baseline, case_sha256(root, "d02", metadata()));
        assert_ne!(
            baseline,
            case_sha256(Path::new("C:/sim/other-output"), "d01", metadata())
        );
        let mut changed_origin = metadata();
        changed_origin.origin_unix += 3_600;
        assert_ne!(baseline, case_sha256(root, "d01", changed_origin));
    }

    #[test]
    fn established_case_rejects_origin_grid_and_spacing_drift() {
        let baseline = metadata();
        let case = ExternalWatchCase::new(Path::new("C:/sim/output"), "d01", baseline);
        validate_case_consistency(&case, baseline).unwrap();

        let mut changed = baseline;
        changed.origin_unix += 1;
        assert!(validate_case_consistency(&case, changed).is_err());
        changed = baseline;
        changed.nz += 1;
        assert!(validate_case_consistency(&case, changed).is_err());
        changed = baseline;
        changed.dx_m += 0.01;
        assert!(validate_case_consistency(&case, changed).is_err());
        changed = baseline;
        changed.grid_sha256[0] ^= 1;
        assert!(validate_case_consistency(&case, changed).is_err());
        changed = baseline;
        changed.projection_sha256[0] ^= 1;
        assert!(validate_case_consistency(&case, changed).is_err());
    }

    #[test]
    fn normalized_projection_changes_are_load_bearing() {
        let baseline = projection_sha256(ProjectionFingerprint::LambertConformal {
            standard_parallel_1_deg: 30.0,
            standard_parallel_2_deg: 60.0,
            central_meridian_deg: -97.0,
        });
        assert_eq!(
            baseline,
            projection_sha256(ProjectionFingerprint::LambertConformal {
                standard_parallel_1_deg: 30.0,
                standard_parallel_2_deg: 60.0,
                central_meridian_deg: -97.0,
            })
        );
        assert_ne!(
            baseline,
            projection_sha256(ProjectionFingerprint::LambertConformal {
                standard_parallel_1_deg: 30.0,
                standard_parallel_2_deg: 60.0,
                central_meridian_deg: -96.5,
            })
        );
        assert_ne!(
            baseline,
            projection_sha256(ProjectionFingerprint::Geographic)
        );
    }

    #[test]
    fn frame_target_enforces_origin_cadence_and_u16_slot() {
        let baseline = metadata();
        let (slot, exact) = frame_target(baseline, 900).unwrap();
        assert_eq!(slot, 4);
        assert_eq!(exact, RwsExactTime::new(3_600, baseline.valid_unix));

        let mut off_cadence = baseline;
        off_cadence.valid_unix += 1;
        assert!(frame_target(off_cadence, 900).is_err());

        let mut before_origin = baseline;
        before_origin.valid_unix = before_origin.origin_unix - 1;
        assert!(frame_target(before_origin, 900).is_err());

        let mut beyond_store = baseline;
        beyond_store.valid_unix = beyond_store.origin_unix + (i64::from(u16::MAX) + 1) * 3_600;
        assert!(frame_target(beyond_store, 3_600).is_err());
        assert!(frame_target(baseline, 0).is_err());
    }

    #[test]
    fn processing_modes_map_to_light_and_full_profiles() {
        let quick = ExternalProcessingMode::QuickLook.wrf_options();
        assert!(quick.core_fields);
        assert!(!quick.diagnostics);
        assert!(!quick.heavy_ecape);
        assert!(!quick.raw_extras);

        let full = ExternalProcessingMode::Full.wrf_options();
        assert!(full.core_fields);
        assert!(full.diagnostics);
        assert!(!full.heavy_ecape);
        assert!(full.raw_extras);
    }

    #[test]
    fn status_counters_are_saturating_and_host_driven() {
        let mut status = ExternalWatchStatus::watching(Path::new("C:/sim/output"), "d01");
        status.processed_frames = u64::MAX;
        status.processing_failures = u64::MAX;
        status.rejected_count = u64::MAX;
        let mut session = ExternalWatchSession {
            settings: ExternalWatchSettings::default(),
            canonical_root: PathBuf::from("C:/sim/output"),
            watcher: StableWrfoutWatcher::default(),
            case: None,
            published_paths: BTreeMap::new(),
            published_slots: BTreeMap::new(),
            status,
        };
        session.set_backlog(7, 1);
        session.record_processing_result(true);
        session.record_processing_result(false);
        session
            .status
            .record_rejection("permanent structural error".to_string());
        assert_eq!(session.status.backlog, 7);
        assert_eq!(session.status.processing, 1);
        assert_eq!(session.status.processed_frames, u64::MAX);
        assert_eq!(session.status.processing_failures, u64::MAX);
        assert_eq!(session.status.rejected_count, u64::MAX);
    }

    #[test]
    fn permanent_rejection_evidence_survives_later_idle_polls() {
        let mut session = ExternalWatchSession {
            settings: ExternalWatchSettings::default(),
            canonical_root: PathBuf::from("C:/sim/output"),
            watcher: StableWrfoutWatcher::default(),
            case: None,
            published_paths: BTreeMap::new(),
            published_slots: BTreeMap::new(),
            status: ExternalWatchStatus::watching(Path::new("C:/sim/output"), "d01"),
        };
        session
            .status
            .record_rejection("wrfout_d01 had two time records".to_string());
        session.status.message = session
            .status
            .watching_message(&session.canonical_root, &session.settings.domain);

        assert_eq!(session.status.rejected_count, 1);
        assert!(
            session
                .status
                .last_rejection
                .as_deref()
                .is_some_and(|message| message.contains("two time records"))
        );
    }
}
