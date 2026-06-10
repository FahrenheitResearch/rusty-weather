//! Run browser: model -> run -> hours tree over a [`StoreTree`], with
//! variable counts per hour and the writer build stamp per run.

use egui::{CollapsingHeader, RichText, Ui};

use crate::store_view::StoreTree;
use crate::worker::HourKey;

/// Tree panel for picking a forecast hour. Pure widget: render with
/// [`RunBrowserPanel::ui`] inside any container; it returns the newly picked
/// hour, and the host drives loading.
#[derive(Debug, Default)]
pub struct RunBrowserPanel {
    selected: Option<HourKey>,
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
    }

    /// Render the tree. Returns `Some(key)` only on the frame the user picks
    /// a different hour.
    pub fn ui(&mut self, ui: &mut Ui, tree: &StoreTree) -> Option<HourKey> {
        let mut picked = None;

        if tree.models.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("No runs found in this store.").weak());
        }

        for model in &tree.models {
            CollapsingHeader::new(RichText::new(&model.model).strong())
                .default_open(tree.models.len() == 1)
                .show(ui, |ui| {
                    for run in &model.runs {
                        CollapsingHeader::new(&run.run)
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
                                for hour in &run.hours {
                                    let key = HourKey {
                                        model: model.model.clone(),
                                        run: run.run.clone(),
                                        hour: hour.hour,
                                    };
                                    let is_selected = self.selected.as_ref() == Some(&key);
                                    let label = format!(
                                        "f{:03}  ·  {} vars",
                                        hour.hour, hour.variable_count
                                    );
                                    if ui.selectable_label(is_selected, label).clicked()
                                        && !is_selected
                                    {
                                        self.selected = Some(key.clone());
                                        picked = Some(key);
                                    }
                                }
                            });
                    }
                });
        }

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
