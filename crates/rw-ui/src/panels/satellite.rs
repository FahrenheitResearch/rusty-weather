//! Satellite follow panel: pick a satellite / sector / layer, start a live
//! follow session against the GOES bucket, and watch frames land with
//! per-frame download/store progress, rolling-window eviction reports, and
//! a live disk-budget readout.
//!
//! Pure widget, same pattern as [`super::download::DownloadPanel`]: it never
//! touches the network, rw-sat, or the store. User intent leaves as
//! [`SatelliteEvent`]s; the host (which owns a sat worker) pushes state back
//! through `set_*` / `apply_*` / `begin_follow` / `finish_follow`. bowecho
//! gets this panel for free without any satellite wiring.
//!
//! Stop semantics are honest: the cancel flag is observed at poll/frame
//! boundaries, so an in-flight download completes first; frame writes are
//! atomic, so no partial files.

use std::time::Instant;

use egui::{Color32, ComboBox, DragValue, RichText, ScrollArea, TextEdit, Ui};

use super::download::format_bytes;

/// What the user wants followed — plain data only, host-interpretable.
#[derive(Debug, Clone, PartialEq)]
pub struct SatFollowSpec {
    /// Satellite slug (e.g. "goes19").
    pub satellite: String,
    /// Sector slug: "conus" | "full_disk" | "meso1" | "meso2".
    pub sector: String,
    /// Layer slug: a band ("c13") or an RGB composite ("geocolor" — the
    /// host expands it to the required bands).
    pub layer: String,
    /// Poll on the sector's default interval.
    pub auto_interval: bool,
    /// Base poll interval when `auto_interval` is off, seconds.
    pub interval_secs: u64,
    /// Rolling window: evict frames older than `keep_hours`.
    pub keep_enabled: bool,
    pub keep_hours: f32,
    /// Rolling window: evict oldest frames beyond `max_gb` per band.
    pub size_enabled: bool,
    pub max_gb: f32,
    /// Stride decimation before storing (1 = native resolution).
    pub downsample: usize,
    /// Raw NetCDF byte cache directory.
    pub cache_dir: String,
}

impl Default for SatFollowSpec {
    fn default() -> Self {
        Self {
            satellite: "goes19".to_string(),
            sector: "conus".to_string(),
            layer: "c13".to_string(),
            auto_interval: true,
            interval_secs: 30,
            keep_enabled: true,
            keep_hours: 6.0,
            size_enabled: true,
            max_gb: 2.0,
            downsample: 1,
            cache_dir: "out/cache".to_string(),
        }
    }
}

impl SatFollowSpec {
    /// Poll interval override (`None` = sector default).
    pub fn interval(&self) -> Option<u64> {
        (!self.auto_interval).then_some(self.interval_secs.max(1))
    }

    /// Rolling-window age limit in minutes (`None` = unlimited).
    pub fn max_age_minutes(&self) -> Option<u32> {
        self.keep_enabled
            .then(|| (self.keep_hours.max(0.0) * 60.0).round() as u32)
    }

    /// Rolling-window byte limit (`None` = unlimited).
    pub fn max_bytes(&self) -> Option<u64> {
        self.size_enabled
            .then(|| (f64::from(self.max_gb.max(0.0)) * 1024.0 * 1024.0 * 1024.0) as u64)
    }
}

/// One pickable satellite.
#[derive(Debug, Clone, PartialEq)]
pub struct SatSatelliteOption {
    pub slug: String,
    pub label: String,
}

/// One pickable sector, with its poll/scan timing for the interval display.
#[derive(Debug, Clone, PartialEq)]
pub struct SatSectorOption {
    pub slug: String,
    pub label: String,
    /// Default poll interval used when `auto_interval` is on, seconds.
    pub default_poll_secs: u64,
    /// Observed scan cadence (how often new frames actually land), seconds.
    pub cadence_secs: u64,
}

/// One pickable layer: a single ABI band or an RGB composite.
#[derive(Debug, Clone, PartialEq)]
pub struct SatLayerOption {
    pub slug: String,
    pub label: String,
    /// Shown next to the picker (e.g. "follows C01+C02+C03" for composites).
    pub note: String,
}

/// What the user did this frame; the host turns these into worker requests.
#[derive(Debug, Clone, PartialEq)]
pub enum SatelliteEvent {
    /// Any picker changed — revalidate and refresh the summary line.
    SpecChanged(SatFollowSpec),
    /// "Follow live" was clicked.
    StartRequested(SatFollowSpec),
    /// "Stop" was clicked.
    StopRequested,
}

/// Follow session lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SatFollowState {
    #[default]
    Idle,
    Running,
    /// Stop requested; the in-flight download/ingest completes first.
    Stopping,
    Finished(String),
    Failed(String),
}

/// Per-frame ingest progress.
#[derive(Debug, Clone, PartialEq)]
enum SatFrameStage {
    Downloading {
        bytes: u64,
    },
    Storing {
        download_ms: u128,
        cache_hit: bool,
    },
    Done {
        run: String,
        hhmm: u16,
        store_bytes: u64,
        encode_ms: u64,
    },
}

/// One frame's row: a download that (hopefully) becomes a stored frame.
#[derive(Debug, Clone, PartialEq)]
struct SatFrameRow {
    /// Host-chosen id (the S3 key) tying the stage events together.
    id: String,
    /// Host-built label (e.g. "C13 19:21:18Z").
    label: String,
    stage: SatFrameStage,
}

/// Host-pushed live disk usage of the followed band(s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SatDiskUsage {
    pub bytes: u64,
    pub frames: usize,
}

/// Cap on retained frame rows (oldest drop first).
const MAX_FRAME_ROWS: usize = 40;
/// Cap on retained note lines.
const MAX_NOTES: usize = 50;

/// The satellite follow panel. Pure widget over host-pushed data.
pub struct SatellitePanel {
    spec: SatFollowSpec,
    satellite_options: Vec<SatSatelliteOption>,
    sector_options: Vec<SatSectorOption>,
    layer_options: Vec<SatLayerOption>,
    /// Host-validated one-line spec summary (bands, interval, window).
    spec_summary: Option<String>,
    /// Host-surfaced spec problem. Start stays disabled while set.
    spec_error: Option<String>,
    state: SatFollowState,
    rows: Vec<SatFrameRow>,
    last_poll: Option<String>,
    /// When the follow engine went to sleep and for how long (countdown).
    sleeping: Option<(Instant, u64)>,
    polls: u32,
    frames_ingested: u32,
    evicted_frames: usize,
    evicted_bytes: u64,
    disk: Option<SatDiskUsage>,
    notes: Vec<String>,
}

impl SatellitePanel {
    pub fn new(spec: SatFollowSpec) -> Self {
        Self {
            spec,
            satellite_options: Vec::new(),
            sector_options: Vec::new(),
            layer_options: Vec::new(),
            spec_summary: None,
            spec_error: None,
            state: SatFollowState::Idle,
            rows: Vec::new(),
            last_poll: None,
            sleeping: None,
            polls: 0,
            frames_ingested: 0,
            evicted_frames: 0,
            evicted_bytes: 0,
            disk: None,
            notes: Vec::new(),
        }
    }

    pub fn spec(&self) -> &SatFollowSpec {
        &self.spec
    }

    pub fn set_satellite_options(&mut self, options: Vec<SatSatelliteOption>) {
        self.satellite_options = options;
    }

    pub fn set_sector_options(&mut self, options: Vec<SatSectorOption>) {
        self.sector_options = options;
    }

    pub fn set_layer_options(&mut self, options: Vec<SatLayerOption>) {
        self.layer_options = options;
    }

    /// Host validation result for the current spec: `Ok` carries the
    /// summary line, `Err` the problem (Start disabled while set).
    pub fn set_spec_status(&mut self, status: Result<String, String>) {
        match status {
            Ok(summary) => {
                self.spec_summary = Some(summary);
                self.spec_error = None;
            }
            Err(message) => {
                self.spec_summary = None;
                self.spec_error = Some(message);
            }
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.state,
            SatFollowState::Running | SatFollowState::Stopping
        )
    }

    pub fn state(&self) -> &SatFollowState {
        &self.state
    }

    /// A follow session started: reset counters and rows.
    pub fn begin_follow(&mut self) {
        self.rows.clear();
        self.notes.clear();
        self.last_poll = None;
        self.sleeping = None;
        self.polls = 0;
        self.frames_ingested = 0;
        self.evicted_frames = 0;
        self.evicted_bytes = 0;
        self.state = SatFollowState::Running;
    }

    /// The session ended: `Ok` = clean stop / natural end (message shown
    /// green), `Err` = failure message.
    pub fn finish_follow(&mut self, result: Result<String, String>) {
        self.sleeping = None;
        self.state = match result {
            Ok(message) => SatFollowState::Finished(message),
            Err(message) => SatFollowState::Failed(message),
        };
    }

    pub fn apply_poll_done(&mut self, band: u8, new_keys: usize, ms: u128) {
        self.polls += 1;
        self.sleeping = None;
        self.last_poll = Some(format!("poll C{band:02}: {new_keys} new in {ms} ms"));
    }

    pub fn apply_download_started(&mut self, id: String, label: String, bytes: u64) {
        self.rows.push(SatFrameRow {
            id,
            label,
            stage: SatFrameStage::Downloading { bytes },
        });
        if self.rows.len() > MAX_FRAME_ROWS {
            let drop = self.rows.len() - MAX_FRAME_ROWS;
            self.rows.drain(..drop);
        }
    }

    pub fn apply_download_done(&mut self, id: &str, ms: u128, cache_hit: bool) {
        if let Some(row) = self.rows.iter_mut().rev().find(|row| row.id == id) {
            row.stage = SatFrameStage::Storing {
                download_ms: ms,
                cache_hit,
            };
        }
    }

    pub fn apply_frame_written(
        &mut self,
        id: &str,
        run: String,
        hhmm: u16,
        store_bytes: u64,
        encode_ms: u64,
    ) {
        self.frames_ingested += 1;
        if let Some(row) = self.rows.iter_mut().rev().find(|row| row.id == id) {
            row.stage = SatFrameStage::Done {
                run,
                hhmm,
                store_bytes,
                encode_ms,
            };
        }
    }

    pub fn apply_evicted(&mut self, frames: usize, bytes: u64) {
        self.evicted_frames += frames;
        self.evicted_bytes += bytes;
    }

    /// The follow engine is sleeping until the next poll (countdown source).
    pub fn apply_sleeping(&mut self, ms: u64) {
        self.sleeping = Some((Instant::now(), ms));
    }

    /// Append one note line (warnings/infos), keeping the newest
    /// [`MAX_NOTES`].
    pub fn apply_note(&mut self, message: String) {
        self.notes.push(message);
        if self.notes.len() > MAX_NOTES {
            let drop = self.notes.len() - MAX_NOTES;
            self.notes.drain(..drop);
        }
    }

    pub fn set_disk_usage(&mut self, usage: SatDiskUsage) {
        self.disk = Some(usage);
    }

    /// Seconds until the next poll, from the last `Sleeping` event.
    fn next_poll_secs(&self) -> Option<f32> {
        let (started, ms) = self.sleeping?;
        let total = ms as f32 / 1000.0;
        Some((total - started.elapsed().as_secs_f32()).max(0.0))
    }

    /// Render the panel. Returns the events the host should act on.
    pub fn ui(&mut self, ui: &mut Ui) -> Vec<SatelliteEvent> {
        let mut events = Vec::new();
        let running = self.is_running();
        let before = self.spec.clone();

        ui.add_enabled_ui(!running, |ui| {
            self.pickers_ui(ui);
        });
        if self.spec != before {
            self.spec_summary = None;
            self.spec_error = None;
            events.push(SatelliteEvent::SpecChanged(self.spec.clone()));
        }

        self.summary_ui(ui);
        ui.separator();
        self.controls_ui(ui, &mut events);
        self.session_ui(ui);
        events
    }

    fn pickers_ui(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("Satellite");
            let selected = self
                .satellite_options
                .iter()
                .find(|option| option.slug == self.spec.satellite)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| self.spec.satellite.clone());
            ComboBox::from_id_salt("rw-ui-sat-satellite")
                .selected_text(selected)
                .width(150.0)
                .show_ui(ui, |ui| {
                    for option in &self.satellite_options {
                        ui.selectable_value(
                            &mut self.spec.satellite,
                            option.slug.clone(),
                            &option.label,
                        );
                    }
                });

            ui.label("Sector");
            let selected = self
                .sector_options
                .iter()
                .find(|option| option.slug == self.spec.sector)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| self.spec.sector.clone());
            ComboBox::from_id_salt("rw-ui-sat-sector")
                .selected_text(selected)
                .width(120.0)
                .show_ui(ui, |ui| {
                    for option in &self.sector_options {
                        ui.selectable_value(
                            &mut self.spec.sector,
                            option.slug.clone(),
                            &option.label,
                        );
                    }
                });
        });

        ui.horizontal(|ui| {
            ui.label("Layer");
            let selected = self
                .layer_options
                .iter()
                .find(|option| option.slug == self.spec.layer)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| self.spec.layer.clone());
            ComboBox::from_id_salt("rw-ui-sat-layer")
                .selected_text(selected)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for option in &self.layer_options {
                        let response = ui.selectable_value(
                            &mut self.spec.layer,
                            option.slug.clone(),
                            &option.label,
                        );
                        if !option.note.is_empty() {
                            response.on_hover_text(&option.note);
                        }
                    }
                });
            if let Some(note) = self
                .layer_options
                .iter()
                .find(|option| option.slug == self.spec.layer)
                .map(|option| option.note.as_str())
                .filter(|note| !note.is_empty())
            {
                ui.label(RichText::new(note).small().weak());
            }
        });

        ui.horizontal(|ui| {
            ui.label("Poll");
            ui.checkbox(&mut self.spec.auto_interval, "auto")
                .on_hover_text("use the sector's default poll interval");
            if !self.spec.auto_interval {
                ui.add(
                    DragValue::new(&mut self.spec.interval_secs)
                        .range(5..=600)
                        .suffix(" s"),
                );
            } else if let Some(sector) = self
                .sector_options
                .iter()
                .find(|option| option.slug == self.spec.sector)
            {
                ui.label(
                    RichText::new(format!(
                        "every ~{} s (new frames ~{} s apart)",
                        sector.default_poll_secs, sector.cadence_secs
                    ))
                    .small()
                    .weak(),
                );
            }
            ui.label("Detail");
            ComboBox::from_id_salt("rw-ui-sat-downsample")
                .selected_text(downsample_label(self.spec.downsample))
                .width(110.0)
                .show_ui(ui, |ui| {
                    for step in [1usize, 2, 4] {
                        ui.selectable_value(
                            &mut self.spec.downsample,
                            step,
                            downsample_label(step),
                        );
                    }
                })
                .response
                .on_hover_text(
                    "stride decimation before storing — keeps hi-res visible \
                     bands (C02 is 0.5 km) at a sane store size",
                );
        });

        ui.horizontal(|ui| {
            ui.label("Window");
            ui.checkbox(&mut self.spec.keep_enabled, "keep")
                .on_hover_text("evict frames older than this");
            ui.add_enabled(
                self.spec.keep_enabled,
                DragValue::new(&mut self.spec.keep_hours)
                    .range(0.5..=72.0)
                    .speed(0.5)
                    .suffix(" h"),
            );
            ui.checkbox(&mut self.spec.size_enabled, "max")
                .on_hover_text("evict oldest frames beyond this total size per band");
            ui.add_enabled(
                self.spec.size_enabled,
                DragValue::new(&mut self.spec.max_gb)
                    .range(0.1..=64.0)
                    .speed(0.1)
                    .suffix(" GB"),
            );
            ui.label("Cache");
            ui.add(TextEdit::singleline(&mut self.spec.cache_dir).desired_width(140.0))
                .on_hover_text(
                    "raw NetCDF staging dir; follow deletes objects once their \
                     frame is stored and prunes stale leftovers with the window",
                );
        });
    }

    fn summary_ui(&mut self, ui: &mut Ui) {
        if let Some(message) = &self.spec_error {
            ui.colored_label(ui.visuals().error_fg_color, message);
            return;
        }
        match &self.spec_summary {
            Some(summary) => {
                ui.label(RichText::new(summary).small());
            }
            None => {
                ui.label(RichText::new("checking spec…").small().weak());
            }
        }
    }

    fn controls_ui(&mut self, ui: &mut Ui, events: &mut Vec<SatelliteEvent>) {
        ui.horizontal(|ui| {
            match &self.state {
                SatFollowState::Running => {
                    ui.spinner();
                    if ui.button("⏹ Stop").clicked() {
                        self.state = SatFollowState::Stopping;
                        events.push(SatelliteEvent::StopRequested);
                    }
                    match self.next_poll_secs() {
                        Some(secs) => {
                            ui.label(
                                RichText::new(format!("next poll in ~{secs:.0} s"))
                                    .small()
                                    .weak(),
                            );
                        }
                        None => {
                            ui.label(RichText::new("polling…").small().weak());
                        }
                    }
                }
                SatFollowState::Stopping => {
                    ui.spinner();
                    ui.label(
                        RichText::new(
                            "stopping — the in-flight frame completes first; \
                             no partial files are left behind",
                        )
                        .small()
                        .weak(),
                    );
                }
                state => {
                    let can_start = self.spec_error.is_none();
                    if ui
                        .add_enabled(can_start, egui::Button::new("▶ Follow live"))
                        .clicked()
                    {
                        events.push(SatelliteEvent::StartRequested(self.spec.clone()));
                    }
                    match state {
                        SatFollowState::Finished(message) => {
                            ui.label(
                                RichText::new(message)
                                    .small()
                                    .color(Color32::from_rgb(96, 192, 96)),
                            );
                        }
                        SatFollowState::Failed(message) => {
                            ui.colored_label(ui.visuals().error_fg_color, message);
                        }
                        _ => {}
                    }
                }
            };
        });
    }

    fn session_ui(&mut self, ui: &mut Ui) {
        let has_session = self.polls > 0
            || !self.rows.is_empty()
            || self.frames_ingested > 0
            || self.disk.is_some();
        if !has_session {
            return;
        }
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let mut status = format!(
                "polls {} · frames {} · evicted {} ({})",
                self.polls,
                self.frames_ingested,
                self.evicted_frames,
                format_bytes(self.evicted_bytes)
            );
            if let Some(poll) = &self.last_poll {
                status.push_str(" · ");
                status.push_str(poll);
            }
            ui.label(RichText::new(status).small().weak());
        });
        if let Some(disk) = &self.disk {
            let budget = match (self.spec.max_age_minutes(), self.spec.max_bytes()) {
                (Some(minutes), Some(bytes)) => format!(
                    " · budget {} / {:.1} h",
                    format_bytes(bytes),
                    f64::from(minutes) / 60.0
                ),
                (Some(minutes), None) => format!(" · budget {:.1} h", f64::from(minutes) / 60.0),
                (None, Some(bytes)) => format!(" · budget {}", format_bytes(bytes)),
                (None, None) => " · unbounded".to_string(),
            };
            ui.label(
                RichText::new(format!(
                    "on disk {} · {} frame(s){budget}",
                    format_bytes(disk.bytes),
                    disk.frames
                ))
                .small()
                .strong(),
            );
        }

        if !self.rows.is_empty() {
            ScrollArea::vertical()
                .id_salt("rw-ui-sat-rows")
                .max_height(160.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for row in &self.rows {
                        frame_row_ui(ui, row);
                    }
                });
        }
        if !self.notes.is_empty() {
            egui::CollapsingHeader::new(format!("notes ({})", self.notes.len()))
                .id_salt("rw-ui-sat-notes")
                .show(ui, |ui| {
                    ScrollArea::vertical()
                        .id_salt("rw-ui-sat-notes-scroll")
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

fn downsample_label(step: usize) -> String {
    match step {
        1 => "native".to_string(),
        step => format!("1/{step} res"),
    }
}

/// One frame row: label + stage chip.
fn frame_row_ui(ui: &mut Ui, row: &SatFrameRow) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&row.label).small().monospace());
        match &row.stage {
            SatFrameStage::Downloading { bytes } => {
                ui.label(
                    RichText::new(format!("▼ downloading {}", format_bytes(*bytes)))
                        .small()
                        .color(Color32::from_rgb(255, 200, 80)),
                );
            }
            SatFrameStage::Storing {
                download_ms,
                cache_hit,
            } => {
                let cached = if *cache_hit { " (cache hit)" } else { "" };
                ui.label(
                    RichText::new(format!("▼ {download_ms} ms{cached} · storing…"))
                        .small()
                        .color(Color32::from_rgb(255, 200, 80)),
                );
            }
            SatFrameStage::Done {
                run,
                hhmm,
                store_bytes,
                encode_ms,
            } => {
                ui.label(
                    RichText::new(format!(
                        "{run}/t{hhmm:04} · {} stored in {encode_ms} ms",
                        format_bytes(*store_bytes)
                    ))
                    .small()
                    .color(Color32::from_rgb(96, 192, 96)),
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> SatellitePanel {
        SatellitePanel::new(SatFollowSpec::default())
    }

    #[test]
    fn spec_maps_interval_and_window_limits() {
        let mut spec = SatFollowSpec::default();
        assert_eq!(spec.interval(), None, "auto interval -> sector default");
        spec.auto_interval = false;
        spec.interval_secs = 45;
        assert_eq!(spec.interval(), Some(45));

        assert_eq!(spec.max_age_minutes(), Some(360), "6 h -> 360 min");
        assert_eq!(
            spec.max_bytes(),
            Some(2 * 1024 * 1024 * 1024),
            "2.0 GB -> bytes"
        );
        spec.keep_enabled = false;
        spec.size_enabled = false;
        assert_eq!(spec.max_age_minutes(), None);
        assert_eq!(spec.max_bytes(), None);

        spec.keep_enabled = true;
        spec.keep_hours = 0.5;
        assert_eq!(spec.max_age_minutes(), Some(30));
    }

    /// The exact event order the follow engine emits per frame:
    /// download started -> download done -> frame written; rows track it.
    #[test]
    fn frame_rows_follow_event_order() {
        let mut panel = panel();
        panel.begin_follow();
        assert!(panel.is_running());

        panel.apply_download_started("key-1".to_string(), "C13 19:21:18Z".to_string(), 4_000_000);
        assert_eq!(panel.rows.len(), 1);
        assert!(matches!(
            panel.rows[0].stage,
            SatFrameStage::Downloading { bytes: 4_000_000 }
        ));

        panel.apply_download_done("key-1", 5_600, false);
        assert!(matches!(
            panel.rows[0].stage,
            SatFrameStage::Storing {
                download_ms: 5_600,
                cache_hit: false
            }
        ));

        panel.apply_frame_written("key-1", "conus_c13_20260610".to_string(), 1921, 8_431_077, 950);
        assert_eq!(panel.frames_ingested, 1);
        match &panel.rows[0].stage {
            SatFrameStage::Done {
                run,
                hhmm,
                store_bytes,
                encode_ms,
            } => {
                assert_eq!(run, "conus_c13_20260610");
                assert_eq!(*hhmm, 1921);
                assert_eq!(*store_bytes, 8_431_077);
                assert_eq!(*encode_ms, 950);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn frame_rows_are_capped_oldest_first() {
        let mut panel = panel();
        panel.begin_follow();
        for index in 0..(MAX_FRAME_ROWS + 5) {
            panel.apply_download_started(format!("key-{index}"), format!("row {index}"), 1);
        }
        assert_eq!(panel.rows.len(), MAX_FRAME_ROWS);
        assert_eq!(panel.rows[0].id, "key-5", "oldest rows drop first");
    }

    #[test]
    fn duplicate_ids_update_the_newest_row() {
        // A cache-hit re-ingest of the same key after an earlier failure
        // must not resurrect the old row.
        let mut panel = panel();
        panel.begin_follow();
        panel.apply_download_started("key".to_string(), "first".to_string(), 1);
        panel.apply_download_started("key".to_string(), "second".to_string(), 2);
        panel.apply_download_done("key", 10, true);
        assert!(
            matches!(panel.rows[0].stage, SatFrameStage::Downloading { .. }),
            "older duplicate row untouched"
        );
        assert!(matches!(
            panel.rows[1].stage,
            SatFrameStage::Storing { cache_hit: true, .. }
        ));
    }

    #[test]
    fn follow_lifecycle_states_are_distinct() {
        let mut panel = panel();
        assert!(!panel.is_running());
        panel.begin_follow();
        assert!(panel.is_running());
        panel.finish_follow(Ok("stopped — 6 frames this session".to_string()));
        assert!(!panel.is_running());
        assert!(matches!(panel.state, SatFollowState::Finished(_)));

        panel.begin_follow();
        panel.finish_follow(Err("bucket unreachable".to_string()));
        assert_eq!(
            panel.state,
            SatFollowState::Failed("bucket unreachable".to_string())
        );
    }

    #[test]
    fn begin_follow_resets_session_counters() {
        let mut panel = panel();
        panel.begin_follow();
        panel.apply_poll_done(13, 2, 800);
        panel.apply_evicted(2, 16_000_000);
        panel.apply_note("warning: x".to_string());
        panel.apply_download_started("k".to_string(), "l".to_string(), 1);
        assert_eq!(panel.polls, 1);

        panel.begin_follow();
        assert_eq!(panel.polls, 0);
        assert_eq!(panel.frames_ingested, 0);
        assert_eq!(panel.evicted_frames, 0);
        assert_eq!(panel.evicted_bytes, 0);
        assert!(panel.rows.is_empty());
        assert!(panel.notes.is_empty());
    }

    #[test]
    fn spec_status_swaps_between_summary_and_error() {
        let mut panel = panel();
        panel.set_spec_status(Ok("g19 CONUS C13 · poll ~30 s".to_string()));
        assert!(panel.spec_error.is_none());
        assert_eq!(
            panel.spec_summary.as_deref(),
            Some("g19 CONUS C13 · poll ~30 s")
        );
        panel.set_spec_status(Err("unknown layer 'c99'".to_string()));
        assert!(panel.spec_summary.is_none());
        assert_eq!(panel.spec_error.as_deref(), Some("unknown layer 'c99'"));
    }

    #[test]
    fn next_poll_countdown_comes_from_sleeping() {
        let mut panel = panel();
        assert_eq!(panel.next_poll_secs(), None);
        panel.apply_sleeping(30_000);
        let secs = panel.next_poll_secs().expect("countdown active");
        assert!((0.0..=30.0).contains(&secs), "got {secs}");
        panel.apply_poll_done(13, 0, 100);
        assert_eq!(panel.next_poll_secs(), None, "poll clears the countdown");
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
}
