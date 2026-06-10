//! Download panel: pick a model run, see LIVE size/download estimates, and
//! drive an in-process ingest with per-hour per-stage progress and cancel.
//!
//! Pure widget, same pattern as the other panels: it never touches the
//! network, the store, or rw-ingest. User intent leaves as
//! [`DownloadEvent`]s; the host (which owns an ingest worker) pushes
//! results back through `set_estimate` / `set_availability` /
//! `apply_stage_*` / `apply_hour_done` / `finish_run`. bowecho gets this
//! panel for free without any ingest wiring.
//!
//! Cancel semantics are honest: the flag is observed at stage boundaries,
//! so the in-flight stage (possibly a multi-hundred-MB file fetch)
//! completes first; the store write is atomic, so no partial files.

use egui::{Color32, ComboBox, RichText, ScrollArea, TextEdit, Ui};

/// What the user wants downloaded — plain data only, host-interpretable.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadSpec {
    /// Model slug (e.g. "hrrr").
    pub model: String,
    /// Run date, YYYYMMDD UTC.
    pub date: String,
    /// Run cycle hour UTC (0-23).
    pub cycle: u8,
    /// Forecast hours in the `parse_hours` grammar: "7", "0-6", "0,3,6".
    pub hours: String,
    /// Profile preset name: full | sounding | view.
    pub profile: String,
    /// Isobaric level step (25 or 50 hPa).
    pub level_step_hpa: u16,
    /// Run the derived compute stage.
    pub derived: bool,
    /// Run the heavy ECAPE compute stage.
    pub heavy: bool,
    /// Fetch source: "auto" (catalog order) or a source slug ("aws", ...).
    pub source: String,
    /// Raw GRIB byte cache directory.
    pub cache_dir: String,
    /// Verify the written hour (bit-exact round-trip) after each write.
    pub verify: bool,
}

impl Default for DownloadSpec {
    fn default() -> Self {
        Self {
            model: "hrrr".to_string(),
            date: String::new(),
            cycle: 0,
            hours: "0-6".to_string(),
            profile: "full".to_string(),
            level_step_hpa: 25,
            derived: true,
            heavy: true,
            source: "auto".to_string(),
            cache_dir: "out/cache".to_string(),
            verify: false,
        }
    }
}

/// One pickable model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelOption {
    pub slug: String,
    pub label: String,
    pub enabled: bool,
    /// Shown for disabled entries (e.g. "ingest not yet supported").
    pub note: String,
}

/// Host-computed size/download estimate for the current spec.
#[derive(Debug, Clone, PartialEq)]
pub struct EstimateView {
    /// One-line profile shape (`IngestProfile::describe()`).
    pub profile_summary: String,
    pub hour_count: u16,
    pub store_bytes: u64,
    pub download_bytes: u64,
    pub per_hour_store_bytes: u64,
    pub per_hour_download_bytes: u64,
    /// Calibration provenance — ALWAYS displayed so a wrong-grid builtin
    /// fallback is visible, never silent.
    pub calibration: String,
    /// Rough wall-clock hint, host-formatted (assumptions included).
    pub time_hint: String,
    /// Per-hour per-variable bytes, largest first.
    pub breakdown: Vec<(String, u64)>,
}

/// Probe result for one (model, date, cycle): which forecast hours exist.
#[derive(Debug, Clone, PartialEq)]
pub struct AvailabilityView {
    pub model: String,
    pub date: String,
    pub cycle: u8,
    /// Every supported hour for the cycle, in order.
    pub candidates: Vec<u16>,
    /// The subset the probe found available.
    pub available: Vec<u16>,
    /// Probe problems (e.g. a source error), shown verbatim.
    pub note: Option<String>,
}

impl AvailabilityView {
    /// Whether this probe result is for `spec`'s (model, date, cycle).
    pub fn matches(&self, spec: &DownloadSpec) -> bool {
        self.model == spec.model && self.date == spec.date && self.cycle == spec.cycle
    }
}

/// Panel-local mirror of the ingest stages, in execution order. The host
/// maps its ingest crate's stage enum onto this (rw-ui stays free of
/// ingest dependencies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStage {
    FetchPrs,
    FetchSfc,
    ExtractPrs,
    ExtractSfc,
    ThermoDecode,
    Derived,
    Heavy,
    Write,
    Verify,
}

impl DownloadStage {
    pub const ALL: [DownloadStage; 9] = [
        DownloadStage::FetchPrs,
        DownloadStage::FetchSfc,
        DownloadStage::ExtractPrs,
        DownloadStage::ExtractSfc,
        DownloadStage::ThermoDecode,
        DownloadStage::Derived,
        DownloadStage::Heavy,
        DownloadStage::Write,
        DownloadStage::Verify,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DownloadStage::FetchPrs => "fetch prs",
            DownloadStage::FetchSfc => "fetch sfc",
            DownloadStage::ExtractPrs => "extract prs",
            DownloadStage::ExtractSfc => "extract sfc",
            DownloadStage::ThermoDecode => "thermo",
            DownloadStage::Derived => "derived",
            DownloadStage::Heavy => "heavy",
            DownloadStage::Write => "write",
            DownloadStage::Verify => "verify",
        }
    }
}

/// One stage's progress state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    Pending,
    Running,
    Done { ms: u128 },
    /// Never ran for this hour (profile skipped it, or the hour finished
    /// without it).
    Skipped,
}

/// Per-hour progress row.
#[derive(Debug, Clone, PartialEq)]
struct HourProgress {
    hour: u16,
    stages: Vec<(DownloadStage, StageState)>,
    done: bool,
    /// One-line completion summary pushed by the host.
    summary: Option<String>,
}

/// Host-pushed completion summary for one ingested hour.
#[derive(Debug, Clone, PartialEq)]
pub struct HourDoneView {
    pub hour: u16,
    pub store_mb: f64,
    pub total_ms: u128,
}

/// Run lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DownloadRunState {
    #[default]
    Idle,
    Running,
    /// Cancel requested; the in-flight stage completes first.
    Cancelling,
    Finished,
    Failed(String),
    Cancelled,
}

/// What the user did this frame; the host turns these into worker requests.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadEvent {
    /// Any picker changed — recompute the (local, cheap) estimate.
    SpecChanged(DownloadSpec),
    /// "Check availability" was clicked — probe the run's hours.
    CheckAvailability(DownloadSpec),
    /// "Latest" was clicked — find the newest available run for the model.
    LatestRequested(DownloadSpec),
    /// "Download" was clicked.
    StartRequested(DownloadSpec),
    /// "Cancel" was clicked.
    CancelRequested,
}

/// The download panel. Pure widget over host-pushed data.
pub struct DownloadPanel {
    spec: DownloadSpec,
    model_options: Vec<ModelOption>,
    cycle_options: Vec<u8>,
    source_options: Vec<String>,
    hours_hint: String,
    estimate: Option<EstimateView>,
    /// Host-surfaced spec problem (e.g. profile validation, bad hours).
    /// Start stays disabled while set.
    spec_error: Option<String>,
    availability: Option<AvailabilityView>,
    probing: bool,
    /// A failed probe / latest lookup (transient, cleared on the next
    /// probe or spec change).
    probe_error: Option<String>,
    run_state: DownloadRunState,
    progress: Vec<HourProgress>,
    /// Recent note lines from the ingest (warnings + infos), newest last.
    notes: Vec<String>,
    show_breakdown: bool,
}

/// Cap on retained note lines.
const MAX_NOTES: usize = 50;

impl DownloadPanel {
    pub fn new(spec: DownloadSpec) -> Self {
        Self {
            spec,
            model_options: Vec::new(),
            cycle_options: (0..24).collect(),
            source_options: vec!["auto".to_string()],
            hours_hint: String::new(),
            estimate: None,
            spec_error: None,
            availability: None,
            probing: false,
            probe_error: None,
            run_state: DownloadRunState::Idle,
            progress: Vec::new(),
            notes: Vec::new(),
            show_breakdown: false,
        }
    }

    pub fn spec(&self) -> &DownloadSpec {
        &self.spec
    }

    pub fn set_model_options(&mut self, options: Vec<ModelOption>) {
        self.model_options = options;
    }

    pub fn set_cycle_options(&mut self, cycles: Vec<u8>) {
        self.cycle_options = cycles;
    }

    pub fn set_source_options(&mut self, sources: Vec<String>) {
        self.source_options = sources;
    }

    /// E.g. "supported: 0-48 (00z cycle)".
    pub fn set_hours_hint(&mut self, hint: String) {
        self.hours_hint = hint;
    }

    pub fn set_estimate(&mut self, estimate: EstimateView) {
        self.estimate = Some(estimate);
        self.spec_error = None;
    }

    /// A spec problem from the host (validation failed); clears the
    /// estimate and disables Start until a valid spec lands.
    pub fn set_spec_error(&mut self, message: String) {
        self.spec_error = Some(message);
        self.estimate = None;
    }

    pub fn set_availability(&mut self, availability: AvailabilityView) {
        self.probing = false;
        self.probe_error = None;
        self.availability = Some(availability);
    }

    pub fn set_probing(&mut self) {
        self.probing = true;
    }

    /// A probe / latest lookup failed (shown by the hours row until the
    /// next probe or spec change).
    pub fn set_probing_failed(&mut self, message: String) {
        self.probing = false;
        self.probe_error = Some(message);
    }

    /// Snap date + cycle to a host-resolved latest run.
    pub fn set_latest(&mut self, date: String, cycle: u8) {
        self.probing = false;
        self.probe_error = None;
        self.spec.date = date;
        self.spec.cycle = cycle;
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.run_state,
            DownloadRunState::Running | DownloadRunState::Cancelling
        )
    }

    pub fn run_state(&self) -> &DownloadRunState {
        &self.run_state
    }

    /// Start tracking a run over `hours`: one row per hour, every stage
    /// pending.
    pub fn begin_run(&mut self, hours: &[u16]) {
        self.progress = hours
            .iter()
            .map(|&hour| HourProgress {
                hour,
                stages: DownloadStage::ALL
                    .iter()
                    .map(|&stage| (stage, StageState::Pending))
                    .collect(),
                done: false,
                summary: None,
            })
            .collect();
        self.notes.clear();
        self.run_state = DownloadRunState::Running;
    }

    pub fn apply_stage_started(&mut self, hour: u16, stage: DownloadStage) {
        if let Some(state) = self.stage_state_mut(hour, stage) {
            *state = StageState::Running;
        }
    }

    pub fn apply_stage_done(&mut self, hour: u16, stage: DownloadStage, ms: u128) {
        if let Some(state) = self.stage_state_mut(hour, stage) {
            *state = StageState::Done { ms };
        }
    }

    /// Mark one hour complete: stages that never ran become Skipped (e.g.
    /// the compute stages under a sounding profile).
    pub fn apply_hour_done(&mut self, done: HourDoneView) {
        if let Some(row) = self.progress.iter_mut().find(|row| row.hour == done.hour) {
            row.done = true;
            row.summary = Some(format!(
                "{:.1} MB stored in {:.1} s",
                done.store_mb,
                done.total_ms as f64 / 1000.0
            ));
            for (_, state) in &mut row.stages {
                if matches!(state, StageState::Pending | StageState::Running) {
                    *state = StageState::Skipped;
                }
            }
        }
    }

    /// Append one ingest note line (warning/info), keeping the newest
    /// [`MAX_NOTES`].
    pub fn apply_note(&mut self, message: String) {
        self.notes.push(message);
        if self.notes.len() > MAX_NOTES {
            let drop = self.notes.len() - MAX_NOTES;
            self.notes.drain(..drop);
        }
    }

    /// The run ended: `Ok` = every hour landed, `Err` = failure message
    /// (a cancel surfaces as `Err` with a cancel message from the host —
    /// use [`DownloadPanel::finish_cancelled`] for the dedicated state).
    pub fn finish_run(&mut self, result: Result<(), String>) {
        self.run_state = match result {
            Ok(()) => DownloadRunState::Finished,
            Err(message) => DownloadRunState::Failed(message),
        };
    }

    /// The run stopped at a stage boundary after a cancel.
    pub fn finish_cancelled(&mut self) {
        self.run_state = DownloadRunState::Cancelled;
    }

    fn stage_state_mut(&mut self, hour: u16, stage: DownloadStage) -> Option<&mut StageState> {
        self.progress
            .iter_mut()
            .find(|row| row.hour == hour)?
            .stages
            .iter_mut()
            .find(|(have, _)| *have == stage)
            .map(|(_, state)| state)
    }

    /// Render the panel. Returns the events the host should act on.
    pub fn ui(&mut self, ui: &mut Ui) -> Vec<DownloadEvent> {
        let mut events = Vec::new();
        let running = self.is_running();
        let before = self.spec.clone();

        ui.add_enabled_ui(!running, |ui| {
            self.pickers_ui(ui, &mut events);
        });

        if self.spec != before {
            self.availability = None;
            self.probe_error = None;
            events.push(DownloadEvent::SpecChanged(self.spec.clone()));
        }

        ui.separator();
        self.estimate_ui(ui);
        ui.separator();
        self.run_controls_ui(ui, &mut events);
        self.progress_ui(ui);
        events
    }

    fn pickers_ui(&mut self, ui: &mut Ui, events: &mut Vec<DownloadEvent>) {
        // --- model ---
        ui.horizontal(|ui| {
            ui.label("Model");
            let selected_label = self
                .model_options
                .iter()
                .find(|option| option.slug == self.spec.model)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| self.spec.model.clone());
            ComboBox::from_id_salt("rw-ui-download-model")
                .selected_text(selected_label)
                .width(180.0)
                .show_ui(ui, |ui| {
                    for option in &self.model_options {
                        if option.enabled {
                            ui.selectable_value(
                                &mut self.spec.model,
                                option.slug.clone(),
                                &option.label,
                            );
                        } else {
                            ui.add_enabled(
                                false,
                                egui::Button::selectable(false, &option.label),
                            )
                            .on_disabled_hover_text(&option.note);
                        }
                    }
                });
        });

        // --- date + cycle ---
        ui.horizontal(|ui| {
            ui.label("Run");
            if ui.button("◀").on_hover_text("previous day").clicked() {
                if let Some(date) = shift_date_yyyymmdd(&self.spec.date, -1) {
                    self.spec.date = date;
                }
            }
            ui.add(
                TextEdit::singleline(&mut self.spec.date)
                    .desired_width(80.0)
                    .hint_text("YYYYMMDD"),
            );
            if ui.button("▶").on_hover_text("next day").clicked() {
                if let Some(date) = shift_date_yyyymmdd(&self.spec.date, 1) {
                    self.spec.date = date;
                }
            }
            ComboBox::from_id_salt("rw-ui-download-cycle")
                .selected_text(format!("{:02}z", self.spec.cycle))
                .width(64.0)
                .show_ui(ui, |ui| {
                    for &cycle in &self.cycle_options {
                        ui.selectable_value(&mut self.spec.cycle, cycle, format!("{cycle:02}z"));
                    }
                });
            if ui
                .button("Latest")
                .on_hover_text("probe for the newest available run and snap to it")
                .clicked()
            {
                self.probing = true;
                events.push(DownloadEvent::LatestRequested(self.spec.clone()));
            }
        });

        // --- hours + availability ---
        ui.horizontal(|ui| {
            ui.label("Hours");
            ui.add(
                TextEdit::singleline(&mut self.spec.hours)
                    .desired_width(110.0)
                    .hint_text("0-6 or 0,3,6"),
            );
            if !self.hours_hint.is_empty() {
                ui.label(RichText::new(&self.hours_hint).small().weak());
            }
            if ui
                .button("Check availability")
                .on_hover_text("probe which hours of this run exist upstream (one HEAD per hour)")
                .clicked()
            {
                self.probing = true;
                events.push(DownloadEvent::CheckAvailability(self.spec.clone()));
            }
            if self.probing {
                ui.spinner();
            }
        });
        if let Some(message) = &self.probe_error {
            ui.colored_label(ui.visuals().error_fg_color, message);
        }
        if let Some(availability) = &self.availability {
            if availability.matches(&self.spec) {
                availability_chips(ui, availability);
            }
        }

        // --- profile ---
        ui.horizontal(|ui| {
            ui.label("Profile");
            let previous_profile = self.spec.profile.clone();
            ComboBox::from_id_salt("rw-ui-download-profile")
                .selected_text(&self.spec.profile)
                .width(100.0)
                .show_ui(ui, |ui| {
                    for preset in ["full", "sounding", "view"] {
                        ui.selectable_value(
                            &mut self.spec.profile,
                            preset.to_string(),
                            preset,
                        );
                    }
                });
            if self.spec.profile != previous_profile {
                // Snap the toggles to the preset's defaults so the combo
                // always lands on a valid combination.
                let (derived, heavy) = match self.spec.profile.as_str() {
                    "sounding" => (false, false),
                    "view" => (true, false),
                    _ => (true, true),
                };
                self.spec.derived = derived;
                self.spec.heavy = heavy;
            }
            ComboBox::from_id_salt("rw-ui-download-levelstep")
                .selected_text(format!("{} hPa", self.spec.level_step_hpa))
                .width(70.0)
                .show_ui(ui, |ui| {
                    for step in [25u16, 50] {
                        ui.selectable_value(
                            &mut self.spec.level_step_hpa,
                            step,
                            format!("{step} hPa"),
                        );
                    }
                });
            ui.checkbox(&mut self.spec.derived, "derived");
            ui.checkbox(&mut self.spec.heavy, "heavy");
            ui.checkbox(&mut self.spec.verify, "verify")
                .on_hover_text("re-open each written hour and verify a bit-exact round-trip");
        });

        // --- source + cache ---
        ui.horizontal(|ui| {
            ui.label("Source");
            ComboBox::from_id_salt("rw-ui-download-source")
                .selected_text(&self.spec.source)
                .width(90.0)
                .show_ui(ui, |ui| {
                    for source in &self.source_options {
                        ui.selectable_value(&mut self.spec.source, source.clone(), source);
                    }
                });
            ui.label("Cache");
            ui.add(TextEdit::singleline(&mut self.spec.cache_dir).desired_width(180.0))
                .on_hover_text(
                    "raw GRIB byte cache; a warm cache makes the fetch stage a disk read",
                );
        });
    }

    fn estimate_ui(&mut self, ui: &mut Ui) {
        if let Some(message) = &self.spec_error {
            ui.colored_label(ui.visuals().error_fg_color, message);
            return;
        }
        let Some(estimate) = &self.estimate else {
            ui.label(RichText::new("estimating…").small().weak());
            return;
        };
        ui.label(
            RichText::new(format!(
                "{} hour(s): store {} | download {} | {}",
                estimate.hour_count,
                format_bytes(estimate.store_bytes),
                format_bytes(estimate.download_bytes),
                estimate.time_hint,
            ))
            .strong(),
        );
        ui.label(
            RichText::new(format!(
                "per hour: store {} | download {} · {}",
                format_bytes(estimate.per_hour_store_bytes),
                format_bytes(estimate.per_hour_download_bytes),
                estimate.profile_summary,
            ))
            .small(),
        );
        ui.label(
            RichText::new(format!("calibration: {}", estimate.calibration))
                .small()
                .weak(),
        );
        ui.checkbox(&mut self.show_breakdown, "per-variable breakdown");
        if self.show_breakdown {
            ScrollArea::vertical()
                .id_salt("rw-ui-download-breakdown")
                .max_height(140.0)
                .show(ui, |ui| {
                    egui::Grid::new("rw-ui-download-breakdown-grid")
                        .striped(true)
                        .min_col_width(40.0)
                        .show(ui, |ui| {
                            for (name, bytes) in &estimate.breakdown {
                                ui.label(RichText::new(name).small().monospace());
                                ui.label(RichText::new(format_bytes(*bytes)).small());
                                ui.end_row();
                            }
                        });
                });
        }
    }

    fn run_controls_ui(&mut self, ui: &mut Ui, events: &mut Vec<DownloadEvent>) {
        ui.horizontal(|ui| {
            match &self.run_state {
                DownloadRunState::Running => {
                    ui.spinner();
                    if ui.button("Cancel").clicked() {
                        self.run_state = DownloadRunState::Cancelling;
                        events.push(DownloadEvent::CancelRequested);
                    }
                }
                DownloadRunState::Cancelling => {
                    ui.spinner();
                    ui.label(
                        RichText::new(
                            "cancelling — the in-flight stage completes first; \
                             no partial files are left behind",
                        )
                        .small()
                        .weak(),
                    );
                }
                state => {
                    let can_start = self.spec_error.is_none() && self.estimate.is_some();
                    if ui
                        .add_enabled(can_start, egui::Button::new("⬇ Download"))
                        .clicked()
                    {
                        events.push(DownloadEvent::StartRequested(self.spec.clone()));
                    }
                    match state {
                        DownloadRunState::Finished => {
                            ui.label(
                                RichText::new("done — all hours landed")
                                    .small()
                                    .color(Color32::from_rgb(96, 192, 96)),
                            );
                        }
                        DownloadRunState::Cancelled => {
                            ui.label(RichText::new("cancelled").small().weak());
                        }
                        DownloadRunState::Failed(message) => {
                            ui.colored_label(ui.visuals().error_fg_color, message);
                        }
                        _ => {}
                    }
                }
            };
        });
    }

    fn progress_ui(&mut self, ui: &mut Ui) {
        if self.progress.is_empty() {
            return;
        }
        ui.add_space(4.0);
        for row in &self.progress {
            ui.horizontal_wrapped(|ui| {
                let hour_label = RichText::new(format!("f{:03}", row.hour)).monospace();
                ui.label(if row.done {
                    hour_label.strong()
                } else {
                    hour_label
                });
                for (stage, state) in &row.stages {
                    stage_chip(ui, *stage, *state);
                }
                if let Some(summary) = &row.summary {
                    ui.label(RichText::new(summary).small().weak());
                }
            });
        }
        if !self.notes.is_empty() {
            egui::CollapsingHeader::new(format!("notes ({})", self.notes.len()))
                .id_salt("rw-ui-download-notes")
                .show(ui, |ui| {
                    ScrollArea::vertical()
                        .id_salt("rw-ui-download-notes-scroll")
                        .max_height(120.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for note in &self.notes {
                                ui.label(RichText::new(note).small().monospace());
                            }
                        });
                });
        }
    }
}

/// One stage chip: gray = pending, spinner-ish highlight = running,
/// green + ms = done, dim strikethrough = skipped.
fn stage_chip(ui: &mut Ui, stage: DownloadStage, state: StageState) {
    let label = stage.label();
    match state {
        StageState::Pending => {
            ui.label(RichText::new(label).small().weak());
        }
        StageState::Running => {
            ui.label(
                RichText::new(format!("▶ {label}"))
                    .small()
                    .color(Color32::from_rgb(255, 200, 80)),
            );
        }
        StageState::Done { ms } => {
            ui.label(
                RichText::new(format!("{label} {ms} ms"))
                    .small()
                    .color(Color32::from_rgb(96, 192, 96)),
            );
        }
        StageState::Skipped => {
            ui.label(RichText::new(label).small().weak().strikethrough());
        }
    }
}

/// Wrapped per-hour availability chips: green = probed available, dim =
/// not seen upstream (yet).
fn availability_chips(ui: &mut Ui, availability: &AvailabilityView) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!(
                "available {} / {} hours:",
                availability.available.len(),
                availability.candidates.len()
            ))
            .small(),
        );
        for &hour in &availability.candidates {
            let text = RichText::new(format!("{hour}")).small().monospace();
            if availability.available.contains(&hour) {
                ui.label(text.color(Color32::from_rgb(96, 192, 96)));
            } else {
                ui.label(text.weak());
            }
        }
    });
    if let Some(note) = &availability.note {
        ui.label(RichText::new(note).small().weak());
    }
}

/// Human-readable bytes (binary units, one decimal).
pub fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.2} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else {
        format!("{:.0} KB", bytes / 1024.0)
    }
}

/// Shift a YYYYMMDD date by whole days using the proleptic Gregorian
/// calendar (Howard Hinnant's `days_from_civil` round trip). Returns `None`
/// for malformed input.
pub fn shift_date_yyyymmdd(date: &str, delta_days: i64) -> Option<String> {
    if date.len() != 8 || !date.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year: i64 = date[0..4].parse().ok()?;
    let month: u32 = date[4..6].parse().ok()?;
    let day: u32 = date[6..8].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day) + delta_days;
    let (year, month, day) = civil_from_days(days);
    Some(format!("{year:04}{month:02}{day:02}"))
}

/// Today's date in UTC as YYYYMMDD (from the system clock; no chrono).
pub fn today_yyyymmdd_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    format!("{year:04}{month:02}{day:02}")
}

/// Days since 1970-01-01 for a civil date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = (year - era * 400) as u64; // [0, 399]
    let mp = ((month + 9) % 12) as u64; // March = 0
    let doy = (153 * mp + 2) / 5 + (day as u64 - 1); // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// Civil date from days since 1970-01-01 (inverse of [`days_from_civil`]).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let doe = (days - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_shift_handles_month_year_and_leap_boundaries() {
        assert_eq!(shift_date_yyyymmdd("20260608", 1).unwrap(), "20260609");
        assert_eq!(shift_date_yyyymmdd("20260608", -1).unwrap(), "20260607");
        assert_eq!(shift_date_yyyymmdd("20260601", -1).unwrap(), "20260531");
        assert_eq!(shift_date_yyyymmdd("20251231", 1).unwrap(), "20260101");
        assert_eq!(shift_date_yyyymmdd("20260101", -1).unwrap(), "20251231");
        // 2024 is a leap year; 2026 is not.
        assert_eq!(shift_date_yyyymmdd("20240228", 1).unwrap(), "20240229");
        assert_eq!(shift_date_yyyymmdd("20260228", 1).unwrap(), "20260301");
        assert_eq!(shift_date_yyyymmdd("garbage", 1), None);
        assert_eq!(shift_date_yyyymmdd("2026130", 1), None);
        assert_eq!(shift_date_yyyymmdd("20261301", 1), None, "month 13");
    }

    #[test]
    fn today_utc_is_a_well_formed_shiftable_date() {
        let today = today_yyyymmdd_utc();
        assert_eq!(today.len(), 8);
        // Round-trips through the same civil-date math.
        assert_eq!(shift_date_yyyymmdd(&today, 0).as_deref(), Some(&*today));
        assert!(today.as_str() >= "20260101", "clock sanity: {today}");
    }

    #[test]
    fn date_shift_round_trips_across_a_decade() {
        let mut date = "20200101".to_string();
        for _ in 0..3653 {
            date = shift_date_yyyymmdd(&date, 1).unwrap();
        }
        assert_eq!(date, "20300101");
        for _ in 0..3653 {
            date = shift_date_yyyymmdd(&date, -1).unwrap();
        }
        assert_eq!(date, "20200101");
    }

    #[test]
    fn format_bytes_picks_sane_units() {
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    fn panel() -> DownloadPanel {
        DownloadPanel::new(DownloadSpec {
            date: "20260608".to_string(),
            ..DownloadSpec::default()
        })
    }

    /// The exact event order the ingest worker emits: per-stage start/done,
    /// hour completion (skipping the never-run stages), then run finish.
    #[test]
    fn progress_bookkeeping_follows_event_order() {
        let mut panel = panel();
        assert!(!panel.is_running());
        panel.begin_run(&[4, 5]);
        assert!(panel.is_running());
        assert_eq!(panel.progress.len(), 2);
        assert!(
            panel.progress[0]
                .stages
                .iter()
                .all(|(_, state)| *state == StageState::Pending)
        );

        panel.apply_stage_started(4, DownloadStage::FetchPrs);
        assert_eq!(
            panel.progress[0].stages[0].1,
            StageState::Running,
            "fetch prs runs"
        );
        panel.apply_stage_done(4, DownloadStage::FetchPrs, 1200);
        assert_eq!(panel.progress[0].stages[0].1, StageState::Done { ms: 1200 });

        // Hour 4 finishes under a sounding-style profile: thermo/derived/
        // heavy/verify never ran and must show as skipped, not pending.
        panel.apply_stage_done(4, DownloadStage::Write, 800);
        panel.apply_hour_done(HourDoneView {
            hour: 4,
            store_mb: 250.0,
            total_ms: 9000,
        });
        let row = &panel.progress[0];
        assert!(row.done);
        assert_eq!(row.summary.as_deref(), Some("250.0 MB stored in 9.0 s"));
        let state_of = |stage: DownloadStage| {
            row.stages
                .iter()
                .find(|(have, _)| *have == stage)
                .unwrap()
                .1
        };
        assert_eq!(state_of(DownloadStage::Write), StageState::Done { ms: 800 });
        assert_eq!(state_of(DownloadStage::Heavy), StageState::Skipped);
        assert_eq!(state_of(DownloadStage::FetchSfc), StageState::Skipped);

        // Hour 5 untouched.
        assert!(
            panel.progress[1]
                .stages
                .iter()
                .all(|(_, state)| *state == StageState::Pending)
        );

        panel.finish_run(Ok(()));
        assert!(!panel.is_running());
        assert_eq!(panel.run_state, DownloadRunState::Finished);
    }

    #[test]
    fn cancel_and_failure_states_are_distinct() {
        let mut panel = panel();
        panel.begin_run(&[7]);
        panel.finish_cancelled();
        assert_eq!(panel.run_state, DownloadRunState::Cancelled);
        assert!(!panel.is_running());

        panel.begin_run(&[7]);
        panel.finish_run(Err("f007: fetch: 404".to_string()));
        assert_eq!(
            panel.run_state,
            DownloadRunState::Failed("f007: fetch: 404".to_string())
        );
    }

    #[test]
    fn notes_are_capped() {
        let mut panel = panel();
        for index in 0..(MAX_NOTES + 10) {
            panel.apply_note(format!("note {index}"));
        }
        assert_eq!(panel.notes.len(), MAX_NOTES);
        assert_eq!(panel.notes[0], "note 10", "oldest notes drop first");
    }

    #[test]
    fn spec_error_clears_estimate_and_vice_versa() {
        let mut panel = panel();
        panel.set_estimate(EstimateView {
            profile_summary: "p".into(),
            hour_count: 1,
            store_bytes: 1,
            download_bytes: 1,
            per_hour_store_bytes: 1,
            per_hour_download_bytes: 1,
            calibration: "c".into(),
            time_hint: "t".into(),
            breakdown: vec![],
        });
        assert!(panel.estimate.is_some());
        panel.set_spec_error("--hours: invalid token 'x'".to_string());
        assert!(panel.estimate.is_none());
        assert!(panel.spec_error.is_some());
        panel.set_estimate(EstimateView {
            profile_summary: "p".into(),
            hour_count: 1,
            store_bytes: 1,
            download_bytes: 1,
            per_hour_store_bytes: 1,
            per_hour_download_bytes: 1,
            calibration: "c".into(),
            time_hint: "t".into(),
            breakdown: vec![],
        });
        assert!(panel.spec_error.is_none());
    }

    #[test]
    fn availability_matches_keys_only() {
        let view = AvailabilityView {
            model: "hrrr".into(),
            date: "20260608".into(),
            cycle: 0,
            candidates: (0..=48).collect(),
            available: (0..=18).collect(),
            note: None,
        };
        let mut spec = DownloadSpec {
            date: "20260608".to_string(),
            cycle: 0,
            ..DownloadSpec::default()
        };
        assert!(view.matches(&spec));
        spec.cycle = 6;
        assert!(!view.matches(&spec));
    }
}
