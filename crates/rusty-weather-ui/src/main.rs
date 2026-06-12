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
//! `--store-root` defaults to `store`. `--cache-dir` presets the Download
//! panel's raw GRIB cache directory (default `out/cache`; point it at an
//! existing cache to ingest without network). The `--download-*` flags
//! preset the Download panel's pickers (handy for scripted/offline runs).
//! `--satellite` opens the Satellite window on launch. `--synthetic`
//! writes a tiny synthetic store to a temp directory and opens that
//! instead.
//!
//! Storage paths (`--store-root`, `--cache-dir`) are configurable in the
//! app via the "Storage" collapsible section in the left browser panel;
//! values are persisted across launches via eframe's built-in storage.
//! Precedence: CLI arg > persisted setting > built-in default ("store" /
//! "out/cache"). Relative paths resolve against the working directory.
//!
//! Profiling: build with `--features profiling` for puffin scopes, a
//! puffin_http server on 127.0.0.1:8585 (external `puffin_viewer`), and
//! the in-app scope-stats window. The stats strip is always available.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ingest_worker;
#[cfg(feature = "profiling")]
mod profiler;
mod sat_worker;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use eframe::egui;
use ingest_worker::{IngestRequest, IngestResponse, IngestWorker};
use rustwx_models::{model_summary, supported_forecast_hours, supported_models};
use rw_ui::{
    DownloadEvent, DownloadPanel, DownloadSpec, FieldViewerEvent, FieldViewerPanel, HourKey,
    ModelOption, RunBrowserPanel, SatFollowSpec, SatPlayerEvent, SatPlayerPanel, SatelliteEvent,
    SatellitePanel, SoundingPanel, StoreRequest, StoreResponse, StoreTree, StoreView, StoreWorker,
};
use sat_worker::{SatRequest, SatResponse, SatWorker};

// ---------------------------------------------------------------------------
// Storage path resolution
// ---------------------------------------------------------------------------

/// eframe Storage key for the serialized [`PersistedPaths`].
const STORAGE_KEY: &str = "rw.storage_paths";

/// Default store root when neither CLI nor persisted settings provide one.
const DEFAULT_STORE_ROOT: &str = "store";
/// Default download cache dir when neither CLI nor persisted settings provide one.
const DEFAULT_CACHE_DIR: &str = "out/cache";

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
#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedPaths {
    store_root: Option<String>,
    cache_dir: Option<String>,
}

/// Serialize [`PersistedPaths`] to a JSON string (no external deps — hand-roll).
///
/// Format: `{"store_root":"<value>","cache_dir":"<value>"}`. Fields that are
/// `None` are omitted.  JSON-escapes backslashes so Windows paths survive the
/// round-trip.
fn serialize_persisted(p: &PersistedPaths) -> String {
    fn escape(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
    let mut parts = Vec::new();
    if let Some(ref v) = p.store_root {
        parts.push(format!("\"store_root\":\"{}\"", escape(v)));
    }
    if let Some(ref v) = p.cache_dir {
        parts.push(format!("\"cache_dir\":\"{}\"", escape(v)));
    }
    format!("{{{}}}", parts.join(","))
}

/// Deserialize [`PersistedPaths`] from the JSON produced by [`serialize_persisted`].
///
/// Deliberately minimal: only recognises the exact keys produced by the
/// serializer.  Unknown keys, malformed JSON, or missing keys all yield `None`
/// for the affected field rather than an error.
fn deserialize_persisted(s: &str) -> PersistedPaths {
    fn extract(json: &str, key: &str) -> Option<String> {
        let needle = format!("\"{}\":\"", key);
        let start = json.find(&needle)? + needle.len();
        let rest = &json[start..];
        let mut value = String::new();
        let mut chars = rest.chars();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    'n' => value.push('\n'),
                    other => {
                        value.push('\\');
                        value.push(other);
                    }
                },
                c => value.push(c),
            }
        }
        Some(value)
    }
    PersistedPaths {
        store_root: extract(s, "store_root"),
        cache_dir: extract(s, "cache_dir"),
    }
}

/// Pure resolution function: merges CLI overrides + persisted settings +
/// compiled-in defaults and returns the effective paths + their sources.
///
/// Precedence (highest first): CLI arg → persisted saved value → built-in default.
///
/// Relative paths are accepted as-is; they resolve against the process's
/// working directory (documented in the storage settings UI).
fn resolve_storage_paths(
    cli_store: Option<&str>,
    cli_cache: Option<&str>,
    saved: Option<&PersistedPaths>,
) -> StoragePaths {
    let (store_root, store_root_source) = if let Some(v) = cli_store {
        (PathBuf::from(v), PathSource::Cli)
    } else if let Some(v) = saved.and_then(|s| s.store_root.as_deref()) {
        (PathBuf::from(v), PathSource::Saved)
    } else {
        (PathBuf::from(DEFAULT_STORE_ROOT), PathSource::Default)
    };

    let (cache_dir, cache_dir_source) = if let Some(v) = cli_cache {
        (PathBuf::from(v), PathSource::Cli)
    } else if let Some(v) = saved.and_then(|s| s.cache_dir.as_deref()) {
        (PathBuf::from(v), PathSource::Saved)
    } else {
        (PathBuf::from(DEFAULT_CACHE_DIR), PathSource::Default)
    };

    StoragePaths {
        store_root,
        store_root_source,
        cache_dir,
        cache_dir_source,
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
                        let store_ok = if self.store_root_source != PathSource::Cli {
                            let p = PathBuf::from(&self.store_root_edit);
                            std::fs::create_dir_all(&p).map_err(|e| format!("store root: {e}"))
                        } else {
                            Ok(())
                        };
                        let cache_ok = if self.cache_dir_source != PathSource::Cli {
                            let p = PathBuf::from(&self.cache_dir_edit);
                            std::fs::create_dir_all(&p).map_err(|e| format!("cache dir: {e}"))
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
                                let new_store = PathBuf::from(&self.store_root_edit);
                                let new_cache = PathBuf::from(&self.cache_dir_edit);
                                self.store_size = dir_size_bytes(&new_store);
                                self.cache_size = dir_size_bytes(&new_cache);
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
                        "Relative paths resolve against the working directory. \
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
        ModelId::Gfs => "hourly ≤120, 3-hourly 123-384",
        _ => "",
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

struct App {
    worker: StoreWorker,
    ingest: IngestWorker,
    store_root: PathBuf,
    cache_dir: PathBuf,
    /// `None` until the first scan lands.
    tree: Option<StoreTree>,
    browser: RunBrowserPanel,
    viewer: FieldViewerPanel,
    sounding: SoundingPanel,
    download: DownloadPanel,
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
    /// Same dedup for the sat player's texture uploads.
    recorded_sat_texture_ms: Option<f32>,
    /// State for the collapsible Storage settings section.
    storage_ui: StorageSettingsUi,
    /// Pending JSON to write via `App::save` on the next eframe save tick.
    ///
    /// Set by `StorageSettingsUi` when the user clicks Apply; drained in
    /// `App::save` which eframe calls after every frame (and on exit).
    pending_persist: Option<String>,
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
        worker.send(StoreRequest::Enumerate);

        let ctx = cc.egui_ctx.clone();
        let ingest = IngestWorker::spawn(store_root.clone(), move || {
            ctx.request_repaint();
        });

        // Satellite frames live under their own subroot so the model-run
        // browser stays free of sat runs.
        let ctx = cc.egui_ctx.clone();
        let sat = SatWorker::spawn(store_root.join("sat"), move || {
            ctx.request_repaint();
        });
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

        Self {
            worker,
            ingest,
            store_root,
            cache_dir,
            tree: None,
            browser: RunBrowserPanel::new(),
            viewer: FieldViewerPanel::new(),
            sounding: SoundingPanel::new(),
            download,
            show_download: false,
            sat,
            sat_panel,
            sat_player: SatPlayerPanel::new(),
            show_satellite,
            sat_initialized: false,
            frame_ms: 0.0,
            recorded_texture_ms: None,
            recorded_sat_texture_ms: None,
            storage_ui,
            pending_persist: None,
            #[cfg(feature = "profiling")]
            profiler: profiler::ProfilerPanel::default(),
            #[cfg(feature = "profiling")]
            show_profiler: false,
            #[cfg(feature = "profiling")]
            _puffin_server: puffin_server,
        }
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

    fn select_hour(&mut self, key: HourKey) {
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
                StoreResponse::HourVars(key, Ok(vars)) => {
                    if self.browser.selected() == Some(&key) {
                        self.viewer.set_hour(key, vars);
                        if let Some(field) = self.viewer.wanted_field() {
                            self.viewer.set_loading(&field.var);
                            self.worker.send(StoreRequest::LoadField(field));
                        }
                    }
                }
                StoreResponse::HourVars(_, Err(message)) => {
                    self.viewer.set_error(message);
                }
                StoreResponse::Field(key, result) => match *result {
                    Ok(field) => {
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
                    if let Some((_, render_ms)) = self.sounding.last_timings() {
                        self.worker.stats().record("skewt.render", render_ms);
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
                    Self::sync_run_pickers(&mut self.download, &spec);
                    self.ingest.send(IngestRequest::Estimate(spec));
                }
                IngestResponse::LatestFailed(message) => {
                    self.download.set_probing_failed(message);
                }
                IngestResponse::Started { hours } => {
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
                    self.download.finish_run(Ok(()));
                    self.worker.send(StoreRequest::Enumerate);
                }
                IngestResponse::Cancelled => {
                    self.download.finish_cancelled();
                    self.worker.send(StoreRequest::Enumerate);
                }
                IngestResponse::Failed(message) => {
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
                    Self::sync_run_pickers(&mut self.download, &spec);
                    self.ingest.send(IngestRequest::Estimate(spec));
                }
                DownloadEvent::CheckAvailability(spec) => {
                    self.ingest.send(IngestRequest::Probe(spec));
                }
                DownloadEvent::LatestRequested(spec) => {
                    self.ingest.send(IngestRequest::Latest(spec));
                }
                DownloadEvent::StartRequested(spec) => {
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
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        #[cfg(feature = "profiling")]
        puffin::GlobalProfiler::lock().new_frame();
        let frame_started = Instant::now();

        self.handle_responses();
        self.handle_ingest_responses();
        self.handle_sat_responses();

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
                ui.toggle_value(&mut self.show_download, "⬇ Download");
                ui.toggle_value(&mut self.show_satellite, "🛰 Satellite");
                #[cfg(feature = "profiling")]
                ui.toggle_value(&mut self.show_profiler, "🔍 Profiler");
                #[cfg(not(feature = "profiling"))]
                ui.label(
                    egui::RichText::new("(profiler: build with --features profiling)")
                        .small()
                        .weak(),
                );
            });
        });

        egui::Panel::bottom("rw-stats").show_inside(ui, |ui| {
            rw_ui::stats::stats_strip(ui, self.frame_ms, self.worker.stats());
        });

        egui::Panel::left("rw-browser")
            .resizable(true)
            .default_size(260.0)
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
            match self.viewer.ui(ui) {
                Some(FieldViewerEvent::VarSelected(var)) => {
                    self.viewer.set_loading(&var);
                    if let Some(field) = self.viewer.wanted_field() {
                        self.worker.send(StoreRequest::LoadField(field));
                    }
                }
                Some(FieldViewerEvent::PointClicked { fx, fy }) => {
                    if let Some(hour) = self.viewer.hour().cloned() {
                        self.sounding.set_loading();
                        self.worker
                            .send(StoreRequest::LoadSounding { hour, fx, fy });
                    }
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

    // ------------------------------------------------------------------
    // resolve_storage_paths: precedence unit tests
    // ------------------------------------------------------------------

    /// No CLI, no saved: resolve to built-in defaults.
    #[test]
    fn resolve_defaults_when_nothing_provided() {
        let paths = resolve_storage_paths(None, None, None);
        assert_eq!(paths.store_root, PathBuf::from(DEFAULT_STORE_ROOT));
        assert_eq!(paths.cache_dir, PathBuf::from(DEFAULT_CACHE_DIR));
        assert_eq!(paths.store_root_source, PathSource::Default);
        assert_eq!(paths.cache_dir_source, PathSource::Default);
    }

    /// CLI arg wins over both saved and default.
    #[test]
    fn cli_wins_over_saved_and_default() {
        let saved = PersistedPaths {
            store_root: Some("/saved/store".to_string()),
            cache_dir: Some("/saved/cache".to_string()),
        };
        let paths = resolve_storage_paths(Some("/cli/store"), Some("/cli/cache"), Some(&saved));
        assert_eq!(paths.store_root, PathBuf::from("/cli/store"));
        assert_eq!(paths.cache_dir, PathBuf::from("/cli/cache"));
        assert_eq!(paths.store_root_source, PathSource::Cli);
        assert_eq!(paths.cache_dir_source, PathSource::Cli);
    }

    /// Saved value wins over default when no CLI arg.
    #[test]
    fn saved_wins_over_default() {
        let saved = PersistedPaths {
            store_root: Some("/saved/store".to_string()),
            cache_dir: Some("/saved/cache".to_string()),
        };
        let paths = resolve_storage_paths(None, None, Some(&saved));
        assert_eq!(paths.store_root, PathBuf::from("/saved/store"));
        assert_eq!(paths.cache_dir, PathBuf::from("/saved/cache"));
        assert_eq!(paths.store_root_source, PathSource::Saved);
        assert_eq!(paths.cache_dir_source, PathSource::Saved);
    }

    /// CLI wins for store_root; saved wins for cache_dir (independent fields).
    #[test]
    fn cli_and_saved_can_mix_per_field() {
        let saved = PersistedPaths {
            store_root: Some("/saved/store".to_string()),
            cache_dir: Some("/saved/cache".to_string()),
        };
        let paths = resolve_storage_paths(Some("/cli/store"), None, Some(&saved));
        assert_eq!(paths.store_root, PathBuf::from("/cli/store"));
        assert_eq!(paths.store_root_source, PathSource::Cli);
        assert_eq!(paths.cache_dir, PathBuf::from("/saved/cache"));
        assert_eq!(paths.cache_dir_source, PathSource::Saved);
    }

    /// Saved with `None` fields falls through to default for those fields.
    #[test]
    fn saved_none_fields_fall_through_to_default() {
        let saved = PersistedPaths {
            store_root: None,
            cache_dir: Some("/saved/cache".to_string()),
        };
        let paths = resolve_storage_paths(None, None, Some(&saved));
        assert_eq!(paths.store_root, PathBuf::from(DEFAULT_STORE_ROOT));
        assert_eq!(paths.store_root_source, PathSource::Default);
        assert_eq!(paths.cache_dir, PathBuf::from("/saved/cache"));
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
    fn gfs_model_option_is_enabled() {
        let options = model_options();
        let gfs = options
            .iter()
            .find(|o| o.slug == "gfs")
            .expect("GFS must appear in model options");
        assert!(
            gfs.enabled,
            "GFS must be enabled (ingest_supported is true)"
        );
        assert!(
            gfs.note.is_empty(),
            "enabled entries have no disabled note, got: {:?}",
            gfs.note
        );
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
    fn hrrr_cadence_hint_is_empty() {
        let hint = cadence_hint(rustwx_core::ModelId::Hrrr, 0);
        assert!(
            hint.is_empty(),
            "HRRR has a uniform stride — no cadence note needed"
        );
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
