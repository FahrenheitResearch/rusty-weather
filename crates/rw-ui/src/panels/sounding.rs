//! Sounding panel: profiles of every 3D variable at a clicked point.
//!
//! Spike-grade chart: temperature/dewpoint vs pressure as plain egui_plot
//! lines on a linear inverted-pressure axis — a real skew-T comes later.
//! All levels of every 3D variable also land in a numeric table.

use egui::{Color32, RichText, Stroke, Ui};
use egui_plot::{Legend, Line, Plot, PlotPoints};

use crate::worker::{ProfileVar, SoundingData};

const TEMP_COLOR: Color32 = Color32::from_rgb(220, 70, 60);
const DEW_COLOR: Color32 = Color32::from_rgb(60, 150, 70);

#[derive(Debug, Default)]
enum SoundingState {
    #[default]
    Empty,
    Loading,
    Error(String),
    Ready(SoundingData),
}

/// Point-profile inspector. Pure widget over host-pushed data:
/// `set_loading` -> `set_data`/`set_error`, render with `ui`.
#[derive(Debug, Default)]
pub struct SoundingPanel {
    state: SoundingState,
}

impl SoundingPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_loading(&mut self) {
        self.state = SoundingState::Loading;
    }

    pub fn set_error(&mut self, message: String) {
        self.state = SoundingState::Error(message);
    }

    pub fn set_data(&mut self, data: SoundingData) {
        self.state = SoundingState::Ready(data);
    }

    pub fn clear(&mut self) {
        self.state = SoundingState::Empty;
    }

    /// Whether the panel has anything to show (host can hide it otherwise).
    pub fn has_content(&self) -> bool {
        !matches!(self.state, SoundingState::Empty)
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        match &self.state {
            SoundingState::Empty => {
                ui.label(RichText::new("Click a point on the field to pull a sounding.").weak());
            }
            SoundingState::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("reading profiles…");
                });
            }
            SoundingState::Error(message) => {
                ui.colored_label(ui.visuals().error_fg_color, message);
            }
            SoundingState::Ready(data) => show_sounding(ui, data),
        }
    }
}

fn show_sounding(ui: &mut Ui, data: &SoundingData) {
    ui.label(RichText::new(format!("{}", data.hour)).strong());
    let place = match (data.lat, data.lon) {
        (Some(lat), Some(lon)) => format!("{lat:.3}°, {lon:.3}°  (grid {:.1}, {:.1})", data.fx, data.fy),
        _ => format!("grid ({:.1}, {:.1})", data.fx, data.fy),
    };
    ui.label(RichText::new(place).small().weak());
    ui.label(
        RichText::new("Simple T/Td vs pressure — skew-T comes later.")
            .small()
            .weak(),
    );
    ui.separator();

    if data.vars.is_empty() {
        ui.label(RichText::new("This hour has no 3D pressure-level variables.").weak());
        return;
    }

    let (temp, dew) = classify_profiles(&data.vars);
    if temp.is_some() || dew.is_some() {
        let plot_height = (ui.available_height() * 0.5).clamp(160.0, 420.0);
        Plot::new("rw-ui-sounding")
            .height(plot_height)
            .legend(Legend::default())
            .y_axis_formatter(|mark, _| format!("{:.0}", -mark.value))
            .x_axis_formatter(|mark, _| format!("{:.0}", mark.value))
            .label_formatter(|name, point| {
                if name.is_empty() {
                    format!("{:.1} °C @ {:.0} hPa", point.x, -point.y)
                } else {
                    format!("{name}: {:.1} °C @ {:.0} hPa", point.x, -point.y)
                }
            })
            .show(ui, |plot_ui| {
                if let Some(var) = temp {
                    plot_ui.line(
                        Line::new("T", profile_points(var)).stroke(Stroke::new(2.0, TEMP_COLOR)),
                    );
                }
                if let Some(var) = dew {
                    plot_ui.line(
                        Line::new("Td", profile_points(var)).stroke(Stroke::new(2.0, DEW_COLOR)),
                    );
                }
            });
    } else {
        ui.label(
            RichText::new("No temperature/dewpoint 3D variables found; values below.").weak(),
        );
    }
    ui.separator();

    // Numeric table: rows = union of levels (descending pressure), one
    // column per 3D variable.
    let mut levels: Vec<u16> = data
        .vars
        .iter()
        .flat_map(|var| var.levels_hpa.iter().copied())
        .collect();
    levels.sort_unstable_by(|a, b| b.cmp(a));
    levels.dedup();

    egui::ScrollArea::both().show(ui, |ui| {
        egui::Grid::new("rw-ui-sounding-table")
            .striped(true)
            .min_col_width(56.0)
            .show(ui, |ui| {
                ui.label(RichText::new("hPa").strong());
                for var in &data.vars {
                    ui.label(RichText::new(&var.name).strong())
                        .on_hover_text(format!("units: {}", var.units));
                }
                ui.end_row();
                for &level in &levels {
                    ui.label(format!("{level}"));
                    for var in &data.vars {
                        let value = var
                            .levels_hpa
                            .iter()
                            .position(|&have| have == level)
                            .map(|i| var.values[i]);
                        match value {
                            Some(v) if v.is_finite() => ui.label(format!("{v:.1}")),
                            Some(_) => ui.label("—"),
                            None => ui.label(""),
                        };
                    }
                    ui.end_row();
                }
            });
    });
}

/// Find the temperature-like and dewpoint-like profiles by name.
pub(crate) fn classify_profiles(vars: &[ProfileVar]) -> (Option<&ProfileVar>, Option<&ProfileVar>) {
    let find = |needle: &str| {
        vars.iter()
            .find(|var| var.name.to_ascii_lowercase().contains(needle))
    };
    let temp = find("temperature").or_else(|| find("temp"));
    let dew = find("dewpoint").or_else(|| find("dew"));
    (temp, dew)
}

/// Plot points for one profile: x = value (Kelvin shown as °C), y = negated
/// pressure so "up" means "higher in the atmosphere". NaN levels are
/// skipped.
pub(crate) fn profile_points(var: &ProfileVar) -> PlotPoints<'static> {
    let to_celsius = var.units == "K";
    let points: Vec<[f64; 2]> = var
        .levels_hpa
        .iter()
        .zip(&var.values)
        .filter(|(_, value)| value.is_finite())
        .map(|(&level, &value)| {
            let x = if to_celsius { value as f64 - 273.15 } else { value as f64 };
            [x, -(level as f64)]
        })
        .collect();
    PlotPoints::from(points)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::HourKey;

    fn var(name: &str, units: &str, levels: &[u16], values: &[f32]) -> ProfileVar {
        ProfileVar {
            name: name.to_string(),
            units: units.to_string(),
            levels_hpa: levels.to_vec(),
            values: values.to_vec(),
        }
    }

    #[test]
    fn classify_finds_temperature_and_dewpoint_by_name() {
        let vars = [
            var("height_iso", "gpm", &[1000], &[110.0]),
            var("temperature_iso", "K", &[1000], &[290.0]),
            var("dewpoint_iso", "K", &[1000], &[283.0]),
        ];
        let (temp, dew) = classify_profiles(&vars);
        assert_eq!(temp.map(|v| v.name.as_str()), Some("temperature_iso"));
        assert_eq!(dew.map(|v| v.name.as_str()), Some("dewpoint_iso"));

        let none: [ProfileVar; 1] = [var("u_iso", "m s-1", &[1000], &[5.0])];
        let (temp, dew) = classify_profiles(&none);
        assert!(temp.is_none() && dew.is_none());
    }

    #[test]
    fn profile_points_convert_kelvin_and_skip_nan() {
        let v = var(
            "temperature_iso",
            "K",
            &[1000, 850, 700],
            &[293.15, f32::NAN, 273.15],
        );
        let points = profile_points(&v).points().to_vec();
        assert_eq!(points.len(), 2, "NaN level skipped");
        assert!((points[0].x - 20.0).abs() < 1e-3, "K converted to °C");
        assert_eq!(points[0].y, -1000.0, "pressure negated for the y axis");
        assert!((points[1].x - 0.0).abs() < 1e-3);
        assert_eq!(points[1].y, -700.0);

        // Non-Kelvin units pass through unconverted.
        let height = var("height_iso", "gpm", &[500], &[5700.0]);
        let points = profile_points(&height).points().to_vec();
        assert_eq!(points[0].x, 5700.0);
    }

    #[test]
    fn has_content_tracks_state() {
        let mut panel = SoundingPanel::new();
        assert!(!panel.has_content());
        panel.set_loading();
        assert!(panel.has_content());
        panel.set_data(SoundingData {
            hour: HourKey {
                model: "m".into(),
                run: "r".into(),
                hour: 0,
            },
            fx: 1.0,
            fy: 2.0,
            lat: None,
            lon: None,
            vars: vec![],
        });
        assert!(panel.has_content());
        panel.clear();
        assert!(!panel.has_content());
    }
}
