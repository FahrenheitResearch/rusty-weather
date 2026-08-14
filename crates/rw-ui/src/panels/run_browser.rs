//! Run browser: model -> run -> timesteps tree over a [`StoreTree`], with
//! exact-time labels where available, variable counts, and the writer build.

use egui::{CollapsingHeader, RichText, ScrollArea, Ui, collapsing_header::CollapsingState};

use crate::store_view::StoreTree;
use crate::worker::HourKey;

/// Tree panel for picking a stored timestep. Pure widget: render with
/// [`RunBrowserPanel::ui`] inside any container; it returns the newly picked
/// hour, and the host drives loading.
#[derive(Debug, Default)]
pub struct RunBrowserPanel {
    selected: Option<HourKey>,
    /// Open the selected model/run once after a host-side selection so newly
    /// downloaded hours cannot remain hidden under the previously viewed run.
    reveal_selected: bool,
}

impl RunBrowserPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn selected(&self) -> Option<&HourKey> {
        self.selected.as_ref()
    }

    /// Set the selection without emitting an event (e.g. host-side
    /// auto-select of the first hour).
    pub fn select(&mut self, key: HourKey) {
        self.selected = Some(key);
        self.reveal_selected = true;
    }

    /// Reconcile the current selection with a freshly enumerated store tree.
    /// Matching uses the stable model/run/slot identity, then refreshes exact
    /// timing from the new manifest snapshot. If the old timestep vanished,
    /// the first available timestep is selected; an empty tree clears it.
    /// Returns true only when the effective key changed.
    pub fn reconcile(&mut self, tree: &StoreTree) -> bool {
        let refreshed = self
            .selected
            .as_ref()
            .and_then(|selected| {
                tree.models
                    .iter()
                    .find(|model| model.model == selected.model)?
                    .runs
                    .iter()
                    .find(|run| run.run == selected.run)?
                    .hours
                    .iter()
                    .find(|hour| hour.hour == selected.hour)
                    .map(|hour| HourKey {
                        model: selected.model.clone(),
                        run: selected.run.clone(),
                        hour: hour.hour,
                        exact_time: hour.exact_time,
                    })
            })
            .or_else(|| {
                let model = tree.models.first()?;
                let run = model.runs.first()?;
                let hour = run.hours.first()?;
                Some(HourKey {
                    model: model.model.clone(),
                    run: run.run.clone(),
                    hour: hour.hour,
                    exact_time: hour.exact_time,
                })
            });
        let changed = self.selected != refreshed;
        self.selected = refreshed;
        self.reveal_selected |= changed;
        changed
    }

    /// Render the tree. Returns `Some(key)` only on the frame the user picks
    /// a different hour.
    pub fn ui(&mut self, ui: &mut Ui, tree: &StoreTree) -> Option<HourKey> {
        let mut picked = None;
        let reveal_selected = self.reveal_selected;
        let selected_identity = self
            .selected
            .as_ref()
            .map(|key| (key.model.clone(), key.run.clone()));

        if tree.models.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("No runs found in this store.").weak());
        }

        for model in &tree.models {
            let model_salt = ("rw-run-model", &model.model);
            if reveal_selected
                && selected_identity
                    .as_ref()
                    .is_some_and(|(selected_model, _)| selected_model == &model.model)
            {
                let id = ui.make_persistent_id(model_salt);
                let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, false);
                state.set_open(true);
                state.store(ui.ctx());
            }
            CollapsingHeader::new(RichText::new(&model.model).strong())
                .id_salt(model_salt)
                .default_open(tree.models.len() == 1)
                .show(ui, |ui| {
                    for run in &model.runs {
                        let run_salt = ("rw-run", &model.model, &run.run);
                        if reveal_selected
                            && selected_identity.as_ref().is_some_and(
                                |(selected_model, selected_run)| {
                                    selected_model == &model.model && selected_run == &run.run
                                },
                            )
                        {
                            let id = ui.make_persistent_id(run_salt);
                            let mut state =
                                CollapsingState::load_with_default_open(ui.ctx(), id, false);
                            state.set_open(true);
                            state.store(ui.ctx());
                        }
                        CollapsingHeader::new(&run.run)
                            .id_salt(run_salt)
                            .default_open(model.runs.len() == 1)
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} x {} · build {}",
                                        run.nx, run.ny, run.build
                                    ))
                                    .small()
                                    .weak(),
                                );
                                let mut show_hour =
                                    |ui: &mut Ui, hour: &crate::store_view::HourEntry| {
                                        let key = HourKey {
                                            model: model.model.clone(),
                                            run: run.run.clone(),
                                            hour: hour.hour,
                                            exact_time: hour.exact_time,
                                        };
                                        let is_selected = self.selected.as_ref() == Some(&key);
                                        let label = format!(
                                            "{}  ·  {} vars",
                                            key.time_label(),
                                            hour.variable_count
                                        );
                                        if ui.selectable_label(is_selected, label).clicked()
                                            && !is_selected
                                        {
                                            self.selected = Some(key.clone());
                                            picked = Some(key);
                                        }
                                    };
                                if run.hours.len() > 256 {
                                    // Minute-cadence runs commonly contain
                                    // thousands of frames. Only build labels
                                    // and HourKeys for visible rows.
                                    let row_height = ui.spacing().interact_size.y;
                                    ScrollArea::vertical()
                                        .id_salt(("rw-run-hours", &model.model, &run.run))
                                        .max_height(420.0)
                                        .show_rows(
                                            ui,
                                            row_height,
                                            run.hours.len(),
                                            |ui, visible| {
                                                for index in visible {
                                                    show_hour(ui, &run.hours[index]);
                                                }
                                            },
                                        );
                                } else {
                                    for hour in &run.hours {
                                        show_hour(ui, hour);
                                    }
                                }
                            });
                    }
                });
        }

        self.reveal_selected = false;

        if !tree.warnings.is_empty() {
            ui.add_space(8.0);
            ui.separator();
            for warning in &tree.warnings {
                ui.label(
                    RichText::new(warning)
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
            }
        }

        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_view::{HourEntry, ModelEntry, RunEntry};

    fn tree(exact_time: Option<rw_store::RwsExactTime>) -> StoreTree {
        StoreTree {
            models: vec![ModelEntry {
                model: "wrf".to_string(),
                runs: vec![RunEntry {
                    run: "run".to_string(),
                    build: "test".to_string(),
                    writer_version: "test".to_string(),
                    nx: 2,
                    ny: 2,
                    exact_time_axis: exact_time.is_some(),
                    hours: vec![HourEntry {
                        hour: 0,
                        file: "f000.rws".to_string(),
                        variable_count: 1,
                        written_unix: 1,
                        exact_time,
                    }],
                }],
            }],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn reconcile_refreshes_timing_and_clears_a_removed_selection() {
        let mut panel = RunBrowserPanel::new();
        assert!(panel.reconcile(&tree(None)));
        assert_eq!(panel.selected().and_then(|key| key.exact_time), None);

        let exact = rw_store::RwsExactTime {
            lead_seconds: 31_680,
            valid_unix: 134_243_280,
        };
        assert!(panel.reconcile(&tree(Some(exact))));
        assert_eq!(panel.selected().and_then(|key| key.exact_time), Some(exact));
        assert!(!panel.reconcile(&tree(Some(exact))));

        assert!(panel.reconcile(&StoreTree::default()));
        assert!(panel.selected().is_none());
    }

    #[test]
    fn host_selection_reveals_the_exact_model_and_run_once() {
        let target = HourKey {
            model: "hrrr".to_owned(),
            run: "20260814_00z".to_owned(),
            hour: 3,
            exact_time: None,
        };
        let tree = StoreTree {
            models: vec![ModelEntry {
                model: "hrrr".to_owned(),
                runs: vec![
                    RunEntry {
                        run: "20260814_19z".to_owned(),
                        build: "test".to_owned(),
                        writer_version: "test".to_owned(),
                        nx: 2,
                        ny: 2,
                        exact_time_axis: false,
                        hours: vec![HourEntry {
                            hour: 0,
                            file: "f000.rws".to_owned(),
                            variable_count: 1,
                            written_unix: 1,
                            exact_time: None,
                        }],
                    },
                    RunEntry {
                        run: target.run.clone(),
                        build: "test".to_owned(),
                        writer_version: "test".to_owned(),
                        nx: 2,
                        ny: 2,
                        exact_time_axis: false,
                        hours: (0..=3)
                            .map(|hour| HourEntry {
                                hour,
                                file: format!("f{hour:03}.rws"),
                                variable_count: 1,
                                written_unix: 1,
                                exact_time: None,
                            })
                            .collect(),
                    },
                ],
            }],
            warnings: Vec::new(),
        };
        let mut panel = RunBrowserPanel::new();
        panel.select(target.clone());
        assert!(panel.reveal_selected);
        assert!(!panel.reconcile(&tree));

        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = panel.ui(ui, &tree);
        });
        assert_eq!(panel.selected(), Some(&target));
        assert!(!panel.reveal_selected);
    }
}
