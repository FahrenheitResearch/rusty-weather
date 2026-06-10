//! Satellite frame player: a time scrubber + play/pause loop over the
//! frames of one sat store run, sibling to the field viewer.
//!
//! Pure widget over host-pushed data: the host pushes the run list
//! (`set_runs`) and already-colored frame images (`set_frame` — the host
//! owns the production palettes; this panel never sees raw values). Frame
//! textures live in a byte-budgeted LRU cache so loop playback over a long
//! window stays memory-bounded. Missing frames are requested through
//! [`SatPlayerEvent::FrameWanted`] and the loop HOLDS until the next frame's
//! texture is cached (honest pacing — no silent frame skips).

use std::collections::HashMap;
use std::time::Instant;

use egui::{
    ColorImage, ComboBox, Image, Rect, RichText, Sense, Slider, Stroke, StrokeKind, TextureFilter,
    TextureHandle, TextureOptions, Ui, Vec2,
};

use crate::profile_scope;

/// One sat run in the store (`g19/conus_c13_20260610`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SatRunKey {
    pub model: String,
    pub run: String,
}

impl std::fmt::Display for SatRunKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.model, self.run)
    }
}

/// What the host pushes per run: title + the frames present (HHMM, sorted
/// ascending — rw-store manifests already are).
#[derive(Debug, Clone, PartialEq)]
pub struct SatRunListing {
    pub key: SatRunKey,
    /// Host-built display title ("g19 · CONUS C13 · 2026-06-10").
    pub title: String,
    pub nx: usize,
    pub ny: usize,
    /// Scan-start HHMM of every frame, ascending.
    pub frames: Vec<u16>,
}

/// One frame's pixels, colored by the host (production palettes).
#[derive(Debug, Clone)]
pub struct SatFrameImage {
    pub key: SatRunKey,
    pub hhmm: u16,
    pub image: ColorImage,
    /// Host-side store read + palette wall, for the stats strip.
    pub read_ms: f32,
}

/// What the user did this frame; the host turns these into worker requests.
#[derive(Debug, Clone, PartialEq)]
pub enum SatPlayerEvent {
    /// A frame's texture is missing from the cache — load it.
    FrameWanted { key: SatRunKey, hhmm: u16 },
    /// The refresh button — re-scan the sat store.
    RefreshRequested,
}

/// Default texture budget: bounds playback memory (a native CONUS C13
/// frame is 2500x1500 RGBA = ~15 MB; this holds a ~2.5 h loop of those).
const DEFAULT_TEXTURE_BUDGET_BYTES: usize = 512 * 1024 * 1024;
/// Frames prefetched ahead of the playhead while playing.
const PREFETCH_AHEAD: usize = 3;
/// A pending frame request is retried after this long without a response.
const PENDING_RETRY_SECS: f32 = 10.0;

/// Byte-budgeted LRU keyed by frame HHMM. Pure bookkeeping (generic over
/// the cached value) so eviction order and budget math are unit-testable
/// without a GPU texture in sight.
struct FrameCache<T> {
    entries: HashMap<u16, (T, usize, u64)>,
    budget_bytes: usize,
    used_bytes: usize,
    tick: u64,
}

impl<T> FrameCache<T> {
    fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget_bytes,
            used_bytes: 0,
            tick: 0,
        }
    }

    /// Insert (replacing any same-key entry), then evict least-recently
    /// used entries until the budget holds. The newest entry always stays,
    /// even if it alone exceeds the budget — the player needs SOMETHING to
    /// draw.
    fn insert(&mut self, hhmm: u16, value: T, bytes: usize) {
        self.remove(hhmm);
        self.tick += 1;
        self.entries.insert(hhmm, (value, bytes, self.tick));
        self.used_bytes += bytes;
        while self.used_bytes > self.budget_bytes && self.entries.len() > 1 {
            let Some(&oldest) = self
                .entries
                .iter()
                .filter(|&(&key, _)| key != hhmm)
                .min_by_key(|(_, (_, _, tick))| *tick)
                .map(|(key, _)| key)
            else {
                break;
            };
            self.remove(oldest);
        }
    }

    /// Fetch and mark as most recently used.
    fn get(&mut self, hhmm: u16) -> Option<&T> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(&hhmm).map(|entry| {
            entry.2 = tick;
            &entry.0
        })
    }

    fn contains(&self, hhmm: u16) -> bool {
        self.entries.contains_key(&hhmm)
    }

    fn remove(&mut self, hhmm: u16) {
        if let Some((_, bytes, _)) = self.entries.remove(&hhmm) {
            self.used_bytes -= bytes;
        }
    }

    /// Drop every entry not in `keep` (evicted frames leave the timeline).
    fn retain_keys(&mut self, keep: &[u16]) {
        let stale: Vec<u16> = self
            .entries
            .keys()
            .copied()
            .filter(|key| !keep.contains(key))
            .collect();
        for key in stale {
            self.remove(key);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.used_bytes = 0;
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn used_bytes(&self) -> usize {
        self.used_bytes
    }
}

/// Index of the frame nearest `hhmm` (frames ascending); 0 for an empty
/// list. Keeps the playhead on the same scan time across frame-list
/// updates (new frames landing, old ones evicted).
fn nearest_index(frames: &[u16], hhmm: u16) -> usize {
    if frames.is_empty() {
        return 0;
    }
    let minutes = |t: u16| u32::from(t / 100) * 60 + u32::from(t % 100);
    let target = minutes(hhmm);
    frames
        .iter()
        .enumerate()
        .min_by_key(|&(_, &frame)| minutes(frame).abs_diff(target))
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// `1851` -> `"18:51"`.
fn hhmm_label(hhmm: u16) -> String {
    format!("{:02}:{:02}", hhmm / 100, hhmm % 100)
}

/// The satellite frame player. Pure widget over host-pushed data.
pub struct SatPlayerPanel {
    runs: Vec<SatRunListing>,
    selected: Option<SatRunKey>,
    /// Playhead index into the selected run's frame list.
    index: usize,
    playing: bool,
    /// Loop speed, frames per second.
    fps: f32,
    /// Pin the playhead to the newest frame as frames land (paused) — the
    /// follow-mode view; while playing the loop simply grows to include
    /// new frames.
    live: bool,
    cache: FrameCache<TextureHandle>,
    /// Outstanding frame requests (avoid re-asking every egui frame).
    pending: HashMap<u16, Instant>,
    /// Host-pushed images awaiting GPU upload (done inside `ui`).
    queued: Vec<SatFrameImage>,
    last_advance: Option<Instant>,
    /// Playing but the next frame's texture is not cached yet.
    buffering: bool,
    /// Wall of the last texture-upload pass, for the stats strip.
    last_texture_ms: Option<f32>,
}

impl Default for SatPlayerPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl SatPlayerPanel {
    pub fn new() -> Self {
        Self {
            runs: Vec::new(),
            selected: None,
            index: 0,
            playing: false,
            fps: 6.0,
            live: true,
            cache: FrameCache::new(DEFAULT_TEXTURE_BUDGET_BYTES),
            pending: HashMap::new(),
            queued: Vec::new(),
            last_advance: None,
            buffering: false,
            last_texture_ms: None,
        }
    }

    pub fn selected_run(&self) -> Option<&SatRunKey> {
        self.selected.as_ref()
    }

    /// Wall of the last texture-upload pass (stats strip).
    pub fn last_texture_ms(&self) -> Option<f32> {
        self.last_texture_ms
    }

    /// Start (or stop) loop playback — host-side preset hook.
    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
    }

    /// Install the run list (host pushes after every scan). Keeps the
    /// current selection and playhead scan time when possible; otherwise
    /// selects the first run. With `live` set, a paused playhead snaps to
    /// the newest frame.
    pub fn set_runs(&mut self, runs: Vec<SatRunListing>) {
        let current_hhmm = self.current_frames().get(self.index).copied();
        let keep = self
            .selected
            .take()
            .filter(|key| runs.iter().any(|run| run.key == *key));
        let switched = keep.is_none();
        self.selected = keep.or_else(|| runs.first().map(|run| run.key.clone()));
        self.runs = runs;

        let frames = self.current_frames().to_vec();
        if switched {
            self.cache.clear();
            self.pending.clear();
            self.queued.clear();
            self.index = frames.len().saturating_sub(1);
        } else {
            self.cache.retain_keys(&frames);
            self.pending.retain(|hhmm, _| frames.contains(hhmm));
            if self.live && !self.playing {
                self.index = frames.len().saturating_sub(1);
            } else if let Some(hhmm) = current_hhmm {
                self.index = nearest_index(&frames, hhmm);
            } else {
                self.index = frames.len().saturating_sub(1);
            }
        }
    }

    /// Install a loaded frame image. Stale responses (other runs) are
    /// ignored; the GPU upload happens inside the next `ui` pass.
    pub fn set_frame(&mut self, frame: SatFrameImage) {
        if Some(&frame.key) != self.selected.as_ref() {
            return;
        }
        self.pending.remove(&frame.hhmm);
        self.queued.push(frame);
    }

    /// A frame load failed: forget the pending marker so the next want
    /// retries (the host surfaces the error itself).
    pub fn frame_failed(&mut self, hhmm: u16) {
        self.pending.remove(&hhmm);
    }

    fn current_listing(&self) -> Option<&SatRunListing> {
        let key = self.selected.as_ref()?;
        self.runs.iter().find(|run| run.key == *key)
    }

    fn current_frames(&self) -> &[u16] {
        self.current_listing()
            .map(|listing| listing.frames.as_slice())
            .unwrap_or(&[])
    }

    /// Render the player. Returns the events the host should act on.
    pub fn ui(&mut self, ui: &mut Ui) -> Vec<SatPlayerEvent> {
        let mut events = Vec::new();
        self.upload_queued(ui);

        // --- run picker ---
        ui.horizontal(|ui| {
            ui.label("Run");
            let selected_title = self
                .current_listing()
                .map(|listing| listing.title.clone())
                .unwrap_or_else(|| "no sat runs in store".to_string());
            let mut picked: Option<SatRunKey> = None;
            ComboBox::from_id_salt("rw-ui-sat-player-run")
                .selected_text(selected_title)
                .width(320.0)
                .show_ui(ui, |ui| {
                    for run in &self.runs {
                        let is_selected = self.selected.as_ref() == Some(&run.key);
                        if ui.selectable_label(is_selected, &run.title).clicked() && !is_selected {
                            picked = Some(run.key.clone());
                        }
                    }
                });
            if let Some(key) = picked {
                self.selected = Some(key);
                self.cache.clear();
                self.pending.clear();
                self.queued.clear();
                self.index = self.current_frames().len().saturating_sub(1);
            }
            if ui
                .button("⟳")
                .on_hover_text("re-scan the sat store")
                .clicked()
            {
                events.push(SatPlayerEvent::RefreshRequested);
            }
            if let Some(listing) = self.current_listing() {
                ui.label(
                    RichText::new(format!(
                        "{} frame(s) · {} x {}",
                        listing.frames.len(),
                        listing.nx,
                        listing.ny
                    ))
                    .small()
                    .weak(),
                );
            }
        });

        let frames = self.current_frames().to_vec();
        if frames.is_empty() {
            ui.add_space(12.0);
            ui.label(
                RichText::new("No frames yet — start a follow session above.").weak(),
            );
            return events;
        }
        self.index = self.index.min(frames.len() - 1);

        // --- transport controls ---
        ui.horizontal(|ui| {
            let play_label = if self.playing { "⏸" } else { "▶" };
            if ui
                .button(play_label)
                .on_hover_text("play/pause the loop")
                .clicked()
            {
                self.playing = !self.playing;
                self.last_advance = None;
                self.buffering = false;
            }
            if ui.button("⏮").on_hover_text("previous frame").clicked() {
                self.playing = false;
                self.index = (self.index + frames.len() - 1) % frames.len();
            }
            if ui.button("⏭").on_hover_text("next frame").clicked() {
                self.playing = false;
                self.index = (self.index + 1) % frames.len();
            }
            ui.add(
                Slider::new(&mut self.fps, 1.0..=20.0)
                    .text("fps")
                    .fixed_decimals(0),
            );
            ui.toggle_value(&mut self.live, "live")
                .on_hover_text("snap the paused playhead to the newest frame as frames land");
            ui.label(
                RichText::new(format!(
                    "{} UTC · {}/{}",
                    hhmm_label(frames[self.index]),
                    self.index + 1,
                    frames.len()
                ))
                .monospace(),
            );
            if self.buffering {
                ui.spinner();
                ui.label(RichText::new("buffering").small().weak());
            }
        });

        // --- scrubber ---
        {
            let frames_for_label = frames.clone();
            let current_label = hhmm_label(frames[self.index]);
            ui.spacing_mut().slider_width = (ui.available_width() - 90.0).max(120.0);
            let response = ui.add(
                Slider::new(&mut self.index, 0..=frames.len() - 1)
                    .show_value(false)
                    .custom_formatter(move |value, _| {
                        let index = (value.round() as usize).min(frames_for_label.len() - 1);
                        hhmm_label(frames_for_label[index])
                    })
                    .text(current_label),
            );
            if response.dragged() || response.changed() {
                // Scrubbing pauses the loop pacing (no jump-ahead on release).
                self.last_advance = None;
            }
        }

        // --- playback advance: hold until the next frame is cached ---
        if self.playing && frames.len() > 1 {
            let interval = 1.0 / self.fps.max(0.1);
            let due = self
                .last_advance
                .is_none_or(|last| last.elapsed().as_secs_f32() >= interval);
            if due {
                let next = (self.index + 1) % frames.len();
                if self.cache.contains(frames[next]) {
                    self.index = next;
                    self.last_advance = Some(Instant::now());
                    self.buffering = false;
                } else {
                    self.buffering = true;
                }
            }
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f32(
                    (interval / 2.0).clamp(0.01, 0.1),
                ));
        }

        // --- frame requests: current + prefetch ---
        let mut wants: Vec<u16> = vec![frames[self.index]];
        if self.playing {
            for step in 1..=PREFETCH_AHEAD {
                wants.push(frames[(self.index + step) % frames.len()]);
            }
        } else {
            // Paused: neighbors so single-step scrubbing is instant.
            wants.push(frames[(self.index + 1) % frames.len()]);
            wants.push(frames[(self.index + frames.len() - 1) % frames.len()]);
        }
        if let Some(key) = self.selected.clone() {
            for hhmm in wants {
                if self.cache.contains(hhmm) {
                    continue;
                }
                let stale = self
                    .pending
                    .get(&hhmm)
                    .is_none_or(|asked| asked.elapsed().as_secs_f32() > PENDING_RETRY_SECS);
                if stale {
                    self.pending.insert(hhmm, Instant::now());
                    events.push(SatPlayerEvent::FrameWanted {
                        key: key.clone(),
                        hhmm,
                    });
                }
            }
        }

        // --- image ---
        let current = frames[self.index];
        match self.cache.get(current) {
            Some(texture) => {
                let size = texture.size_vec2();
                let avail = ui.available_size() - Vec2::new(0.0, 18.0);
                let scale = (avail.x / size.x).min(avail.y / size.y).max(0.01);
                let response = ui.add(
                    Image::new(texture)
                        .fit_to_exact_size(size * scale)
                        .sense(Sense::hover()),
                );
                ui.painter_at(response.rect.expand(1.0)).rect_stroke(
                    Rect::from_min_max(response.rect.min, response.rect.max),
                    0.0,
                    Stroke::new(1.0, ui.visuals().weak_text_color()),
                    StrokeKind::Outside,
                );
            }
            None => {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("loading {}…", hhmm_label(current)));
                });
            }
        }
        ui.label(
            RichText::new(format!(
                "texture cache {} frame(s) · {}",
                self.cache.len(),
                super::download::format_bytes(self.cache.used_bytes() as u64)
            ))
            .small()
            .weak(),
        );

        events
    }

    /// Upload host-pushed images to the GPU (LRU-bounded).
    fn upload_queued(&mut self, ui: &Ui) {
        if self.queued.is_empty() {
            return;
        }
        profile_scope!("sat_frame_texture");
        let started = Instant::now();
        for frame in std::mem::take(&mut self.queued) {
            if Some(&frame.key) != self.selected.as_ref() {
                continue;
            }
            let bytes = frame.image.width() * frame.image.height() * 4;
            let texture = ui.ctx().load_texture(
                format!("rw-ui-sat-frame-{:04}", frame.hhmm),
                frame.image,
                TextureOptions {
                    magnification: TextureFilter::Linear,
                    minification: TextureFilter::Linear,
                    ..Default::default()
                },
            );
            self.cache.insert(frame.hhmm, texture, bytes);
        }
        self.last_texture_ms = Some(started.elapsed().as_secs_f32() * 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_cache_respects_budget_and_touch_order() {
        let mut cache: FrameCache<u32> = FrameCache::new(100);
        cache.insert(1000, 1, 40);
        cache.insert(1005, 2, 40);
        assert_eq!(cache.used_bytes(), 80);

        // Touch 1000 so 1005 becomes the LRU candidate.
        assert_eq!(cache.get(1000), Some(&1));
        cache.insert(1010, 3, 40);
        assert_eq!(cache.len(), 2);
        assert!(cache.contains(1000), "recently touched stays");
        assert!(!cache.contains(1005), "LRU evicted");
        assert!(cache.contains(1010));
        assert_eq!(cache.used_bytes(), 80);
    }

    #[test]
    fn lru_cache_keeps_an_oversized_newest_entry() {
        let mut cache: FrameCache<u32> = FrameCache::new(10);
        cache.insert(1000, 1, 50);
        assert_eq!(cache.len(), 1, "the only entry survives over-budget");
        cache.insert(1005, 2, 50);
        assert_eq!(cache.len(), 1);
        assert!(cache.contains(1005), "newest always wins");
    }

    #[test]
    fn lru_cache_replaces_same_key_without_double_count() {
        let mut cache: FrameCache<u32> = FrameCache::new(100);
        cache.insert(1000, 1, 30);
        cache.insert(1000, 2, 50);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used_bytes(), 50);
        assert_eq!(cache.get(1000), Some(&2));
    }

    #[test]
    fn retain_keys_drops_evicted_frames() {
        let mut cache: FrameCache<u32> = FrameCache::new(1000);
        cache.insert(1000, 1, 10);
        cache.insert(1005, 2, 10);
        cache.insert(1010, 3, 10);
        cache.retain_keys(&[1005, 1010]);
        assert!(!cache.contains(1000));
        assert_eq!(cache.used_bytes(), 20);
    }

    #[test]
    fn nearest_index_maps_scan_times_across_updates() {
        let frames = [1850u16, 1855, 1900, 1905];
        assert_eq!(nearest_index(&frames, 1850), 0);
        assert_eq!(
            nearest_index(&frames, 1859),
            2,
            "18:59 is 1 min from 19:00 but 4 min from 18:55 — minute math, not numeric distance"
        );
        assert_eq!(nearest_index(&frames, 1857), 1, "18:57 stays on 18:55");
        assert_eq!(nearest_index(&frames, 1902), 2);
        assert_eq!(nearest_index(&frames, 2300), 3, "clamps to the end");
        assert_eq!(nearest_index(&[], 1200), 0);
    }

    fn listing(model: &str, run: &str, frames: Vec<u16>) -> SatRunListing {
        SatRunListing {
            key: SatRunKey {
                model: model.to_string(),
                run: run.to_string(),
            },
            title: format!("{model} {run}"),
            nx: 4,
            ny: 2,
            frames,
        }
    }

    #[test]
    fn set_runs_selects_newest_and_snaps_live_playhead() {
        let mut panel = SatPlayerPanel::new();
        panel.set_runs(vec![listing("g19", "conus_c13_20260610", vec![1850, 1855])]);
        assert_eq!(
            panel.selected_run().map(|key| key.run.as_str()),
            Some("conus_c13_20260610")
        );
        assert_eq!(panel.index, 1, "starts on the newest frame");

        // A new frame lands; live + paused snaps to it.
        panel.set_runs(vec![listing(
            "g19",
            "conus_c13_20260610",
            vec![1850, 1855, 1900],
        )]);
        assert_eq!(panel.index, 2);

        // Not live: the playhead keeps its scan time even as the window
        // slides (1850 evicted, 1905 landed).
        panel.live = false;
        panel.index = 1; // 1855
        panel.set_runs(vec![listing(
            "g19",
            "conus_c13_20260610",
            vec![1855, 1900, 1905],
        )]);
        assert_eq!(panel.index, 0, "still on 18:55");
    }

    #[test]
    fn set_runs_keeps_selection_and_clears_cache_on_switch() {
        let mut panel = SatPlayerPanel::new();
        panel.set_runs(vec![
            listing("g19", "conus_c13_20260610", vec![1850]),
            listing("g19", "conus_c02_20260610", vec![1850]),
        ]);
        let first = panel.selected_run().cloned().expect("selected");

        // Same runs again (rescan): selection survives.
        panel.set_runs(vec![
            listing("g19", "conus_c13_20260610", vec![1850, 1855]),
            listing("g19", "conus_c02_20260610", vec![1850]),
        ]);
        assert_eq!(panel.selected_run(), Some(&first));

        // Selected run vanished (window evicted the whole day): fall back
        // to the first listed run and reset the playhead.
        panel.set_runs(vec![listing("g19", "conus_c02_20260610", vec![1850])]);
        assert_eq!(
            panel.selected_run().map(|key| key.run.as_str()),
            Some("conus_c02_20260610")
        );
        assert_eq!(panel.index, 0);
    }

    #[test]
    fn stale_frames_for_other_runs_are_ignored() {
        let mut panel = SatPlayerPanel::new();
        panel.set_runs(vec![listing("g19", "conus_c13_20260610", vec![1850])]);
        panel.set_frame(SatFrameImage {
            key: SatRunKey {
                model: "g19".to_string(),
                run: "conus_c02_20260610".to_string(),
            },
            hhmm: 1850,
            image: ColorImage::filled([2, 2], egui::Color32::BLACK),
            read_ms: 1.0,
        });
        assert!(panel.queued.is_empty(), "stale frame dropped");

        panel.set_frame(SatFrameImage {
            key: SatRunKey {
                model: "g19".to_string(),
                run: "conus_c13_20260610".to_string(),
            },
            hhmm: 1850,
            image: ColorImage::filled([2, 2], egui::Color32::BLACK),
            read_ms: 1.0,
        });
        assert_eq!(panel.queued.len(), 1, "matching frame queued for upload");
    }

    #[test]
    fn hhmm_labels_format_as_utc_clock() {
        assert_eq!(hhmm_label(0), "00:00");
        assert_eq!(hhmm_label(1851), "18:51");
        assert_eq!(hhmm_label(905), "09:05");
    }
}
