//! Dev/test helper: write a tiny, fully synthetic rw-store so the UI and the
//! rw-ui tests run anywhere — no ingested model data required.
//!
//! The store mimics the real layout: one model, one run, two forecast hours,
//! a few 2D surface fields (one with a NaN hole, to exercise the missing
//! color), and two 3D pressure volumes with standard-atmosphere-ish
//! temperature/dewpoint profiles so soundings look plausible.

use std::path::Path;

use rustwx_core::{
    CanonicalField, FieldSelector, GridProjection, GridShape, LatLonGrid, SelectedField2D,
};
use rw_store::{PressureVolumeInput, RwResult, WrittenHour, write_hour_from_fields};

pub const SYNTHETIC_MODEL: &str = "synthetic";
pub const SYNTHETIC_RUN: &str = "20260609_00z";
pub const SYNTHETIC_BUILD: &str = "rw-ui-synthetic";
pub const SYNTHETIC_HOURS: [u16; 2] = [0, 6];
/// Pressure levels of the synthetic 3D volumes, descending.
pub const SYNTHETIC_LEVELS: [u16; 7] = [1000, 925, 850, 700, 500, 300, 250];

const NX: usize = 90;
const NY: usize = 70;
/// Deterministic "written at" stamp (the library never reads the clock).
const WRITTEN_UNIX: u64 = 1_780_000_000;

/// Write the synthetic store under `root` and return the written hours.
pub fn write_synthetic_store(root: &Path) -> RwResult<Vec<WrittenHour>> {
    SYNTHETIC_HOURS
        .iter()
        .map(|&hour| write_hour(root, hour))
        .collect()
}

fn write_hour(root: &Path, hour: u16) -> RwResult<WrittenHour> {
    let shift = hour as f32; // fields evolve a little between hours

    let temperature = surface_field(
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
        "K",
        |x, y| 288.0 + 6.0 * (x as f32 / NX as f32) - 9.0 * (y as f32 / NY as f32) + 0.4 * shift,
    );
    let dewpoint = surface_field(
        FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
        "K",
        |x, y| 282.0 + 4.0 * (x as f32 / NX as f32) - 7.0 * (y as f32 / NY as f32) + 0.3 * shift,
    );
    // Gusts with a NaN hole: exercises EMPTY tiles and the missing color.
    let gust = surface_field(
        FieldSelector::height_agl(CanonicalField::WindGust, 10),
        "m s-1",
        |x, y| {
            if (20..35).contains(&x) && (15..30).contains(&y) {
                f32::NAN
            } else {
                3.0 + 12.0 * ((x + y) as f32 / (NX + NY) as f32) + 0.2 * shift
            }
        },
    );

    // Standard-atmosphere-ish profiles: T(p) from the barometric power law,
    // dewpoint depression growing with height. Values land in plausible
    // ranges (≈288 K at 1000 hPa, ≈220 K at 250 hPa).
    let temp_planes: Vec<Vec<f32>> = SYNTHETIC_LEVELS
        .iter()
        .map(|&level| volume_plane(level, shift, 0.0))
        .collect();
    let dew_planes: Vec<Vec<f32>> = SYNTHETIC_LEVELS
        .iter()
        .map(|&level| {
            let depression = 3.0 + 0.035 * (1000.0 - level as f32);
            volume_plane(level, shift, -depression)
        })
        .collect();

    let volumes = [
        volume_input("temperature_iso", &temp_planes),
        volume_input("dewpoint_iso", &dew_planes),
    ];

    write_hour_from_fields(
        root,
        SYNTHETIC_MODEL,
        SYNTHETIC_RUN,
        hour,
        &[
            ("temperature_2m", &temperature),
            ("dewpoint_2m", &dewpoint),
            ("wind_gust_10m", &gust),
        ],
        &volumes,
        SYNTHETIC_BUILD,
        WRITTEN_UNIX + u64::from(hour) * 3600,
    )
}

/// Regular lat/lon grid: lat 30..36.9 by 0.1 (south to north), lon -105..-96.1.
fn grid() -> LatLonGrid {
    let mut lat = Vec::with_capacity(NX * NY);
    let mut lon = Vec::with_capacity(NX * NY);
    for y in 0..NY {
        for x in 0..NX {
            lat.push((30.0 + 0.1 * y as f64) as f32);
            lon.push((-105.0 + 0.1 * x as f64) as f32);
        }
    }
    LatLonGrid::new(GridShape::new(NX, NY).expect("nonzero dims"), lat, lon)
        .expect("coordinate arrays sized to the grid")
}

fn surface_field(
    selector: FieldSelector,
    units: &str,
    value: impl Fn(usize, usize) -> f32,
) -> SelectedField2D {
    let values: Vec<f32> = (0..NY)
        .flat_map(|y| (0..NX).map(|x| value(x, y)).collect::<Vec<_>>())
        .collect();
    SelectedField2D::new(selector, units, grid(), values)
        .expect("values sized to the grid")
        .with_projection(GridProjection::Geographic)
}

/// One 3D plane: barometric temperature at `level` plus a small horizontal
/// gradient and an additive offset (used for the dewpoint depression).
fn volume_plane(level_hpa: u16, shift: f32, offset: f32) -> Vec<f32> {
    let base = 288.15 * (level_hpa as f32 / 1000.0).powf(0.1903);
    (0..NY)
        .flat_map(|y| {
            (0..NX)
                .map(|x| base + 0.02 * x as f32 - 0.015 * y as f32 + 0.3 * shift + offset)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn volume_input<'a>(name: &'a str, planes: &'a [Vec<f32>]) -> PressureVolumeInput<'a> {
    PressureVolumeInput {
        name,
        units: "K",
        selector_template: serde_json::json!({ "synthetic": name }),
        levels: SYNTHETIC_LEVELS
            .iter()
            .zip(planes)
            .map(|(&level, plane)| (level, plane.as_slice()))
            .collect(),
    }
}
