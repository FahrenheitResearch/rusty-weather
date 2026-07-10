//! rusty-weather UI shell: a thin eframe window mounting the rw-ui panels.
//!
//! Layout: run browser on the left, false-color field viewer in the center,
//! sounding panel on the right (appears after a click on the field), an
//! always-on stats strip along the bottom, a toggleable Download window
//! that runs in-process ingests through [`ingest_worker::IngestWorker`],
//! and a toggleable Satellite window that follows the live GOES buckets
//! through [`sat_worker::SatWorker`] (rolling-window store under
//! `<store-root>/sat`) with loop playback of the stored frames.
//! All store IO runs on the rw-ui store worker thread; all ingest work
//! (network fetch + extraction/compute on a dedicated below-normal rayon
//! pool) runs behind the ingest worker — this shell only wires panel
//! events to worker requests and worker responses back into the panels.
//!
//! Usage:
//!   rusty-weather-ui [--store-root <dir>] [--cache-dir <dir>] [--synthetic]
//!                    [--download-date YYYYMMDD] [--download-cycle N]
//!                    [--download-hours SPEC] [--download-profile NAME]
//!                    [--satellite]
//!
//! `--store-root` defaults to an existing nearby rw-store when one is found,
//! otherwise the per-user app data directory. `--cache-dir` presets the
//! Download panel's raw GRIB cache directory (default per-user cache dir;
//! point it at an existing cache to ingest without network). The `--download-*` flags
//! preset the Download panel's pickers (handy for scripted/offline runs).
//! `--satellite` opens the Satellite window on launch. `--synthetic`
//! writes a tiny synthetic store to a temp directory and opens that
//! instead.
//!
//! Storage paths (`--store-root`, `--cache-dir`) are configurable in the
//! app via the "Storage" collapsible section in the left browser panel;
//! values are persisted across launches via eframe's built-in storage.
//! Precedence: CLI arg > persisted setting > automatic default. Relative paths
//! are resolved through the launch context and saved as absolute paths.
//!
//! Profiling: build with `--features profiling` for puffin scopes, a
//! puffin_http server on 127.0.0.1:8585 (external `puffin_viewer`), and
//! the in-app scope-stats window. The stats strip is always available.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod batch_render;
mod formula_lab;
mod gdex_ui;
mod grib_import;
mod ingest_worker;
mod local_import;
mod postproc_severe;
#[cfg(feature = "profiling")]
mod profiler;
mod sat_worker;
mod wrf_process;
mod wrf_volumes;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use batch_render::BatchRenderPanel;
use eframe::egui;
use formula_lab::{
    FormulaLabPanel, FormulaLabSources, FormulaResultSource, FormulaSourceKind,
    RawWrfFormulaSource, StoreFormulaSource,
};
use gdex_ui::GdexBrowser;
use ingest_worker::{IngestRequest, IngestResponse, IngestWorker};
use local_import::{LocalImportMessage, LocalImportSummary, LocalImportTask};
use rustwx_models::{model_summary, supported_forecast_hours, supported_models};
use rw_ui::{
    ColorTableEditorPanel, CustomDomain, DownloadEvent, DownloadPanel, DownloadSpec,
    FieldViewerEvent, FieldViewerPanel, HourKey, ModelOption, PlotViewerPanel, RunBrowserPanel,
    SatFollowSpec, SatPlayerEvent, SatPlayerPanel, SatelliteEvent, SatellitePanel, SoundingPanel,
    StoreRequest, StoreResponse, StoreTree, StoreView, StoreWorker, StyleOverrideSettings,
};
use sat_worker::{SatRequest, SatResponse, SatWorker};
use serde::{Deserialize, Serialize};
use wrf_process::{WrfProcessMessage, WrfProcessOptions, WrfProcessSummary, WrfProcessTask};

// ---------------------------------------------------------------------------
// Storage path resolution
// ---------------------------------------------------------------------------

/// eframe Storage key for the serialized [`PersistedPaths`].
const STORAGE_KEY: &str = "rw.storage_paths";
/// eframe Storage key for user-saved native plot domains.
const DOMAIN_STORAGE_KEY: &str = "rw.custom_domains";
/// eframe Storage key for user color tables and product -> table bindings.
const STYLE_STORAGE_KEY: &str = "rw.style_overrides";
/// eframe Storage key for local WRF processing options.
const WRF_PROCESS_STORAGE_KEY: &str = "rw.wrf_process_options";

/// Legacy/default store leaf when neither CLI nor persisted settings provide one.
const DEFAULT_STORE_ROOT: &str = "store";
/// Legacy/default download cache path when neither CLI nor persisted settings provide one.
const DEFAULT_CACHE_DIR: &str = "out/cache";
/// Stable app-data folder name used for installed/default storage.
const APP_DATA_DIR_NAME: &str = "rusty-weather";

/// Where a resolved storage path came from — shown in the Settings UI.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSource {
    Cli,
    Saved,
    Default,
}

impl PathSource {
    fn label(&self) -> &'static str {
        match self {
            PathSource::Cli => "cli",
            PathSource::Saved => "saved",
            PathSource::Default => "default",
        }
    }
}

/// Fully resolved storage paths + their sources, computed once at startup.
#[derive(Debug, Clone)]
struct StoragePaths {
    store_root: PathBuf,
    store_root_source: PathSource,
    cache_dir: PathBuf,
    cache_dir_source: PathSource,
}

/// The subset of [`StoragePaths`] that is persisted across launches.
/// Stored as a JSON object under [`STORAGE_KEY`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
struct PersistedPaths {
    store_root: Option<String>,
    cache_dir: Option<String>,
}

/// Serialize [`PersistedPaths`] to a JSON string via serde_json.
///
/// serde_json correctly JSON-escapes backslashes and other special characters,
/// so Windows paths (e.g. `C:\Users\drew\store`) survive the round-trip.
///
/// # Persistence feature note
///
/// This function is only called from `App::save` / `StorageSettingsUi::ui`,
/// which are themselves only exercised when eframe's persistence feature is
/// compiled in.  The `eframe` dependency MUST carry `features = ["persistence"]`
/// in Cargo.toml — without it `cc.storage` is always `None`, `App::save` is
/// never invoked, and the entire persisted-settings path is a compile-time
/// no-op (even though the Rust code compiles fine without the flag).
fn serialize_persisted(p: &PersistedPaths) -> String {
    // Infallible for this type: all fields are Option<String>.
    serde_json::to_string(p).unwrap_or_default()
}

/// Deserialize [`PersistedPaths`] from JSON produced by [`serialize_persisted`].
///
/// Returns a value with `None` fields on any parse error so that garbled or
/// stale storage data degrades gracefully to built-in defaults.
fn deserialize_persisted(s: &str) -> PersistedPaths {
    serde_json::from_str(s).unwrap_or_default()
}

fn serialize_custom_domains(domains: &[CustomDomain]) -> String {
    serde_json::to_string(domains).unwrap_or_default()
}

fn deserialize_custom_domains(s: &str) -> Vec<CustomDomain> {
    serde_json::from_str(s).unwrap_or_default()
}

fn serialize_style_settings(settings: &StyleOverrideSettings) -> String {
    serde_json::to_string(settings).unwrap_or_default()
}

fn deserialize_style_settings(s: &str) -> StyleOverrideSettings {
    serde_json::from_str::<StyleOverrideSettings>(s)
        .unwrap_or_default()
        .normalized()
}

fn serialize_wrf_process_options(options: &WrfProcessOptions) -> String {
    serde_json::to_string(options).unwrap_or_default()
}

fn deserialize_wrf_process_options(s: &str) -> WrfProcessOptions {
    serde_json::from_str::<WrfProcessOptions>(s)
        .unwrap_or_default()
        .normalized()
}

/// Pure resolution function: merges CLI overrides + persisted settings +
/// compiled-in defaults and returns the effective paths + their sources.
///
/// Precedence (highest first): CLI arg → persisted saved value → built-in default.
///
/// Relative paths are resolved against the launch context so double-clicking
/// the executable from different folders does not silently point at a
/// different empty store.
fn resolve_storage_paths(
    cli_store: Option<&str>,
    cli_cache: Option<&str>,
    saved: Option<&PersistedPaths>,
) -> StoragePaths {
    let (store_root, store_root_source) = if let Some(v) = cli_store {
        (resolve_store_path_input(v), PathSource::Cli)
    } else if let Some(v) = saved.and_then(|s| s.store_root.as_deref()) {
        (resolve_store_path_input(v), PathSource::Saved)
    } else {
        (default_store_root(), PathSource::Default)
    };

    let (cache_dir, cache_dir_source) = if let Some(v) = cli_cache {
        (resolve_cache_path_input(v), PathSource::Cli)
    } else if let Some(v) = saved.and_then(|s| s.cache_dir.as_deref()) {
        (resolve_cache_path_input(v), PathSource::Saved)
    } else {
        (default_cache_dir(), PathSource::Default)
    };

    StoragePaths {
        store_root,
        store_root_source,
        cache_dir,
        cache_dir_source,
    }
}

fn resolve_store_path_input(value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return path;
    }
    if path == Path::new(DEFAULT_STORE_ROOT) {
        if let Some(discovered) = discover_existing_store_root() {
            return discovered;
        }
    }
    absolutize_from_current_dir(path)
}

fn resolve_cache_path_input(value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return path;
    }
    if let Some(discovered) = discover_existing_relative_path(&path) {
        return discovered;
    }
    absolutize_from_current_dir(path)
}

fn absolutize_from_current_dir(path: PathBuf) -> PathBuf {
    std::env::current_dir()
        .map(|cwd| cwd.join(&path))
        .unwrap_or(path)
}

fn default_store_root() -> PathBuf {
    discover_existing_store_root()
        .or_else(|| app_data_dir().map(|dir| dir.join(DEFAULT_STORE_ROOT)))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STORE_ROOT))
}

fn default_cache_dir() -> PathBuf {
    discover_existing_relative_path(Path::new(DEFAULT_CACHE_DIR))
        .or_else(app_cache_dir)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CACHE_DIR))
}

fn discover_existing_store_root() -> Option<PathBuf> {
    discover_existing_store_root_from(launch_search_roots())
}

fn discover_existing_store_root_from<I>(starts: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    for start in starts {
        for ancestor in start.ancestors().take(8) {
            let candidate = ancestor.join(DEFAULT_STORE_ROOT);
            if looks_like_rw_store_root(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn discover_existing_relative_path(relative: &Path) -> Option<PathBuf> {
    for start in launch_search_roots() {
        for ancestor in start.ancestors().take(8) {
            let candidate = ancestor.join(relative);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn launch_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
        }
    }
    roots
}

fn looks_like_rw_store_root(path: &Path) -> bool {
    let Ok(models) = std::fs::read_dir(path) else {
        return false;
    };

    for model in models.flatten() {
        let model_path = model.path();
        if !model_path.is_dir() {
            continue;
        }
        let Ok(runs) = std::fs::read_dir(&model_path) else {
            continue;
        };
        for run in runs.flatten() {
            let run_path = run.path();
            if run_path.join("run.json").is_file()
                || run_path.join("grid.rwg").is_file()
                || contains_rws_file(&run_path)
            {
                return true;
            }
        }
    }

    false
}

fn contains_rws_file(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("rws"))
    })
}

fn app_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(PathBuf::from)
                    .map(|home| home.join("AppData").join("Roaming"))
            })
            .map(|dir| dir.join(APP_DATA_DIR_NAME))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(PathBuf::from).map(|home| {
            home.join("Library")
                .join("Application Support")
                .join(APP_DATA_DIR_NAME)
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local").join("share"))
            })
            .map(|dir| dir.join(APP_DATA_DIR_NAME))
    }
}

fn app_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
            .map(|dir| dir.join(APP_DATA_DIR_NAME).join("cache"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Caches").join(APP_DATA_DIR_NAME))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".cache"))
            })
            .map(|dir| dir.join(APP_DATA_DIR_NAME))
    }
}

// ---------------------------------------------------------------------------
// Disk-usage helper
// ---------------------------------------------------------------------------

/// Recursively sum the sizes of all files under `dir`.
///
/// Returns `None` if `dir` does not exist or cannot be read.  Errors on
/// individual entries are silently skipped (permission-denied sub-dirs, etc.).
/// This is a one-shot blocking call — never invoke it per-frame; callers must
/// cache the result.
fn dir_size_bytes(dir: &std::path::Path) -> Option<u64> {
    if !dir.exists() {
        return None;
    }
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    Some(total)
}

/// Format a byte count as a human-readable string (B / KB / MB / GB).
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

// ---------------------------------------------------------------------------
// Storage settings UI (app-shell only — not in the rw-ui library)
// ---------------------------------------------------------------------------

/// State for the collapsible "Storage" section in the browser panel.
///
/// Fields are edit buffers separate from the effective runtime paths so the
/// user can type and discard without affecting the live workers.
struct StorageSettingsUi {
    /// Edit buffer for the store root text field.
    store_root_edit: String,
    /// Edit buffer for the download cache dir text field.
    cache_dir_edit: String,
    /// Source labels for the current effective values (shown as hints).
    store_root_source: PathSource,
    cache_dir_source: PathSource,
    /// Inline error text after a failed Apply (validation error).
    apply_error: Option<String>,
    /// Status text shown after a successful Apply.
    apply_status: Option<String>,
    /// Cached disk-usage results (populated on Apply or first open).
    store_size: Option<u64>,
    cache_size: Option<u64>,
    /// Guard so we run the initial size scan exactly once.
    sizes_computed: bool,
}

impl StorageSettingsUi {
    fn new(paths: &StoragePaths) -> Self {
        Self {
            store_root_edit: paths.store_root.display().to_string(),
            cache_dir_edit: paths.cache_dir.display().to_string(),
            store_root_source: paths.store_root_source.clone(),
            cache_dir_source: paths.cache_dir_source.clone(),
            apply_error: None,
            apply_status: None,
            store_size: None,
            cache_size: None,
            sizes_computed: false,
        }
    }

    /// Run the disk-size scan once (lazily on first open).
    fn compute_sizes_once(&mut self, store_root: &std::path::Path, cache_dir: &std::path::Path) {
        if !self.sizes_computed {
            self.sizes_computed = true;
            self.store_size = dir_size_bytes(store_root);
            self.cache_size = dir_size_bytes(cache_dir);
        }
    }

    /// Render the Storage section into `ui`.
    ///
    /// Returns `Some(PersistedPaths)` when the user clicks Apply and
    /// validation succeeds — the caller must persist the value and show a
    /// restart notice.
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        effective_store_root: &std::path::Path,
        effective_cache_dir: &std::path::Path,
    ) -> Option<PersistedPaths> {
        self.compute_sizes_once(effective_store_root, effective_cache_dir);

        let mut new_paths: Option<PersistedPaths> = None;

        egui::CollapsingHeader::new("Storage")
            .id_salt("rw-storage-settings")
            .default_open(false)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 3.0;

                // Store root row
                ui.label(egui::RichText::new("Store root").small().strong());
                let store_hint = if self.store_root_source == PathSource::Cli {
                    "overridden by --store-root for this session"
                } else {
                    self.store_root_source.label()
                };
                ui.horizontal(|ui| {
                    let resp = ui.add_enabled(
                        self.store_root_source != PathSource::Cli,
                        egui::TextEdit::singleline(&mut self.store_root_edit)
                            .desired_width(f32::INFINITY)
                            .hint_text("path/to/store"),
                    );
                    if resp.changed() {
                        self.apply_error = None;
                        self.apply_status = None;
                    }
                    ui.label(egui::RichText::new(store_hint).small().weak());
                });
                // Disk usage hint for store root
                ui.label(
                    egui::RichText::new(match self.store_size {
                        Some(bytes) => format!("disk: {}", format_bytes(bytes)),
                        None => "disk: (dir not found)".to_string(),
                    })
                    .small()
                    .weak(),
                );

                ui.add_space(4.0);

                // Cache dir row
                ui.label(egui::RichText::new("Download cache").small().strong());
                let cache_hint = if self.cache_dir_source == PathSource::Cli {
                    "overridden by --cache-dir for this session"
                } else {
                    self.cache_dir_source.label()
                };
                ui.horizontal(|ui| {
                    let resp = ui.add_enabled(
                        self.cache_dir_source != PathSource::Cli,
                        egui::TextEdit::singleline(&mut self.cache_dir_edit)
                            .desired_width(f32::INFINITY)
                            .hint_text("path/to/cache"),
                    );
                    if resp.changed() {
                        self.apply_error = None;
                        self.apply_status = None;
                    }
                    ui.label(egui::RichText::new(cache_hint).small().weak());
                });
                ui.label(
                    egui::RichText::new(match self.cache_size {
                        Some(bytes) => format!("disk: {}", format_bytes(bytes)),
                        None => "disk: (dir not found)".to_string(),
                    })
                    .small()
                    .weak(),
                );

                ui.add_space(6.0);

                // Apply button + status
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            self.store_root_source != PathSource::Cli
                                || self.cache_dir_source != PathSource::Cli,
                            egui::Button::new("Apply"),
                        )
                        .on_disabled_hover_text(
                            "Both paths are overridden by CLI arguments for this session",
                        )
                        .clicked()
                    {
                        // Validate: try create_dir_all for any path not
                        // overridden by CLI (editable fields only).
                        let resolved_store = resolve_store_path_input(&self.store_root_edit);
                        let resolved_cache = resolve_cache_path_input(&self.cache_dir_edit);
                        let store_ok = if self.store_root_source != PathSource::Cli {
                            std::fs::create_dir_all(&resolved_store)
                                .map_err(|e| format!("store root: {e}"))
                        } else {
                            Ok(())
                        };
                        let cache_ok = if self.cache_dir_source != PathSource::Cli {
                            std::fs::create_dir_all(&resolved_cache)
                                .map_err(|e| format!("cache dir: {e}"))
                        } else {
                            Ok(())
                        };
                        match (store_ok, cache_ok) {
                            (Err(e), _) | (_, Err(e)) => {
                                self.apply_error = Some(e);
                                self.apply_status = None;
                            }
                            (Ok(()), Ok(())) => {
                                // Refresh disk sizes after Apply
                                self.store_root_edit = resolved_store.display().to_string();
                                self.cache_dir_edit = resolved_cache.display().to_string();
                                self.store_size = dir_size_bytes(&resolved_store);
                                self.cache_size = dir_size_bytes(&resolved_cache);
                                self.apply_error = None;
                                self.apply_status =
                                    Some("Saved — restart to apply to live workers".to_string());
                                // Only persist the editable (non-CLI) values
                                new_paths = Some(PersistedPaths {
                                    store_root: if self.store_root_source != PathSource::Cli {
                                        Some(self.store_root_edit.clone())
                                    } else {
                                        None
                                    },
                                    cache_dir: if self.cache_dir_source != PathSource::Cli {
                                        Some(self.cache_dir_edit.clone())
                                    } else {
                                        None
                                    },
                                });
                            }
                        }
                    }

                    if let Some(ref err) = self.apply_error {
                        ui.label(
                            egui::RichText::new(err)
                                .small()
                                .color(ui.visuals().error_fg_color),
                        );
                    } else if let Some(ref status) = self.apply_status {
                        ui.label(egui::RichText::new(status).small().weak());
                    }
                });

                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(
                        "Relative paths are saved as absolute paths. \
                         Changes take effect on the next launch (workers hold the \
                         old paths until restart).",
                    )
                    .small()
                    .weak(),
                );
            });

        new_paths
    }
}

// ---------------------------------------------------------------------------
// WRF processing settings UI (app-shell only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct WrfProcessSettingsUi {
    only_edit: String,
    skip_edit: String,
}

impl WrfProcessSettingsUi {
    fn new(options: &WrfProcessOptions) -> Self {
        Self {
            only_edit: format_product_filter_tokens(&options.only),
            skip_edit: format_product_filter_tokens(&options.skip),
        }
    }

    fn set_from_options(&mut self, options: &WrfProcessOptions) {
        self.only_edit = format_product_filter_tokens(&options.only);
        self.skip_edit = format_product_filter_tokens(&options.skip);
    }

    fn ui(&mut self, ui: &mut egui::Ui, options: &mut WrfProcessOptions) -> bool {
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Fast core").clicked() {
                *options = WrfProcessOptions {
                    core_fields: true,
                    diagnostics: false,
                    heavy_ecape: false,
                    raw_extras: false,
                    only: Vec::new(),
                    skip: Vec::new(),
                };
                self.set_from_options(options);
                changed = true;
            }
            if ui.button("WRF default").clicked() {
                *options = WrfProcessOptions::default();
                self.set_from_options(options);
                changed = true;
            }
            if ui.button("Everything").clicked() {
                *options = WrfProcessOptions {
                    core_fields: true,
                    diagnostics: true,
                    heavy_ecape: true,
                    raw_extras: true,
                    only: Vec::new(),
                    skip: Vec::new(),
                };
                self.set_from_options(options);
                changed = true;
            }
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            changed |= ui
                .checkbox(&mut options.core_fields, "Core fields")
                .changed();
            changed |= ui
                .checkbox(&mut options.diagnostics, "Diagnostics")
                .changed();
            changed |= ui
                .checkbox(&mut options.heavy_ecape, "ECAPE/heavy")
                .changed();
            changed |= ui.checkbox(&mut options.raw_extras, "Raw extras").changed();
        });
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Only products").small().strong());
        if ui
            .add(
                egui::TextEdit::singleline(&mut self.only_edit)
                    .desired_width(f32::INFINITY)
                    .hint_text("blank = any selected group"),
            )
            .changed()
        {
            options.only = parse_product_filter_tokens(&self.only_edit);
            changed = true;
        }
        ui.label(egui::RichText::new("Skip products").small().strong());
        if ui
            .add(
                egui::TextEdit::singleline(&mut self.skip_edit)
                    .desired_width(f32::INFINITY)
                    .hint_text("ecape, hail, graupel, ..."),
            )
            .changed()
        {
            options.skip = parse_product_filter_tokens(&self.skip_edit);
            changed = true;
        }
        ui.label(
            egui::RichText::new(
                "Filters match WRF names, store product names, and stripped wrf_ aliases.",
            )
            .small()
            .weak(),
        );
        changed
    }
}

fn parse_product_filter_tokens(value: &str) -> Vec<String> {
    value
        .split([',', ';', '\n', '\r', '\t'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn format_product_filter_tokens(tokens: &[String]) -> String {
    tokens.join(", ")
}

// ---------------------------------------------------------------------------
// main + CLI parsing
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprintln!(
                "usage: rusty-weather-ui [--store-root <dir>] [--cache-dir <dir>] [--synthetic]"
            );
            return ExitCode::FAILURE;
        }
    };

    // --synthetic overrides everything: ignore persisted / CLI store-root.
    // Extract owned copies up front so the closure can move them without
    // borrowing `args` across the move boundary.
    let synthetic = args.synthetic;
    let satellite = args.satellite;
    // `cli_store_owned` / `cli_cache_owned` are the raw CLI strings (owned),
    // `None` when not provided on the command line.
    let cli_store_owned: Option<String> = if synthetic { None } else { args.store_root };
    let cli_cache_owned: Option<String> = args.spec_overrides.cache_dir.clone();
    let spec_overrides = args.spec_overrides;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("rusty-weather"),
        ..Default::default()
    };
    let result = eframe::run_native(
        "rusty-weather",
        options,
        Box::new(move |cc| {
            let cli_store = cli_store_owned.as_deref();
            let cli_cache = cli_cache_owned.as_deref();
            let store_root = if synthetic {
                let root = std::env::temp_dir().join("rusty-weather-ui-synthetic");
                rw_ui::synthetic::write_synthetic_store(&root)
                    .map_err(|e| format!("failed to write the synthetic store: {e}"))?;
                root
            } else {
                // Read persisted paths from eframe Storage.
                let saved = cc
                    .storage
                    .and_then(|s| s.get_string(STORAGE_KEY).map(|v| deserialize_persisted(&v)));
                let paths = resolve_storage_paths(cli_store, cli_cache, saved.as_ref());
                paths.store_root
            };

            Ok(Box::new(App::new(
                cc,
                store_root,
                spec_overrides,
                satellite,
                cli_store_owned.clone(),
                cli_cache_owned.clone(),
            )))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ui error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// CLI presets for the Download panel's initial spec.
#[derive(Default)]
struct SpecOverrides {
    cache_dir: Option<String>,
    date: Option<String>,
    cycle: Option<u8>,
    hours: Option<String>,
    profile: Option<String>,
}

struct Args {
    /// Raw CLI value; `None` means not provided (use persisted/default).
    store_root: Option<String>,
    synthetic: bool,
    satellite: bool,
    spec_overrides: SpecOverrides,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut store_root: Option<String> = None;
        let mut synthetic = false;
        let mut satellite = false;
        let mut spec_overrides = SpecOverrides::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| -> Result<String, String> {
                args.next().ok_or(format!("{flag} requires a value"))
            };
            match arg.as_str() {
                "--store-root" => store_root = Some(value("--store-root")?),
                "--cache-dir" => spec_overrides.cache_dir = Some(value("--cache-dir")?),
                "--download-date" => spec_overrides.date = Some(value("--download-date")?),
                "--download-cycle" => {
                    spec_overrides.cycle = Some(
                        value("--download-cycle")?
                            .parse()
                            .map_err(|_| "--download-cycle expects 0-23".to_string())?,
                    );
                }
                "--download-hours" => spec_overrides.hours = Some(value("--download-hours")?),
                "--download-profile" => {
                    spec_overrides.profile = Some(value("--download-profile")?);
                }
                "--satellite" => satellite = true,
                "--synthetic" => synthetic = true,
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(Self {
            store_root,
            synthetic,
            satellite,
            spec_overrides,
        })
    }
}

/// A short cadence note for models whose forecast-hour stride changes within
/// the supported range. Returns an empty string for models with a uniform
/// stride (or no hours at all) so callers can skip appending it.
///
/// GFS: hourly out to f120, then 3-hourly from f123 to f384.
fn cadence_hint(model: rustwx_core::ModelId, _cycle: u8) -> &'static str {
    use rustwx_core::ModelId;
    match model {
        ModelId::Gfs => "hourly <=120, 3-hourly 123-384",
        ModelId::Gefs => "3-hourly <=240, 6-hourly 246-384",
        ModelId::Aigfs | ModelId::Aigefs => "6-hourly 000-384",
        ModelId::Hgefs => "6-hourly 000-240",
        ModelId::EcmwfOpenData => {
            "00/12z: 3-hourly <=144 then 6-hourly <=360; 06/18z: 3-hourly <=144"
        }
        ModelId::Rap => "f000-f021 most cycles, f000-f051 at 03/09/15/21z",
        ModelId::Nam => "hourly <=36, 3-hourly 39-84",
        _ => "",
    }
}

fn normalize_download_spec(mut spec: DownloadSpec) -> DownloadSpec {
    let Ok(model) = spec.model.parse::<rustwx_core::ModelId>() else {
        return spec;
    };
    if let Some(hours) = normalize_hour_spec_for_model(model, spec.cycle, &spec.hours) {
        spec.hours = hours;
    }
    spec
}

fn normalize_hour_spec_for_model(
    model: rustwx_core::ModelId,
    cycle: u8,
    hour_spec: &str,
) -> Option<String> {
    let supported = supported_forecast_hours(model, cycle);
    if supported.is_empty() {
        return None;
    }
    let requested = rw_ingest::parse_hours(hour_spec).ok()?;
    if requested.iter().all(|hour| supported.contains(hour)) {
        return None;
    }

    let mut normalized = Vec::new();
    for token in hour_spec
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        if let Some((start, end)) = token.split_once('-') {
            let start: u16 = start.trim().parse().ok()?;
            let end: u16 = end.trim().parse().ok()?;
            if start > end {
                return None;
            }
            let before = normalized.len();
            normalized.extend(
                supported
                    .iter()
                    .copied()
                    .filter(|hour| *hour >= start && *hour <= end),
            );
            if normalized.len() == before {
                return None;
            }
        } else {
            let hour: u16 = token.parse().ok()?;
            if !supported.contains(&hour) {
                return None;
            }
            normalized.push(hour);
        }
    }

    normalized.sort_unstable();
    normalized.dedup();
    if normalized.is_empty() {
        None
    } else {
        Some(
            normalized
                .iter()
                .map(|hour| hour.to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
    }
}

/// Every user-facing model, honestly labeled: only ingest-supported ones
/// are pickable; the rest are visible but disabled with a note.
fn model_options() -> Vec<ModelOption> {
    supported_models()
        .iter()
        .map(|&model| {
            let enabled = rw_ingest::ingest_supported(model);
            ModelOption {
                slug: model.as_str().to_string(),
                label: model.as_str().to_uppercase(),
                enabled,
                note: if enabled {
                    String::new()
                } else {
                    "ingest not yet supported — multi-model coming soon".to_string()
                },
            }
        })
        .collect()
}

const LARGE_WRF_WARN_CELLS_3D: usize = 10_000_000;
const LARGE_WRF_WARN_FILE_BYTES: u64 = 1 << 30;

struct PendingWrfImport {
    files: Vec<PathBuf>,
    warning: String,
    wrf_options: Option<WrfProcessOptions>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportRecordGeometry {
    path: PathBuf,
    shape: Vec<usize>,
    elements: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbedFileGeometry {
    shape: Option<Vec<usize>>,
    record_elements: Option<usize>,
    records: usize,
    records_exact: bool,
}

/// Conservative, selection-wide preflight result. Probe failures deliberately
/// force confirmation instead of silently treating an unknown file as small.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportSizeAssessment {
    file_count: usize,
    record_count: usize,
    records_exact: bool,
    total_record_elements: u128,
    max_bytes: u64,
    largest_record: Option<ImportRecordGeometry>,
    uncertainties: Vec<String>,
}

impl ImportSizeAssessment {
    fn new(file_count: usize) -> Self {
        Self {
            file_count,
            record_count: 0,
            records_exact: true,
            total_record_elements: 0,
            max_bytes: 0,
            largest_record: None,
            uncertainties: Vec::new(),
        }
    }

    fn failed(file_count: usize, error: String) -> Self {
        let mut assessment = Self::new(file_count);
        assessment.record_count = file_count;
        assessment.records_exact = false;
        assessment.uncertainties.push(error);
        assessment
    }

    fn include_geometry(&mut self, path: &Path, geometry: ProbedFileGeometry) {
        let records = geometry.records.max(1);
        match self.record_count.checked_add(records) {
            Some(record_count) => self.record_count = record_count,
            None => {
                self.record_count = usize::MAX;
                self.records_exact = false;
                self.uncertainties.push(format!(
                    "{}: time-record count overflows usize",
                    path.display()
                ));
            }
        }
        self.records_exact &= geometry.records_exact;
        if let (Some(shape), Some(elements)) = (geometry.shape, geometry.record_elements) {
            self.total_record_elements = self
                .total_record_elements
                .saturating_add((elements as u128).saturating_mul(records as u128));
            let replace = self
                .largest_record
                .as_ref()
                .is_none_or(|largest| elements > largest.elements);
            if replace {
                self.largest_record = Some(ImportRecordGeometry {
                    path: path.to_path_buf(),
                    shape,
                    elements,
                });
            }
        }
    }

    fn include_probe_failure(&mut self, path: &Path, error: String) {
        self.record_count = self.record_count.saturating_add(1);
        self.records_exact = false;
        self.uncertainties
            .push(format!("{}: {error}", path.display()));
    }

    fn needs_confirmation(&self) -> bool {
        self.max_bytes >= LARGE_WRF_WARN_FILE_BYTES
            || self
                .largest_record
                .as_ref()
                .is_some_and(|record| record.elements >= LARGE_WRF_WARN_CELLS_3D)
            || self.total_record_elements >= LARGE_WRF_WARN_CELLS_3D as u128
            || !self.uncertainties.is_empty()
    }

    fn description(&self) -> Option<String> {
        if !self.needs_confirmation() {
            return None;
        }
        let mut parts = Vec::new();
        if let Some(record) = &self.largest_record {
            let shape = record
                .shape
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("x");
            let millions = record.elements as f64 / 1.0e6;
            parts.push(format!(
                "largest record {shape} (~{millions:.0}M elements) in {}",
                record.path.display()
            ));
        }
        if self.max_bytes > 0 {
            parts.push(format!(
                "largest file {:.1} GB",
                self.max_bytes as f64 / 1.0e9
            ));
        }
        if self.total_record_elements >= LARGE_WRF_WARN_CELLS_3D as u128 {
            let total_millions = self.total_record_elements as f64 / 1.0e6;
            parts.push(format!(
                "~{total_millions:.0}M grid elements across all distinct records"
            ));
        }
        let record_count = if self.records_exact {
            self.record_count.to_string()
        } else {
            format!("at least {}", self.record_count)
        };
        parts.push(format!(
            "{record_count} distinct time record(s) across {} file(s)",
            self.file_count
        ));
        if let Some(first) = self.uncertainties.first() {
            let extra = self.uncertainties.len().saturating_sub(1);
            let suffix = if extra == 0 {
                String::new()
            } else {
                format!(" (+{extra} more)")
            };
            parts.push(format!(
                "size could not be fully verified ({} probe issue(s)): {first}{suffix}",
                self.uncertainties.len()
            ));
        }
        Some(parts.join(", "))
    }
}

#[derive(Debug)]
enum ImportProbeLaunch {
    Wrf {
        files: Vec<PathBuf>,
        options: WrfProcessOptions,
    },
    Local {
        files: Vec<PathBuf>,
    },
}

impl ImportProbeLaunch {
    fn wrf(files: Vec<PathBuf>, options: WrfProcessOptions) -> Self {
        Self::Wrf {
            files: normalize_import_probe_files(files),
            options,
        }
    }

    fn local(files: Vec<PathBuf>) -> Self {
        Self::Local {
            files: normalize_import_probe_files(files),
        }
    }

    fn files(&self) -> &[PathBuf] {
        match self {
            Self::Wrf { files, .. } | Self::Local { files } => files,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Wrf { files, .. } => {
                format!("Inspecting {} WRF file(s) before processing", files.len())
            }
            Self::Local { files } => {
                format!("Inspecting {} model file(s) before import", files.len())
            }
        }
    }
}

struct ImportSizeProbeTask {
    launch: ImportProbeLaunch,
    label: String,
    rx: Receiver<Result<ImportSizeAssessment, String>>,
}

impl ImportSizeProbeTask {
    fn spawn(launch: ImportProbeLaunch) -> Result<Self, String> {
        if launch.files().is_empty() {
            return Err("cannot inspect an empty import selection".to_string());
        }
        let label = launch.label();
        let files = launch.files().to_vec();
        let (tx, rx) = channel();
        let _worker = std::thread::Builder::new()
            .name("rw-ui-import-size-probe".to_string())
            .spawn(move || {
                wrf_process::lower_import_thread_priority();
                let result = wrf_process::isolate_panics("import size probe", || {
                    Ok(inspect_import_selection(&files))
                });
                let _ = tx.send(result);
            })
            .map_err(|error| format!("could not start import size probe: {error}"))?;
        Ok(Self { launch, label, rx })
    }
}

fn normalize_import_probe_files(mut files: Vec<PathBuf>) -> Vec<PathBuf> {
    files.sort();
    files.dedup();
    files
}

fn checked_shape_elements(shape: &[usize], context: &str) -> Result<usize, String> {
    if shape.is_empty() || shape.iter().any(|value| *value == 0) {
        return Err(format!(
            "{context} has an empty or zero-length grid dimension"
        ));
    }
    shape.iter().try_fold(1usize, |elements, value| {
        elements
            .checked_mul(*value)
            .ok_or_else(|| format!("{context} grid dimensions overflow usize"))
    })
}

fn is_probe_time_dimension(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "time" | "times" | "xtime" | "valid_time" | "forecast_time" | "t"
    ) || lower.starts_with("time")
}

fn probe_netcdf_geometry(path: &Path) -> Result<ProbedFileGeometry, String> {
    let nc =
        netcrust::open(path).map_err(|error| format!("open NetCDF metadata failed: {error}"))?;
    let dimensions = nc
        .dimensions()
        .map_err(|error| format!("read NetCDF dimensions failed: {error}"))?;
    let time_dimensions = dimensions
        .iter()
        .filter(|dimension| is_probe_time_dimension(dimension.name()))
        .collect::<Vec<_>>();
    if time_dimensions.iter().any(|dimension| dimension.len() == 0) {
        return Err("NetCDF time dimension is empty".to_string());
    }
    let records = time_dimensions
        .iter()
        .map(|dimension| dimension.len())
        .max()
        .unwrap_or(1);
    let records_exact = time_dimensions.len() <= 1;

    let variables = nc
        .variables()
        .map_err(|error| format!("read NetCDF variables failed: {error}"))?;
    let mut largest_shape = None::<Vec<usize>>;
    let mut largest_elements = 0usize;
    for variable in variables {
        let shape = variable
            .dimensions()
            .iter()
            .filter(|dimension| !is_probe_time_dimension(dimension.name()))
            .map(|dimension| dimension.len())
            .collect::<Vec<_>>();
        if shape.is_empty() || shape.iter().any(|value| *value == 0) {
            continue;
        }
        let context = format!("NetCDF variable '{}'", variable.name());
        let elements = checked_shape_elements(&shape, &context)?;
        if elements > largest_elements {
            largest_elements = elements;
            largest_shape = Some(shape);
        }
    }
    let shape = largest_shape.ok_or_else(|| {
        "NetCDF metadata contains no non-empty record-shaped variable".to_string()
    })?;
    Ok(ProbedFileGeometry {
        shape: Some(shape),
        record_elements: Some(largest_elements),
        records,
        records_exact,
    })
}

fn probe_model_geometry(path: &Path) -> Result<ProbedFileGeometry, String> {
    if grib_import::is_grib1_file(path) {
        // GRIB1 import is streaming and does not use wrf-core's 3-D cache. Its
        // byte size is still assessed selection-wide; record count is a lower
        // bound because the GRIB inventory is intentionally left to import.
        return Ok(ProbedFileGeometry {
            shape: None,
            record_elements: None,
            records: 1,
            records_exact: false,
        });
    }

    let wrf = wrf_process::isolate_panics("open WRF metadata for size probe", || {
        wrf_core::WrfFile::open(path).map_err(|error| error.to_string())
    });
    match wrf {
        Ok(file) => {
            let shape = vec![file.nx, file.ny, file.nz.max(1)];
            let record_elements = checked_shape_elements(&shape, "WRF")?;
            if file.nt == 0 {
                return Err("WRF Time dimension is empty".to_string());
            }
            Ok(ProbedFileGeometry {
                shape: Some(shape),
                record_elements: Some(record_elements),
                records: file.nt,
                records_exact: true,
            })
        }
        Err(wrf_error) => probe_netcdf_geometry(path).map_err(|netcdf_error| {
            format!("WRF metadata probe failed ({wrf_error}); {netcdf_error}")
        }),
    }
}

fn inspect_import_selection(files: &[PathBuf]) -> ImportSizeAssessment {
    let mut assessment = ImportSizeAssessment::new(files.len());
    for path in files {
        match std::fs::metadata(path) {
            Ok(metadata) => assessment.max_bytes = assessment.max_bytes.max(metadata.len()),
            Err(error) => assessment.uncertainties.push(format!(
                "{}: file metadata could not be read: {error}",
                path.display()
            )),
        }
        let what = format!("size probe for {}", path.display());
        match wrf_process::isolate_panics(&what, || probe_model_geometry(path)) {
            Ok(geometry) => assessment.include_geometry(path, geometry),
            Err(error) => assessment.include_probe_failure(path, error),
        }
    }
    assessment
}

fn heavy_import_size_warning(assessment: &ImportSizeAssessment) -> Option<String> {
    Some(format!(
        "{}. Full diagnostics computes the selected severe/thermodynamic suite through wrf-core. Expect long processing and, for large individual grids, several GB of RAM; save other work first.",
        assessment.description()?
    ))
}

fn light_import_size_warning(assessment: &ImportSizeAssessment) -> Option<String> {
    Some(format!(
        "{}. Even the light import may interpolate five 3-D sounding fields to 37 pressure levels for every record. Expect long processing and, for large individual grids, several GB of RAM.",
        assessment.description()?
    ))
}

struct App {
    worker: StoreWorker,
    ingest: IngestWorker,
    store_root: PathBuf,
    cache_dir: PathBuf,
    /// Lazy NSF NCAR GDEX catalog/subset browser and download worker.
    gdex: GdexBrowser,
    /// Safe, unit-aware custom diagnostic editor/evaluator.
    formula_lab: FormulaLabPanel,
    /// `None` until the first scan lands.
    tree: Option<StoreTree>,
    browser: RunBrowserPanel,
    viewer: FieldViewerPanel,
    plot_viewer: PlotViewerPanel,
    show_plot_viewer: bool,
    batch_render: BatchRenderPanel,
    show_batch_render: bool,
    color_tables: ColorTableEditorPanel,
    show_color_tables: bool,
    sounding: SoundingPanel,
    download: DownloadPanel,
    /// A download-ingest Start request queued before its Started response.
    download_start_pending: bool,
    show_download: bool,
    sat: SatWorker,
    sat_panel: SatellitePanel,
    sat_player: SatPlayerPanel,
    show_satellite: bool,
    /// First-open initialization of the Satellite window (validate + scan).
    sat_initialized: bool,
    /// CPU time of the previous `App::ui` pass (stats strip).
    frame_ms: f32,
    /// Last texture-build wall already recorded into the stats registry
    /// (the panel re-reports the same value every frame).
    recorded_texture_ms: Option<f32>,
    /// Same dedup for native map plot render/upload timings.
    recorded_plot_timings: Option<(f32, f32)>,
    /// Same dedup for the sat player's texture uploads.
    recorded_sat_texture_ms: Option<f32>,
    /// Background local file/folder import, currently focused on WRF NetCDF.
    local_import: Option<LocalImportTask>,
    /// Short file/open/import status shown in the toolbar.
    local_import_status: Option<String>,
    /// Completed GDEX downloads waiting for the single local-import worker.
    pending_auto_imports: VecDeque<PathBuf>,
    /// Background metadata preflight shared by full and light model imports.
    import_size_probe: Option<ImportSizeProbeTask>,
    /// Large full-diagnostic import awaiting explicit confirmation.
    pending_heavy_import: Option<PendingWrfImport>,
    /// Large light/store import awaiting explicit confirmation.
    pending_light_import: Option<PendingWrfImport>,
    /// WRF files staged by File -> Open before explicit product processing.
    pending_wrf_paths: Vec<PathBuf>,
    /// Last explicitly staged raw WRF file retained as a Formula Lab source.
    formula_raw_path: Option<PathBuf>,
    /// Background WRF diagnostic/product processing.
    wrf_process: Option<WrfProcessTask>,
    /// Short WRF open/process status shown in the toolbar.
    wrf_process_status: Option<String>,
    /// Persistent local WRF product-processing profile.
    wrf_options: WrfProcessOptions,
    /// Edit buffers for WRF product filters.
    wrf_options_ui: WrfProcessSettingsUi,
    /// Toggle for the WRF processing settings window.
    show_wrf_options: bool,
    /// State for the collapsible Storage settings section.
    storage_ui: StorageSettingsUi,
    /// Pending JSON to write via `App::save` on the next eframe save tick.
    ///
    /// Set by `StorageSettingsUi` when the user clicks Apply; drained in
    /// `App::save` which eframe calls after every frame (and on exit).
    pending_persist: Option<String>,
    /// Pending saved-domain JSON written by the native plot panel.
    pending_domain_persist: Option<String>,
    /// Pending color table/product binding JSON.
    pending_style_persist: Option<String>,
    /// Pending WRF processing options JSON.
    pending_wrf_options_persist: Option<String>,
    #[cfg(feature = "profiling")]
    profiler: profiler::ProfilerPanel,
    #[cfg(feature = "profiling")]
    show_profiler: bool,
    /// Serves frames to the external puffin_viewer while profiling.
    #[cfg(feature = "profiling")]
    _puffin_server: Option<puffin_http::Server>,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        store_root: PathBuf,
        overrides: SpecOverrides,
        show_satellite: bool,
        cli_store: Option<String>,
        cli_cache: Option<String>,
    ) -> Self {
        // Belt and braces: pre-build the GLOBAL rayon pool small and
        // below-normal so any stray par_iter reached outside the ingest
        // worker's dedicated pool (e.g. a rustwx-products helper called
        // from the store worker) cannot saturate all cores at normal
        // priority. The ingest compute itself rides the dedicated pool.
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(rw_ingest::throttle::polite_thread_count(None))
            .thread_name(|index| format!("rw-global-{index}"))
            .start_handler(|_| {
                rw_ingest::throttle::set_current_thread_background_priority();
            })
            .build_global();

        let ctx = cc.egui_ctx.clone();
        let worker = StoreWorker::spawn(StoreView::new(&store_root), move || {
            ctx.request_repaint();
        });
        let mut startup_errors = worker
            .startup_error()
            .map(str::to_string)
            .into_iter()
            .collect::<Vec<_>>();
        let style_settings = cc
            .storage
            .and_then(|storage| {
                storage
                    .get_string(STYLE_STORAGE_KEY)
                    .map(|value| deserialize_style_settings(&value))
            })
            .unwrap_or_default()
            .normalized();
        let mut color_tables = ColorTableEditorPanel::new();
        color_tables.set_settings(style_settings.clone());
        worker.send(StoreRequest::SetStyleOverrides(style_settings));
        worker.send(StoreRequest::Enumerate);

        let ctx = cc.egui_ctx.clone();
        let ingest = IngestWorker::spawn(store_root.clone(), move || {
            ctx.request_repaint();
        });
        if let Some(error) = ingest.startup_error() {
            startup_errors.push(error.to_string());
        }

        // Satellite frames live under their own subroot so the model-run
        // browser stays free of sat runs.
        let ctx = cc.egui_ctx.clone();
        let sat = SatWorker::spawn(store_root.join("sat"), move || {
            ctx.request_repaint();
        });
        if let Some(error) = sat.startup_error() {
            startup_errors.push(error.to_string());
        }
        let mut sat_panel = SatellitePanel::new(SatFollowSpec::default());
        sat_panel.set_satellite_options(sat_worker::satellite_options());
        sat_panel.set_sector_options(sat_worker::sector_options());
        sat_panel.set_layer_options(sat_worker::layer_options());

        // Resolve the full StoragePaths so the settings UI shows correct
        // source labels (cli / saved / default).
        let saved = cc
            .storage
            .and_then(|s| s.get_string(STORAGE_KEY).map(|v| deserialize_persisted(&v)));
        let paths =
            resolve_storage_paths(cli_store.as_deref(), cli_cache.as_deref(), saved.as_ref());
        let cache_dir = paths.cache_dir.clone();

        let defaults = DownloadSpec::default();
        let mut spec = DownloadSpec {
            date: overrides.date.unwrap_or_else(rw_ui::today_yyyymmdd_utc),
            hours: overrides.hours.unwrap_or_else(|| "0-6".to_string()),
            cycle: overrides.cycle.unwrap_or(defaults.cycle),
            profile: overrides.profile.unwrap_or(defaults.profile),
            cache_dir: overrides
                .cache_dir
                .unwrap_or_else(|| cache_dir.display().to_string()),
            ..defaults
        };
        // Presets follow the same toggle-snapping the profile combo does.
        match spec.profile.as_str() {
            "sounding" => {
                spec.derived = false;
                spec.heavy = false;
            }
            "view" => {
                spec.derived = true;
                spec.heavy = false;
            }
            _ => {}
        }
        spec = normalize_download_spec(spec);
        let mut download = DownloadPanel::new(spec.clone());
        download.set_model_options(model_options());
        Self::sync_run_pickers(&mut download, &spec);
        // Seed the live estimate for the default spec.
        ingest.send(IngestRequest::Estimate(spec));

        #[cfg(feature = "profiling")]
        let puffin_server = match puffin_http::Server::new("127.0.0.1:8585") {
            Ok(server) => {
                eprintln!("puffin server on 127.0.0.1:8585 (connect puffin_viewer)");
                Some(server)
            }
            Err(err) => {
                eprintln!("puffin server failed to start: {err}");
                None
            }
        };
        // Scope recording on by default when profiling is compiled in —
        // otherwise the profiler panel and viewer show empty data until the
        // "record scopes" switch is found (review finding).
        #[cfg(feature = "profiling")]
        puffin::set_scopes_on(true);

        let storage_ui = StorageSettingsUi::new(&paths);
        let saved_domains = cc
            .storage
            .and_then(|storage| {
                storage
                    .get_string(DOMAIN_STORAGE_KEY)
                    .map(|value| deserialize_custom_domains(&value))
            })
            .unwrap_or_default();
        let mut plot_viewer = PlotViewerPanel::new();
        plot_viewer.set_saved_domains(saved_domains);
        let wrf_options = cc
            .storage
            .and_then(|storage| {
                storage
                    .get_string(WRF_PROCESS_STORAGE_KEY)
                    .map(|value| deserialize_wrf_process_options(&value))
            })
            .unwrap_or_default()
            .normalized();
        let wrf_options_ui = WrfProcessSettingsUi::new(&wrf_options);

        Self {
            worker,
            ingest,
            store_root,
            cache_dir,
            gdex: GdexBrowser::new(),
            formula_lab: FormulaLabPanel::new(),
            tree: None,
            browser: RunBrowserPanel::new(),
            viewer: FieldViewerPanel::new(),
            plot_viewer,
            show_plot_viewer: true,
            batch_render: BatchRenderPanel::new(),
            show_batch_render: false,
            color_tables,
            show_color_tables: false,
            sounding: SoundingPanel::new(),
            download,
            download_start_pending: false,
            show_download: false,
            sat,
            sat_panel,
            sat_player: SatPlayerPanel::new(),
            show_satellite,
            sat_initialized: false,
            frame_ms: 0.0,
            recorded_texture_ms: None,
            recorded_plot_timings: None,
            recorded_sat_texture_ms: None,
            local_import: None,
            local_import_status: (!startup_errors.is_empty()).then(|| startup_errors.join("; ")),
            pending_auto_imports: VecDeque::new(),
            import_size_probe: None,
            pending_heavy_import: None,
            pending_light_import: None,
            pending_wrf_paths: Vec::new(),
            formula_raw_path: None,
            wrf_process: None,
            wrf_process_status: None,
            wrf_options,
            wrf_options_ui,
            show_wrf_options: false,
            storage_ui,
            pending_persist: None,
            pending_domain_persist: None,
            pending_style_persist: None,
            pending_wrf_options_persist: None,
            #[cfg(feature = "profiling")]
            profiler: profiler::ProfilerPanel::default(),
            #[cfg(feature = "profiling")]
            show_profiler: false,
            #[cfg(feature = "profiling")]
            _puffin_server: puffin_server,
        }
    }

    fn file_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("File", |ui| {
            if ui.button("Open File(s)...").clicked() {
                if let Some(paths) = rfd::FileDialog::new()
                    .set_title("Open WRF file(s)")
                    .pick_files()
                {
                    let supported = paths
                        .into_iter()
                        .filter(|path| wrf_process::is_supported_wrf_file(path))
                        .collect::<Vec<_>>();
                    self.stage_wrf_paths(supported);
                }
                ui.close();
            }

            if ui
                .add_enabled(
                    !self.pending_wrf_paths.is_empty()
                        && self.wrf_process.is_none()
                        && self.local_import.is_none()
                        && self.import_size_probe.is_none()
                        && !self.download.is_running()
                        && !self.download_start_pending
                        && !self.batch_render.is_running()
                        && !self.formula_lab.busy()
                        && self.pending_heavy_import.is_none()
                        && self.pending_light_import.is_none(),
                    egui::Button::new("Process Open WRF File(s)"),
                )
                .clicked()
            {
                self.start_wrf_process();
                ui.close();
            }

            if ui.button("WRF Processing Settings...").clicked() {
                self.show_wrf_options = true;
                ui.close();
            }

            if ui.button("Open Folder...").clicked() {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Open store or WRF folder")
                    .pick_folder()
                {
                    if looks_like_rw_store_root(&folder) {
                        self.switch_store_root(folder, ui.ctx());
                    } else {
                        let files = wrf_process::wrf_files_in_folder(&folder);
                        if files.is_empty() {
                            self.wrf_process_status = Some(format!(
                                "No supported WRF files found in {}",
                                folder.display()
                            ));
                        } else {
                            self.stage_wrf_paths(files);
                        }
                    }
                }
                ui.close();
            }

            ui.separator();

            if ui.button("Color Tables...").clicked() {
                self.show_color_tables = true;
                ui.close();
            }

            ui.separator();

            if ui.button("Import NetCDF/WRF/GRIB1 File(s)...").clicked() {
                if let Some(paths) = rfd::FileDialog::new()
                    .set_title("Import NetCDF, WRF, or GRIB1 file(s)")
                    .pick_files()
                {
                    let supported = paths
                        .into_iter()
                        .filter(|path| local_import::is_supported_model_file(path))
                        .collect::<Vec<_>>();
                    if supported.is_empty() {
                        self.local_import_status =
                            Some("No supported WRF/NetCDF/GRIB1 files selected".to_string());
                    } else {
                        self.start_local_import(supported);
                    }
                }
                ui.close();
            }

            if ui.button("Import NetCDF/WRF/GRIB1 Folder...").clicked() {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Import NetCDF, WRF, or GRIB1 folder")
                    .pick_folder()
                {
                    let files = local_import::supported_files_in_folder(&folder);
                    if files.is_empty() {
                        self.local_import_status = Some(format!(
                            "No supported WRF/NetCDF/GRIB1 files found in {}",
                            folder.display()
                        ));
                    } else {
                        self.start_local_import(files);
                    }
                }
                ui.close();
            }
        });
    }

    fn switch_store_root(&mut self, store_root: PathBuf, ctx: &egui::Context) {
        if self.local_import.is_some()
            || self.wrf_process.is_some()
            || self.import_size_probe.is_some()
            || self.download.is_running()
            || self.download_start_pending
            || self.batch_render.is_running()
            || self.formula_lab.busy()
            || self.pending_heavy_import.is_some()
            || self.pending_light_import.is_some()
        {
            self.local_import_status = Some(
                "Wait for the active model download/import, Formula Lab evaluation, batch render, or confirmation before switching stores"
                    .to_string(),
            );
            return;
        }
        if let Err(err) = std::fs::create_dir_all(&store_root) {
            self.local_import_status = Some(format!("Open folder failed: {err}"));
            return;
        }
        let repaint = ctx.clone();
        self.worker = StoreWorker::spawn(StoreView::new(&store_root), move || {
            repaint.request_repaint();
        });
        let mut startup_errors = self
            .worker
            .startup_error()
            .map(str::to_string)
            .into_iter()
            .collect::<Vec<_>>();
        self.worker.send(StoreRequest::SetStyleOverrides(
            self.color_tables.settings().clone(),
        ));
        self.worker.send(StoreRequest::Enumerate);

        let repaint = ctx.clone();
        self.ingest = IngestWorker::spawn(store_root.clone(), move || {
            repaint.request_repaint();
        });
        if let Some(error) = self.ingest.startup_error() {
            startup_errors.push(error.to_string());
        }

        self.sat.stop_follow();
        let repaint = ctx.clone();
        self.sat = SatWorker::spawn(store_root.join("sat"), move || {
            repaint.request_repaint();
        });
        if let Some(error) = self.sat.startup_error() {
            startup_errors.push(error.to_string());
        }
        self.sat.send(SatRequest::Scan);

        self.store_root = store_root.clone();
        self.tree = None;
        self.browser = RunBrowserPanel::new();
        self.viewer.clear();
        self.plot_viewer.clear();
        self.sounding.clear();
        self.sat_player = SatPlayerPanel::new();
        self.sat_initialized = false;
        self.recorded_texture_ms = None;
        self.recorded_plot_timings = None;
        self.recorded_sat_texture_ms = None;
        self.pending_wrf_paths.clear();
        self.pending_auto_imports.clear();
        self.import_size_probe = None;
        self.download_start_pending = false;
        self.pending_heavy_import = None;
        self.pending_light_import = None;
        self.wrf_process = None;
        self.wrf_process_status = None;

        self.storage_ui.store_root_edit = store_root.display().to_string();
        self.storage_ui.store_root_source = PathSource::Saved;
        self.storage_ui.store_size = dir_size_bytes(&store_root);
        self.storage_ui.sizes_computed = true;
        self.storage_ui.apply_error = None;
        self.storage_ui.apply_status = Some("Opened for this session".to_string());

        self.pending_persist = Some(serialize_persisted(&PersistedPaths {
            store_root: Some(store_root.display().to_string()),
            cache_dir: Some(self.cache_dir.display().to_string()),
        }));
        self.local_import_status = Some(if startup_errors.is_empty() {
            format!("Opened {}", store_root.display())
        } else {
            startup_errors.join("; ")
        });
    }

    fn stage_wrf_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.wrf_process_status = Some("No supported WRF files selected".to_string());
            return;
        }
        self.pending_heavy_import = None;
        self.formula_raw_path = paths.first().cloned();
        let count = paths.len();
        let first = paths
            .first()
            .and_then(|path| path.file_name())
            .and_then(|value| value.to_str())
            .unwrap_or("WRF file")
            .to_string();
        self.pending_wrf_paths = paths;
        self.wrf_process_status = if count == 1 {
            Some(format!("Ready to process {first}"))
        } else {
            Some(format!(
                "Ready to process {count} WRF files starting with {first}"
            ))
        };
    }

    fn start_wrf_process(&mut self) {
        if self.wrf_process.is_some()
            || self.local_import.is_some()
            || self.import_size_probe.is_some()
            || self.download.is_running()
            || self.download_start_pending
            || self.formula_lab.busy()
            || self.batch_render.is_running()
        {
            self.wrf_process_status = Some(
                "Another model import, Formula Lab evaluation, or batch render is active"
                    .to_string(),
            );
            return;
        }
        if self.pending_wrf_paths.is_empty() {
            self.wrf_process_status = Some("Open WRF file(s) first".to_string());
            return;
        }
        if self.pending_heavy_import.is_some() || self.pending_light_import.is_some() {
            self.wrf_process_status = Some("Finish the open import confirmation first".to_string());
            return;
        }
        let files = self.pending_wrf_paths.clone();
        self.start_import_size_probe(ImportProbeLaunch::wrf(files, self.wrf_options.clone()));
    }

    fn launch_wrf_process(&mut self, files: Vec<PathBuf>, options: WrfProcessOptions) {
        if self.wrf_process.is_some()
            || self.local_import.is_some()
            || self.import_size_probe.is_some()
            || self.download.is_running()
            || self.download_start_pending
            || self.formula_lab.busy()
            || self.batch_render.is_running()
            || self.pending_heavy_import.is_some()
            || self.pending_light_import.is_some()
        {
            self.wrf_process_status = Some(
                "Another model import, Formula Lab evaluation, or batch render is active"
                    .to_string(),
            );
            return;
        }
        let task = wrf_process::spawn_process_paths(files, self.store_root.clone(), options);
        self.wrf_process_status = Some(task.label.clone());
        self.wrf_process = Some(task);
    }

    fn apply_color_table_changes(&mut self) {
        let settings = self.color_tables.settings().clone().normalized();
        self.worker
            .send(StoreRequest::SetStyleOverrides(settings.clone()));
        self.pending_style_persist = Some(serialize_style_settings(&settings));
        self.plot_viewer.clear();
        self.recorded_plot_timings = None;
        if let Some(field) = self.viewer.wanted_field() {
            if !self.viewer.restore_generated_field(&field.var) {
                self.viewer.set_loading(&field.var);
                self.worker.send(StoreRequest::LoadField(field));
            }
        }
    }

    fn persist_wrf_process_options(&mut self) {
        self.wrf_options = self.wrf_options.clone().normalized();
        self.wrf_options_ui.set_from_options(&self.wrf_options);
        self.pending_wrf_options_persist = Some(serialize_wrf_process_options(&self.wrf_options));
    }

    fn start_local_import(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.local_import_status = Some("No supported model files selected".to_string());
            return;
        }
        if self.local_import.is_some()
            || self.wrf_process.is_some()
            || self.import_size_probe.is_some()
            || self.download.is_running()
            || self.download_start_pending
            || self.formula_lab.busy()
            || self.batch_render.is_running()
        {
            self.local_import_status = Some(
                "Another model import, Formula Lab evaluation, or batch render is active"
                    .to_string(),
            );
            return;
        }
        if self.pending_heavy_import.is_some() || self.pending_light_import.is_some() {
            self.local_import_status =
                Some("Finish the open import confirmation first".to_string());
            return;
        }
        self.start_import_size_probe(ImportProbeLaunch::local(paths));
    }

    fn launch_local_import(&mut self, paths: Vec<PathBuf>) {
        if self.local_import.is_some()
            || self.wrf_process.is_some()
            || self.import_size_probe.is_some()
            || self.download.is_running()
            || self.download_start_pending
            || self.formula_lab.busy()
            || self.batch_render.is_running()
            || self.pending_heavy_import.is_some()
            || self.pending_light_import.is_some()
        {
            self.local_import_status = Some(
                "Another model import, Formula Lab evaluation, or batch render is active"
                    .to_string(),
            );
            return;
        }
        let task = local_import::spawn_import_paths(paths, self.store_root.clone());
        self.local_import_status = Some(task.label.clone());
        self.local_import = Some(task);
    }

    fn start_import_size_probe(&mut self, launch: ImportProbeLaunch) {
        let is_wrf = matches!(&launch, ImportProbeLaunch::Wrf { .. });
        match ImportSizeProbeTask::spawn(launch) {
            Ok(task) => {
                let label = task.label.clone();
                self.import_size_probe = Some(task);
                if is_wrf {
                    self.wrf_process_status = Some(label);
                } else {
                    self.local_import_status = Some(label);
                }
            }
            Err(error) => {
                if is_wrf {
                    self.wrf_process_status = Some(error);
                } else {
                    self.local_import_status = Some(error);
                }
            }
        }
    }

    fn handle_import_size_probe_response(&mut self) {
        let received = match self.import_size_probe.as_ref() {
            Some(task) => task.rx.try_recv(),
            None => return,
        };
        let outcome = match received {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                let Some(task) = self.import_size_probe.take() else {
                    return;
                };
                let message = "Import size probe stopped before returning a result".to_string();
                match task.launch {
                    ImportProbeLaunch::Wrf { .. } => self.wrf_process_status = Some(message),
                    ImportProbeLaunch::Local { .. } => self.local_import_status = Some(message),
                }
                return;
            }
        };
        let Some(task) = self.import_size_probe.take() else {
            return;
        };
        let file_count = task.launch.files().len();
        let assessment = outcome.unwrap_or_else(|error| {
            ImportSizeAssessment::failed(
                file_count,
                format!("background import size probe failed: {error}"),
            )
        });

        if self.local_import.is_some()
            || self.wrf_process.is_some()
            || self.download.is_running()
            || self.download_start_pending
            || self.formula_lab.busy()
            || self.batch_render.is_running()
            || self.pending_heavy_import.is_some()
            || self.pending_light_import.is_some()
        {
            let message =
                "Import size probe finished after another import became active; start it again"
                    .to_string();
            match task.launch {
                ImportProbeLaunch::Wrf { .. } => self.wrf_process_status = Some(message),
                ImportProbeLaunch::Local { .. } => self.local_import_status = Some(message),
            }
            return;
        }

        match task.launch {
            ImportProbeLaunch::Wrf { files, options } => {
                if normalize_import_probe_files(self.pending_wrf_paths.clone()) != files {
                    self.wrf_process_status = Some(
                        "The staged WRF selection changed during size inspection; process it again"
                            .to_string(),
                    );
                    return;
                }
                if let Some(warning) = heavy_import_size_warning(&assessment) {
                    self.pending_heavy_import = Some(PendingWrfImport {
                        files,
                        warning,
                        wrf_options: Some(options),
                    });
                    self.wrf_process_status = None;
                } else {
                    self.launch_wrf_process(files, options);
                }
            }
            ImportProbeLaunch::Local { files } => {
                if let Some(warning) = light_import_size_warning(&assessment) {
                    self.pending_light_import = Some(PendingWrfImport {
                        files,
                        warning,
                        wrf_options: None,
                    });
                    self.local_import_status = None;
                } else {
                    self.launch_local_import(files);
                }
            }
        }
    }

    fn show_import_confirmations(&mut self, ctx: &egui::Context) {
        if let Some(pending) = &self.pending_heavy_import {
            let warning = pending.warning.clone();
            let count = pending.files.len();
            let mut action = 0u8;
            egui::Window::new("Large WRF full-diagnostics import")
                .collapsible(false)
                .resizable(true)
                .default_width(560.0)
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new("Large WRF import").strong());
                    ui.label(warning);
                    ui.label(
                        egui::RichText::new(format!(
                            "The selection contains {count} file(s). Core-only keeps surface fields and sounding volumes while skipping severe diagnostics and raw extras."
                        ))
                        .small()
                        .weak(),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Start core-only").clicked() {
                            action = 1;
                        }
                        if ui.button("Start full selection anyway").clicked() {
                            action = 2;
                        }
                        if ui.button("Cancel").clicked() {
                            action = 3;
                        }
                    });
                });
            match action {
                1 => {
                    if let Some(pending) = self.pending_heavy_import.take() {
                        self.launch_wrf_process(
                            pending.files,
                            WrfProcessOptions {
                                core_fields: true,
                                diagnostics: false,
                                heavy_ecape: false,
                                raw_extras: false,
                                only: Vec::new(),
                                skip: Vec::new(),
                            },
                        );
                    }
                }
                2 => {
                    if let Some(pending) = self.pending_heavy_import.take() {
                        let options = pending
                            .wrf_options
                            .unwrap_or_else(|| self.wrf_options.clone());
                        self.launch_wrf_process(pending.files, options);
                    }
                }
                3 => {
                    self.pending_heavy_import = None;
                    self.wrf_process_status = Some("Large WRF import cancelled".to_string());
                }
                _ => {}
            }
        }

        if let Some(pending) = &self.pending_light_import {
            let warning = pending.warning.clone();
            let mut action = 0u8;
            egui::Window::new("Large WRF/NetCDF import")
                .collapsible(false)
                .resizable(true)
                .default_width(560.0)
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new("Large model import").strong());
                    ui.label(warning);
                    ui.horizontal(|ui| {
                        if ui.button("Import anyway").clicked() {
                            action = 1;
                        }
                        if ui.button("Cancel").clicked() {
                            action = 2;
                        }
                    });
                });
            match action {
                1 => {
                    if let Some(pending) = self.pending_light_import.take() {
                        self.launch_local_import(pending.files);
                    }
                }
                2 => {
                    self.pending_light_import = None;
                    self.local_import_status = Some("Large model import cancelled".to_string());
                }
                _ => {}
            }
        }
    }

    fn handle_local_import_response(&mut self, ctx: &egui::Context) {
        let Some(task) = self.local_import.take() else {
            return;
        };

        let mut finished = false;
        loop {
            match task.rx.try_recv() {
                Ok(LocalImportMessage::Progress(message)) => {
                    self.local_import_status = Some(message);
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
                Ok(LocalImportMessage::Done(Ok(summary))) => {
                    self.local_import_status = Some(Self::local_import_summary_text(&summary));
                    self.worker.send(StoreRequest::Enumerate);
                    finished = true;
                    break;
                }
                Ok(LocalImportMessage::Done(Err(message))) => {
                    self.local_import_status = Some(format!("Import failed: {message}"));
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(250));
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.local_import_status = Some("Import worker stopped".to_string());
                    finished = true;
                    break;
                }
            }
        }

        if !finished {
            self.local_import = Some(task);
        }
    }

    fn handle_wrf_process_response(&mut self, ctx: &egui::Context) {
        let Some(task) = self.wrf_process.take() else {
            return;
        };

        let mut finished = false;
        loop {
            match task.rx.try_recv() {
                Ok(WrfProcessMessage::Progress(message)) => {
                    self.wrf_process_status = Some(message);
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
                Ok(WrfProcessMessage::Done(Ok(summary))) => {
                    self.wrf_process_status = Some(Self::wrf_process_summary_text(&summary));
                    self.pending_wrf_paths.clear();
                    self.worker.send(StoreRequest::Enumerate);
                    finished = true;
                    break;
                }
                Ok(WrfProcessMessage::Done(Err(message))) => {
                    self.wrf_process_status = Some(format!("WRF process failed: {message}"));
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(250));
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.wrf_process_status = Some("WRF processor stopped".to_string());
                    finished = true;
                    break;
                }
            }
        }

        if !finished {
            self.wrf_process = Some(task);
        }
    }

    fn local_import_summary_text(summary: &LocalImportSummary) -> String {
        let shown = summary
            .variables
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let extra = summary.variables.len().saturating_sub(6);
        let suffix = if extra == 0 {
            String::new()
        } else {
            format!(", +{extra} more")
        };
        let notes = if summary.notes.is_empty() {
            String::new()
        } else {
            let shown = summary
                .notes
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ");
            let extra = summary.notes.len().saturating_sub(3);
            let suffix = if extra == 0 {
                String::new()
            } else {
                format!(" | +{extra} more")
            };
            format!("; {} warning(s): {shown}{suffix}", summary.notes.len())
        };
        format!(
            "Imported {}/{} local files into {}/{} under {} ({} vars: {}{}){}",
            summary.hours_written,
            summary.files_seen,
            summary.model,
            summary.run,
            summary.store_root.display(),
            summary.variables.len(),
            shown,
            suffix,
            notes
        )
    }

    fn wrf_process_summary_text(summary: &WrfProcessSummary) -> String {
        let shown = summary
            .variables
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let extra = summary.variables.len().saturating_sub(6);
        let suffix = if extra == 0 {
            String::new()
        } else {
            format!(", +{extra} more")
        };
        let note_suffix = if summary.notes.is_empty() {
            String::new()
        } else {
            format!("; {} skipped/unavailable", summary.notes.len())
        };
        format!(
            "Processed {} WRF hour(s) from {} file(s) into {}/{} under {} ({} vars: {}{}{})",
            summary.hours_written,
            summary.files_seen,
            summary.model,
            summary.run,
            summary.store_root.display(),
            summary.variables.len(),
            shown,
            suffix,
            note_suffix
        )
    }

    /// Cycle list, source list, and hours hint follow the spec's model +
    /// cycle (static catalog data, no network).
    fn sync_run_pickers(download: &mut DownloadPanel, spec: &DownloadSpec) {
        let Ok(model) = spec.model.parse::<rustwx_core::ModelId>() else {
            return;
        };
        let summary = model_summary(model);
        download.set_cycle_options(summary.cycle_hours_utc.to_vec());
        let mut sources = vec!["auto".to_string()];
        sources.extend(summary.sources.iter().map(|source| source.id.to_string()));
        download.set_source_options(sources);
        let supported = supported_forecast_hours(model, spec.cycle);
        match (supported.first(), supported.last()) {
            (Some(first), Some(last)) => {
                // Add a model-aware cadence note when the stride changes within
                // the range (e.g. GFS: hourly ≤120, 3-hourly 123-384).
                let cadence_note = cadence_hint(model, spec.cycle);
                let hint = if cadence_note.is_empty() {
                    format!("supported: {first}-{last} ({:02}z)", spec.cycle)
                } else {
                    format!(
                        "supported: {first}-{last} ({:02}z) · {}",
                        spec.cycle, cadence_note
                    )
                };
                download.set_hours_hint(hint);
            }
            _ => download.set_hours_hint("no supported hours for this cycle".to_string()),
        }
    }

    fn apply_normalized_download_spec(&mut self, spec: DownloadSpec) -> DownloadSpec {
        let normalized = normalize_download_spec(spec);
        if self.download.spec() != &normalized {
            self.download.set_spec(normalized.clone());
        }
        normalized
    }

    fn select_hour(&mut self, key: HourKey) {
        self.plot_viewer.clear();
        self.recorded_plot_timings = None;
        self.worker.send(StoreRequest::LoadHour(key));
    }

    /// Drain store-worker responses into panel state.
    fn handle_responses(&mut self) {
        while let Some(response) = self.worker.try_recv() {
            match response {
                StoreResponse::Tree(tree) => {
                    // First scan: auto-select the first hour so a store with
                    // data shows something immediately.
                    if self.browser.selected().is_none() {
                        let first = tree.models.first().and_then(|model| {
                            model.runs.first().and_then(|run| {
                                run.hours.first().map(|hour| HourKey {
                                    model: model.model.clone(),
                                    run: run.run.clone(),
                                    hour: hour.hour,
                                })
                            })
                        });
                        if let Some(key) = first {
                            self.browser.select(key.clone());
                            self.select_hour(key);
                        }
                    }
                    self.tree = Some(tree);
                }
                StoreResponse::StyleOverridesApplied => {}
                StoreResponse::HourVars(key, Ok(vars)) => {
                    if self.browser.selected() == Some(&key) {
                        self.plot_viewer.clear();
                        self.recorded_plot_timings = None;
                        self.viewer.set_hour(key, vars);
                        if let Some(field) = self.viewer.wanted_field() {
                            if !self.viewer.restore_generated_field(&field.var) {
                                self.viewer.set_loading(&field.var);
                                self.worker.send(StoreRequest::LoadField(field));
                            }
                        }
                    }
                }
                StoreResponse::HourVars(_, Err(message)) => {
                    self.viewer.set_error(message);
                }
                StoreResponse::Field(key, result) => match *result {
                    Ok(field) => {
                        self.plot_viewer.clear();
                        self.recorded_plot_timings = None;
                        self.viewer.set_field(field);
                    }
                    Err(message) => {
                        if self.viewer.wanted_field().as_ref() == Some(&key) {
                            self.viewer.set_error(message);
                        }
                    }
                },
                StoreResponse::Sounding(_, Ok(data)) => {
                    self.worker.stats().record("sounding.read", data.read_ms);
                    self.sounding.set_data(data);
                    if let Some((read_ms, scene_ms)) = self.sounding.last_timings() {
                        self.worker.stats().record("sounding.scene", scene_ms);
                        self.worker
                            .stats()
                            .record("sounding.native_total", read_ms + scene_ms);
                    }
                }
                StoreResponse::Sounding(_, Err(message)) => {
                    self.sounding.set_error(message);
                }
            }
        }
    }

    /// Drain ingest-worker responses into the download panel (and refresh
    /// the run browser as hours land).
    fn handle_ingest_responses(&mut self) {
        while let Some(response) = self.ingest.try_recv() {
            match response {
                IngestResponse::Estimate(result) => match *result {
                    Ok(view) => self.download.set_estimate(view),
                    Err(message) => self.download.set_spec_error(message),
                },
                IngestResponse::Availability(view) => self.download.set_availability(view),
                IngestResponse::Latest { date, cycle } => {
                    self.download.set_latest(date, cycle);
                    let spec = self.download.spec().clone();
                    let spec = self.apply_normalized_download_spec(spec);
                    Self::sync_run_pickers(&mut self.download, &spec);
                    self.ingest.send(IngestRequest::Estimate(spec));
                }
                IngestResponse::LatestFailed(message) => {
                    self.download.set_probing_failed(message);
                }
                IngestResponse::Started { hours } => {
                    self.download_start_pending = false;
                    self.download.begin_run(&hours);
                }
                IngestResponse::StageStarted { hour, stage } => {
                    self.download.apply_stage_started(hour, stage);
                }
                IngestResponse::StageDone { hour, stage, ms } => {
                    self.worker
                        .stats()
                        .record(&format!("ingest.{}", stage.label()), ms as f32);
                    self.download.apply_stage_done(hour, stage, ms);
                }
                IngestResponse::Note(message) => {
                    self.download.apply_note(message);
                }
                IngestResponse::HourDone(done) => {
                    self.download.apply_hour_done(done);
                    // The hour is on disk and run.json is updated: refresh
                    // the run browser so it appears as it lands.
                    self.worker.send(StoreRequest::Enumerate);
                }
                IngestResponse::Finished => {
                    self.download_start_pending = false;
                    self.download.finish_run(Ok(()));
                    self.worker.send(StoreRequest::Enumerate);
                }
                IngestResponse::Cancelled => {
                    self.download_start_pending = false;
                    self.download.finish_cancelled();
                    self.worker.send(StoreRequest::Enumerate);
                }
                IngestResponse::Failed(message) => {
                    self.download_start_pending = false;
                    if self.download.is_running() {
                        self.download.finish_run(Err(message));
                    } else {
                        // Pre-start validation failure: a spec problem.
                        self.download.set_spec_error(message);
                    }
                }
            }
        }
    }

    /// Drain sat-worker responses into the satellite panels (and record
    /// the sat-path timings into the always-on stats registry).
    fn handle_sat_responses(&mut self) {
        while let Some(response) = self.sat.try_recv() {
            match response {
                SatResponse::SpecStatus(status) => self.sat_panel.set_spec_status(status),
                SatResponse::Runs(runs) => self.sat_player.set_runs(runs),
                SatResponse::FollowStarted => self.sat_panel.begin_follow(),
                SatResponse::FollowFinished(result) => {
                    if self.sat_panel.is_running() {
                        self.sat_panel.finish_follow(result);
                    } else if let Err(message) = result {
                        // Pre-start validation failure: a spec problem.
                        self.sat_panel.set_spec_status(Err(message));
                    }
                }
                SatResponse::PollDone { band, new_keys, ms } => {
                    self.worker.stats().record("sat.poll", ms as f32);
                    self.sat_panel.apply_poll_done(band, new_keys, ms);
                }
                SatResponse::DownloadStarted { id, label, bytes } => {
                    self.sat_panel.apply_download_started(id, label, bytes);
                }
                SatResponse::DownloadDone { id, ms, cache_hit } => {
                    self.worker.stats().record("sat.download", ms as f32);
                    self.sat_panel.apply_download_done(&id, ms, cache_hit);
                }
                SatResponse::FrameWritten {
                    id,
                    run,
                    hhmm,
                    bytes,
                    encode_ms,
                } => {
                    self.worker.stats().record("sat.encode", encode_ms as f32);
                    self.sat_panel
                        .apply_frame_written(&id, run, hhmm, bytes, encode_ms);
                    // The frame is on disk and run.json is updated: refresh
                    // the player's timeline so it appears as it lands.
                    self.sat.send(SatRequest::Scan);
                }
                SatResponse::Evicted { frames, bytes } => {
                    self.sat_panel.apply_evicted(frames, bytes);
                    // Evicted frames must leave the player's timeline too.
                    self.sat.send(SatRequest::Scan);
                }
                SatResponse::Sleeping { ms } => self.sat_panel.apply_sleeping(ms),
                SatResponse::Note(message) => self.sat_panel.apply_note(message),
                SatResponse::DiskUsage(usage) => self.sat_panel.set_disk_usage(usage),
                SatResponse::Frame { key, hhmm, result } => match *result {
                    Ok(frame) => {
                        self.worker.stats().record("sat.frame.read", frame.read_ms);
                        self.sat_player.set_frame(frame);
                    }
                    Err(message) => {
                        // Only clear the retry marker when the failure is
                        // for the run the player is actually showing.
                        if self.sat_player.selected_run() == Some(&key) {
                            self.sat_player.frame_failed(hhmm);
                        }
                        self.sat_panel.apply_note(format!("frame load: {message}"));
                    }
                },
            }
        }
    }

    fn handle_satellite_events(&mut self, events: Vec<SatelliteEvent>) {
        for event in events {
            match event {
                SatelliteEvent::SpecChanged(spec) => {
                    self.sat.send(SatRequest::Validate(spec));
                }
                SatelliteEvent::StartRequested(spec) => {
                    self.sat.send(SatRequest::Follow(spec));
                }
                SatelliteEvent::StopRequested => {
                    self.sat.stop_follow();
                }
            }
        }
    }

    fn handle_sat_player_events(&mut self, events: Vec<SatPlayerEvent>) {
        for event in events {
            match event {
                SatPlayerEvent::FrameWanted { key, hhmm } => {
                    self.sat.send(SatRequest::LoadFrame { key, hhmm });
                }
                SatPlayerEvent::FrameSelected { .. } => {}
                SatPlayerEvent::RefreshRequested => {
                    self.sat.send(SatRequest::Scan);
                }
            }
        }
    }

    fn handle_download_events(&mut self, events: Vec<DownloadEvent>) {
        for event in events {
            match event {
                DownloadEvent::SpecChanged(spec) => {
                    let spec = self.apply_normalized_download_spec(spec);
                    Self::sync_run_pickers(&mut self.download, &spec);
                    self.ingest.send(IngestRequest::Estimate(spec));
                }
                DownloadEvent::CheckAvailability(spec) => {
                    let spec = self.apply_normalized_download_spec(spec);
                    Self::sync_run_pickers(&mut self.download, &spec);
                    self.download.set_probing();
                    self.ingest.send(IngestRequest::Probe(spec));
                }
                DownloadEvent::LatestRequested(spec) => {
                    let spec = self.apply_normalized_download_spec(spec);
                    Self::sync_run_pickers(&mut self.download, &spec);
                    self.download.set_probing();
                    self.ingest.send(IngestRequest::Latest(spec));
                }
                DownloadEvent::StartRequested(spec) => {
                    if self.local_import.is_some()
                        || self.wrf_process.is_some()
                        || self.import_size_probe.is_some()
                        || self.pending_heavy_import.is_some()
                        || self.pending_light_import.is_some()
                        || self.download.is_running()
                        || self.download_start_pending
                        || self.formula_lab.busy()
                        || self.batch_render.is_running()
                    {
                        self.download.set_probing_failed(
                            "Finish the active model import, size confirmation, Formula Lab evaluation, or batch render before starting a download"
                                .to_string(),
                        );
                        continue;
                    }
                    let spec = self.apply_normalized_download_spec(spec);
                    Self::sync_run_pickers(&mut self.download, &spec);
                    self.download_start_pending = true;
                    self.ingest.send(IngestRequest::Start(spec));
                }
                DownloadEvent::CancelRequested => {
                    self.ingest.cancel();
                }
            }
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // Drain the pending JSON written by StorageSettingsUi on Apply.
        // eframe calls this after every frame and on clean exit, so the
        // value persists within one frame of the user clicking Apply.
        if let Some(json) = self.pending_persist.take() {
            storage.set_string(STORAGE_KEY, json);
        }
        if let Some(json) = self.pending_domain_persist.take() {
            storage.set_string(DOMAIN_STORAGE_KEY, json);
        }
        if let Some(json) = self.pending_style_persist.take() {
            storage.set_string(STYLE_STORAGE_KEY, json);
        }
        if let Some(json) = self.pending_wrf_options_persist.take() {
            storage.set_string(WRF_PROCESS_STORAGE_KEY, json);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        #[cfg(feature = "profiling")]
        puffin::GlobalProfiler::lock().new_frame();
        let frame_started = Instant::now();

        self.handle_responses();
        self.handle_ingest_responses();
        self.handle_sat_responses();
        self.handle_wrf_process_response(ui.ctx());
        self.handle_local_import_response(ui.ctx());
        self.handle_import_size_probe_response();
        if let Some(path) = self.gdex.poll(ui.ctx()) {
            self.pending_auto_imports.push_back(path);
        }
        if self.local_import.is_none()
            && self.wrf_process.is_none()
            && self.import_size_probe.is_none()
            && !self.download.is_running()
            && !self.download_start_pending
            && !self.formula_lab.busy()
            && !self.batch_render.is_running()
            && self.pending_heavy_import.is_none()
            && self.pending_light_import.is_none()
        {
            if let Some(path) = self.pending_auto_imports.pop_front() {
                self.start_local_import(vec![path]);
            }
        }
        if self.gdex.busy() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        if self.import_size_probe.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        // Smooth progress while a download runs, even through long silent
        // stages (a 60 s heavy stage emits nothing between its events).
        if self.download.is_running() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }
        // Keep the next-poll countdown and frame rows live during a follow
        // session (the engine sleeps between polls and emits nothing).
        if self.sat_panel.is_running() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }

        egui::Panel::top("rw-toolbar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                self.file_menu(ui);
                ui.separator();
                if !self.pending_wrf_paths.is_empty()
                    && self.wrf_process.is_none()
                    && self.local_import.is_none()
                    && self.import_size_probe.is_none()
                    && !self.download.is_running()
                    && !self.download_start_pending
                    && !self.formula_lab.busy()
                    && !self.batch_render.is_running()
                    && self.pending_heavy_import.is_none()
                    && self.pending_light_import.is_none()
                {
                    if ui
                        .button(format!("Process WRF ({})", self.pending_wrf_paths.len()))
                        .clicked()
                    {
                        self.start_wrf_process();
                    }
                }
                ui.toggle_value(&mut self.show_download, "⬇ Download");
                ui.toggle_value(&mut self.show_satellite, "🛰 Satellite");
                ui.toggle_value(&mut self.gdex.open, "GDEX");
                ui.toggle_value(&mut self.formula_lab.open, "Formula Lab");
                ui.toggle_value(&mut self.show_batch_render, "Batch render");
                ui.toggle_value(&mut self.show_wrf_options, "WRF products");
                ui.toggle_value(&mut self.show_color_tables, "Color tables");
                #[cfg(feature = "profiling")]
                ui.toggle_value(&mut self.show_profiler, "🔍 Profiler");
                #[cfg(not(feature = "profiling"))]
                ui.label(
                    egui::RichText::new("(profiler: build with --features profiling)")
                        .small()
                        .weak(),
                );
                if let Some(task) = &self.import_size_probe {
                    ui.spinner();
                    ui.label(egui::RichText::new(&task.label).small().weak());
                } else if self.download_start_pending {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("Starting model download")
                            .small()
                            .weak(),
                    );
                } else if let Some(task) = &self.wrf_process {
                    ui.spinner();
                    ui.label(egui::RichText::new(&task.label).small().weak());
                } else if let Some(task) = &self.local_import {
                    ui.spinner();
                    ui.label(egui::RichText::new(&task.label).small().weak());
                } else if let Some(status) = &self.wrf_process_status {
                    ui.label(egui::RichText::new(status).small().weak());
                } else if let Some(status) = &self.local_import_status {
                    ui.label(egui::RichText::new(status).small().weak());
                }
            });
        });

        egui::Panel::bottom("rw-stats").show_inside(ui, |ui| {
            rw_ui::stats::stats_strip(ui, self.frame_ms, self.worker.stats());
        });

        egui::Panel::left("rw-browser")
            .resizable(true)
            .default_size(260.0)
            .min_size(220.0)
            .max_size(420.0)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.heading("Runs");
                    if ui.button("⟳").on_hover_text("re-scan the store").clicked() {
                        self.worker.send(StoreRequest::Enumerate);
                    }
                });
                ui.label(
                    egui::RichText::new(self.store_root.display().to_string())
                        .small()
                        .weak(),
                );
                ui.separator();

                // Storage settings: collapsible section for path config +
                // persistence.  Lives right below the store-root label so
                // users can find it near the path they want to change.
                let store_root = self.store_root.clone();
                let cache_dir = self.cache_dir.clone();
                if let Some(new_paths) = self.storage_ui.ui(ui, &store_root, &cache_dir) {
                    // Queue the JSON for App::save, which eframe calls after
                    // each frame and on clean exit.
                    self.pending_persist = Some(serialize_persisted(&new_paths));
                }

                ui.separator();

                let mut picked = None;
                match &self.tree {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("scanning store…");
                        });
                    }
                    Some(tree) if tree.models.is_empty() => {
                        ui.add_space(8.0);
                        ui.label(format!(
                            "No runs found under\n{}",
                            self.store_root.display()
                        ));
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Point --store-root at an rw-store directory, or \
                                 configure it in the Storage section above, run \
                                 with --synthetic for demo data, or use the \
                                 Download panel to ingest a run.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                    Some(tree) => {
                        let browser = &mut self.browser;
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            picked = browser.ui(ui, tree);
                        });
                    }
                }
                if let Some(key) = picked {
                    self.select_hour(key);
                }
            });

        if self.sounding.has_content() {
            egui::Panel::right("rw-sounding")
                .resizable(true)
                .default_size(560.0)
                .show_inside(ui, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.heading("Sounding");
                        if ui.button("✕").on_hover_text("close").clicked() {
                            self.sounding.clear();
                        }
                    });
                    ui.separator();
                    self.sounding.ui(ui);
                });
        }

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.toggle_value(&mut self.show_plot_viewer, "Native plot");
                let selected_store_hour = self.browser.selected().cloned();
                if selected_store_hour.as_ref() != self.viewer.hour() {
                    if let Some(hour) = selected_store_hour {
                        if ui.button("Return to selected store hour").clicked() {
                            self.select_hour(hour);
                        }
                    }
                }
            });
            ui.separator();

            if self.show_plot_viewer {
                self.plot_viewer.ui(ui, self.viewer.current_field());
                if self.plot_viewer.take_saved_domains_changed() {
                    self.pending_domain_persist =
                        Some(serialize_custom_domains(self.plot_viewer.saved_domains()));
                }
                if let Some((render_ms, upload_ms)) = self.plot_viewer.last_timings() {
                    let timings = (render_ms, upload_ms);
                    if self.recorded_plot_timings != Some(timings) {
                        self.worker.stats().record("plot.render", render_ms);
                        self.worker.stats().record("plot.upload", upload_ms);
                        self.recorded_plot_timings = Some(timings);
                    }
                }
                ui.separator();
            }

            match self.viewer.ui(ui) {
                Some(FieldViewerEvent::VarSelected(var)) => {
                    self.plot_viewer.clear();
                    self.recorded_plot_timings = None;
                    if !self.viewer.restore_generated_field(&var) {
                        self.viewer.set_loading(&var);
                        if let Some(field) = self.viewer.wanted_field() {
                            self.worker.send(StoreRequest::LoadField(field));
                        }
                    }
                }
                Some(FieldViewerEvent::PointClicked { fx, fy }) => {
                    if let Some(hour) = self.viewer.hour().cloned() {
                        if self.browser.selected() == Some(&hour) {
                            self.sounding.set_loading();
                            self.worker
                                .send(StoreRequest::LoadSounding { hour, fx, fy });
                        }
                    }
                }
                Some(FieldViewerEvent::DomainSelected(domain)) => {
                    self.show_plot_viewer = true;
                    self.plot_viewer.set_active_domain(domain);
                    self.recorded_plot_timings = None;
                }
                Some(FieldViewerEvent::DomainRotationChanged { rotation_deg }) => {
                    self.show_plot_viewer = true;
                    self.plot_viewer.set_active_domain_rotation(rotation_deg);
                    self.recorded_plot_timings = None;
                }
                None => {}
            }
            // Record texture-build walls once per change (the panel keeps
            // reporting the same value until the next build).
            if let Some(ms) = self.viewer.last_texture_ms() {
                if self.recorded_texture_ms != Some(ms) {
                    self.worker.stats().record("ui.texture", ms);
                    self.recorded_texture_ms = Some(ms);
                }
            }
        });

        if self.show_download {
            let mut open = self.show_download;
            let mut events = Vec::new();
            egui::Window::new("Download")
                .open(&mut open)
                .default_width(520.0)
                .resizable(true)
                .show(ui.ctx(), |ui| {
                    events = self.download.ui(ui);
                });
            self.show_download = open;
            self.handle_download_events(events);
        }

        if self.show_batch_render {
            let mut open = self.show_batch_render;
            let batch_start_blocked = (self.local_import.is_some()
                || self.wrf_process.is_some()
                || self.import_size_probe.is_some()
                || self.pending_heavy_import.is_some()
                || self.pending_light_import.is_some()
                || self.download.is_running()
                || self.download_start_pending
                || self.formula_lab.busy())
                .then_some(
                    "Finish the active import/download, size confirmation, or Formula Lab evaluation before rendering",
                );
            let current_hour = self.browser.selected().cloned();
            let current_var = if self.viewer.hour() == current_hour.as_ref() {
                self.viewer.selected_var().map(str::to_string)
            } else {
                None
            };
            egui::Window::new("Batch render")
                .open(&mut open)
                .default_width(720.0)
                .default_height(680.0)
                .resizable(true)
                .show(ui.ctx(), |ui| {
                    self.batch_render.ui(
                        ui,
                        &self.store_root,
                        current_hour.as_ref(),
                        current_var.as_deref(),
                        batch_start_blocked,
                    );
                });
            self.show_batch_render = open;
        }

        let store_formula_source =
            self.browser
                .selected()
                .cloned()
                .map(|hour| StoreFormulaSource {
                    store_root: self.store_root.clone(),
                    hour,
                    // rw-store v1 does not persist verified valid timestamps. Keep
                    // temporal derivatives disabled rather than infer a cadence.
                    exact_times: BTreeMap::new(),
                });
        let raw_formula_source = self.formula_raw_path.clone().map(|path| {
            let display_hour = HourKey {
                model: "raw-wrf".to_string(),
                run: path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.is_empty())
                    .unwrap_or("raw_wrf")
                    .to_string(),
                hour: 0,
            };
            RawWrfFormulaSource {
                path,
                initial_time_index: 0,
                display_hour,
            }
        });
        let formula_evaluation_blocked = (self.local_import.is_some()
            || self.wrf_process.is_some()
            || self.import_size_probe.is_some()
            || self.pending_heavy_import.is_some()
            || self.pending_light_import.is_some()
            || self.download.is_running()
            || self.download_start_pending
            || self.batch_render.is_running())
        .then_some("a model import/download, its size confirmation, or a batch render is active");
        if let Some(result) = self.formula_lab.show(
            ui.ctx(),
            FormulaLabSources {
                store: store_formula_source.as_ref(),
                raw_wrf: raw_formula_source.as_ref(),
                evaluation_blocked: formula_evaluation_blocked,
            },
        ) {
            let raw_result = matches!(&result.source, FormulaResultSource::RawWrf { .. });
            let still_current = (match &result.source {
                FormulaResultSource::Store { store_root, hour } => {
                    self.formula_lab.source_kind() == FormulaSourceKind::Store
                        && store_root == &self.store_root
                        && self.browser.selected() == Some(hour)
                }
                FormulaResultSource::RawWrf {
                    path, time_index, ..
                } => {
                    self.formula_lab.source_kind() == FormulaSourceKind::RawWrf
                        && self.formula_raw_path.as_ref() == Some(path)
                        && self.formula_lab.raw_time_index() == *time_index
                }
            }) && result.source.revision_is_current();
            if still_current {
                self.plot_viewer.clear();
                self.recorded_plot_timings = None;
                if raw_result {
                    self.sounding.clear();
                }
                self.viewer.install_generated_field(result.field);
            } else {
                self.formula_lab
                    .note_result_discarded("the selected data source changed while it ran");
            }
        }
        if self.formula_lab.busy() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        self.gdex.ui(ui, &self.cache_dir.join("gdex"));

        if self.show_satellite {
            if !self.sat_initialized {
                self.sat_initialized = true;
                self.sat
                    .send(SatRequest::Validate(self.sat_panel.spec().clone()));
                self.sat.send(SatRequest::Scan);
            }
            let mut open = self.show_satellite;
            let mut panel_events = Vec::new();
            let mut player_events = Vec::new();
            egui::Window::new("Satellite")
                .open(&mut open)
                .default_pos([40.0, 60.0])
                .default_width(900.0)
                .default_height(740.0)
                .resizable(true)
                .show(ui.ctx(), |ui| {
                    egui::CollapsingHeader::new("Follow live")
                        .id_salt("rw-sat-follow-section")
                        .default_open(true)
                        .show(ui, |ui| {
                            panel_events = self.sat_panel.ui(ui);
                        });
                    ui.separator();
                    player_events = self.sat_player.ui(ui);
                });
            self.show_satellite = open;
            self.handle_satellite_events(panel_events);
            self.handle_sat_player_events(player_events);
            // Record sat texture-upload walls once per change.
            if let Some(ms) = self.sat_player.last_texture_ms() {
                if self.recorded_sat_texture_ms != Some(ms) {
                    self.worker.stats().record("sat.texture", ms);
                    self.recorded_sat_texture_ms = Some(ms);
                }
            }
        }

        if self.show_color_tables {
            let mut open = self.show_color_tables;
            egui::Window::new("Color Tables")
                .open(&mut open)
                .default_width(760.0)
                .default_height(680.0)
                .resizable(true)
                .show(ui.ctx(), |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("rw-color-tables-window-scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.color_tables.ui(ui, self.viewer.current_field());
                        });
                });
            self.show_color_tables = open;
            if self.color_tables.take_changed() {
                self.apply_color_table_changes();
            }
        }

        if self.show_wrf_options {
            let mut open = self.show_wrf_options;
            let mut changed = false;
            egui::Window::new("WRF Products")
                .open(&mut open)
                .default_width(520.0)
                .resizable(true)
                .show(ui.ctx(), |ui| {
                    changed = self.wrf_options_ui.ui(ui, &mut self.wrf_options);
                });
            self.show_wrf_options = open;
            if changed {
                self.persist_wrf_process_options();
            }
        }

        self.show_import_confirmations(ui.ctx());

        #[cfg(feature = "profiling")]
        if self.show_profiler {
            let mut open = self.show_profiler;
            egui::Window::new("Profiler")
                .open(&mut open)
                .default_width(520.0)
                .resizable(true)
                .show(ui.ctx(), |ui| {
                    self.profiler.ui(ui);
                });
            self.show_profiler = open;
        }

        self.frame_ms = frame_started.elapsed().as_secs_f32() * 1000.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_abs_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rw-ui-{name}"))
    }

    #[test]
    fn import_probe_normalizes_duplicate_selected_paths() {
        let a = PathBuf::from("a.nc");
        let b = PathBuf::from("b.nc");
        assert_eq!(
            normalize_import_probe_files(vec![b.clone(), a.clone(), b]),
            vec![a, PathBuf::from("b.nc")]
        );
    }

    #[test]
    fn import_probe_uses_largest_record_from_every_selected_file() {
        let mut assessment = ImportSizeAssessment::new(2);
        assessment.include_geometry(
            Path::new("first-small.nc"),
            ProbedFileGeometry {
                shape: Some(vec![100, 100, 10]),
                record_elements: Some(100_000),
                records: 1,
                records_exact: true,
            },
        );
        assessment.include_geometry(
            Path::new("second-large.nc"),
            ProbedFileGeometry {
                shape: Some(vec![500, 500, 50]),
                record_elements: Some(12_500_000),
                records: 3,
                records_exact: true,
            },
        );

        assert!(assessment.needs_confirmation());
        assert_eq!(assessment.record_count, 4);
        let description = assessment.description().expect("large record warns");
        assert!(description.contains("second-large.nc"), "{description}");
        assert!(
            description.contains("4 distinct time record"),
            "{description}"
        );
    }

    #[test]
    fn import_probe_failure_fails_closed_to_confirmation() {
        let mut assessment = ImportSizeAssessment::new(1);
        assessment.include_probe_failure(Path::new("malformed.nc"), "metadata panic".to_string());
        assert!(assessment.needs_confirmation());
        let warning = light_import_size_warning(&assessment).expect("unknown size warns");
        assert!(warning.contains("could not be fully verified"), "{warning}");
        assert!(warning.contains("malformed.nc"), "{warning}");
    }

    #[test]
    fn import_probe_small_known_selection_launches_without_confirmation() {
        let mut assessment = ImportSizeAssessment::new(1);
        assessment.max_bytes = 64 * 1_024 * 1_024;
        assessment.include_geometry(
            Path::new("small.nc"),
            ProbedFileGeometry {
                shape: Some(vec![100, 100, 10]),
                record_elements: Some(100_000),
                records: 2,
                records_exact: true,
            },
        );
        assert!(!assessment.needs_confirmation());
        assert!(heavy_import_size_warning(&assessment).is_none());
        assert!(light_import_size_warning(&assessment).is_none());
    }

    #[test]
    fn import_probe_counts_multi_time_record_work_conservatively() {
        let mut assessment = ImportSizeAssessment::new(1);
        assessment.include_geometry(
            Path::new("many-times.nc"),
            ProbedFileGeometry {
                shape: Some(vec![200, 200, 50]),
                record_elements: Some(2_000_000),
                records: 6,
                records_exact: true,
            },
        );
        assert!(
            assessment.needs_confirmation(),
            "six individually-small records still represent a large processing workload"
        );
        let description = assessment.description().expect("total workload warns");
        assert!(description.contains("12M grid elements"), "{description}");
    }

    #[test]
    fn import_probe_shape_overflow_is_an_error() {
        let error = checked_shape_elements(&[usize::MAX, 2], "test")
            .expect_err("overflow must not wrap into a small selection");
        assert!(error.contains("overflow"), "{error}");
    }

    // ------------------------------------------------------------------
    // resolve_storage_paths: precedence unit tests
    // ------------------------------------------------------------------

    /// No CLI, no saved: resolve to built-in defaults.
    #[test]
    fn resolve_defaults_when_nothing_provided() {
        let paths = resolve_storage_paths(None, None, None);
        assert!(paths.store_root.ends_with(DEFAULT_STORE_ROOT));
        assert!(paths.cache_dir.ends_with("cache"));
        assert_eq!(paths.store_root_source, PathSource::Default);
        assert_eq!(paths.cache_dir_source, PathSource::Default);
    }

    #[test]
    fn store_discovery_finds_ancestor_store_from_release_dir() {
        let root = std::env::temp_dir().join(format!(
            "rw-store-discovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let release_dir = root.join("target").join("release-fast");
        let run_dir = root.join("store").join("hrrr").join("20260629_05z");
        std::fs::create_dir_all(&release_dir).unwrap();
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("run.json"), "{}").unwrap();

        let discovered = discover_existing_store_root_from([release_dir]).unwrap();
        assert_eq!(discovered, root.join("store"));

        let _ = std::fs::remove_dir_all(root);
    }

    /// CLI arg wins over both saved and default.
    #[test]
    fn cli_wins_over_saved_and_default() {
        let cli_store = test_abs_path("cli-store");
        let cli_cache = test_abs_path("cli-cache");
        let saved_store = test_abs_path("saved-store");
        let saved_cache = test_abs_path("saved-cache");
        let saved = PersistedPaths {
            store_root: Some(saved_store.display().to_string()),
            cache_dir: Some(saved_cache.display().to_string()),
        };
        let paths = resolve_storage_paths(
            Some(&cli_store.display().to_string()),
            Some(&cli_cache.display().to_string()),
            Some(&saved),
        );
        assert_eq!(paths.store_root, cli_store);
        assert_eq!(paths.cache_dir, cli_cache);
        assert_eq!(paths.store_root_source, PathSource::Cli);
        assert_eq!(paths.cache_dir_source, PathSource::Cli);
    }

    /// Saved value wins over default when no CLI arg.
    #[test]
    fn saved_wins_over_default() {
        let saved_store = test_abs_path("saved-store");
        let saved_cache = test_abs_path("saved-cache");
        let saved = PersistedPaths {
            store_root: Some(saved_store.display().to_string()),
            cache_dir: Some(saved_cache.display().to_string()),
        };
        let paths = resolve_storage_paths(None, None, Some(&saved));
        assert_eq!(paths.store_root, saved_store);
        assert_eq!(paths.cache_dir, saved_cache);
        assert_eq!(paths.store_root_source, PathSource::Saved);
        assert_eq!(paths.cache_dir_source, PathSource::Saved);
    }

    /// CLI wins for store_root; saved wins for cache_dir (independent fields).
    #[test]
    fn cli_and_saved_can_mix_per_field() {
        let cli_store = test_abs_path("cli-store");
        let saved_store = test_abs_path("saved-store");
        let saved_cache = test_abs_path("saved-cache");
        let saved = PersistedPaths {
            store_root: Some(saved_store.display().to_string()),
            cache_dir: Some(saved_cache.display().to_string()),
        };
        let paths =
            resolve_storage_paths(Some(&cli_store.display().to_string()), None, Some(&saved));
        assert_eq!(paths.store_root, cli_store);
        assert_eq!(paths.store_root_source, PathSource::Cli);
        assert_eq!(paths.cache_dir, saved_cache);
        assert_eq!(paths.cache_dir_source, PathSource::Saved);
    }

    /// Saved with `None` fields falls through to default for those fields.
    #[test]
    fn saved_none_fields_fall_through_to_default() {
        let saved_cache = test_abs_path("saved-cache");
        let saved = PersistedPaths {
            store_root: None,
            cache_dir: Some(saved_cache.display().to_string()),
        };
        let paths = resolve_storage_paths(None, None, Some(&saved));
        assert!(paths.store_root.ends_with(DEFAULT_STORE_ROOT));
        assert_eq!(paths.store_root_source, PathSource::Default);
        assert_eq!(paths.cache_dir, saved_cache);
        assert_eq!(paths.cache_dir_source, PathSource::Saved);
    }

    // ------------------------------------------------------------------
    // Persistence round-trip (no eframe context needed)
    // ------------------------------------------------------------------

    #[test]
    fn persist_roundtrip_both_fields() {
        let original = PersistedPaths {
            store_root: Some("C:\\Users\\drew\\store".to_string()),
            cache_dir: Some("out/cache".to_string()),
        };
        let json = serialize_persisted(&original);
        let decoded = deserialize_persisted(&json);
        assert_eq!(decoded, original);
    }

    #[test]
    fn persist_roundtrip_only_store_root() {
        let original = PersistedPaths {
            store_root: Some("/my/store".to_string()),
            cache_dir: None,
        };
        let json = serialize_persisted(&original);
        let decoded = deserialize_persisted(&json);
        assert_eq!(decoded, original);
    }

    #[test]
    fn persist_roundtrip_empty() {
        let original = PersistedPaths {
            store_root: None,
            cache_dir: None,
        };
        let json = serialize_persisted(&original);
        let decoded = deserialize_persisted(&json);
        assert_eq!(decoded, original);
    }

    #[test]
    fn persist_roundtrip_windows_backslash_path() {
        let original = PersistedPaths {
            store_root: Some("C:\\Users\\drew\\rw\\store".to_string()),
            cache_dir: Some("C:\\Temp\\cache".to_string()),
        };
        let json = serialize_persisted(&original);
        // The JSON must not contain bare backslashes (they'd break decoding).
        // Every backslash must appear as \\ in the JSON string value.
        let store_field_start = json.find("\"store_root\":\"").unwrap();
        let after_key = &json[store_field_start + "\"store_root\":\"".len()..];
        let end = after_key.find('"').unwrap();
        let encoded_value = &after_key[..end];
        assert!(
            !encoded_value.contains('\\') || encoded_value.contains("\\\\"),
            "backslashes must be escaped in JSON: {encoded_value}"
        );
        let decoded = deserialize_persisted(&json);
        assert_eq!(decoded, original);
    }

    #[test]
    fn persist_roundtrip_garbled_input_returns_none_fields() {
        // Garbage input must not panic; unrecognised fields return None.
        let decoded = deserialize_persisted("not json at all {{{}}}");
        assert_eq!(decoded.store_root, None);
        assert_eq!(decoded.cache_dir, None);
    }

    // ------------------------------------------------------------------
    // Path validation behavior
    // ------------------------------------------------------------------

    /// A non-existent path with a valid parent (relative, under temp) is
    /// accepted by create_dir_all without panicking.
    #[test]
    fn path_validation_creates_dir_without_panic() {
        let tmp = std::env::temp_dir().join("rw_ui_path_validation_test_dir");
        // Clean up in case a previous run left it
        let _ = std::fs::remove_dir_all(&tmp);
        let result = std::fs::create_dir_all(&tmp);
        assert!(
            result.is_ok(),
            "create_dir_all must succeed for a valid path"
        );
        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// An invalid path (null byte in path) produces an Err, not a panic.
    #[cfg(unix)]
    #[test]
    fn path_validation_bad_path_returns_error_not_panic() {
        // Null bytes in path names are invalid on Unix.
        let bad = PathBuf::from("/tmp/bad\x00path");
        let result = std::fs::create_dir_all(&bad);
        assert!(result.is_err(), "null byte in path must fail");
    }

    // ------------------------------------------------------------------
    // dir_size_bytes
    // ------------------------------------------------------------------

    #[test]
    fn dir_size_none_for_nonexistent() {
        let path = PathBuf::from("/this/path/definitely/does/not/exist/rw_test");
        assert_eq!(dir_size_bytes(&path), None);
    }

    #[test]
    fn dir_size_returns_some_for_existing_dir() {
        let tmp = std::env::temp_dir();
        // temp dir always exists; we just want Some(_) back.
        assert!(dir_size_bytes(&tmp).is_some());
    }

    // ------------------------------------------------------------------
    // format_bytes
    // ------------------------------------------------------------------

    #[test]
    fn format_bytes_ranges() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
    }

    // ------------------------------------------------------------------
    // Existing model / cadence tests (unchanged)
    // ------------------------------------------------------------------

    /// GFS is ingest-supported and therefore appears in model_options() as an
    /// enabled entry — the download picker un-greys it without any hardcoded
    /// special case.
    #[test]
    fn ingest_supported_model_options_are_enabled() {
        let options = model_options();
        for slug in [
            "hrrr",
            "hrrr-ak",
            "rap",
            "gfs",
            "gdas",
            "gefs",
            "aigfs",
            "aigefs",
            "hgefs",
            "ecmwf-open-data",
            "nam",
            "rrfs-a",
        ] {
            let option = options
                .iter()
                .find(|o| o.slug == slug)
                .unwrap_or_else(|| panic!("{slug} must appear in model options"));
            assert!(
                option.enabled,
                "{slug} must be enabled (ingest_supported is true)"
            );
            assert!(
                option.note.is_empty(),
                "enabled entries have no disabled note, got: {:?}",
                option.note
            );
        }
    }

    /// GFS cycle options from the model summary are exactly [0, 6, 12, 18].
    #[test]
    fn gfs_cycle_options_are_synoptic_only() {
        let summary = rustwx_models::model_summary(rustwx_core::ModelId::Gfs);
        assert_eq!(
            summary.cycle_hours_utc,
            &[0u8, 6, 12, 18],
            "GFS publishes only the four synoptic cycles"
        );
    }

    /// The hours hint for a GFS 00z cycle includes the cadence note so the
    /// user knows hours above 120 are 3-hourly.
    #[test]
    fn gfs_hours_hint_includes_cadence_note() {
        let hint = cadence_hint(rustwx_core::ModelId::Gfs, 0);
        assert!(
            !hint.is_empty(),
            "GFS cadence_hint must return a non-empty string"
        );
        assert!(
            hint.contains("120") && hint.contains("3"),
            "GFS cadence note must mention the f120 boundary and 3-hourly stride, got: {hint}"
        );
    }

    /// Non-GFS models (e.g. HRRR) get an empty cadence hint — the hint is
    /// only appended when non-empty, so HRRR's hours row stays clean.
    #[test]
    fn regional_hours_hints_include_non_uniform_cadence_notes() {
        let gefs = cadence_hint(rustwx_core::ModelId::Gefs, 0);
        assert!(
            gefs.contains("240") && gefs.contains("246-384"),
            "GEFS cadence note must mention the high-hour split, got: {gefs}"
        );

        let aigfs = cadence_hint(rustwx_core::ModelId::Aigfs, 0);
        assert!(
            aigfs.contains("6-hourly") && aigfs.contains("384"),
            "AI-GFS cadence note must mention 6-hourly range, got: {aigfs}"
        );

        let ecmwf = cadence_hint(rustwx_core::ModelId::EcmwfOpenData, 12);
        assert!(
            ecmwf.contains("00/12z") && ecmwf.contains("360"),
            "ECMWF cadence note must mention 00/12z longer horizon, got: {ecmwf}"
        );

        let rap = cadence_hint(rustwx_core::ModelId::Rap, 3);
        assert!(
            rap.contains("f051") && rap.contains("03/09/15/21"),
            "RAP cadence note must mention extended cycles, got: {rap}"
        );

        let nam = cadence_hint(rustwx_core::ModelId::Nam, 0);
        assert!(
            nam.contains("36") && nam.contains("39-84"),
            "NAM cadence note must mention the hourly/3-hourly split, got: {nam}"
        );
    }

    #[test]
    fn default_hour_range_normalizes_to_model_cadence() {
        assert_eq!(
            normalize_hour_spec_for_model(rustwx_core::ModelId::Aigefs, 0, "0-6"),
            Some("0,6".to_string())
        );
        assert_eq!(
            normalize_hour_spec_for_model(rustwx_core::ModelId::Gefs, 0, "0-6"),
            Some("0,3,6".to_string())
        );
        assert_eq!(
            normalize_hour_spec_for_model(rustwx_core::ModelId::Gfs, 0, "0-6"),
            None,
            "GFS accepts every hour in the default 0-6 range"
        );
        assert_eq!(
            normalize_hour_spec_for_model(rustwx_core::ModelId::Aigefs, 0, "1"),
            None,
            "a direct invalid hour should stay invalid and surface validation"
        );
    }

    #[test]
    fn hrrr_cadence_hint_is_empty() {
        let hint = cadence_hint(rustwx_core::ModelId::Hrrr, 0);
        assert!(
            hint.is_empty(),
            "HRRR has a uniform stride — no cadence note needed"
        );
    }

    // ------------------------------------------------------------------
    // eframe persistence feature gate
    // ------------------------------------------------------------------

    /// Verify that the eframe `persistence` feature is compiled in.
    ///
    /// `eframe::storage_dir` is `#[cfg(feature = "persistence")]`-gated and
    /// only exported from eframe's public API when that feature is enabled.
    /// If `Cargo.toml` carries only `eframe = "0.34"` (no features list) this
    /// test FAILS TO COMPILE with "unresolved import" — making the missing flag
    /// a hard compile-time failure rather than a silent runtime bug.
    ///
    /// Without the persistence feature:
    ///   - `cc.storage` is always `None` in `CreationContext`
    ///   - `App::save` is never called by eframe
    ///   - the entire persisted-settings path is a no-op at runtime
    ///
    /// Adding `features = ["persistence"]` is therefore load-bearing for the
    /// configurable storage-paths feature introduced in commit 90d72d8.
    #[test]
    fn eframe_persistence_feature_is_enabled() {
        // eframe::storage_dir is only compiled when the "persistence" feature
        // is active (see eframe/src/lib.rs: #[cfg(feature = "persistence")]).
        // Calling it with a dummy app-id verifies the symbol exists and that
        // the eframe dep was built with the flag.  The return value (the OS
        // config-dir path for the app) is not relevant to this check.
        let _ = eframe::storage_dir("rusty-weather-persistence-probe");
    }

    /// GFS store orientation: the 0.25° global grid is stored lat-descending
    /// (row 0 = 90°N, last row = 90°S), so lat_descending must be true and
    /// the viewer must NOT flip it. Requires the live GFS store.
    #[test]
    #[ignore = "requires the live GFS store at out/gfs_store"]
    fn gfs_store_field_is_north_to_south_lat_descending() {
        use rw_ui::{FieldKey, HourKey, StoreRequest, StoreResponse, StoreView, StoreWorker};
        use std::time::Duration;

        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let store_root = workspace.join("out/gfs_store");
        let view = StoreView::new(&store_root);
        let worker = StoreWorker::spawn(view, || {});
        let field_key = FieldKey {
            hour: HourKey {
                model: "gfs".to_string(),
                run: "20260611_00z".to_string(),
                hour: 0,
            },
            var: "temperature_2m".to_string(),
        };
        worker.send(StoreRequest::LoadField(field_key.clone()));
        match worker.recv_timeout(Duration::from_secs(30)) {
            Some(StoreResponse::Field(key, result)) => {
                assert_eq!(key, field_key);
                let field = result.expect("GFS temperature_2m loads from the live store");
                assert!(
                    field.lat_descending,
                    "GFS 0.25° global grid: row 0 must be 90°N (lat_descending = true)"
                );
                let grid = field.grid.as_ref().expect("grid.rwg attached");
                let first_row_lat = grid.lat[0];
                let last_row_lat = grid.lat[(grid.ny - 1) * grid.nx];
                assert!(
                    first_row_lat > last_row_lat,
                    "lat must decrease top-to-bottom: first={first_row_lat}, last={last_row_lat}"
                );
                assert!(
                    (89.5..=90.5).contains(&first_row_lat),
                    "first row must be near 90°N, got {first_row_lat}"
                );
            }
            other => panic!("expected Field response, got {other:?}"),
        }
    }
}
