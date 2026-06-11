//! Decode tests pinned to a **real** GOES-19 GLM L2 LCFA granule committed
//! under `tests/fixtures/`. The granule is unmodified NOAA public data; see
//! `tests/fixtures/README.md` for provenance.
//!
//! The expected numbers below were derived by dumping the granule's raw
//! variables/attributes and computing the mapping by hand (see the
//! `flash0_absolute_time_is_hand_verified` arithmetic). If a future fixture
//! swap changes them, update the constants deliberately.

use std::path::PathBuf;

use rw_glm::{decode_granule, saturate_duration_ms};

/// The committed real granule.
fn granule_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"))
        .join("OR_GLM-L2-LCFA_G19_s20261620805000_e20261620805200_c20261620805214.nc")
}

/// Flash count == the granule's `number_of_flashes` dimension (107).
const EXPECTED_FLASH_COUNT: usize = 107;
/// `product_time` (834437100 J2000 s) -> Unix ms.
const PRODUCT_UNIX_MS: i64 = 1_781_165_100_000;
/// quality_flag != 0 count, dumped from the granule.
const EXPECTED_DEGRADED: usize = 3;

#[test]
fn decodes_expected_flash_count_and_key() {
    let g = decode_granule(&granule_path()).expect("decode");
    assert_eq!(
        g.flashes.len(),
        EXPECTED_FLASH_COUNT,
        "decoded count must equal the number_of_flashes dimension"
    );
    assert_eq!(
        g.granule_key, "OR_GLM-L2-LCFA_G19_s20261620805000_e20261620805200_c20261620805214",
        "granule_key is the filename stem (Task 3 dedup key)"
    );
    assert_eq!(g.satellite.as_deref(), Some("G19"), "platform_ID hint");
}

#[test]
fn positions_are_within_the_goes_east_disk() {
    let g = decode_granule(&granule_path()).expect("decode");
    let mut in_disk = 0usize;
    for f in &g.flashes {
        // Lat must be within the GLM field of regard (generous ±66).
        assert!(
            f.lat >= -66.0 && f.lat <= 66.0,
            "lat {} out of GLM disk for flash {}",
            f.lat,
            f.flash_id
        );
        // Everything within ±180 lon (sanity); bulk within the G19 (75.2W)
        // disk extent (~ -157..13).
        assert!(
            f.lon >= -180.0 && f.lon <= 180.0,
            "lon {} not a valid longitude for flash {}",
            f.lon,
            f.flash_id
        );
        if f.lon >= -157.0 && f.lon <= 13.0 {
            in_disk += 1;
        }
    }
    // The overwhelming majority must sit inside the GOES-East disk.
    assert!(
        in_disk * 10 >= g.flashes.len() * 9,
        "expected the bulk within the G19 disk, got {in_disk}/{}",
        g.flashes.len()
    );
}

#[test]
fn energies_are_finite_nonnegative_and_within_envelope() {
    let g = decode_granule(&granule_path()).expect("decode");
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut energies: Vec<f32> = g.flashes.iter().map(|f| f.energy).collect();
    for &e in &energies {
        assert!(e.is_finite() && e >= 0.0, "energy {e} not finite & >= 0");
        min = min.min(e);
        max = max.max(e);
    }
    energies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = energies[energies.len() / 2];
    // GLM flash radiant energies live in roughly 1e-15 .. 1e-10 J. Print the
    // distribution in the failure message so a fixture swap is diagnosable.
    assert!(
        max <= 1.0e-9,
        "max energy {max:e} above sane envelope (min={min:e} median={median:e} max={max:e})"
    );
    assert!(
        (0.0..1.0e-12).contains(&min),
        "min energy {min:e} unexpected (min={min:e} median={median:e} max={max:e})"
    );
}

#[test]
fn flash0_absolute_time_is_hand_verified() {
    // Hand arithmetic against the granule's raw values:
    //   product_time = 834437100.0 J2000 s
    //   J2000 epoch  = 946_728_000 Unix s
    //   => product_unix_ms = (946_728_000 + 834437100) * 1000 = 1_781_165_100_000
    //   flash[0] first_event: raw 11913, scale 0.00038147560553625226,
    //            add_offset -5.0  =>  11913*scale - 5.0 = -0.45548111... s
    //   => flash0 time_unix_ms = 1_781_165_100_000 + round(-0.45548*1000)
    //                          = 1_781_165_100_000 - 455 = 1_781_165_099_545
    const EXPECTED_FLASH0_MS: i64 = 1_781_165_099_545;

    let g = decode_granule(&granule_path()).expect("decode");
    // The fixture's flash[0] has the smallest flash_id seen first in file order;
    // decode preserves file order, so flashes[0] is the granule's first flash.
    let f0 = g.flashes.first().expect("at least one flash");
    assert_eq!(
        f0.time_unix_ms, EXPECTED_FLASH0_MS,
        "flash0 absolute time mismatch (product_unix_ms={PRODUCT_UNIX_MS})"
    );
    // Sanity: flash0 sits within ~5 s of the granule reference instant.
    assert!(
        (f0.time_unix_ms - PRODUCT_UNIX_MS).abs() < 5_000,
        "flash0 too far from product_time: {} vs {PRODUCT_UNIX_MS}",
        f0.time_unix_ms
    );
}

#[test]
fn duration_saturation_is_unit_tested_with_synthetic_values() {
    // (last - first) ms -> u16, saturating. Independent of the granule.
    assert_eq!(saturate_duration_ms(34), 34);
    assert_eq!(saturate_duration_ms(0), 0);
    assert_eq!(saturate_duration_ms(65_535), 65_535);
    assert_eq!(
        saturate_duration_ms(65_536),
        65_535,
        "must saturate, not wrap"
    );
    assert_eq!(saturate_duration_ms(1_000_000), 65_535);
    assert_eq!(saturate_duration_ms(-1), 0, "last-before-first clamps to 0");

    // And the real granule's flashes all produce in-range durations.
    let g = decode_granule(&granule_path()).expect("decode");
    let max_dur = g.flashes.iter().map(|f| f.duration_ms).max().unwrap_or(0);
    // GLM flashes are sub-second; durations comfortably below the u16 ceiling.
    assert!(
        max_dur < 65_535,
        "no real GLM flash should saturate duration; max was {max_dur}"
    );
}

#[test]
fn degraded_bit_set_iff_quality_nonzero() {
    let g = decode_granule(&granule_path()).expect("decode");
    let degraded = g.flashes.iter().filter(|f| f.is_degraded()).count();
    assert_eq!(
        degraded, EXPECTED_DEGRADED,
        "granule has {EXPECTED_DEGRADED} flashes with quality != 0"
    );
    // Every degraded flash carries exactly bit 0; non-degraded carry no flags.
    for f in &g.flashes {
        if f.is_degraded() {
            assert_eq!(f.flags, 1, "degraded flash must carry exactly bit0");
        } else {
            assert_eq!(f.flags, 0, "good flash must carry no flags");
        }
    }
}

#[test]
fn areas_are_finite_positive_km2() {
    let g = decode_granule(&granule_path()).expect("decode");
    for f in &g.flashes {
        assert!(
            f.area.is_finite() && f.area > 0.0,
            "area {} km2 not finite & positive for flash {}",
            f.area,
            f.flash_id
        );
        // m² -> km²: a single GLM flash footprint is well under 1e6 km².
        assert!(
            f.area < 1.0e6,
            "area {} km2 implausibly large for flash {}",
            f.area,
            f.flash_id
        );
    }
}

#[test]
fn non_glm_netcdf_is_a_clean_error_not_a_panic() {
    // A garbage / non-NetCDF file must decode to an Err, never panic.
    let tmp = std::env::temp_dir().join(format!("rw-glm-bad-granule-{}.nc", std::process::id()));
    std::fs::write(
        &tmp,
        b"this is not a netcdf file, just garbage bytes \x00\x01\x02",
    )
    .unwrap();

    let result = decode_granule(&tmp);
    assert!(
        result.is_err(),
        "garbage file must decode to Err, got {result:?}"
    );
    // The error is a Format error mentioning the file (no I/O variant: the file
    // opened fine, it just is not a valid granule).
    match result {
        Err(rw_glm::RwlError::Format(msg)) => {
            assert!(
                msg.contains("NetCDF") || msg.contains("granule") || msg.contains(DIM_MARKER),
                "unexpected format message: {msg}"
            );
        }
        other => panic!("expected Format error, got {other:?}"),
    }

    let _ = std::fs::remove_file(&tmp);
}

/// Substring that a missing-dimension error would carry, used only by the
/// non-GLM test's message assertion.
const DIM_MARKER: &str = "number_of_flashes";
