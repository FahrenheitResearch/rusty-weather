//! rusty-weather UI shell: a thin eframe window mounting the rw-ui panels.
//!
//! Layout: run browser on the left, false-color field viewer in the center,
//! sounding panel on the right (appears after a click on the field). All
//! store IO runs on the rw-ui worker thread; this shell only wires panel
//! events to worker requests and worker responses back into the panels.
//!
//! Usage:
//!   rusty-weather-ui [--store-root <dir>] [--synthetic]
//!
//! `--store-root` defaults to `store`. `--synthetic` writes a tiny synthetic
//! store to a temp directory and opens that instead — handy for trying the
//! UI without ingested data.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

use eframe::egui;
use rw_ui::{
    FieldViewerEvent, FieldViewerPanel, HourKey, RunBrowserPanel, SoundingPanel, StoreRequest,
    StoreResponse, StoreTree, StoreView, StoreWorker,
};

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("usage: rusty-weather-ui [--store-root <dir>] [--synthetic]");
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
        Box::new(move |cc| Ok(Box::new(App::new(cc, store_root)))),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ui error: {err}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    store_root: PathBuf,
    synthetic: bool,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut store_root = PathBuf::from("store");
        let mut synthetic = false;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--store-root" => {
                    store_root = PathBuf::from(
                        args.next().ok_or("--store-root requires a path argument")?,
                    );
                }
                "--synthetic" => synthetic = true,
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(Self {
            store_root,
            synthetic,
        })
    }
}

struct App {
    worker: StoreWorker,
    store_root: PathBuf,
    /// `None` until the first scan lands.
    tree: Option<StoreTree>,
    browser: RunBrowserPanel,
    viewer: FieldViewerPanel,
    sounding: SoundingPanel,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, store_root: PathBuf) -> Self {
        let ctx = cc.egui_ctx.clone();
        let worker = StoreWorker::spawn(StoreView::new(&store_root), move || {
            ctx.request_repaint();
        });
        worker.send(StoreRequest::Enumerate);
        Self {
            worker,
            store_root,
            tree: None,
            browser: RunBrowserPanel::new(),
            viewer: FieldViewerPanel::new(),
            sounding: SoundingPanel::new(),
        }
    }

    fn select_hour(&mut self, key: HourKey) {
        self.worker.send(StoreRequest::LoadHour(key));
    }

    /// Drain worker responses into panel state.
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
                StoreResponse::Field(_, Ok(field)) => {
                    self.viewer.set_field(field);
                }
                StoreResponse::Field(key, Err(message)) => {
                    if self.viewer.wanted_field().as_ref() == Some(&key) {
                        self.viewer.set_error(message);
                    }
                }
                StoreResponse::Sounding(_, Ok(data)) => {
                    self.sounding.set_data(data);
                }
                StoreResponse::Sounding(_, Err(message)) => {
                    self.sounding.set_error(message);
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_responses();

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
                                "Point --store-root at an rw-store directory, or run \
                                 with --synthetic for demo data.",
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
        });
    }
}
