//! Fixture tests for the L2 cloud-product decode, DQF gating and CWP
//! derivation, against REAL GOES-19 granules from the public
//! `noaa-goes19` bucket (scan 2026-08-04 18:01 UTC, day 216). Every
//! asserted number below was computed independently with Python
//! (netCDF4/numpy) from the same granules' raw integers — see
//! `tests/fixtures/README.md` for provenance, checksums and the fetch
//! script.
//!
//! The granules themselves are not committed (`tests/fixtures/.gitignore`
//! excludes `*.nc`); the directory holding the README, the checksums and
//! the fetch scripts is. When any granule is missing the tests announce
//! themselves loudly and pass vacuously; run
//! `tests/fixtures/fetch_fixtures.sh` (or `.ps1`) once to arm them, or
//! point `RW_SAT_CLOUD_FIXTURE_DIR` at a directory that already holds
//! them.

use std::path::PathBuf;

use rw_sat::abi::read_goes_abi_scene;
use rw_sat::archive::archive_goes_l2_source;
use rw_sat::cloud::{
    CloudProduct, CloudWindow, DEFAULT_CLOUD_PREVIEW_CELLS, DqfReport, read_archived_cloud_preview,
    read_archived_cloud_window, read_cloud_product_field, read_cloud_product_field_window,
};
use rw_sat::cwp::{CwpCounts, cloud_water_path_plane};

const ACHAM_GRANULE: &str =
    "OR_ABI-L2-ACHAM1-M6_G19_s20262161801249_e20262161801336_c20262161801594.nc";
const CODC_GRANULE: &str =
    "OR_ABI-L2-CODC-M6_G19_s20262161801170_e20262161803545_c20262161805324.nc";
const CPSC_GRANULE: &str =
    "OR_ABI-L2-CPSC-M6_G19_s20262161801170_e20262161803545_c20262161805325.nc";
const ACTPC_GRANULE: &str =
    "OR_ABI-L2-ACTPC-M6_G19_s20262161801170_e20262161803545_c20262161804390.nc";

/// The shared CONUS test window (2 km fixed grid): x 1360..1520,
/// y 640..800.
const WIN_X: (usize, usize) = (1360, 160);
const WIN_Y: (usize, usize) = (640, 160);

const GRANULES: [&str; 4] = [ACHAM_GRANULE, CODC_GRANULE, CPSC_GRANULE, ACTPC_GRANULE];

/// The fixture directory itself is committed — it carries the README, the
/// checksums and the fetch scripts — so presence of the directory proves
/// nothing. The gate is the granules.
fn fixture_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("RW_SAT_CLOUD_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
        });
    let missing = GRANULES
        .into_iter()
        .filter(|name| !dir.join(name).is_file())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Some(dir);
    }
    eprintln!(
        "SKIPPING cloud fixture test: {} of {} granules missing from {} ({}). \
         Run tests/fixtures/fetch_fixtures.sh (or .ps1), or point \
         RW_SAT_CLOUD_FIXTURE_DIR at the granules.",
        missing.len(),
        GRANULES.len(),
        dir.display(),
        missing.join(", ")
    );
    None
}

fn fixture(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    assert!(
        path.is_file(),
        "fixture dir exists but granule {name} is missing — re-run the fetch script"
    );
    path
}

/// Compare two gated planes bit for bit, treating a gated NaN as equal to
/// a gated NaN — `Vec<f32>` equality can never hold once the gate has run.
fn assert_planes_identical(actual: &[f32], expected: &[f32], what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}: plane length");
    for (index, (&actual, &expected)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(
            actual.is_nan(),
            expected.is_nan(),
            "{what}: gate disagreement at {index}"
        );
        if !expected.is_nan() {
            assert_eq!(actual, expected, "{what}: value at {index}");
        }
    }
}

fn assert_close(actual: f32, expected: f32, what: &str) {
    let scale = expected.abs().max(1e-6);
    assert!(
        (actual - expected).abs() / scale < 1e-4,
        "{what}: {actual} != {expected}"
    );
}

#[test]
fn acham_height_decodes_and_gates_fail_closed() {
    let Some(dir) = fixture_dir() else { return };
    let read = read_cloud_product_field(fixture(&dir, ACHAM_GRANULE), CloudProduct::CloudTopHeight)
        .expect("decode ACHAM1 granule");

    let grid = &read.field.scene.fixed_grid;
    assert_eq!((grid.nx, grid.ny), (250, 250), "meso 2 km grid");
    assert_eq!(read.field.units.as_deref(), Some("m"));
    assert_eq!(read.field.variable_name, "HT");

    // Independently computed with Python netCDF4/numpy from raw integers:
    // every DQF != 0 pixel is fill in HT too, so the gate masks nothing
    // new but still condemns 36,648 of 62,500 pixels.
    assert_eq!(
        read.dqf,
        DqfReport {
            total: 62_500,
            primary_missing: 36_648,
            dqf_missing: 0,
            dqf_bad: 36_648,
            masked: 0,
            finite: 25_852,
        }
    );

    // Spot pixels (row-major, y*nx + x), decoded raw*0.30520370602607727:
    let at = |y: usize, x: usize| read.field.values[y * grid.nx + x];
    assert_close(at(0, 16), 2398.9011, "HT[0,16] (raw 7860, DQF 0)");
    assert_close(at(120, 120), 10553.028, "HT[120,120] (raw 34577, DQF 0)");
    assert_close(at(249, 249), 3128.338, "HT[249,249] (raw 10250, DQF 0)");

    let mean = mean_of_finite(&read.field.values);
    assert_close(mean as f32, 5733.527, "mean of gated HT");
}

#[test]
fn conus_window_decodes_cod_cps_phase_with_dqf_accounting() {
    let Some(dir) = fixture_dir() else { return };
    let (xs, xc) = WIN_X;
    let (ys, yc) = WIN_Y;

    let cod = read_cloud_product_field_window(
        fixture(&dir, CODC_GRANULE),
        CloudProduct::OpticalDepth,
        xs,
        xc,
        ys,
        yc,
    )
    .expect("decode CODC window");
    let cps = read_cloud_product_field_window(
        fixture(&dir, CPSC_GRANULE),
        CloudProduct::ParticleSize,
        xs,
        xc,
        ys,
        yc,
    )
    .expect("decode CPSC window");
    let phase = read_cloud_product_field_window(
        fixture(&dir, ACTPC_GRANULE),
        CloudProduct::CloudTopPhase,
        xs,
        xc,
        ys,
        yc,
    )
    .expect("decode ACTPC window");

    assert_eq!(cps.field.units.as_deref(), Some("um"), "CPS is µm");

    // Independently computed DQF accounting for the window. The DCOMP
    // (COD/CPS) DQF planes are bit-identical, so their gate counts match;
    // the primary planes differ (clear sky is COD 0.0 but CPS fill).
    assert_eq!(
        cod.dqf,
        DqfReport {
            total: 25_600,
            primary_missing: 30,
            dqf_missing: 0,
            dqf_bad: 850,
            masked: 840,
            finite: 24_730,
        }
    );
    assert_eq!(
        cps.dqf,
        DqfReport {
            total: 25_600,
            primary_missing: 8_028,
            dqf_missing: 0,
            dqf_bad: 850,
            masked: 840,
            finite: 16_732,
        }
    );
    assert_eq!(
        phase.dqf,
        DqfReport {
            total: 25_600,
            primary_missing: 0,
            dqf_missing: 0,
            dqf_bad: 1_640,
            masked: 1_640,
            finite: 23_960,
        }
    );

    // A sun-glint pixel (DQF 742 = glint bit 64 set): finite raw COD
    // (3.1765606) gated to NaN. Fail-closed, and the reason is a recorded
    // count rather than a lost bit.
    let nx = cod.field.scene.fixed_grid.nx;
    assert!(cod.field.values[147 * nx + 145].is_nan());
}

#[test]
fn cwp_from_real_granules_matches_independent_computation() {
    let Some(dir) = fixture_dir() else { return };
    let (xs, xc) = WIN_X;
    let (ys, yc) = WIN_Y;

    let cod = read_cloud_product_field_window(
        fixture(&dir, CODC_GRANULE),
        CloudProduct::OpticalDepth,
        xs,
        xc,
        ys,
        yc,
    )
    .unwrap();
    let cps = read_cloud_product_field_window(
        fixture(&dir, CPSC_GRANULE),
        CloudProduct::ParticleSize,
        xs,
        xc,
        ys,
        yc,
    )
    .unwrap();
    let phase = read_cloud_product_field_window(
        fixture(&dir, ACTPC_GRANULE),
        CloudProduct::CloudTopPhase,
        xs,
        xc,
        ys,
        yc,
    )
    .unwrap();

    // The three products must share one fixed grid, bit for bit —
    // otherwise combining their planes would be fabrication.
    assert_eq!(cod.field.scene.fixed_grid, cps.field.scene.fixed_grid);
    assert_eq!(cod.field.scene.fixed_grid, phase.field.scene.fixed_grid);

    let (cwp, counts) =
        cloud_water_path_plane(&cod.field.values, &cps.field.values, &phase.field.values)
            .expect("derive CWP plane");

    // Independently computed with Python netCDF4/numpy (same DQF gates,
    // same coefficients) over the window.
    assert_eq!(
        counts,
        CwpCounts {
            clear_zero: 3_857,
            liquid: 5_141,
            supercooled: 434,
            mixed: 531,
            ice: 10_303,
            unknown: 0,
            phase_missing: 1_640,
            input_missing: 3_694,
        }
    );
    assert_eq!(counts.finite(), 20_266);
    assert_eq!(cwp.len(), 25_600);

    let nx = cod.field.scene.fixed_grid.nx;
    let at = |y: usize, x: usize| cwp[y * nx + x];
    // Liquid pixel: COD 2.7810166, CPS 12.371739 µm -> 22.937342 g/m².
    assert_close(at(45, 20), 22.937342, "liquid CWP");
    // Ice pixel: COD 21.115215, CPS 34.402565 µm -> 444.0833 g/m².
    assert_close(at(108, 123), 444.0833, "ice CWP");
    // Clear-sky pixel: exact zero observation (CPS is fill there).
    assert_eq!(at(88, 33), 0.0, "clear-sky zero");

    let finite_count = cwp.iter().filter(|value| value.is_finite()).count();
    assert_eq!(finite_count, 20_266);
    assert_close(mean_of_finite(&cwp) as f32, 124.07099, "mean CWP");
    let max = cwp.iter().copied().fold(f32::NAN, f32::max);
    assert_close(max, 5541.37, "max CWP");
}

/// A window must be a literal excerpt of the dense plane — same decoded
/// values, same gate decisions, no edge effect at the window boundary.
/// The mesoscale granule is small enough to hold both at once.
#[test]
fn a_window_is_an_exact_excerpt_of_the_dense_plane() {
    let Some(dir) = fixture_dir() else { return };
    let path = fixture(&dir, ACHAM_GRANULE);
    let dense = read_cloud_product_field(&path, CloudProduct::CloudTopHeight)
        .expect("decode the whole ACHAM1 plane");
    let nx = dense.field.scene.fixed_grid.nx;

    let (xs, xc, ys, yc) = (37, 64, 91, 48);
    let window =
        read_cloud_product_field_window(&path, CloudProduct::CloudTopHeight, xs, xc, ys, yc)
            .expect("decode an ACHAM1 window");

    let grid = &window.field.scene.fixed_grid;
    assert_eq!((grid.nx, grid.ny), (xc, yc));
    assert_eq!(window.dqf.total, xc * yc);
    assert_eq!(
        grid.x_scan_rad.as_slice(),
        &dense.field.scene.fixed_grid.x_scan_rad[xs..xs + xc],
        "the window carries the native scan angles of its own cells"
    );

    let mut recomputed = DqfReport {
        total: xc * yc,
        ..DqfReport::default()
    };
    for row in 0..yc {
        for column in 0..xc {
            let expected = dense.field.values[(ys + row) * nx + xs + column];
            let actual = window.field.values[row * xc + column];
            assert_eq!(
                actual.is_nan(),
                expected.is_nan(),
                "gate disagreement at ({}, {})",
                xs + column,
                ys + row
            );
            if expected.is_nan() {
                recomputed.primary_missing += 1;
            } else {
                assert_eq!(actual, expected, "value at ({}, {})", xs + column, ys + row);
                recomputed.finite += 1;
            }
        }
    }
    // Every pixel the window reports missing was already missing densely,
    // so nothing about the gate depends on how much was read.
    assert_eq!(window.dqf.finite, recomputed.finite);
    assert_eq!(
        window.dqf.primary_missing + window.dqf.masked,
        recomputed.primary_missing
    );
}

/// The archive door: a granule filed by its NOAA key, then read back one
/// rectangle at a time, must reproduce the direct windowed read exactly.
#[test]
fn an_archived_granule_reads_back_window_for_window() {
    let Some(dir) = fixture_dir() else { return };
    let path = fixture(&dir, ACTPC_GRANULE);
    let scene = read_goes_abi_scene(&path).expect("read the ACTPC scene");
    let store = tempfile::tempdir().expect("temp store");
    let store_root = store.path().join("store");
    let object_key = format!("ABI-L2-ACTPC/2026/216/18/{ACTPC_GRANULE}");
    let manifest = archive_goes_l2_source(&store_root, &path, &scene, &object_key)
        .expect("archive the ACTPC granule");
    assert_eq!(manifest.platform, "g19");
    assert_eq!(manifest.sector, "conus");
    assert_eq!(manifest.frame_id, "20260804T1801");

    let (xs, xc) = WIN_X;
    let (ys, yc) = WIN_Y;
    let direct =
        read_cloud_product_field_window(&path, CloudProduct::CloudTopPhase, xs, xc, ys, yc)
            .expect("direct windowed read");
    for frame in [manifest.frame_id.as_str(), "latest"] {
        let archived = read_archived_cloud_window(
            &store_root,
            "g19",
            "conus",
            CloudProduct::CloudTopPhase,
            frame,
            CloudWindow::new(xs, xc, ys, yc),
        )
        .expect("archived windowed read");
        assert_eq!(archived.dqf, direct.dqf, "frame {frame}");
        assert_planes_identical(
            &archived.field.values,
            &direct.field.values,
            &format!("frame {frame}"),
        );
        // The archived copy lives under a content-addressed name, yet its
        // scene identity still comes from the retained NOAA object key.
        assert_eq!(archived.field.scene.product, "ABI-L2-ACTPC");
        assert_eq!(
            archived.field.scene.start_time_utc.to_rfc3339(),
            "2026-08-04T18:01:17+00:00"
        );
    }

    // Filing the same granule under the wrong product is impossible: the
    // window door checks the archived bytes against the request.
    let mismatch = read_archived_cloud_window(
        &store_root,
        "g19",
        "conus",
        CloudProduct::OpticalDepth,
        "latest",
        CloudWindow::new(xs, xc, ys, yc),
    )
    .expect_err("COD was never archived here");
    assert!(
        mismatch.to_string().contains("l2_cloud_optical_depth"),
        "{mismatch}"
    );
}

/// The preview door decimates the primary and its DQF on one stride, so
/// every preview pixel is a real native pixel judged by its own flag.
#[test]
fn a_preview_decimates_both_planes_on_the_same_stride() {
    let Some(dir) = fixture_dir() else { return };
    let path = fixture(&dir, ACTPC_GRANULE);
    let scene = read_goes_abi_scene(&path).expect("read the ACTPC scene");
    let (nx, ny) = (scene.fixed_grid.nx, scene.fixed_grid.ny);
    let store = tempfile::tempdir().expect("temp store");
    let store_root = store.path().join("store");
    archive_goes_l2_source(
        &store_root,
        &path,
        &scene,
        &format!("ABI-L2-ACTPC/2026/216/18/{ACTPC_GRANULE}"),
    )
    .expect("archive the ACTPC granule");

    // A budget far below the native plane, so a stride is forced.
    let budget = 65_536;
    assert!(nx * ny > budget, "the CONUS plane must exceed the budget");
    let preview = read_archived_cloud_preview(
        &store_root,
        "g19",
        "conus",
        CloudProduct::CloudTopPhase,
        "latest",
        budget,
    )
    .expect("archived preview read");

    let step = rw_sat::automatic_preview_stride(nx, ny, budget);
    assert!(step > 1);
    let grid = &preview.field.scene.fixed_grid;
    assert_eq!((grid.nx, grid.ny), (nx.div_ceil(step), ny.div_ceil(step)));
    assert!(grid.nx * grid.ny <= budget, "the preview honors its budget");
    assert_eq!(preview.dqf.total, grid.nx * grid.ny);

    // Spot-check that a preview cell is the native cell at that stride
    // phase, gated identically — not an average of its neighbours.
    for (row, column) in [(0usize, 0usize), (7, 11), (grid.ny - 1, grid.nx - 1)] {
        let native = read_cloud_product_field_window(
            &path,
            CloudProduct::CloudTopPhase,
            column * step,
            1,
            row * step,
            1,
        )
        .expect("single-cell native read");
        let expected = native.field.values[0];
        let actual = preview.field.values[row * grid.nx + column];
        assert_eq!(
            actual.is_nan(),
            expected.is_nan(),
            "gate disagreement at preview ({column}, {row})"
        );
        if !expected.is_nan() {
            assert_eq!(actual, expected, "preview cell ({column}, {row})");
        }
    }

    // The default budget is a real budget, not a rounding of the plane.
    assert!(DEFAULT_CLOUD_PREVIEW_CELLS < nx * ny);
}

#[test]
fn fixture_filenames_parse_to_the_cloud_products() {
    // Pure filename checks — no fixture download required.
    use rw_sat::goes::parse_goes_abi_filename;
    let acham = parse_goes_abi_filename(ACHAM_GRANULE).unwrap();
    assert_eq!(acham.product, "ABI-L2-ACHAM1");
    assert_eq!(acham.mode, Some(6));
    assert_eq!(acham.channel, None, "cloud products carry no band token");
    let codc = parse_goes_abi_filename(CODC_GRANULE).unwrap();
    assert_eq!(codc.product, "ABI-L2-CODC");
    assert_eq!(
        codc.start_time_utc.to_rfc3339(),
        "2026-08-04T18:01:17+00:00"
    );
}

fn mean_of_finite(values: &[f32]) -> f64 {
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for &value in values {
        if value.is_finite() {
            sum += f64::from(value);
            count += 1;
        }
    }
    sum / count.max(1) as f64
}
