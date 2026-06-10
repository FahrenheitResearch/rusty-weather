//! Always-on lightweight perf stats: a tiny registry of named operation
//! timings (last + EMA + count) shared between worker threads and the UI,
//! plus a one-line strip widget. Costs a few `Instant::now` calls and a
//! short mutex hold per recorded op — independent of the `profiling`
//! feature, so every host (including bowecho) gets the readout for free.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Exponential-moving-average weight for new samples.
const EMA_ALPHA: f32 = 0.2;

/// One operation's running timing summary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpStat {
    pub last_ms: f32,
    pub ema_ms: f32,
    pub count: u64,
}

/// Thread-safe registry of named operation timings. Cheap to record into
/// from worker threads; the UI takes [`StatsRegistry::snapshot`] once per
/// frame.
#[derive(Debug, Default)]
pub struct StatsRegistry {
    inner: Mutex<BTreeMap<String, OpStat>>,
}

impl StatsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one sample of `name` taking `ms` milliseconds.
    pub fn record(&self, name: &str, ms: f32) {
        let mut inner = self.inner.lock().expect("stats registry poisoned");
        match inner.get_mut(name) {
            Some(stat) => {
                stat.last_ms = ms;
                stat.ema_ms += EMA_ALPHA * (ms - stat.ema_ms);
                stat.count += 1;
            }
            None => {
                inner.insert(
                    name.to_string(),
                    OpStat {
                        last_ms: ms,
                        ema_ms: ms,
                        count: 1,
                    },
                );
            }
        }
    }

    /// All stats, sorted by name (BTreeMap order — stable across frames).
    pub fn snapshot(&self) -> Vec<(String, OpStat)> {
        self.inner
            .lock()
            .expect("stats registry poisoned")
            .iter()
            .map(|(name, stat)| (name.clone(), *stat))
            .collect()
    }
}

/// One-line stats strip: `frame 1.2 ms · store.field 38/41 ms · …` where
/// the pair is last/EMA. Renders nothing but labels — hosts drop it into
/// any bar.
pub fn stats_strip(ui: &mut egui::Ui, frame_ms: f32, registry: &StatsRegistry) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(format!("frame {frame_ms:.1} ms"))
                .small()
                .monospace(),
        );
        for (name, stat) in registry.snapshot() {
            ui.label(
                egui::RichText::new(format!(
                    "· {name} {:.0}/{:.0} ms",
                    stat.last_ms, stat.ema_ms
                ))
                .small()
                .monospace()
                .weak(),
            )
            .on_hover_text(format!(
                "{name}: last {:.2} ms, EMA {:.2} ms over {} op(s)",
                stat.last_ms, stat.ema_ms, stat.count
            ));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_tracks_last_ema_and_count() {
        let stats = StatsRegistry::new();
        stats.record("op", 10.0);
        let snap = stats.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "op");
        assert_eq!(snap[0].1.last_ms, 10.0);
        assert_eq!(snap[0].1.ema_ms, 10.0, "first sample seeds the EMA");
        assert_eq!(snap[0].1.count, 1);

        stats.record("op", 20.0);
        let stat = stats.snapshot()[0].1;
        assert_eq!(stat.last_ms, 20.0);
        assert_eq!(stat.ema_ms, 12.0, "10 + 0.2 * (20 - 10)");
        assert_eq!(stat.count, 2);
    }

    #[test]
    fn snapshot_is_sorted_by_name() {
        let stats = StatsRegistry::new();
        stats.record("zeta", 1.0);
        stats.record("alpha", 2.0);
        let names: Vec<String> = stats.snapshot().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }
}
