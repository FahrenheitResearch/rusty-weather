//! rusty-weather UI shell: a thin eframe window mounting the rw-ui panels.
//!
//! Layout: run browser on the left, false-color field viewer in the center,
//! sounding panel on the right (appears after a click on the field), an
//! always-on stats strip along the bottom, and a toggleable Download window
//! that runs in-process ingests through [`ingest_worker::IngestWorker`].
//! All store IO runs on the rw-ui store worker thread; all ingest work
//! (network fetch + extraction/compute on a dedicated below-normal rayon
//! pool) runs behind the ingest worker — this shell only wires panel
//! events to worker requests and worker responses back into the panels.
//!
//! Usage:
//!   rusty-weather-ui [--store-root <dir>] [--cache-dir <dir>] [--synthetic]
//!                    [--download-date YYYYMMDD] [--download-cycle N]
//!                    [--download-hours SPEC] [--download-profile NAME]
//!
//! `--store-root` defaults to `store`. `--cache-dir` presets the Download
//! panel's raw GRIB cache directory (default `out/cache`; point it at an
//! existing cache to ingest without network). The `--download-*` flags
//! preset the Download panel's pickers (handy for scripted/offline runs).
//! `--synthetic` writes a tiny synthetic store to a temp directory and
//! opens that instead.
//!
//! Profiling: build with `--features profiling` for puffin scopes, a
//! puffin_http server on 127.0.0.1:8585 (external `puffin_viewer`), and
//! the in-app scope-stats window. The stats strip is always available.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ingest_worker;
#[cfg(feature = "profiling")]
mod profiler;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use eframe::egui;
use ingest_worker::{IngestRequest, IngestResponse, IngestWorker};
use rustwx_models::{model_summary, supported_forecast_hours, supported_models};
use rw_ui::{
    DownloadEvent, DownloadPanel, DownloadSpec, FieldViewerEvent, FieldViewerPanel, HourKey,
    ModelOption, RunBrowserPanel, SoundingPanel, StoreRequest, StoreResponse, StoreTree,
    StoreView, StoreWorker,
};

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

    let store_root = if args.synthetic {
        let root = std::env::temp_dir().join("rusty-weather-ui-synthetic");
        if let Err(err) = rw_ui::synthetic::write_synthetic_store(&root) {
            eprintln!("failed to write the synthetic store: {err}");
            return ExitCode::FAILURE;
        }
        root
    } else {
        args.store_root
    };

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
        Box::new(move |cc| Ok(Box::new(App::new(cc, store_root, args.spec_overrides)))),
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
    store_root: PathBuf,
    synthetic: bool,
    spec_overrides: SpecOverrides,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut store_root = PathBuf::from("store");
        let mut synthetic = false;
        let mut spec_overrides = SpecOverrides::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| -> Result<String, String> {
                args.next().ok_or(format!("{flag} requires a value"))
            };
            match arg.as_str() {
                "--store-root" => store_root = PathBuf::from(value("--store-root")?),
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
                "--synthetic" => synthetic = true,
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(Self {
            store_root,
            synthetic,
            spec_overrides,
        })
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
    /// `None` until the first scan lands.
    tree: Option<StoreTree>,
    browser: RunBrowserPanel,
    viewer: FieldViewerPanel,
    sounding: SoundingPanel,
    download: DownloadPanel,
    show_download: bool,
    /// CPU time of the previous `App::ui` pass (stats strip).
    frame_ms: f32,
    /// Last texture-build wall already recorded into the stats registry
    /// (the panel re-reports the same value every frame).
    recorded_texture_ms: Option<f32>,
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

        let defaults = DownloadSpec::default();
        let mut spec = DownloadSpec {
            date: overrides.date.unwrap_or_else(rw_ui::today_yyyymmdd_utc),
            hours: overrides.hours.unwrap_or_else(|| "0-6".to_string()),
            cycle: overrides.cycle.unwrap_or(defaults.cycle),
            profile: overrides.profile.unwrap_or(defaults.profile),
            cache_dir: overrides.cache_dir.unwrap_or(defaults.cache_dir),
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

        Self {
            worker,
            ingest,
            store_root,
            tree: None,
            browser: RunBrowserPanel::new(),
            viewer: FieldViewerPanel::new(),
            sounding: SoundingPanel::new(),
            download,
            show_download: false,
            frame_ms: 0.0,
            recorded_texture_ms: None,
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
        sources.extend(
            summary
                .sources
                .iter()
                .map(|source| source.id.to_string()),
        );
        download.set_source_options(sources);
        let supported = supported_forecast_hours(model, spec.cycle);
        match (supported.first(), supported.last()) {
            (Some(first), Some(last)) => {
                download.set_hours_hint(format!(
                    "supported: {first}-{last} ({:02}z)",
                    spec.cycle
                ));
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        #[cfg(feature = "profiling")]
        puffin::GlobalProfiler::lock().new_frame();
        let frame_started = Instant::now();

        self.handle_responses();
        self.handle_ingest_responses();

        // Smooth progress while a download runs, even through long silent
        // stages (a 60 s heavy stage emits nothing between its events).
        if self.download.is_running() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }

        egui::Panel::top("rw-toolbar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.toggle_value(&mut self.show_download, "⬇ Download");
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
                                "Point --store-root at an rw-store directory, run \
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
                        self.worker.send(StoreRequest::LoadSounding { hour, fx, fy });
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
