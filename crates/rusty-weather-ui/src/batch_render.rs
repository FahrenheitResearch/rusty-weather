//! Native production-map batch rendering for the egui shell.
//!
//! Catalog inspection and rendering both run off the egui thread.  This
//! module contains no renderer: it drives `rusty_weather::batch_render`, the
//! library facade over the same `render_all` path used by `rw-render`.

use std::collections::{BTreeSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread::JoinHandle;

use eframe::egui;
use rustwx_core::CycleSpec;
use rusty_weather::batch_render::{
    BatchHourScope, BatchProductKind, BatchRenderCatalog, BatchRenderDomain, BatchRenderEvent,
    BatchRenderLimits, BatchRenderRequest, BatchRenderSummary, infer_run_cycle,
    inspect_renderable_products, run_batch_render,
};
use rw_ui::HourKey;

const MAX_LOG_ROWS: usize = 120;

/// Plain-data messages emitted by [`BatchRenderTask`].
#[derive(Debug, Clone)]
pub enum BatchRenderTaskMessage {
    Event(BatchRenderEvent),
    Fatal(String),
}

/// One cancellable background render job.  Dropping the handle requests
/// cancellation; the current image is allowed to finish atomically.
pub struct BatchRenderTask {
    pub label: String,
    rx: Receiver<BatchRenderTaskMessage>,
    cancel: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl BatchRenderTask {
    pub fn spawn(
        request: BatchRenderRequest,
        notify: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let label = format!("{}/{}", request.model_slug, request.run_slug);
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let thread = std::thread::Builder::new()
            .name("rw-ui-batch-render".to_string())
            .spawn(move || {
                rw_ingest::throttle::set_current_thread_background_priority();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_batch_render(request, &worker_cancel, |event| {
                        let _ = tx.send(BatchRenderTaskMessage::Event(event));
                        notify();
                    })
                }));
                let fatal = match result {
                    Ok(Ok(_)) => None,
                    Ok(Err(error)) => Some(error),
                    Err(payload) => Some(format!(
                        "batch render worker panicked: {}",
                        panic_message(payload)
                    )),
                };
                if let Some(error) = fatal {
                    let _ = tx.send(BatchRenderTaskMessage::Fatal(error));
                    notify();
                }
            })?;
        Ok(Self {
            label,
            rx,
            cancel,
            _thread: thread,
        })
    }

    pub fn try_recv(&self) -> Result<BatchRenderTaskMessage, TryRecvError> {
        self.rx.try_recv()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for BatchRenderTask {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogKey {
    store_root: PathBuf,
    hour: HourKey,
}

struct CatalogTask {
    rx: Receiver<Result<BatchRenderCatalog, String>>,
    _thread: JoinHandle<()>,
}

impl CatalogTask {
    fn spawn(key: CatalogKey, repaint: egui::Context) -> std::io::Result<Self> {
        let (tx, rx) = channel();
        let thread = std::thread::Builder::new()
            .name("rw-ui-render-catalog".to_string())
            .spawn(move || {
                rw_ingest::throttle::set_current_thread_background_priority();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    inspect_renderable_products(
                        &key.store_root,
                        &key.hour.model,
                        &key.hour.run,
                        key.hour.hour,
                    )
                }));
                let result = match result {
                    Ok(result) => result,
                    Err(payload) => Err(format!(
                        "product catalog worker panicked: {}",
                        panic_message(payload)
                    )),
                };
                let _ = tx.send(result);
                repaint.request_repaint();
            })?;
        Ok(Self {
            rx,
            _thread: thread,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct ProgressState {
    completed: usize,
    total: usize,
    current: Option<String>,
}

/// Reusable egui panel state.  The host only supplies the store root and its
/// current browser/viewer selection each frame.
pub struct BatchRenderPanel {
    catalog_key: Option<CatalogKey>,
    catalog_task: Option<CatalogTask>,
    catalog: Option<BatchRenderCatalog>,
    catalog_error: Option<String>,
    selected_products: BTreeSet<String>,
    product_filter: String,
    all_hours: bool,
    output_dir: String,
    output_context: Option<(PathBuf, String, String)>,
    output_width: u32,
    output_height: u32,
    native_domain: bool,
    domain_slug: String,
    domain_bounds: [f64; 4],
    date_override: String,
    cycle_override: String,
    task: Option<BatchRenderTask>,
    progress: Option<ProgressState>,
    summary: Option<BatchRenderSummary>,
    error: Option<String>,
    log: VecDeque<String>,
}

impl Default for BatchRenderPanel {
    fn default() -> Self {
        Self {
            catalog_key: None,
            catalog_task: None,
            catalog: None,
            catalog_error: None,
            selected_products: BTreeSet::new(),
            product_filter: String::new(),
            all_hours: false,
            output_dir: String::new(),
            output_context: None,
            output_width: 1_200,
            output_height: 900,
            native_domain: true,
            domain_slug: "custom".to_string(),
            domain_bounds: [-127.0, -66.0, 23.0, 51.5],
            date_override: String::new(),
            cycle_override: String::new(),
            task: None,
            progress: None,
            summary: None,
            error: None,
            log: VecDeque::new(),
        }
    }
}

impl BatchRenderPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_running(&self) -> bool {
        self.task.is_some()
    }

    pub fn cancel(&self) {
        if let Some(task) = &self.task {
            task.cancel();
        }
    }

    /// Seed a non-native render extent, for example from the viewer's saved
    /// custom-domain picker.  The next job uses it unless the user switches
    /// back to "full native grid".
    pub fn set_domain_bounds(&mut self, slug: impl Into<String>, bounds: (f64, f64, f64, f64)) {
        self.domain_slug = slug.into();
        self.domain_bounds = [bounds.0, bounds.1, bounds.2, bounds.3];
        self.native_domain = false;
    }

    /// Draw the complete panel and poll its workers.  `current_var` is used
    /// only to choose a conservative initial recipe; direct/derived catalog
    /// coverage is proven from store metadata on its worker thread.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        store_root: &Path,
        current_hour: Option<&HourKey>,
        current_var: Option<&str>,
        start_blocked: Option<&str>,
    ) {
        let current_hour = current_hour.cloned();
        self.ensure_catalog(ui.ctx(), store_root, current_hour.as_ref());
        self.poll_catalog(current_var);
        self.poll_render_task();

        if self.is_running() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }

        ui.heading("Batch render");
        ui.label(
            egui::RichText::new(
                "Production PNGs through Rusty Weather's native renderer. Jobs run sequentially at background priority.",
            )
            .small()
            .weak(),
        );
        ui.separator();

        let Some(hour) = current_hour.as_ref() else {
            ui.label("Select a model run hour first.");
            self.render_status(ui);
            return;
        };

        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(hour.to_string()).strong());
            if let Some(var) = current_var {
                ui.label(egui::RichText::new(format!("field: {var}")).small().weak());
            }
            if ui
                .small_button("Refresh products")
                .on_hover_text("Re-read this hour's selector metadata")
                .clicked()
            {
                self.catalog_key = None;
                self.catalog_task = None;
                self.catalog = None;
                self.catalog_error = None;
            }
        });

        let running = self.is_running();
        ui.add_enabled_ui(!running, |ui| {
            self.render_product_picker(ui, current_var);
            ui.separator();
            self.render_hour_scope(ui, hour.hour);
            ui.separator();
            self.render_output_options(ui, store_root);
        });

        let validation = self.validate_start(hour);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !running && validation.is_ok() && start_blocked.is_none(),
                    egui::Button::new("Render PNGs"),
                )
                .clicked()
            {
                self.start(hour.clone(), store_root.to_path_buf(), ui.ctx().clone());
            }
            if running && ui.button("Cancel after current image").clicked() {
                self.cancel();
            }
            if let Ok(work) = &validation {
                ui.label(
                    egui::RichText::new(format!("{work} image task(s)"))
                        .small()
                        .weak(),
                );
            }
        });
        if let Err(message) = &validation {
            ui.label(
                egui::RichText::new(message)
                    .small()
                    .color(egui::Color32::YELLOW),
            );
        }
        if let Some(message) = start_blocked {
            ui.label(
                egui::RichText::new(message)
                    .small()
                    .color(egui::Color32::YELLOW),
            );
        }
        ui.label(
            egui::RichText::new("Existing PNGs with the same production filename are replaced.")
                .small()
                .weak(),
        );

        self.render_status(ui);
    }

    fn ensure_catalog(&mut self, ctx: &egui::Context, store_root: &Path, hour: Option<&HourKey>) {
        let Some(hour) = hour else {
            return;
        };
        let key = CatalogKey {
            store_root: store_root.to_path_buf(),
            hour: hour.clone(),
        };
        if self.catalog_key.as_ref() == Some(&key) {
            return;
        }

        let output_context = (
            store_root.to_path_buf(),
            hour.model.clone(),
            hour.run.clone(),
        );
        if self.output_context.as_ref() != Some(&output_context) {
            self.output_dir = default_output_dir(store_root, hour).display().to_string();
            self.output_context = Some(output_context);
            self.date_override.clear();
            self.cycle_override.clear();
        }

        self.catalog_key = Some(key.clone());
        self.catalog = None;
        self.catalog_error = None;
        self.selected_products.clear();
        self.product_filter.clear();
        match CatalogTask::spawn(key, ctx.clone()) {
            Ok(task) => self.catalog_task = Some(task),
            Err(error) => {
                self.catalog_task = None;
                self.catalog_error = Some(format!("start product catalog worker: {error}"));
            }
        }
    }

    fn poll_catalog(&mut self, current_var: Option<&str>) {
        let result = match self.catalog_task.as_ref().map(|task| task.rx.try_recv()) {
            Some(Ok(result)) => Some(result),
            Some(Err(TryRecvError::Disconnected)) => Some(Err(
                "product catalog worker stopped without a result".to_string(),
            )),
            Some(Err(TryRecvError::Empty)) | None => None,
        };
        let Some(result) = result else {
            return;
        };
        self.catalog_task = None;
        match result {
            Ok(catalog) => {
                if let Some(slug) = preferred_product(&catalog, current_var) {
                    self.selected_products.insert(slug);
                }
                self.catalog = Some(catalog);
                self.catalog_error = None;
            }
            Err(error) => {
                self.catalog = None;
                self.catalog_error = Some(error);
            }
        }
    }

    fn render_product_picker(&mut self, ui: &mut egui::Ui, current_var: Option<&str>) {
        ui.label(egui::RichText::new("Products").strong());
        if self.catalog_task.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Inspecting stored selector metadata...");
            });
            return;
        }
        if let Some(error) = &self.catalog_error {
            ui.label(egui::RichText::new(error).color(egui::Color32::RED));
            return;
        }
        let Some(catalog) = self.catalog.clone() else {
            ui.label("No product catalog loaded.");
            return;
        };
        let products = catalog.products.clone();
        if products.is_empty() {
            ui.label("This hour has no complete production-map recipes.");
            return;
        }

        ui.horizontal_wrapped(|ui| {
            if ui.small_button("Current field").clicked() {
                self.selected_products.clear();
                if let Some(slug) = preferred_product(&catalog, current_var) {
                    self.selected_products.insert(slug);
                }
            }
            if ui.small_button("Select shown").clicked() {
                let filter = self.product_filter.trim().to_ascii_lowercase();
                let limit = BatchRenderLimits::default().max_products_per_hour;
                for product in products.iter().filter(|product| {
                    filter.is_empty()
                        || product.slug.to_ascii_lowercase().contains(&filter)
                        || product.kind.label().contains(&filter)
                }) {
                    if product.kind != BatchProductKind::Windowed
                        && self
                            .selected_products
                            .iter()
                            .filter(|slug| {
                                products.iter().any(|candidate| {
                                    &candidate.slug == *slug
                                        && candidate.kind != BatchProductKind::Windowed
                                })
                            })
                            .count()
                            >= limit
                    {
                        break;
                    }
                    self.selected_products.insert(product.slug.clone());
                }
            }
            if ui.small_button("Clear").clicked() {
                self.selected_products.clear();
            }
            ui.label(
                egui::RichText::new(format!("{} selected", self.selected_products.len()))
                    .small()
                    .weak(),
            );
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.product_filter)
                .desired_width(f32::INFINITY)
                .hint_text("filter production slugs"),
        );

        let filter = self.product_filter.trim().to_ascii_lowercase();
        egui::ScrollArea::vertical()
            .id_salt("rw-batch-products")
            .max_height(230.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for product in &products {
                    if !filter.is_empty()
                        && !product.slug.to_ascii_lowercase().contains(&filter)
                        && !product.kind.label().contains(&filter)
                    {
                        continue;
                    }
                    let mut selected = self.selected_products.contains(&product.slug);
                    let label = format!("{}  [{}]", product.slug, product.kind.label());
                    if ui.checkbox(&mut selected, label).changed() {
                        if selected {
                            self.selected_products.insert(product.slug.clone());
                        } else {
                            self.selected_products.remove(&product.slug);
                        }
                    }
                }
            });
        if self
            .selected_products
            .iter()
            .any(|slug| product_kind(&products, slug) == Some(BatchProductKind::Heavy))
        {
            ui.label(
                egui::RichText::new(
                    "Heavy diagnostics are selected; they remain sequential but may use substantial memory.",
                )
                .small()
                .color(egui::Color32::YELLOW),
            );
        }
    }

    fn render_hour_scope(&mut self, ui: &mut egui::Ui, current_hour: u16) {
        ui.label(egui::RichText::new("Hours").strong());
        let stored = self
            .catalog
            .as_ref()
            .map(|catalog| catalog.stored_hours.len())
            .unwrap_or(0);
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(
                &mut self.all_hours,
                false,
                format!("Current (F{current_hour:03})"),
            );
            ui.radio_value(&mut self.all_hours, true, format!("All stored ({stored})"));
        });
    }

    fn render_output_options(&mut self, ui: &mut egui::Ui, store_root: &Path) {
        let limits = BatchRenderLimits::default();
        ui.label(egui::RichText::new("Output").strong());
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.output_dir)
                    .desired_width(f32::INFINITY)
                    .hint_text("output directory"),
            );
            if ui.button("Browse...").clicked() {
                let mut dialog = rfd::FileDialog::new().set_title("Batch render output");
                let current = PathBuf::from(self.output_dir.trim());
                if current.is_dir() {
                    dialog = dialog.set_directory(current);
                } else if store_root.is_dir() {
                    dialog = dialog.set_directory(store_root);
                }
                if let Some(path) = dialog.pick_folder() {
                    self.output_dir = path.display().to_string();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Size");
            ui.add(
                egui::DragValue::new(&mut self.output_width)
                    .range(320..=limits.max_output_width)
                    .suffix(" px"),
            );
            ui.label("x");
            ui.add(
                egui::DragValue::new(&mut self.output_height)
                    .range(240..=limits.max_output_height)
                    .suffix(" px"),
            );
            ui.label(egui::RichText::new("PNG / fast compression").small().weak());
        });

        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut self.native_domain, true, "Full native grid");
            ui.radio_value(&mut self.native_domain, false, "Custom bounds");
        });
        if !self.native_domain {
            ui.horizontal_wrapped(|ui| {
                ui.label("W");
                ui.add(egui::DragValue::new(&mut self.domain_bounds[0]).speed(0.1));
                ui.label("E");
                ui.add(egui::DragValue::new(&mut self.domain_bounds[1]).speed(0.1));
                ui.label("S");
                ui.add(egui::DragValue::new(&mut self.domain_bounds[2]).speed(0.1));
                ui.label("N");
                ui.add(egui::DragValue::new(&mut self.domain_bounds[3]).speed(0.1));
            });
        }

        egui::CollapsingHeader::new("Init label override")
            .id_salt("rw-batch-init-override")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Leave blank to infer YYYYMMDD and cycle from the run name.",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal(|ui| {
                    ui.label("Date");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.date_override)
                            .desired_width(100.0)
                            .hint_text("YYYYMMDD"),
                    );
                    ui.label("Cycle");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cycle_override)
                            .desired_width(50.0)
                            .hint_text("0-23"),
                    );
                });
                if let Some(key) = &self.catalog_key {
                    if let Some((date, cycle)) = infer_run_cycle(&key.hour.run) {
                        ui.label(
                            egui::RichText::new(format!("Inferred: {date} {cycle:02}Z"))
                                .small()
                                .weak(),
                        );
                    }
                }
            });
    }

    fn validate_start(&self, _hour: &HourKey) -> Result<usize, String> {
        let catalog = self
            .catalog
            .as_ref()
            .ok_or_else(|| "Waiting for the product catalog.".to_string())?;
        if self.selected_products.is_empty() {
            return Err("Select at least one product.".to_string());
        }
        if self.output_dir.trim().is_empty() {
            return Err("Choose an output directory.".to_string());
        }
        let inferred = infer_run_cycle(&_hour.run);
        let date = if self.date_override.trim().is_empty() {
            inferred
                .as_ref()
                .map(|(date, _)| date.clone())
                .ok_or_else(|| "Set an init date; this run name has no timestamp.".to_string())?
        } else {
            self.date_override.trim().to_string()
        };
        let cycle = if self.cycle_override.trim().is_empty() {
            inferred
                .as_ref()
                .map(|(_, cycle)| *cycle)
                .ok_or_else(|| "Set a cycle; this run name has no timestamp.".to_string())?
        } else {
            self.cycle_override
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|hour| *hour < 24)
                .ok_or_else(|| "Cycle override must be 0-23.".to_string())?
        };
        CycleSpec::new(date, cycle).map_err(|error| format!("Invalid init label: {error}"))?;
        if !self.native_domain {
            let [west, east, south, north] = self.domain_bounds;
            if ![west, east, south, north]
                .iter()
                .all(|value| value.is_finite())
                || south >= north
                || (east - west).abs() == 0.0
                || (east - west).abs() > 360.0
                || south < -90.0
                || north > 90.0
            {
                return Err(
                    "Custom bounds need distinct W/E, S < N, finite values, and valid latitude."
                        .to_string(),
                );
            }
        }
        let limits = BatchRenderLimits::default();
        let hours = if self.all_hours {
            catalog.stored_hours.len()
        } else {
            1
        };
        if hours == 0 {
            return Err("This run has no stored hours.".to_string());
        }
        if hours > limits.max_hours {
            return Err(format!(
                "{hours} hours exceeds the GUI limit of {}.",
                limits.max_hours
            ));
        }
        let per_hour = self
            .selected_products
            .iter()
            .filter(|slug| {
                product_kind(&catalog.products, slug) != Some(BatchProductKind::Windowed)
            })
            .count();
        let windowed = self.selected_products.len() - per_hour;
        if per_hour > limits.max_products_per_hour {
            return Err(format!(
                "{per_hour} per-hour products exceeds the GUI limit of {}.",
                limits.max_products_per_hour
            ));
        }
        let work = hours
            .checked_mul(per_hour)
            .and_then(|count| count.checked_add(windowed))
            .ok_or_else(|| "Selected work count overflowed.".to_string())?;
        if work > limits.max_work_items {
            return Err(format!(
                "{work} product-hours exceeds the GUI limit of {}; split the job.",
                limits.max_work_items
            ));
        }
        Ok(work)
    }

    fn start(&mut self, hour: HourKey, store_root: PathBuf, repaint: egui::Context) {
        let cycle_utc = if self.cycle_override.trim().is_empty() {
            None
        } else {
            self.cycle_override.trim().parse::<u8>().ok()
        };
        let date_yyyymmdd =
            (!self.date_override.trim().is_empty()).then(|| self.date_override.trim().to_string());
        let domain = if self.native_domain {
            BatchRenderDomain::NativeGrid
        } else {
            BatchRenderDomain::Bounds {
                slug: self.domain_slug.clone(),
                west: self.domain_bounds[0],
                east: self.domain_bounds[1],
                south: self.domain_bounds[2],
                north: self.domain_bounds[3],
            }
        };
        let request = BatchRenderRequest {
            store_root,
            model_slug: hour.model.clone(),
            run_slug: hour.run.clone(),
            hours: if self.all_hours {
                BatchHourScope::AllStored
            } else {
                BatchHourScope::Current(hour.hour)
            },
            product_spec: self
                .selected_products
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
            out_dir: PathBuf::from(self.output_dir.trim()),
            domain,
            date_yyyymmdd,
            cycle_utc,
            source: None,
            output_width: self.output_width,
            output_height: self.output_height,
            limits: BatchRenderLimits::default(),
        };
        self.error = None;
        self.summary = None;
        self.progress = None;
        self.log.clear();
        let notify = move || repaint.request_repaint();
        match BatchRenderTask::spawn(request, notify) {
            Ok(task) => self.task = Some(task),
            Err(error) => self.error = Some(format!("start batch render worker: {error}")),
        }
    }

    fn poll_render_task(&mut self) {
        let Some(task) = self.task.as_ref() else {
            return;
        };
        let mut messages = Vec::new();
        let mut disconnected = false;
        loop {
            match task.try_recv() {
                Ok(message) => messages.push(message),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        let has_terminal = messages.iter().any(|message| {
            matches!(
                message,
                BatchRenderTaskMessage::Fatal(_)
                    | BatchRenderTaskMessage::Event(BatchRenderEvent::Finished(_))
            )
        });
        let mut terminal = false;
        for message in messages {
            terminal |= self.handle_task_message(message);
        }
        if disconnected && !has_terminal {
            self.error = Some("batch render worker stopped without a final result".to_string());
            terminal = true;
        }
        if terminal {
            self.task = None;
        }
    }

    fn handle_task_message(&mut self, message: BatchRenderTaskMessage) -> bool {
        match message {
            BatchRenderTaskMessage::Fatal(error) => {
                self.error = Some(error);
                true
            }
            BatchRenderTaskMessage::Event(event) => match event {
                BatchRenderEvent::Started {
                    planned_items,
                    hours,
                    products,
                    output_dir,
                } => {
                    self.progress = Some(ProgressState {
                        completed: 0,
                        total: planned_items,
                        current: None,
                    });
                    self.push_log(format!(
                        "Started: {} product(s), {} hour(s) -> {}",
                        products.len(),
                        hours.len(),
                        output_dir.display()
                    ));
                    false
                }
                BatchRenderEvent::HourStarted { hour, index, total } => {
                    self.push_log(format!("Hour {index}/{total}: F{hour:03}"));
                    false
                }
                BatchRenderEvent::ItemStarted {
                    hour,
                    slug,
                    kind,
                    completed,
                    total,
                } => {
                    self.progress = Some(ProgressState {
                        completed,
                        total,
                        current: Some(format_item(hour, &slug, kind.label())),
                    });
                    false
                }
                BatchRenderEvent::ItemRendered {
                    hour,
                    slug,
                    output_path,
                    render_ms,
                    completed,
                    total,
                } => {
                    self.update_progress(completed, total, None);
                    self.push_log(format!(
                        "OK {} ({} ms) -> {}",
                        format_item(hour, &slug, "render"),
                        render_ms,
                        output_path.display()
                    ));
                    false
                }
                BatchRenderEvent::ItemSkipped {
                    hour,
                    slug,
                    reason,
                    completed,
                    total,
                } => {
                    self.update_progress(completed, total, None);
                    self.push_log(format!(
                        "SKIP {}: {reason}",
                        format_item(hour, &slug, "render")
                    ));
                    false
                }
                BatchRenderEvent::ItemFailed {
                    hour,
                    slug,
                    error,
                    completed,
                    total,
                } => {
                    self.update_progress(completed, total, None);
                    self.push_log(format!(
                        "ERROR {}: {error}",
                        format_item(hour, &slug, "render")
                    ));
                    false
                }
                BatchRenderEvent::Finished(summary) => {
                    self.update_progress(summary.planned, summary.planned, None);
                    self.push_log(format!(
                        "Finished: {} rendered, {} skipped, {} failed in {} ms{}",
                        summary.rendered,
                        summary.skipped,
                        summary.failed,
                        summary.elapsed_ms,
                        if summary.cancelled {
                            " (cancelled)"
                        } else {
                            ""
                        }
                    ));
                    self.summary = Some(summary);
                    true
                }
            },
        }
    }

    fn update_progress(&mut self, completed: usize, total: usize, current: Option<String>) {
        self.progress = Some(ProgressState {
            completed,
            total,
            current,
        });
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() == MAX_LOG_ROWS {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn render_status(&self, ui: &mut egui::Ui) {
        if let Some(progress) = &self.progress {
            ui.separator();
            let fraction = if progress.total == 0 {
                0.0
            } else {
                progress.completed as f32 / progress.total as f32
            };
            ui.add(
                egui::ProgressBar::new(fraction)
                    .show_percentage()
                    .text(format!("{} / {}", progress.completed, progress.total)),
            );
            if let Some(current) = &progress.current {
                ui.label(egui::RichText::new(current).small().weak());
            }
        }
        if let Some(summary) = &self.summary {
            let color = if summary.failed == 0 {
                egui::Color32::LIGHT_GREEN
            } else {
                egui::Color32::YELLOW
            };
            ui.label(
                egui::RichText::new(format!(
                    "{} rendered, {} skipped, {} failed{}",
                    summary.rendered,
                    summary.skipped,
                    summary.failed,
                    if summary.cancelled {
                        " (cancelled)"
                    } else {
                        ""
                    }
                ))
                .color(color),
            );
        }
        if let Some(error) = &self.error {
            ui.label(egui::RichText::new(error).color(egui::Color32::RED));
        }
        if !self.log.is_empty() {
            egui::CollapsingHeader::new("Job log")
                .id_salt("rw-batch-job-log")
                .default_open(self.is_running())
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("rw-batch-job-log-scroll")
                        .max_height(190.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &self.log {
                                ui.label(egui::RichText::new(line).small().monospace());
                            }
                        });
                });
        }
    }
}

fn preferred_product(catalog: &BatchRenderCatalog, current_var: Option<&str>) -> Option<String> {
    let current_var = current_var?;
    let alias = match current_var {
        "temperature_2m" => Some("2m_temperature"),
        "dewpoint_2m" => Some("2m_dewpoint"),
        "mslp" => Some("mslp_10m_winds"),
        "wind_gust_10m" => Some("10m_wind_gusts"),
        "composite_reflectivity" => Some("composite_reflectivity"),
        other => Some(other),
    };
    if let Some(product) = catalog
        .products
        .iter()
        .find(|product| Some(product.slug.as_str()) == alias)
    {
        return Some(product.slug.clone());
    }
    catalog
        .products
        .iter()
        .filter(|product| {
            product
                .source_fields
                .iter()
                .any(|source| source == current_var)
        })
        .min_by_key(|product| (product.source_fields.len(), product.slug.len()))
        .map(|product| product.slug.clone())
        .or_else(|| catalog.products.first().map(|product| product.slug.clone()))
}

fn product_kind(
    products: &[rusty_weather::batch_render::BatchProductOption],
    slug: &str,
) -> Option<BatchProductKind> {
    products
        .iter()
        .find(|product| product.slug == slug)
        .map(|product| product.kind)
}

fn default_output_dir(store_root: &Path, hour: &HourKey) -> PathBuf {
    let render_root = store_root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.join("renders"))
        .unwrap_or_else(|| store_root.join("_renders"));
    render_root
        .join(sanitize_component(&hour.model))
        .join(sanitize_component(&hour.run))
}

fn sanitize_component(value: &str) -> String {
    let value = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        "run".to_string()
    } else {
        value
    }
}

fn format_item(hour: Option<u16>, slug: &str, kind: &str) -> String {
    match hour {
        Some(hour) => format!("F{hour:03} {slug} [{kind}]"),
        None => format!("window {slug} [{kind}]"),
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}
