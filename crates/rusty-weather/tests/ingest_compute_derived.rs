//! Wiring proof for the ingest derived precompute path: `sbcape` and
//! `srh_0_3km` grids computed through
//! `ingest_compute::assemble_products_inputs` +
//! `compute_derived_2d_from_inputs` must match calling the underlying
//! rustwx-calc kernels directly with identically prepared inputs,
//! bit-exactly — same code path, same f32->f64 conversions, same
//! mixing-ratio math, same height-AGL assembly. This proves the wiring
//! (input assembly, level alignment, recipe fan-out), not the science,
//! which lives in rustwx-calc and is tested there.

use rustwx_calc::{
    EcapeVolumeInputs, GridShape as CalcGridShape, SurfaceInputs, VolumeShape, WindGridInputs,
    compute_sbcape_cin, compute_srh_03km_hemispheric,
};
use rustwx_core::{CanonicalField, FieldSelector, GridShape, LatLonGrid, SelectedField2D};
use rustwx_products::gridded::{NativeCapePlanes, mixing_ratio_from_dewpoint_k};

// The bin-shared module carries both compute stages; this test exercises
// only the non-heavy lane, so the heavy entry is intentionally unused here.
#[path = "../src/ingest_compute.rs"]
#[allow(dead_code)]
mod ingest_compute;
use ingest_compute::{IngestVolumes, MoistureKind};

const NX: usize = 3;
const NY: usize = 2;
const NXY: usize = NX * NY;
/// Descending pressure (ground up) — the canonical aligned order.
const LEVELS: [u16; 5] = [1000, 925, 850, 700, 500];
const NZ: usize = LEVELS.len();

fn grid() -> LatLonGrid {
    let mut lat = Vec::with_capacity(NXY);
    let mut lon = Vec::with_capacity(NXY);
    for y in 0..NY {
        for x in 0..NX {
            lat.push(35.0 + 0.01 * y as f32);
            lon.push(-97.0 + 0.01 * x as f32);
        }
    }
    LatLonGrid::new(GridShape::new(NX, NY).unwrap(), lat, lon).unwrap()
}

fn field(selector: FieldSelector, units: &str, values: Vec<f32>) -> SelectedField2D {
    SelectedField2D::new(selector, units, grid(), values).unwrap()
}

/// A warm, moist, sheared synthetic hour: enough instability that sbcape is
/// finite and nonzero somewhere, so the comparison is not all-NaN-trivial.
struct Synthetic {
    fields_2d: Vec<(&'static str, SelectedField2D)>,
    temperature_k: Vec<(u16, Vec<f32>)>,
    dewpoint_k: Vec<(u16, Vec<f32>)>,
    u_ms: Vec<(u16, Vec<f32>)>,
    v_ms: Vec<(u16, Vec<f32>)>,
    height_m: Vec<(u16, Vec<f32>)>,
}

fn synthetic() -> Synthetic {
    let psfc: Vec<f32> = (0..NXY).map(|ij| 97_500.0 + 50.0 * ij as f32).collect();
    let t2: Vec<f32> = (0..NXY).map(|ij| 302.0 + 0.3 * ij as f32).collect();
    let td2: Vec<f32> = (0..NXY).map(|ij| 295.0 + 0.2 * ij as f32).collect();
    let u10: Vec<f32> = (0..NXY).map(|ij| 2.0 + 0.5 * ij as f32).collect();
    let v10: Vec<f32> = (0..NXY).map(|ij| 5.0 - 0.25 * ij as f32).collect();
    let orog: Vec<f32> = (0..NXY).map(|ij| 300.0 + 5.0 * ij as f32).collect();

    let fields_2d = vec![
        (
            "temperature_2m",
            field(
                FieldSelector::height_agl(CanonicalField::Temperature, 2),
                "K",
                t2,
            ),
        ),
        (
            "dewpoint_2m",
            field(
                FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
                "K",
                td2,
            ),
        ),
        (
            "u_10m",
            field(
                FieldSelector::height_agl(CanonicalField::UWind, 10),
                "m s-1",
                u10,
            ),
        ),
        (
            "v_10m",
            field(
                FieldSelector::height_agl(CanonicalField::VWind, 10),
                "m s-1",
                v10,
            ),
        ),
        (
            "surface_pressure",
            field(FieldSelector::surface(CanonicalField::Pressure), "Pa", psfc),
        ),
        (
            "orography",
            field(
                FieldSelector::surface(CanonicalField::GeopotentialHeight),
                "gpm",
                orog,
            ),
        ),
    ];

    // Per-level planes keyed on (k, ij) so any reordering or misalignment
    // of levels/planes changes values and breaks the bit-compare.
    let plane = |base: f32, dk: f32, dij: f32, k: usize| -> Vec<f32> {
        (0..NXY)
            .map(|ij| base + dk * k as f32 + dij * ij as f32)
            .collect()
    };
    let temperature_k: Vec<(u16, Vec<f32>)> = LEVELS
        .iter()
        .enumerate()
        .map(|(k, &level)| (level, plane(301.0, -7.5, 0.2, k)))
        .collect();
    let dewpoint_k: Vec<(u16, Vec<f32>)> = LEVELS
        .iter()
        .enumerate()
        .map(|(k, &level)| (level, plane(294.0, -8.5, 0.15, k)))
        .collect();
    let u_ms: Vec<(u16, Vec<f32>)> = LEVELS
        .iter()
        .enumerate()
        .map(|(k, &level)| (level, plane(3.0, 4.0, 0.3, k)))
        .collect();
    let v_ms: Vec<(u16, Vec<f32>)> = LEVELS
        .iter()
        .enumerate()
        .map(|(k, &level)| (level, plane(6.0, 2.5, -0.2, k)))
        .collect();
    // Geopotential heights: well above the orography (300 m + a bit) and
    // strictly increasing by far more than the +1 m monotonic clamp, so the
    // AGL assembly is a pure (gh - orog) on this data.
    let height_m: Vec<(u16, Vec<f32>)> = LEVELS
        .iter()
        .enumerate()
        .map(|(k, &level)| (level, plane(400.0, 1400.0, 2.0, k)))
        .collect();

    Synthetic {
        fields_2d,
        temperature_k,
        dewpoint_k,
        u_ms,
        v_ms,
        height_m,
    }
}

fn to_f64(values: &[f32]) -> Vec<f64> {
    values.iter().map(|&value| f64::from(value)).collect()
}

/// Flatten `(level, plane)` pairs into `[k][ij]` f64 in LEVELS order with a
/// per-value conversion — the reference prep the ingest path must match.
fn flatten_with(planes: &[(u16, Vec<f32>)], convert: impl Fn(f32) -> f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(NZ * NXY);
    for &level in &LEVELS {
        let (_, plane) = planes
            .iter()
            .find(|(have, _)| *have == level)
            .expect("synthetic volumes carry every level");
        out.extend(plane.iter().map(|&value| convert(value)));
    }
    out
}

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

#[test]
fn ingest_derived_matches_direct_calc_kernels_bit_exactly() {
    let synthetic = synthetic();

    // --- the new path: shuffle the input level order to also prove the
    //     descending-pressure alignment inside compute_derived_2d ---
    let shuffle = |planes: &[(u16, Vec<f32>)]| -> Vec<(u16, Vec<f32>)> {
        let mut shuffled = planes.to_vec();
        shuffled.rotate_left(2);
        shuffled.swap(0, 1);
        shuffled
    };
    let inputs = ingest_compute::assemble_products_inputs(
        &synthetic.fields_2d,
        &IngestVolumes {
            temperature_k: &shuffle(&synthetic.temperature_k),
            moisture: &shuffle(&synthetic.dewpoint_k),
            moisture_kind: MoistureKind::DewpointK,
            u_ms: &shuffle(&synthetic.u_ms),
            v_ms: &shuffle(&synthetic.v_ms),
            height_m: &shuffle(&synthetic.height_m),
        },
        NativeCapePlanes::default(),
    )
    .expect("input assembly must succeed on the synthetic hour");
    let derived = ingest_compute::compute_derived_2d_from_inputs(&inputs)
        .expect("derived precompute must succeed on the synthetic hour");
    assert_eq!(
        derived.len(),
        29,
        "all 29 non-heavy recipes must realize; got: {:?}",
        derived.iter().map(|grid| grid.name).collect::<Vec<_>>()
    );
    let take = |name: &str| {
        derived
            .iter()
            .find(|grid| grid.name == name)
            .unwrap_or_else(|| panic!("derived output missing '{name}'"))
    };

    // --- the direct path: identical prep, then the calc kernels the
    //     derived lane dispatches to ---
    let lookup = |name: &str| {
        &synthetic
            .fields_2d
            .iter()
            .find(|(have, _)| *have == name)
            .unwrap()
            .1
    };
    let lat = to_f64(&lookup("temperature_2m").grid.lat_deg);
    let psfc_pa = to_f64(&lookup("surface_pressure").values);
    let t2_k = to_f64(&lookup("temperature_2m").values);
    let q2_kgkg: Vec<f64> = psfc_pa
        .iter()
        .zip(lookup("dewpoint_2m").values.iter())
        .map(|(&psfc, &td_k)| mixing_ratio_from_dewpoint_k(psfc / 100.0, f64::from(td_k)))
        .collect();
    let u10_ms = to_f64(&lookup("u_10m").values);
    let v10_ms = to_f64(&lookup("v_10m").values);
    let orog_m = to_f64(&lookup("orography").values);

    let temperature_c_3d = flatten_with(&synthetic.temperature_k, |v| f64::from(v) - 273.15);
    let qvapor_kgkg_3d: Vec<f64> = LEVELS
        .iter()
        .flat_map(|&level| {
            let (_, plane) = synthetic
                .dewpoint_k
                .iter()
                .find(|(have, _)| *have == level)
                .unwrap();
            plane
                .iter()
                .map(move |&td_k| mixing_ratio_from_dewpoint_k(f64::from(level), f64::from(td_k)))
        })
        .collect();
    let u_3d = flatten_with(&synthetic.u_ms, f64::from);
    let v_3d = flatten_with(&synthetic.v_ms, f64::from);
    let gh_3d = flatten_with(&synthetic.height_m, f64::from);

    // Height AGL exactly as the derived lane assembles it: (gh - orog)
    // clamped at 0, then forced monotonic by >= 1 m per level upward.
    let mut height_agl_3d: Vec<f64> = gh_3d
        .iter()
        .enumerate()
        .map(|(idx, &value)| (value - orog_m[idx % NXY]).max(0.0))
        .collect();
    for k in 1..NZ {
        for ij in 0..NXY {
            let min_height = height_agl_3d[(k - 1) * NXY + ij] + 1.0;
            if height_agl_3d[k * NXY + ij] < min_height {
                height_agl_3d[k * NXY + ij] = min_height;
            }
        }
    }

    // The compute lane has no pressure_3d_pa here, so the volume pressure is
    // the per-level vector (hPa * 100).
    let pressure_pa: Vec<f64> = LEVELS
        .iter()
        .map(|&level| f64::from(level) * 100.0)
        .collect();

    let calc_grid = CalcGridShape::new(NX, NY).unwrap();
    let surface_inputs = SurfaceInputs {
        psfc_pa: &psfc_pa,
        t2_k: &t2_k,
        q2_kgkg: &q2_kgkg,
        u10_ms: &u10_ms,
        v10_ms: &v10_ms,
    };
    let volume_inputs = EcapeVolumeInputs {
        pressure_pa: &pressure_pa,
        temperature_c: &temperature_c_3d,
        qvapor_kgkg: &qvapor_kgkg_3d,
        height_agl_m: &height_agl_3d,
        u_ms: &u_3d,
        v_ms: &v_3d,
        nz: NZ,
    };
    let sb = compute_sbcape_cin(calc_grid, volume_inputs, surface_inputs, None)
        .expect("direct sbcape kernel");
    let srh_03km = compute_srh_03km_hemispheric(
        WindGridInputs {
            shape: VolumeShape::new(calc_grid, NZ).unwrap(),
            u_3d_ms: &u_3d,
            v_3d_ms: &v_3d,
            height_agl_3d_m: &height_agl_3d,
        },
        &lat,
    )
    .expect("direct srh kernel");

    let expected_sbcape: Vec<f32> = sb.cape_jkg.iter().map(|&value| value as f32).collect();
    let expected_srh: Vec<f32> = srh_03km.iter().map(|&value| value as f32).collect();

    let sbcape = take("sbcape");
    assert_eq!(sbcape.units, "J/kg", "sbcape units");
    assert_bits_eq(&sbcape.values, &expected_sbcape, "sbcape");
    assert!(
        sbcape.values.iter().any(|value| value.is_finite()),
        "synthetic hour must produce at least one finite sbcape value, \
         or the comparison is NaN-trivial"
    );

    let srh = take("srh_0_3km");
    assert_eq!(srh.units, "m^2/s^2", "srh_0_3km units");
    assert_bits_eq(&srh.values, &expected_srh, "srh_0_3km");
    assert!(
        srh.values.iter().any(|value| value.is_finite()),
        "synthetic hour must produce at least one finite srh_0_3km value"
    );
}
