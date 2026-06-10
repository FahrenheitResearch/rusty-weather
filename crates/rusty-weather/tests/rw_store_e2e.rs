//! Offline end-to-end proof of the GRIB -> store pipeline, on the committed
//! fixture `tests/fixtures/hrrr_mini.grib2` (see `tests/fixtures/README.md`
//! for provenance): GRIB bytes -> `extract_fields_partial_from_model_bytes_at_forecast_hour`
//! -> `write_hour_from_fields` -> read back through `HourReader` /
//! `read_field_2d`. No network: the fixture ships in the repo.
//!
//! Layout under test mirrors `rw_ingest`: 2m temperature as a 2D field, TMP
//! at 850/700/500 hPa as one 3-level pressure volume, and the 500 hPa
//! U/V/HGT planes stored as additional 2D fields (a valid use of the 2D
//! path that exercises multi-2D writes).

use std::fs;
use std::path::{Path, PathBuf};

use rustwx_core::{CanonicalField, FieldSelector, ModelId, SelectedField2D};
use rustwx_io::extract_fields_partial_from_model_bytes_at_forecast_hour;
use rw_store::grid::GridFile;
use rw_store::ingest::{read_field_2d, write_hour_from_fields, PressureVolumeInput};
use rw_store::reader::HourReader;
use rw_store::run::RwsRunManifest;

const MODEL: &str = "hrrr";
const RUN: &str = "20260608_00z";
const HOUR: u16 = 6;
const BUILD: &str = "rw-store-e2e-test";
const WRITTEN_UNIX: u64 = 1_780_000_000;
/// HRRR CONUS grid dims; also guards against a truncated fixture.
const NX: usize = 1799;
const NY: usize = 1059;
/// Volume levels in canonical (descending) order.
const VLEVELS: [u16; 3] = [850, 700, 500];

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hrrr_mini.grib2")
}

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rw-store-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The exact selector set the fixture was built to contain.
fn fixture_selectors() -> Vec<FieldSelector> {
    vec![
        FieldSelector::isobaric(CanonicalField::Temperature, 850),
        FieldSelector::isobaric(CanonicalField::Temperature, 700),
        FieldSelector::isobaric(CanonicalField::Temperature, 500),
        FieldSelector::isobaric(CanonicalField::UWind, 500),
        FieldSelector::isobaric(CanonicalField::VWind, 500),
        FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 500),
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
    ]
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
fn bilinear_finite(plane: &[f32], fx: f64, fy: f64) -> f32 {
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
        let value = plane[iy * NX + ix];
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

#[test]
fn fixture_extract_write_read_back_round_trips() {
    // --- 1. fixture bytes -> extraction: everything present, nothing extra ---
    let bytes = fs::read(fixture_path()).expect("committed fixture must be readable");
    let selectors = fixture_selectors();
    let extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
        ModelId::Hrrr,
        &bytes,
        None,
        &selectors,
        Some(HOUR),
    )
    .expect("fixture must parse as GRIB2");
    assert!(
        extraction.missing.is_empty(),
        "fixture must serve every selector; missing: {:?}",
        extraction
            .missing
            .iter()
            .map(|s| s.key())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        extraction.extracted.len(),
        7,
        "fixture holds exactly the 7 requested fields"
    );

    let mut extracted = extraction.extracted;
    let temp_2m = take(
        &mut extracted,
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
    );
    let u_500 = take(
        &mut extracted,
        FieldSelector::isobaric(CanonicalField::UWind, 500),
    );
    let v_500 = take(
        &mut extracted,
        FieldSelector::isobaric(CanonicalField::VWind, 500),
    );
    let height_500 = take(
        &mut extracted,
        FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 500),
    );
    let tmp_planes: Vec<(u16, SelectedField2D)> = VLEVELS
        .iter()
        .map(|&level| {
            (
                level,
                take(
                    &mut extracted,
                    FieldSelector::isobaric(CanonicalField::Temperature, level),
                ),
            )
        })
        .collect();
    assert!(extracted.is_empty(), "no unexpected extra fields");
    assert_eq!(
        (temp_2m.grid.shape.nx, temp_2m.grid.shape.ny),
        (NX, NY),
        "fixture must be on the full HRRR CONUS grid"
    );

    // --- 2. write the hour: multi-2D + one 3-level TMP volume ---
    let dir = test_dir();
    let store_root = dir.join("store");
    let fields_2d: Vec<(&str, &SelectedField2D)> = vec![
        ("temperature_2m", &temp_2m),
        ("u_500", &u_500),
        ("v_500", &v_500),
        ("height_500", &height_500),
    ];
    let volume = PressureVolumeInput {
        name: "temperature_iso",
        units: FieldSelector::isobaric(CanonicalField::Temperature, 500).native_units(),
        selector_template: serde_json::json!({
            "field": CanonicalField::Temperature.as_str(),
            "vertical": "isobaric",
        }),
        levels: tmp_planes
            .iter()
            .map(|(level, field)| (*level, field.values.as_slice()))
            .collect(),
    };
    let written = write_hour_from_fields(
        &store_root,
        MODEL,
        RUN,
        HOUR,
        &fields_2d,
        &[volume],
        BUILD,
        WRITTEN_UNIX,
    )
    .expect("hour write must succeed");
    assert_eq!(
        written.vars,
        vec!["temperature_2m", "u_500", "v_500", "height_500", "temperature_iso"]
    );

    // --- 3. read back ---
    let run_dir = store_root.join(MODEL).join(RUN);
    let reader = HourReader::open(&written.path).expect("hour file must open");
    let grid = GridFile::open(&run_dir.join("grid.rwg")).expect("grid.rwg must open");
    assert_eq!((grid.nx, grid.ny), (NX, NY));

    // Every 2D field round-trips bit-exactly: values, selector, units.
    for (name, original) in &fields_2d {
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

    // Window read == crop of the full field.
    let (x0, y0, x1, y1) = (50usize, 50usize, 150usize, 150usize);
    let window = reader
        .read_window_2d("temperature_2m", x0, y0, x1, y1)
        .expect("window read");
    assert_eq!((window.x0, window.y0), (x0, y0));
    assert_eq!((window.nx, window.ny), (x1 - x0, y1 - y0));
    let crop: Vec<f32> = (y0..y1)
        .flat_map(|y| temp_2m.values[y * NX + x0..y * NX + x1].iter().copied())
        .collect();
    assert_bits_eq(&window.values, &crop, "temperature_2m window (50,50,150,150)");

    // Volume columns at 3 sample points, within the quantization bound of
    // the extracted planes.
    let planes: Vec<&[f32]> = tmp_planes
        .iter()
        .map(|(_, field)| field.values.as_slice())
        .collect();
    let bound = quant_bound(&planes);
    let var = reader
        .variable("temperature_iso")
        .expect("volume variable present");
    assert_eq!(var.levels_hpa, VLEVELS.to_vec(), "levels stored descending");
    let sample_points = [(100usize, 100usize), (900, 500), (1700, 1000)];
    for &(ix, iy) in &sample_points {
        let column = reader
            .read_column_3d("temperature_iso", ix, iy)
            .expect("column read");
        assert_eq!(column.len(), VLEVELS.len());
        for (k, value) in column.iter().enumerate() {
            let expected = planes[k][iy * NX + ix];
            if expected.is_nan() {
                assert!(value.is_nan(), "column ({ix},{iy}) level {}: NaN must survive", VLEVELS[k]);
                continue;
            }
            assert!(
                (value - expected).abs() <= bound,
                "column ({ix},{iy}) level {} hPa: got {value}, expected {expected}, bound {bound}",
                VLEVELS[k]
            );
        }
    }

    // Bilinear profile at a fractional point between the first two sample
    // points: each value within the quantization bound of the same
    // interpolation of the extracted planes.
    let (fx, fy) = (500.5f64, 300.5f64);
    let profile = reader
        .read_profile_3d("temperature_iso", fx, fy)
        .expect("profile read");
    assert_eq!(profile.len(), VLEVELS.len());
    let expected_profile: Vec<f32> = planes
        .iter()
        .map(|plane| bilinear_finite(plane, fx, fy))
        .collect();
    for (k, (got, expected)) in profile.iter().zip(&expected_profile).enumerate() {
        if expected.is_nan() {
            assert!(got.is_nan(), "profile level {}: NaN must survive", VLEVELS[k]);
            continue;
        }
        assert!(
            (got - expected).abs() <= bound,
            "profile level {} hPa: got {got}, expected {expected}, bound {bound}",
            VLEVELS[k]
        );
    }
    // Vertical ordering consistency: assert 850 > 500 (or the reverse) only
    // when the extracted data itself says so beyond the quantization noise —
    // this checks the store against the data, not the data against
    // meteorology (even though 850 hPa being warmer is the normal case).
    let delta = expected_profile[0] - expected_profile[2];
    if delta > 2.0 * bound {
        assert!(
            profile[0] > profile[2],
            "850 hPa ({}) must stay warmer than 500 hPa ({}) as in the source data",
            profile[0],
            profile[2]
        );
    } else if delta < -2.0 * bound {
        assert!(
            profile[0] < profile[2],
            "850 hPa ({}) must stay colder than 500 hPa ({}) as in the source data",
            profile[0],
            profile[2]
        );
    }

    // --- 4. manifest + grid-hash chain ---
    let manifest_bytes = fs::read(run_dir.join("run.json")).expect("run.json must exist");
    let manifest: RwsRunManifest =
        serde_json::from_slice(&manifest_bytes).expect("run.json must parse");
    assert_eq!(manifest.model, MODEL);
    assert_eq!(manifest.run, RUN);
    assert_eq!((manifest.nx, manifest.ny), (NX, NY));
    let entry = manifest.hours.get(&HOUR).expect("hour 6 registered");
    assert_eq!(entry.file, "f006.rws");
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
