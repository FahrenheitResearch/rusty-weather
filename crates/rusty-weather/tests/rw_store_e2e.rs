//! Offline end-to-end proof of the GRIB -> store pipeline, on the committed
//! fixtures `tests/fixtures/hrrr_mini.grib2` and `tests/fixtures/gfs_mini.grib2`
//! (see `tests/fixtures/README.md` for provenance): GRIB bytes ->
//! `extract_fields_partial_from_model_bytes_at_forecast_hour` ->
//! `write_hour_from_fields` -> read back through `HourReader` /
//! `read_field_2d`. No network: the fixtures ship in the repo.
//!
//! The body is shared across models via [`Case`]; each model contributes its
//! own grid dims, fixture, expected 2D fields, and one pressure volume:
//!   * HRRR (Lambert, 1799x1059): 2m temperature + 500 hPa U/V/HGT as 2D, and
//!     TMP at 850/700/500 hPa as one 3-level volume.
//!   * GFS (lat/lon, 1440x721): 2m temperature/dewpoint + MSLP + 850/500 hPa
//!     HGT as 2D, and TMP at 850/500 hPa as one 2-level volume.

use std::fs;
use std::path::{Path, PathBuf};

use rustwx_core::{CanonicalField, FieldSelector, ModelId, SelectedField2D};
use rustwx_io::extract_fields_partial_from_model_bytes_at_forecast_hour;
use rw_store::grid::GridFile;
use rw_store::ingest::{PressureVolumeInput, read_field_2d, write_hour_from_fields};
use rw_store::reader::HourReader;
use rw_store::run::RwsRunManifest;

const BUILD: &str = "rw-store-e2e-test";
const WRITTEN_UNIX: u64 = 1_780_000_000;

/// One model's end-to-end case: which fixture, on what grid, and the exact
/// fields it was built to serve.
struct Case {
    model: ModelId,
    fixture: &'static str,
    run: &'static str,
    hour: u16,
    nx: usize,
    ny: usize,
    /// 2D fields stored as plain tiles: (store name, selector). The first is
    /// used for the windowed-read crop check.
    fields_2d: Vec<(&'static str, FieldSelector)>,
    /// One pressure volume: (store name, canonical field, levels descending).
    volume_name: &'static str,
    volume_field: CanonicalField,
    volume_levels: Vec<u16>,
    /// Grid sample points (ix, iy) for the column checks; all in-bounds.
    sample_points: Vec<(usize, usize)>,
    /// Fractional point for the bilinear profile check.
    profile_fx: f64,
    profile_fy: f64,
    /// Window for the windowed-read crop check (x0, y0, x1, y1).
    window: (usize, usize, usize, usize),
}

fn hrrr_case() -> Case {
    Case {
        model: ModelId::Hrrr,
        fixture: "hrrr_mini.grib2",
        run: "20260608_00z",
        hour: 6,
        nx: 1799,
        ny: 1059,
        fields_2d: vec![
            (
                "temperature_2m",
                FieldSelector::height_agl(CanonicalField::Temperature, 2),
            ),
            ("u_500", FieldSelector::isobaric(CanonicalField::UWind, 500)),
            ("v_500", FieldSelector::isobaric(CanonicalField::VWind, 500)),
            (
                "height_500",
                FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 500),
            ),
        ],
        volume_name: "temperature_iso",
        volume_field: CanonicalField::Temperature,
        volume_levels: vec![850, 700, 500],
        sample_points: vec![(100, 100), (900, 500), (1700, 1000)],
        profile_fx: 500.5,
        profile_fy: 300.5,
        window: (50, 50, 150, 150),
    }
}

fn gfs_case() -> Case {
    Case {
        model: ModelId::Gfs,
        fixture: "gfs_mini.grib2",
        run: "20260611_00z",
        hour: 0,
        nx: 1440,
        ny: 721,
        fields_2d: vec![
            (
                "temperature_2m",
                FieldSelector::height_agl(CanonicalField::Temperature, 2),
            ),
            (
                "dewpoint_2m",
                FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
            ),
            (
                "mslp",
                FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel),
            ),
            (
                "height_850",
                FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 850),
            ),
            (
                "height_500",
                FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 500),
            ),
        ],
        volume_name: "temperature_iso",
        volume_field: CanonicalField::Temperature,
        volume_levels: vec![850, 500],
        sample_points: vec![(100, 100), (720, 360), (1439, 720)],
        profile_fx: 700.5,
        profile_fy: 300.5,
        window: (50, 50, 150, 150),
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rw-store-e2e-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Pull the field for `selector` out of the extracted set (panics with the
/// selector name if absent — extraction completeness is asserted first).
fn take(extracted: &mut Vec<SelectedField2D>, selector: FieldSelector) -> SelectedField2D {
    let index = extracted
        .iter()
        .position(|field| field.selector == selector)
        .unwrap_or_else(|| panic!("extracted set is missing '{}'", selector.key()));
    extracted.swap_remove(index)
}

/// NaN-safe bit-exact slice comparison.
fn assert_bits_eq(actual: &[f32], expected: &[f32], context: &str) {
    assert_eq!(actual.len(), expected.len(), "{context}: length mismatch");
    for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            a.to_bits(),
            e.to_bits(),
            "{context}: value mismatch at index {i} (actual {a}, expected {e})"
        );
    }
}

/// Conservative 3D quantization error bound over `planes`: the codec
/// quantizes whole chunks with one scale each, every scale is <= (global
/// value range) / 65534, half of that bounds the rounding error; the epsilon
/// covers f32 decode noise.
fn quant_bound(planes: &[&[f32]]) -> f32 {
    let mut vmin = f32::INFINITY;
    let mut vmax = f32::NEG_INFINITY;
    for plane in planes {
        for value in *plane {
            if value.is_finite() {
                vmin = vmin.min(*value);
                vmax = vmax.max(*value);
            }
        }
    }
    assert!(vmax >= vmin, "volume planes hold no finite values");
    (vmax - vmin) / 65534.0 + 1e-3
}

/// Bilinear interpolation of `plane` at fractional grid coordinates, with
/// the same finite-corner weight renormalization `read_profile_3d` applies,
/// so expected values stay comparable in the presence of NaN corners.
fn bilinear_finite(plane: &[f32], nx: usize, fx: f64, fy: f64) -> f32 {
    let (x0, x1) = (fx.floor() as usize, fx.ceil() as usize);
    let (y0, y1) = (fy.floor() as usize, fy.ceil() as usize);
    let wx = (fx - x0 as f64) as f32;
    let wy = (fy - y0 as f64) as f32;
    let corners = [
        (x0, y0, (1.0 - wx) * (1.0 - wy)),
        (x1, y0, wx * (1.0 - wy)),
        (x0, y1, (1.0 - wx) * wy),
        (x1, y1, wx * wy),
    ];
    let mut weight_sum = 0.0f32;
    let mut value_sum = 0.0f32;
    for (ix, iy, weight) in corners {
        let value = plane[iy * nx + ix];
        if value.is_finite() {
            weight_sum += weight;
            value_sum += weight * value;
        }
    }
    if weight_sum > 0.0 {
        value_sum / weight_sum
    } else {
        f32::NAN
    }
}

/// The shared extract -> write -> read-back proof, run per model.
fn run_case(case: Case) {
    let Case {
        model,
        fixture,
        run,
        hour,
        nx,
        ny,
        fields_2d: planned_2d,
        volume_name,
        volume_field,
        volume_levels,
        sample_points,
        profile_fx,
        profile_fy,
        window,
    } = case;
    let model_slug = model.as_str();

    // --- 1. fixture bytes -> extraction: everything present, nothing extra ---
    let bytes = fs::read(fixture_path(fixture)).expect("committed fixture must be readable");
    let mut selectors: Vec<FieldSelector> = planned_2d.iter().map(|(_, sel)| *sel).collect();
    for &level in &volume_levels {
        selectors.push(FieldSelector::isobaric(volume_field, level));
    }
    let expected_field_count = selectors.len();
    let extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
        model,
        &bytes,
        None,
        &selectors,
        Some(hour),
    )
    .expect("fixture must parse as GRIB2");
    assert!(
        extraction.missing.is_empty(),
        "{model_slug} fixture must serve every selector; missing: {:?}",
        extraction
            .missing
            .iter()
            .map(|s| s.key())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        extraction.extracted.len(),
        expected_field_count,
        "{model_slug} fixture holds exactly the requested fields"
    );

    let mut extracted = extraction.extracted;
    // Pull the planned 2D fields (in declared order) out of the extracted set.
    let surface_fields: Vec<(&'static str, SelectedField2D)> = planned_2d
        .iter()
        .map(|(name, sel)| (*name, take(&mut extracted, *sel)))
        .collect();
    // Pull the volume planes (in declared level order).
    let volume_planes: Vec<(u16, SelectedField2D)> = volume_levels
        .iter()
        .map(|&level| {
            (
                level,
                take(&mut extracted, FieldSelector::isobaric(volume_field, level)),
            )
        })
        .collect();
    assert!(
        extracted.is_empty(),
        "{model_slug}: no unexpected extra fields"
    );
    let first = &surface_fields[0].1;
    assert_eq!(
        (first.grid.shape.nx, first.grid.shape.ny),
        (nx, ny),
        "{model_slug} fixture must be on the full model grid"
    );

    // --- 2. write the hour: multi-2D + one pressure volume ---
    let dir = test_dir(model_slug);
    let store_root = dir.join("store");
    let fields_2d: Vec<(&str, &SelectedField2D)> = surface_fields
        .iter()
        .map(|(name, field)| (*name, field))
        .collect();
    let volume = PressureVolumeInput {
        name: volume_name,
        units: FieldSelector::isobaric(volume_field, volume_levels[0]).native_units(),
        selector_template: serde_json::json!({
            "field": volume_field.as_str(),
            "vertical": "isobaric",
        }),
        levels: volume_planes
            .iter()
            .map(|(level, field)| (*level, field.values.as_slice()))
            .collect(),
    };
    let written = write_hour_from_fields(
        &store_root,
        model_slug,
        run,
        hour,
        &fields_2d,
        &[volume],
        BUILD,
        WRITTEN_UNIX,
    )
    .expect("hour write must succeed");
    let mut expected_vars: Vec<&str> = planned_2d.iter().map(|(name, _)| *name).collect();
    expected_vars.push(volume_name);
    assert_eq!(
        written.vars, expected_vars,
        "{model_slug}: stored var order"
    );

    // --- 3. read back ---
    let run_dir = store_root.join(model_slug).join(run);
    let reader = HourReader::open(&written.path).expect("hour file must open");
    let grid = GridFile::open(&run_dir.join("grid.rwg")).expect("grid.rwg must open");
    assert_eq!((grid.nx, grid.ny), (nx, ny));

    // Every 2D field round-trips bit-exactly: values, selector, units.
    for (name, original) in &surface_fields {
        let round_trip = read_field_2d(&reader, &grid, name).expect("2D read-back");
        assert_eq!(
            round_trip.selector, original.selector,
            "{name}: selector must round-trip"
        );
        assert_eq!(round_trip.units, original.units, "{name}: units");
        assert_bits_eq(
            &round_trip.values,
            &original.values,
            &format!("{name}: values"),
        );
    }

    // Window read == crop of the full field (first 2D field).
    let (x0, y0, x1, y1) = window;
    let crop_field = &surface_fields[0].1;
    let crop_name = surface_fields[0].0;
    let win = reader
        .read_window_2d(crop_name, x0, y0, x1, y1)
        .expect("window read");
    assert_eq!((win.x0, win.y0), (x0, y0));
    assert_eq!((win.nx, win.ny), (x1 - x0, y1 - y0));
    let crop: Vec<f32> = (y0..y1)
        .flat_map(|y| crop_field.values[y * nx + x0..y * nx + x1].iter().copied())
        .collect();
    assert_bits_eq(
        &win.values,
        &crop,
        &format!("{crop_name} window ({x0},{y0},{x1},{y1})"),
    );

    // Volume columns at the sample points, within the quantization bound.
    let planes: Vec<&[f32]> = volume_planes
        .iter()
        .map(|(_, field)| field.values.as_slice())
        .collect();
    let bound = quant_bound(&planes);
    let var = reader
        .variable(volume_name)
        .expect("volume variable present");
    assert_eq!(var.levels_hpa, volume_levels, "levels stored descending");
    for &(ix, iy) in &sample_points {
        let column = reader
            .read_column_3d(volume_name, ix, iy)
            .expect("column read");
        assert_eq!(column.len(), volume_levels.len());
        for (k, value) in column.iter().enumerate() {
            let expected = planes[k][iy * nx + ix];
            if expected.is_nan() {
                assert!(
                    value.is_nan(),
                    "column ({ix},{iy}) level {}: NaN must survive",
                    volume_levels[k]
                );
                continue;
            }
            assert!(
                (value - expected).abs() <= bound,
                "column ({ix},{iy}) level {} hPa: got {value}, expected {expected}, bound {bound}",
                volume_levels[k]
            );
        }
    }

    // Bilinear profile at a fractional point: each value within the
    // quantization bound of the same interpolation of the extracted planes.
    let profile = reader
        .read_profile_3d(volume_name, profile_fx, profile_fy)
        .expect("profile read");
    assert_eq!(profile.len(), volume_levels.len());
    let expected_profile: Vec<f32> = planes
        .iter()
        .map(|plane| bilinear_finite(plane, nx, profile_fx, profile_fy))
        .collect();
    for (k, (got, expected)) in profile.iter().zip(&expected_profile).enumerate() {
        if expected.is_nan() {
            assert!(
                got.is_nan(),
                "profile level {}: NaN must survive",
                volume_levels[k]
            );
            continue;
        }
        assert!(
            (got - expected).abs() <= bound,
            "profile level {} hPa: got {got}, expected {expected}, bound {bound}",
            volume_levels[k]
        );
    }
    // Vertical ordering consistency: assert the lowest vs highest level
    // relationship only when the extracted data itself says so beyond the
    // quantization noise — this checks the store against the data, not the
    // data against meteorology (the lowest pressure level being warmest is the
    // normal case).
    let last = volume_levels.len() - 1;
    let delta = expected_profile[0] - expected_profile[last];
    if delta > 2.0 * bound {
        assert!(
            profile[0] > profile[last],
            "{} hPa ({}) must stay warmer than {} hPa ({}) as in the source data",
            volume_levels[0],
            profile[0],
            volume_levels[last],
            profile[last]
        );
    } else if delta < -2.0 * bound {
        assert!(
            profile[0] < profile[last],
            "{} hPa ({}) must stay colder than {} hPa ({}) as in the source data",
            volume_levels[0],
            profile[0],
            volume_levels[last],
            profile[last]
        );
    }

    // --- 4. manifest + grid-hash chain ---
    let manifest_bytes = fs::read(run_dir.join("run.json")).expect("run.json must exist");
    let manifest: RwsRunManifest =
        serde_json::from_slice(&manifest_bytes).expect("run.json must parse");
    assert_eq!(manifest.model, model_slug);
    assert_eq!(manifest.run, run);
    assert_eq!((manifest.nx, manifest.ny), (nx, ny));
    let entry = manifest.hours.get(&hour).expect("hour registered");
    assert_eq!(entry.file, format!("f{hour:03}.rws"));
    assert_eq!(entry.written_unix, WRITTEN_UNIX);
    assert_eq!(entry.variables, written.vars);
    assert_eq!(
        reader.meta().grid_hash,
        grid.hash,
        "hour meta must reference the grid file's hash"
    );
    assert_eq!(
        manifest.grid_hash, grid.hash,
        "manifest must reference the grid file's hash"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn hrrr_fixture_extract_write_read_back_round_trips() {
    run_case(hrrr_case());
}

#[test]
fn gfs_fixture_extract_write_read_back_round_trips() {
    run_case(gfs_case());
}
