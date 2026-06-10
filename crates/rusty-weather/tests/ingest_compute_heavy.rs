//! Wiring proof for the ingest heavy (ECAPE-class) precompute path:
//! `sbecape` computed through `ingest_compute::assemble_products_inputs` +
//! `compute_heavy_2d_from_inputs` must match calling the rustwx-calc ECAPE
//! triplet kernel directly with identically prepared inputs, bit-exactly —
//! same code path, same f32->f64 conversions, same mixing-ratio math, same
//! height-AGL assembly, same per-level pressure vector. This proves the
//! wiring (input assembly, level alignment, slug fan-out, native-CAPE
//! plumbing), not the science, which lives in rustwx-calc/metrust and is
//! tested there.

use rustwx_calc::{
    EcapeTripletOptions, EcapeVolumeInputs, GridShape as CalcGridShape, SurfaceInputs,
    compute_ecape_triplet_with_failure_mask_from_parts,
};
use rustwx_core::{CanonicalField, FieldSelector, GridShape, LatLonGrid, SelectedField2D};
use rustwx_products::derived::store_heavy_recipe_slugs;
use rustwx_products::gridded::{NativeCapePlanes, mixing_ratio_from_dewpoint_k};

// The bin-shared module carries both compute stages; this test exercises
// only the heavy lane, so the non-heavy entry is intentionally unused here.
#[path = "../src/ingest_compute.rs"]
#[allow(dead_code)]
mod ingest_compute;
use ingest_compute::{IngestVolumes, MoistureKind};

const NX: usize = 3;
const NY: usize = 2;
const NXY: usize = NX * NY;
/// Descending pressure (ground up) — the canonical aligned order. Deeper
/// than the derived test's profile: ECAPE's entraining ascent and storm
/// motion need a full troposphere to produce finite values.
const LEVELS: [u16; 9] = [1000, 925, 850, 700, 500, 400, 300, 250, 200];
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

/// A warm, moist, strongly sheared synthetic hour with a conditionally
/// unstable troposphere: enough instability and shear that sbecape is
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
    let td2: Vec<f32> = (0..NXY).map(|ij| 296.0 + 0.2 * ij as f32).collect();
    let u10: Vec<f32> = (0..NXY).map(|ij| 3.0 + 0.5 * ij as f32).collect();
    let v10: Vec<f32> = (0..NXY).map(|ij| 6.0 - 0.25 * ij as f32).collect();
    let orog: Vec<f32> = (0..NXY).map(|ij| 100.0 + 5.0 * ij as f32).collect();

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

    // Per-level base values keyed on (k, ij) so any reordering or
    // misalignment of levels/planes changes values and breaks the
    // bit-compare. The profile is a loaded, sheared springtime sounding.
    let plane = |bases: [f32; NZ], dij: f32| -> Vec<(u16, Vec<f32>)> {
        LEVELS
            .iter()
            .zip(bases.iter())
            .map(|(&level, &base)| {
                (
                    level,
                    (0..NXY).map(|ij| base + dij * ij as f32).collect::<Vec<f32>>(),
                )
            })
            .collect()
    };
    let temperature_k = plane(
        [300.0, 295.0, 290.0, 279.0, 258.0, 246.0, 230.0, 221.0, 213.0],
        0.2,
    );
    let dewpoint_k = plane(
        [295.0, 292.0, 287.0, 271.0, 244.0, 230.0, 214.0, 205.0, 197.0],
        0.15,
    );
    let u_ms = plane(
        [4.0, 9.0, 13.0, 18.0, 24.0, 28.0, 33.0, 36.0, 38.0],
        0.3,
    );
    let v_ms = plane(
        [8.0, 11.0, 9.0, 6.0, 4.0, 3.0, 2.0, 1.0, 0.0],
        -0.2,
    );
    // Geopotential heights: above the orography and strictly increasing by
    // far more than the +1 m monotonic clamp, so the AGL assembly is a pure
    // (gh - orog) on this data.
    let height_m = plane(
        [
            120.0, 780.0, 1500.0, 3100.0, 5750.0, 7300.0, 9400.0, 10700.0, 12200.0,
        ],
        2.0,
    );

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

/// Shuffle the input level order to also prove the descending-pressure
/// alignment inside the assembly.
fn shuffle(planes: &[(u16, Vec<f32>)]) -> Vec<(u16, Vec<f32>)> {
    let mut shuffled = planes.to_vec();
    shuffled.rotate_left(3);
    shuffled.swap(0, 2);
    shuffled
}

fn ingest_volumes(synthetic: &Synthetic) -> IngestVolumes<'_> {
    // Leaked shuffles keep the borrows simple for the test's lifetime.
    fn leak(planes: Vec<(u16, Vec<f32>)>) -> &'static [(u16, Vec<f32>)] {
        Box::leak(planes.into_boxed_slice())
    }
    IngestVolumes {
        temperature_k: leak(shuffle(&synthetic.temperature_k)),
        moisture: leak(shuffle(&synthetic.dewpoint_k)),
        moisture_kind: MoistureKind::DewpointK,
        u_ms: leak(shuffle(&synthetic.u_ms)),
        v_ms: leak(shuffle(&synthetic.v_ms)),
        height_m: leak(shuffle(&synthetic.height_m)),
    }
}

fn native_cape() -> NativeCapePlanes {
    NativeCapePlanes {
        sbcape_jkg: Some((0..NXY).map(|ij| 600.0 + 40.0 * ij as f64).collect()),
        mlcape_jkg: Some((0..NXY).map(|ij| 450.0 + 30.0 * ij as f64).collect()),
        mucape_jkg: Some((0..NXY).map(|ij| 700.0 + 25.0 * ij as f64).collect()),
    }
}

#[test]
fn ingest_heavy_sbecape_matches_direct_ecape_triplet_bit_exactly() {
    let synthetic = synthetic();

    // --- the new path: assemble once (with native CAPE), run the heavy
    //     stage exactly as rw_ingest does ---
    let inputs = ingest_compute::assemble_products_inputs(
        &synthetic.fields_2d,
        &ingest_volumes(&synthetic),
        native_cape(),
    )
    .expect("input assembly must succeed on the synthetic hour");
    let heavy = ingest_compute::compute_heavy_2d_from_inputs(&inputs)
        .expect("heavy precompute must succeed on the synthetic hour");

    let expected_slugs = store_heavy_recipe_slugs();
    assert_eq!(
        heavy.grids.iter().map(|grid| grid.name).collect::<Vec<_>>(),
        expected_slugs,
        "all heavy recipes must realize, in inventory order, when native CAPE is present"
    );
    assert!(
        heavy.skipped.is_empty(),
        "nothing may skip with native CAPE present: {:?}",
        heavy.skipped
    );

    // --- the direct path: identical prep, then the rustwx-calc ECAPE
    //     triplet kernel the heavy lane dispatches to ---
    let lookup = |name: &str| {
        &synthetic
            .fields_2d
            .iter()
            .find(|(have, _)| *have == name)
            .unwrap()
            .1
    };
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

    // Height AGL exactly as the heavy lane assembles it: (gh - orog)
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

    // prepare_heavy_volume is called with include_pressure_3d = false, so
    // the volume pressure is the per-level vector (hPa * 100).
    let pressure_pa: Vec<f64> = LEVELS
        .iter()
        .map(|&level| f64::from(level) * 100.0)
        .collect();

    let calc_grid = CalcGridShape::new(NX, NY).unwrap();
    let triplet = compute_ecape_triplet_with_failure_mask_from_parts(
        calc_grid,
        EcapeVolumeInputs {
            pressure_pa: &pressure_pa,
            temperature_c: &temperature_c_3d,
            qvapor_kgkg: &qvapor_kgkg_3d,
            height_agl_m: &height_agl_3d,
            u_ms: &u_3d,
            v_ms: &v_3d,
            nz: NZ,
        },
        SurfaceInputs {
            psfc_pa: &psfc_pa,
            t2_k: &t2_k,
            q2_kgkg: &q2_kgkg,
            u10_ms: &u10_ms,
            v10_ms: &v10_ms,
        },
        // The heavy lane pins right-moving storm motion (see
        // compute_ecape_map_fields_with_prepared_volume).
        EcapeTripletOptions::new("right_moving"),
    )
    .expect("direct ECAPE triplet kernel");

    let take = |name: &str| {
        heavy
            .grids
            .iter()
            .find(|grid| grid.name == name)
            .unwrap_or_else(|| panic!("heavy output missing '{name}'"))
    };

    let expected_sbecape: Vec<f32> = triplet
        .sb
        .fields
        .ecape_jkg
        .iter()
        .map(|&value| value as f32)
        .collect();
    let sbecape = take("sbecape");
    assert_eq!(sbecape.units, "J/kg", "sbecape units");
    assert_bits_eq(&sbecape.values, &expected_sbecape, "sbecape");
    assert!(
        sbecape.values.iter().any(|value| value.is_finite()),
        "synthetic hour must produce at least one finite sbecape value, \
         or the comparison is NaN-trivial"
    );

    let expected_sbecin: Vec<f32> = triplet
        .sb
        .fields
        .cin_jkg
        .iter()
        .map(|&value| value as f32)
        .collect();
    let sbecin = take("sbecin");
    assert_eq!(sbecin.units, "J/kg", "sbecin units");
    assert_bits_eq(&sbecin.values, &expected_sbecin, "sbecin");

    let expected_sbncape: Vec<f32> = triplet
        .sb
        .fields
        .ncape_jkg
        .iter()
        .map(|&value| value as f32)
        .collect();
    let sbncape = take("sbncape");
    assert_eq!(sbncape.units, "J/kg", "sbncape units");
    assert_bits_eq(&sbncape.values, &expected_sbncape, "sbncape");

    assert_eq!(
        heavy.ecape_failure_count,
        triplet.total_failure_count(),
        "failure count must ride through unchanged"
    );

    // Native ratio plumbing: stored ratio grid == stored ecape / native
    // plane wherever both are finite (the lane NaNs cells whose native
    // CAPE denominator is below its 100 J/kg floor).
    let native = native_cape();
    let native_sb = native.sbcape_jkg.as_ref().unwrap();
    let ratio = take("sb_ecape_native_cape_ratio");
    assert_eq!(ratio.units, "ratio", "sb native ratio units");
    let mut finite_ratio_checked = 0usize;
    for ij in 0..NXY {
        let ecape = f64::from(sbecape.values[ij]);
        if ratio.values[ij].is_finite() && ecape.is_finite() {
            let expected = (triplet.sb.fields.ecape_jkg[ij] / native_sb[ij]) as f32;
            assert_eq!(
                ratio.values[ij].to_bits(),
                expected.to_bits(),
                "sb native ratio at index {ij}"
            );
            finite_ratio_checked += 1;
        }
    }
    assert!(
        finite_ratio_checked > 0,
        "at least one finite native-ratio cell must exist, or the ratio check is trivial"
    );
}

#[test]
fn ingest_heavy_without_native_cape_skips_only_the_native_ratios() {
    let synthetic = synthetic();
    let inputs = ingest_compute::assemble_products_inputs(
        &synthetic.fields_2d,
        &ingest_volumes(&synthetic),
        NativeCapePlanes::default(),
    )
    .expect("input assembly must succeed on the synthetic hour");
    let heavy = ingest_compute::compute_heavy_2d_from_inputs(&inputs)
        .expect("heavy precompute must succeed without native CAPE");

    let expected_realized: Vec<&str> = store_heavy_recipe_slugs()
        .into_iter()
        .filter(|slug| !slug.ends_with("_ecape_native_cape_ratio"))
        .collect();
    assert_eq!(
        heavy.grids.iter().map(|grid| grid.name).collect::<Vec<_>>(),
        expected_realized,
        "every non-native-ratio heavy recipe must realize"
    );
    let skipped_slugs: Vec<&str> = heavy.skipped.iter().map(|(slug, _)| *slug).collect();
    assert_eq!(
        skipped_slugs,
        vec![
            "sb_ecape_native_cape_ratio",
            "ml_ecape_native_cape_ratio",
            "mu_ecape_native_cape_ratio",
        ],
        "exactly the three native-ratio recipes must skip"
    );
    for (slug, reason) in &heavy.skipped {
        assert!(
            reason.contains("native") && reason.contains("CAPE"),
            "skip reason for '{slug}' must document the missing native CAPE plane, got: {reason}"
        );
    }
}
