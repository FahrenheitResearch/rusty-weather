//! In-app puffin scope-stats panel (feature `profiling` only).
//!
//! puffin_egui 0.30 pins egui ^0.33 and does not compile against this
//! tree's egui 0.34, so the in-app flamegraph is off the table for now
//! (re-check on the next puffin_egui release and swap this table for
//! `puffin_egui::profiler_window` behind the same feature). Until then:
//! a [`puffin::GlobalFrameView`] sink aggregates the last N frames into a
//! per-scope count/total/mean/max table, and a `puffin_http` server (see
//! `main.rs`) serves the full flamegraph to the external `puffin_viewer`
//! (`cargo install puffin_viewer`, connect to 127.0.0.1:8585).

use std::collections::HashMap;

use puffin::{GlobalFrameView, Reader, Stream};

/// Frames aggregated per table refresh.
const AGGREGATE_FRAMES: usize = 60;

/// One scope's aggregate over the last N frames.
#[derive(Debug, Clone, Default)]
struct ScopeAgg {
    count: u64,
    total_ns: i64,
    max_ns: i64,
}

/// Scope-stats table over puffin's own recorded frames.
pub struct ProfilerPanel {
    frame_view: GlobalFrameView,
    sort_by_total: bool,
}

impl Default for ProfilerPanel {
    fn default() -> Self {
        Self {
            frame_view: GlobalFrameView::default(),
            sort_by_total: true,
        }
    }
}

impl ProfilerPanel {
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let mut scopes_on = puffin::are_scopes_on();
        if ui
            .checkbox(&mut scopes_on, "record scopes")
            .on_hover_text("master switch for puffin scope recording")
            .changed()
        {
            puffin::set_scopes_on(scopes_on);
        }
        ui.label(
            egui::RichText::new(
                "flamegraph: connect the external puffin_viewer to 127.0.0.1:8585 \
                 (in-app flamegraph returns when puffin_egui supports egui 0.34)",
            )
            .small()
            .weak(),
        );
        ui.checkbox(&mut self.sort_by_total, "sort by total time");
        ui.separator();

        let view = self.frame_view.lock();
        let mut aggregates: HashMap<String, ScopeAgg> = HashMap::new();
        let mut frames = 0usize;
        for frame in view.latest_frames(AGGREGATE_FRAMES) {
            let Ok(unpacked) = frame.unpacked() else {
                continue;
            };
            frames += 1;
            for stream_info in unpacked.thread_streams.values() {
                accumulate_stream(
                    &view,
                    &stream_info.stream,
                    Reader::from_start(&stream_info.stream),
                    &mut aggregates,
                );
            }
        }
        drop(view);

        if aggregates.is_empty() {
            ui.label(
                egui::RichText::new(
                    "no scopes recorded yet — enable \"record scopes\" and interact \
                     (load a field, pull a sounding, run a download)",
                )
                .weak(),
            );
            return;
        }

        let mut rows: Vec<(String, ScopeAgg)> = aggregates.into_iter().collect();
        if self.sort_by_total {
            rows.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns));
        } else {
            rows.sort_by(|a, b| a.0.cmp(&b.0));
        }

        ui.label(
            egui::RichText::new(format!(
                "last {frames} frame(s); parent scopes include child time"
            ))
            .small()
            .weak(),
        );
        egui::ScrollArea::vertical()
            .id_salt("rw-profiler-table")
            .show(ui, |ui| {
                egui::Grid::new("rw-profiler-grid")
                    .striped(true)
                    .min_col_width(56.0)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("scope").strong());
                        ui.label(egui::RichText::new("count").strong());
                        ui.label(egui::RichText::new("total ms").strong());
                        ui.label(egui::RichText::new("mean ms").strong());
                        ui.label(egui::RichText::new("max ms").strong());
                        ui.end_row();
                        for (name, agg) in &rows {
                            ui.label(egui::RichText::new(name).monospace().small());
                            ui.label(format!("{}", agg.count));
                            ui.label(format!("{:.2}", agg.total_ns as f64 / 1e6));
                            ui.label(format!(
                                "{:.2}",
                                agg.total_ns as f64 / 1e6 / agg.count.max(1) as f64
                            ));
                            ui.label(format!("{:.2}", agg.max_ns as f64 / 1e6));
                            ui.end_row();
                        }
                    });
            });
    }
}

/// Walk one stream's scopes recursively, accumulating per-scope-name
/// count/total/max. Recursion depth is bounded by scope nesting depth.
fn accumulate_stream(
    view: &puffin::FrameView,
    stream: &Stream,
    reader: Reader<'_>,
    aggregates: &mut HashMap<String, ScopeAgg>,
) {
    for scope in reader.flatten() {
        let name = view
            .scope_collection()
            .fetch_by_id(&scope.id)
            .map(|details| {
                details
                    .scope_name
                    .as_ref()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| details.function_name.to_string())
            })
            .unwrap_or_else(|| format!("scope#{:?}", scope.id));
        let agg = aggregates.entry(name).or_default();
        agg.count += 1;
        agg.total_ns += scope.record.duration_ns;
        agg.max_ns = agg.max_ns.max(scope.record.duration_ns);
        if scope.child_begin_position < scope.child_end_position {
            if let Ok(child_reader) = Reader::with_offset(stream, scope.child_begin_position) {
                accumulate_stream(view, stream, child_reader, aggregates);
            }
        }
    }
}
