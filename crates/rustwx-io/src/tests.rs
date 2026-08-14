use super::*;
use grib_core::grib2::{DataRepresentation, GridDefinition, ProductDefinition};
use rustwx_core::CycleSpec;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const SAMPLE_IDX: &str = "\
1:0:d=2026041420:TMP:2 m above ground:anl:
2:47843:d=2026041420:SPFH:2 m above ground:anl:
3:96542:d=2026041420:CAPE:surface:anl:
4:143210:d=2026041420:UGRD:10 m above ground:anl:
5:200000:d=2026041420:VGRD:10 m above ground:anl:
";

const AIFS_INDEX_SAMPLE: &str = r#"{"domain": "g", "date": "20260810", "time": "0000", "expver": "0001", "class": "ai", "type": "fc", "stream": "oper", "step": "24", "levelist": "925", "levtype": "pl", "param": "q", "model": "aifs-single", "_offset": 2465216, "_length": 647313}
{"domain": "g", "date": "20260810", "time": "0000", "expver": "0001", "class": "ai", "type": "fc", "stream": "oper", "step": "24", "levelist": "250", "levtype": "pl", "param": "t", "model": "aifs-single", "_offset": 3112529, "_length": 488724}
{"domain": "g", "date": "20260810", "time": "0000", "expver": "0001", "class": "ai", "type": "fc", "stream": "oper", "step": "24", "levtype": "sfc", "param": "2t", "model": "aifs-single", "_offset": 82676279, "_length": 551319}
"#;

const DWD_ICON_REGULAR_LATLON_INVENTORY: &str =
    include_str!("../tests/fixtures/dwd-icon-regular-latlon-20260814.inventory.txt");

fn dwd_inventory_rows(kind: &str) -> Vec<Vec<&'static str>> {
    DWD_ICON_REGULAR_LATLON_INVENTORY
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split('|').collect::<Vec<_>>())
        .filter(|row| row.first().copied() == Some(kind))
        .collect()
}

#[test]
fn dwd_regular_latlon_fixture_pins_schedule_and_canonical_object_inventory() {
    let all_rows = DWD_ICON_REGULAR_LATLON_INVENTORY
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split('|').collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert!(all_rows.iter().all(|row| row.len() == 8));

    let schedules = dwd_inventory_rows("SCHEDULE");
    assert_eq!(schedules.len(), 3);
    assert!(schedules.iter().any(|row| {
        row[1] == "icon-eu"
            && row[2] == "main-00-06-12-18"
            && row[4] == "93"
            && row[5] == "f000-f078 hourly; f081-f120 three-hourly"
    }));
    assert!(schedules.iter().any(|row| {
        row[1] == "icon-eu"
            && row[2] == "short-03-09-15-21"
            && row[4] == "34"
            && row[5] == "f000-f030 hourly; f036/f042/f048"
    }));
    assert!(
        schedules
            .iter()
            .any(|row| { row[1] == "icon-d2" && row[4] == "49" && row[5] == "f000-f048 hourly" })
    );

    let surfaces = dwd_inventory_rows("SURFACE");
    let expected_surface_keys = [
        "t_2m",
        "td_2m",
        "relhum_2m",
        "u_10m",
        "v_10m",
        "pmsl",
        "ps",
        "hsurf",
        "tot_prec",
    ];
    for model in ["icon-eu", "icon-d2"] {
        let model_rows = surfaces
            .iter()
            .filter(|row| row[1] == model)
            .collect::<Vec<_>>();
        assert_eq!(model_rows.len(), expected_surface_keys.len());
        for key in expected_surface_keys {
            let row = model_rows
                .iter()
                .find(|row| row[2] == key)
                .unwrap_or_else(|| panic!("missing {model} surface object {key}"));
            assert!(row[3].ends_with(".grib2.bz2"));
        }
    }

    let pressure = dwd_inventory_rows("PRESSURE");
    for model in ["icon-eu", "icon-d2"] {
        let keys = pressure
            .iter()
            .filter(|row| row[1] == model)
            .map(|row| row[2])
            .collect::<Vec<_>>();
        assert_eq!(keys, ["t", "relhum", "u", "v", "fi"]);
    }
    assert_eq!(
        pressure
            .iter()
            .find(|row| row[1] == "icon-eu" && row[2] == "t")
            .unwrap()[5],
        "50,70,100,150,200,250,300,400,500,600,700,775,800,825,850,875,900,925,950,1000"
    );
    assert_eq!(
        pressure
            .iter()
            .find(|row| row[1] == "icon-d2" && row[2] == "t")
            .unwrap()[5],
        "200,250,300,400,500,600,700,850,950,975,1000"
    );
}

#[test]
fn dwd_regular_latlon_fixture_pins_bounded_live_payload_evidence() {
    let payloads = dwd_inventory_rows("PAYLOAD");
    assert_eq!(payloads.len(), 30);
    for row in payloads {
        assert!(row[3].starts_with("https://opendata.dwd.de/weather/nwp/"));
        assert!(row[3].ends_with(".grib2.bz2"));
        assert!(row[4].parse::<usize>().is_ok_and(|bytes| bytes > 0));
        assert_eq!(row[5].len(), 64);
        assert!(row[5].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(row[6].contains("decoded="));
        assert!(row[6].contains("packing="));
    }
    assert!(
        DWD_ICON_REGULAR_LATLON_INVENTORY.contains(
            "windows=0-60/75/90/105min|f001 selector should choose the first message only"
        )
    );
}

#[test]
fn gzip_grib_payloads_are_decompressed_before_decode() {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"GRIBtest").unwrap();
    let compressed = encoder.finish().unwrap();

    let decoded = maybe_decompress_grib_payload(
        "https://example.invalid/MRMS_ReflectivityAtLowestAltitude.latest.grib2.gz",
        compressed,
    )
    .expect("gzip payload decodes");

    assert_eq!(decoded, b"GRIBtest");
}

#[test]
fn bzip2_grib_payloads_are_decompressed_before_decode() {
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::best());
    encoder.write_all(b"GRIBtest").unwrap();
    let compressed = encoder.finish().unwrap();

    let decoded = maybe_decompress_grib_payload(
        "https://opendata.dwd.de/weather/nwp/icon-eu/grib/00/t_2m/icon-eu.grib2.bz2",
        compressed,
    )
    .expect("bzip2 payload decodes");

    assert_eq!(decoded, b"GRIBtest");
}

#[test]
fn bzip2_magic_decodes_even_without_a_filename_suffix() {
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::best());
    encoder.write_all(b"GRIBmagic").unwrap();
    let compressed = encoder.finish().unwrap();

    let decoded = maybe_decompress_grib_payload(
        "https://example.invalid/redirected-provider-object",
        compressed,
    )
    .expect("bzip2 magic decodes");

    assert_eq!(decoded, b"GRIBmagic");
}

#[test]
fn download_oom_guard_rejects_gzip_output_past_limit() {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&[0_u8; 256]).unwrap();
    let compressed = encoder.finish().unwrap();

    let error = decompress_gzip_payload_with_limit(
        "https://example.invalid/decompression-bomb.grib2.gz",
        &compressed,
        64,
    )
    .expect_err("expanded payload must be bounded");
    assert!(error.contains("exceeds the 64 byte limit"));
}

#[test]
fn download_oom_guard_rejects_bzip2_output_past_limit() {
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::best());
    encoder.write_all(&[0_u8; 256]).unwrap();
    let compressed = encoder.finish().unwrap();

    let error = decompress_bzip2_payload_with_limit(
        "https://example.invalid/decompression-bomb.grib2.bz2",
        &compressed,
        64,
    )
    .expect_err("expanded payload must be bounded");
    assert!(error.contains("exceeds the 64 byte limit"));
}

#[test]
fn plain_grib_payloads_are_left_unchanged() {
    let decoded = maybe_decompress_grib_payload(
        "https://example.invalid/hrrr.t00z.wrfsfcf006.grib2",
        b"GRIBtest".to_vec(),
    )
    .expect("plain payload passes through");

    assert_eq!(decoded, b"GRIBtest");
}

#[test]
fn wmo_geopotential_parameter_normalizes_to_canonical_height() {
    let geopotential = ieee_f32_message(
        ParameterCode {
            discipline: 0,
            category: 3,
            number: 4,
        },
        100,
        50_000.0,
        &[9_806.65, 49_033.25],
        -99.0,
        -98.0,
    );
    let height = extract_field_from_grib2(
        &Grib2File {
            messages: vec![geopotential],
        },
        FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 500),
    )
    .expect("WMO geopotential maps to canonical height");

    assert_eq!(height.units, "gpm");
    assert!((height.values[0] - 1_000.0).abs() < 1.0e-3);
    assert!((height.values[1] - 5_000.0).abs() < 1.0e-3);
}

#[test]
fn wmo_surface_geometric_height_maps_to_canonical_orography_without_conversion() {
    let geometric_height = ieee_f32_message(
        ParameterCode {
            discipline: 0,
            category: 3,
            number: 6,
        },
        1,
        0.0,
        &[0.0, 326.5, 2_962.25],
        -99.0,
        -98.0,
    );
    let orography = extract_field_from_grib2(
        &Grib2File {
            messages: vec![geometric_height],
        },
        FieldSelector::surface(CanonicalField::GeopotentialHeight),
    )
    .expect("WMO geometric surface height maps to canonical orography");

    assert_eq!(orography.units, "gpm");
    assert_eq!(orography.values, vec![0.0, 326.5, 2_962.25]);
}

#[test]
fn cycle_static_surface_height_is_reused_at_later_forecast_hours() {
    let geometric_height = ieee_f32_message(
        ParameterCode {
            discipline: 0,
            category: 3,
            number: 6,
        },
        1,
        0.0,
        &[0.0, 326.5, 2_962.25],
        -99.0,
        -98.0,
    );
    let selector = FieldSelector::surface(CanonicalField::GeopotentialHeight);
    let partial = extract_fields_from_grib2_partial_at_forecast_hour(
        &Grib2File {
            messages: vec![geometric_height],
        },
        &[selector],
        2,
    )
    .expect("time-invariant orography should be reusable throughout its cycle");

    assert!(partial.missing.is_empty());
    assert_eq!(partial.extracted[0].values, vec![0.0, 326.5, 2_962.25]);
}

#[test]
fn mrms_latest_product_url_rejects_pathlike_tokens() {
    assert_eq!(
        mrms_latest_product_url("ReflectivityAtLowestAltitude").unwrap(),
        "https://mrms.ncep.noaa.gov/2D/ReflectivityAtLowestAltitude/MRMS_ReflectivityAtLowestAltitude.latest.grib2.gz"
    );
    assert!(mrms_latest_product_url("../ReflectivityAtLowestAltitude").is_err());
}

#[test]
fn mrms_lowest_altitude_reflectivity_maps_to_500m_msl_selector() {
    let message = ieee_f32_message(
        ParameterCode {
            discipline: 209,
            category: 3,
            number: 57,
        },
        102,
        500.0,
        &[10.0, 20.0],
        -130.0,
        -129.0,
    );
    let grib = Grib2File {
        messages: vec![message],
    };

    let field = extract_field_from_grib2(
        &grib,
        FieldSelector::altitude_msl(CanonicalField::RadarReflectivity, 500),
    )
    .expect("MRMS lowest-altitude reflectivity selector matches");

    assert_eq!(field.values, vec![10.0, 20.0]);
}

#[test]
fn mrms_composite_reflectivity_maps_to_composite_selector() {
    let message = ieee_f32_message(
        ParameterCode {
            discipline: 209,
            category: 10,
            number: 0,
        },
        102,
        500.0,
        &[30.0, 40.0],
        -130.0,
        -129.0,
    );
    let grib = Grib2File {
        messages: vec![message],
    };

    let field = extract_field_from_grib2(
        &grib,
        FieldSelector::altitude_msl(CanonicalField::CompositeReflectivity, 500),
    )
    .expect("MRMS composite reflectivity selector matches");

    assert_eq!(field.values, vec![30.0, 40.0]);
}

#[test]
fn eumetnet_opera_dbzh_coverage_url_encodes_datetime_range() {
    let url = eumetnet_opera_dbzh_coverage_url("2026-06-27T05:00Z/2026-06-27T05:30Z")
        .expect("range encodes");
    assert!(url.contains("datetime=2026-06-27T05%3A00Z%2F2026-06-27T05%3A30Z"));
    assert!(url.contains("standard_name=DBZH"));
    assert!(eumetnet_opera_dbzh_coverage_url("2026-06-27T05:00Z\\bad").is_err());
}

/// A real EUMETNET OPERA coverage document, trimmed to two download links.
///
/// The `metocean:radar_meta` block is the published composite's own statement
/// about itself: its `projdef` and the four corner coordinates it declares.
/// Vendored here so the georeference proof below has the frame's own oracle to
/// check against without reaching the network.
const OPERA_COVERAGE_JSON: &[u8] = br#"{
        "type": "Coverage",
        "links": [
            {"href":"https://eumetnet.eu/","type":"text/html","title":"Website"},
            {"href":"https://s3.waw3-1.cloudferro.com/openradar-24h/2026/06/27/OPERA/COMP/OPERA@20260627T0530@0@DBZH.h5","rel":"items","type":"application/x-odim","title":"Data download link.","length":1940764},
            {"href":"https://s3.waw3-1.cloudferro.com/openradar-24h/2026/06/27/OPERA/COMP/OPERA@20260627T0535@0@DBZH.h5","rel":"items","type":"application/x-odim","title":"Data download link.","length":1953411}
        ],
        "metocean:radar_meta": {
            "projdef":"+proj=laea +lat_0=55.0 +lon_0=10.0 +x_0=1950000.0 +y_0=-2100000.0 +units=m +ellps=WGS84",
            "xsize":3800,
            "ysize":4400,
            "xscale":1000.0,
            "yscale":1000.0,
            "LL_lon":-10.4345768386404,
            "LL_lat":31.7462153182675,
            "UL_lon":-39.5357864125034,
            "UL_lat":67.0228327624372,
            "UR_lon":57.8119647501499,
            "UR_lat":67.6210371071631,
            "LR_lon":29.421038635578,
            "LR_lat":31.987650276733
        }
    }"#;

#[test]
fn eumetnet_opera_coverage_json_extracts_odim_links_and_meta() {
    let coverage =
        parse_eumetnet_opera_dbzh_coverage_json(OPERA_COVERAGE_JSON).expect("coverage parses");

    assert_eq!(coverage.download_links.len(), 2);
    assert_eq!(
        coverage.latest_odim_link().map(|link| link.length),
        Some(Some(1_953_411))
    );
    let meta = coverage.radar_meta.expect("radar metadata exists");
    assert_eq!(meta.xsize, 3800);
    assert_eq!(meta.ysize, 4400);
    assert_eq!(meta.xscale_m, 1000.0);
}

fn opera_frame_meta() -> OperaRadarMeta {
    parse_eumetnet_opera_dbzh_coverage_json(OPERA_COVERAGE_JSON)
        .expect("coverage parses")
        .radar_meta
        .expect("radar metadata exists")
}

/// The frame-corner self-proof: the grid this module derives must land on the
/// corners the frame itself declares.
///
/// This replaces a check that allowed 0.25 deg of longitude slack, which was
/// wide enough to pass while the inversion was spherical — the very defect
/// being fixed. The frame states its own corners, so the tolerance does not
/// have to be guessed at: the ellipsoidal inversion reproduces them to ~6e-14
/// deg, and pyproj independently agrees to the same order.
#[test]
fn opera_laea_grid_reproduces_the_frames_own_declared_corners() {
    let meta = opera_frame_meta();
    assert!(
        meta.projdef.contains("+ellps=WGS84"),
        "the frame declares an ellipsoid, which is what makes the spherical inversion wrong: {}",
        meta.projdef
    );
    let projection = opera_laea_projection(&meta).expect("LAEA projection builds");
    let (derived, declared) = opera_laea_corners(&meta, &projection);

    for (name, (got, want)) in OPERA_CORNER_NAMES
        .iter()
        .zip(derived.iter().zip(declared.iter()))
    {
        assert!(
            (got.0 - want.0).abs() < OPERA_CORNER_TOLERANCE_DEG,
            "{name} latitude: derived {} vs declared {}",
            got.0,
            want.0
        );
        assert!(
            (got.1 - want.1).abs() < OPERA_CORNER_TOLERANCE_DEG,
            "{name} longitude: derived {} vs declared {}",
            got.1,
            want.1
        );
    }
    assert!(
        opera_corner_offset_deg(&meta, &projection) < 1.0e-9,
        "the ellipsoidal inversion should reproduce the declared corners far inside the ceiling"
    );
}

/// The corner check is a live screen, not a formality: it refuses a frame
/// whose georeference misses by the amount the spherical inversion missed by.
#[test]
fn opera_grid_refuses_a_frame_whose_declared_corners_disagree() {
    let mut meta = opera_frame_meta();
    // 0.2155 deg is the measured longitude error the spherical inversion put
    // on this frame's upper-left corner: ~9.4 km at 67 N, nine cells on the
    // published 1 km grid.
    meta.ul_lon_deg -= 0.2155;

    let error = opera_laea_latlon_grid(&meta).expect_err("a displaced corner is refused");
    let message = error.to_string();
    assert!(message.contains("misses the corners"), "{message}");
    assert!(message.contains("UL"), "{message}");
}

/// ODIM's two sentinels mean opposite things and must survive decode as
/// different things.
///
/// `nodata` is no radar coverage — unobserved, and NaN. `undetect` is no echo
/// — the network looked and found nothing, which is an observation and the
/// most common true one on a live frame. Mapping both to NaN discarded ~46 %
/// of a measured frame's cells, every one of them a correct negative.
#[test]
fn opera_odim_nodata_and_undetect_decode_to_three_distinct_states() {
    // A synthetic slab carrying the sentinels the measured frame declared,
    // plus a non-finite cell and two real echoes.
    let nodata = -9_999_000.0;
    let undetect = -8_888_000.0;
    let raw = vec![nodata, undetect, 15.0, undetect, f64::NAN, 40.0];

    let (values, classes, counts) = classify_opera_dbzh_slab(
        &raw,
        0.5,
        -32.0,
        Some(nodata),
        Some(undetect),
        OPERA_NO_ECHO_DBZ,
    );

    use OperaCellClass::{Echo, NoCoverage, NoEcho};
    assert_eq!(
        classes,
        vec![NoCoverage, NoEcho, Echo, NoEcho, NoCoverage, Echo]
    );
    assert_eq!(counts, [2, 2, 2], "[no_coverage, no_echo, echo]");

    // NaN now means exactly one thing: no radar covered the cell.
    assert!(values[0].is_nan());
    assert!(values[4].is_nan());
    assert_eq!(
        values.iter().filter(|value| value.is_nan()).count(),
        2,
        "collapsing undetect into nodata would make this four"
    );

    // The no-echo cells survive as a finite, scorable clear-air value.
    assert_eq!(values[1], OPERA_NO_ECHO_DBZ);
    assert_eq!(values[3], OPERA_NO_ECHO_DBZ);
    assert!(values[1].is_finite());

    // Only the measurements are calibrated by gain and offset.
    assert_eq!(values[2], -24.5);
    assert_eq!(values[5], -12.0);

    // And the three states are genuinely distinguishable from the values
    // alone as well as from the classes.
    assert!(OPERA_NO_ECHO_DBZ < values[2] && OPERA_NO_ECHO_DBZ < values[5]);
}

/// A frame that declares no `undetect` has no no-echo cells to keep apart, so
/// nothing is collapsed and nothing is refused. `nodata` still wins a tie, so
/// a degenerate frame reads as unobserved rather than as a fabricated
/// observation.
#[test]
fn opera_sentinel_classification_handles_absent_and_colliding_sentinels() {
    let (_, classes, counts) = classify_opera_dbzh_slab(
        &[-9_999_000.0, 12.0],
        1.0,
        0.0,
        Some(-9_999_000.0),
        None,
        OPERA_NO_ECHO_DBZ,
    );
    assert_eq!(
        classes,
        vec![OperaCellClass::NoCoverage, OperaCellClass::Echo]
    );
    assert_eq!(counts, [1, 0, 1]);

    let (values, classes, _) = classify_opera_dbzh_slab(
        &[-9_999_000.0],
        1.0,
        0.0,
        Some(-9_999_000.0),
        Some(-9_999_000.0),
        OPERA_NO_ECHO_DBZ,
    );
    assert_eq!(classes, vec![OperaCellClass::NoCoverage]);
    assert!(values[0].is_nan());
}

#[test]
#[ignore = "network smoke test against live MRMS realtime feed"]
fn live_mrms_latest_reflectivity_gzip_parses_grib2() {
    let url = "https://mrms.ncep.noaa.gov/2D/ReflectivityAtLowestAltitude/MRMS_ReflectivityAtLowestAltitude.latest.grib2.gz";
    let compressed = client()
        .expect("download client")
        .get_bytes(url)
        .expect("MRMS latest reflectivity downloads");
    let bytes = maybe_decompress_grib_payload(url, compressed).expect("MRMS gzip decompresses");
    let grib = Grib2File::from_bytes(&bytes).expect("MRMS payload parses as GRIB2");
    assert_eq!(grib.messages.len(), 1, "MRMS reflectivity is one field");
    let message = &grib.messages[0];
    eprintln!(
        "MRMS latest: discipline={} category={} parameter={} level_type={} level_value={} grid={}x{} template={}",
        message.discipline,
        message.product.parameter_category,
        message.product.parameter_number,
        message.product.level_type,
        message.product.level_value,
        message.grid.nx,
        message.grid.ny,
        message.grid.template
    );

    assert_eq!(message.discipline, 209);
    assert_eq!(message.product.parameter_category, 3);
    assert_eq!(message.product.parameter_number, 57);
}

#[test]
#[ignore = "network smoke test against live MRMS realtime feed"]
fn live_mrms_latest_reflectivity_helpers_extract_fields() {
    let lowest = retry_live(|| extract_mrms_latest_reflectivity_at_lowest_altitude())
        .expect("MRMS lowest-altitude reflectivity extracts");
    eprintln!(
        "MRMS lowest-altitude: {} values on {}x{}",
        lowest.values.len(),
        lowest.grid.shape.nx,
        lowest.grid.shape.ny
    );
    assert_eq!(
        lowest.selector,
        FieldSelector::altitude_msl(CanonicalField::RadarReflectivity, 500)
    );

    let composite_bytes = retry_live(|| fetch_mrms_latest_product("MergedReflectivityQCComposite"))
        .expect("MRMS composite reflectivity downloads");
    let composite_grib =
        Grib2File::from_bytes(&composite_bytes).expect("MRMS composite parses as GRIB2");
    let composite_message = &composite_grib.messages[0];
    eprintln!(
        "MRMS composite metadata: discipline={} category={} parameter={} level_type={} level_value={} grid={}x{} template={}",
        composite_message.discipline,
        composite_message.product.parameter_category,
        composite_message.product.parameter_number,
        composite_message.product.level_type,
        composite_message.product.level_value,
        composite_message.grid.nx,
        composite_message.grid.ny,
        composite_message.grid.template
    );

    let composite = retry_live(|| extract_mrms_latest_composite_reflectivity())
        .expect("MRMS composite reflectivity extracts");
    eprintln!(
        "MRMS composite: {} values on {}x{}",
        composite.values.len(),
        composite.grid.shape.nx,
        composite.grid.shape.ny
    );
    assert_eq!(
        composite.selector,
        FieldSelector::altitude_msl(CanonicalField::CompositeReflectivity, 500)
    );
}

#[test]
#[ignore = "network smoke test against live EUMETNET OPERA ODIM HDF5 feed"]
fn live_eumetnet_opera_latest_dbzh_helpers_extract_field() {
    let now = chrono::Utc::now();
    let start = now - chrono::Duration::minutes(45);
    let end = now - chrono::Duration::minutes(5);
    let range = format!(
        "{}/{}",
        start.format("%Y-%m-%dT%H:%MZ"),
        end.format("%Y-%m-%dT%H:%MZ")
    );
    let coverage =
        retry_live(|| fetch_eumetnet_opera_dbzh_coverage(&range)).expect("OPERA coverage resolves");
    let link = coverage
        .latest_odim_link()
        .expect("OPERA coverage has ODIM HDF5 link");
    eprintln!("OPERA latest link: {}", link.href);
    let bytes =
        retry_live(|| fetch_eumetnet_opera_odim_h5(&link.href)).expect("OPERA HDF5 downloads");
    let decoded =
        extract_eumetnet_opera_dbzh_classified_from_odim_h5(&bytes).expect("OPERA HDF5 extracts");
    let field = &decoded.field;
    eprintln!(
        "OPERA DBZH: {} values on {}x{}; no_coverage={} no_echo={} echo={} observed={:.3}",
        field.values.len(),
        field.grid.shape.nx,
        field.grid.shape.ny,
        decoded.no_coverage_cells,
        decoded.no_echo_cells,
        decoded.echo_cells,
        decoded.observed_fraction()
    );
    assert_eq!(
        field.selector,
        FieldSelector::entire_atmosphere(CanonicalField::CompositeReflectivity)
    );
    assert_eq!(field.units, "dBZ");
    assert_eq!(field.grid.shape.nx, 3800);
    assert_eq!(field.grid.shape.ny, 4400);
    assert!(field.values.iter().any(|value| value.is_finite()));

    // The whole point of the sentinel fix: a live frame carries a large body
    // of clear-air negatives, and they must arrive as observations rather than
    // as holes. The measured frame ran 46.2 % undetect against 49.7 % nodata.
    assert!(
        decoded.no_echo_cells > 0,
        "a continental composite with no clear-air cell at all means the sentinels collapsed again"
    );
    assert!(decoded.no_coverage_cells > 0);
    assert_eq!(
        decoded.classes.len(),
        field.values.len(),
        "one class per cell, in the same order"
    );
}

fn retry_live<T>(mut f: impl FnMut() -> Result<T, IoError>) -> Result<T, IoError> {
    let mut last = None;
    for attempt in 1..=3 {
        match f() {
            Ok(value) => return Ok(value),
            Err(err) => {
                eprintln!("live attempt {attempt} failed: {err}");
                last = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
    Err(last.expect("retry loop ran at least once"))
}

#[test]
fn alternating_i_scan_rows_are_normalized_to_row_major_order() {
    let mut values = vec![
        1.0, 2.0, 3.0, 4.0, //
        8.0, 7.0, 6.0, 5.0, //
        9.0, 10.0, 11.0, 12.0,
    ];

    normalize_alternating_i_scan_rows(&mut values, 4, 3, 0x50);

    assert_eq!(
        values,
        vec![
            1.0, 2.0, 3.0, 4.0, //
            5.0, 6.0, 7.0, 8.0, //
            9.0, 10.0, 11.0, 12.0,
        ]
    );
}

#[test]
fn plain_i_scan_rows_are_left_unchanged() {
    let mut values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];

    normalize_alternating_i_scan_rows(&mut values, 3, 2, 0x40);

    assert_eq!(values, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
}

fn ieee_f32_message(
    parameter: ParameterCode,
    level_type: u8,
    level_value: f64,
    values: &[f32],
    lon1: f64,
    lon2: f64,
) -> Grib2Message {
    let raw_data = values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect::<Vec<_>>();
    Grib2Message {
        discipline: parameter.discipline,
        reference_time: chrono::NaiveDate::from_ymd_opt(2026, 4, 14)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
        grid: GridDefinition {
            template: 0,
            nx: values.len() as u32,
            ny: 1,
            lat1: 35.0,
            lon1,
            lat2: 35.0,
            lon2,
            dx: 1.0,
            dy: 0.0,
            scan_mode: 0,
            num_data_points: values.len() as u32,
            ..Default::default()
        },
        product: ProductDefinition {
            template: 0,
            parameter_category: parameter.category,
            parameter_number: parameter.number,
            level_type,
            level_value,
            ..Default::default()
        },
        data_rep: DataRepresentation {
            template: 4,
            bits_per_value: 32,
            section5_num_data_points: values.len() as u32,
            ..Default::default()
        },
        bitmap: None,
        raw_data,
    }
}

#[test]
fn parsed_model_grib_repeated_passes_match_fresh_parse_results() {
    use wx_core::grib2::{
        Grib2Writer, GridDefinition as WxGridDefinition, MessageBuilder, PackingMethod,
        ProductDefinition as WxProductDefinition,
    };

    let grid = WxGridDefinition {
        template: 0,
        nx: 2,
        ny: 2,
        lat1: 40.0,
        lon1: -105.0,
        lat2: 39.0,
        lon2: -104.0,
        dx: 1.0,
        dy: 1.0,
        scan_mode: 0,
        ..WxGridDefinition::default()
    };
    let message = |category, number, values| {
        MessageBuilder::new(0, values)
            .grid(grid.clone())
            .product(WxProductDefinition {
                template: 0,
                parameter_category: category,
                parameter_number: number,
                generating_process: 2,
                forecast_time: 6,
                time_range_unit: 1,
                level_type: 100,
                level_value: 50_000.0,
            })
            .packing(PackingMethod::Simple { bits_per_value: 16 })
    };
    let bytes = Grib2Writer::new()
        .add_message(message(0, 0, vec![250.0, 251.0, 252.0, 253.0]))
        .add_message(message(1, 1, vec![40.0, 50.0, 60.0, 70.0]))
        .to_bytes()
        .unwrap();
    let primary = [
        FieldSelector::isobaric(CanonicalField::Temperature, 500),
        FieldSelector::isobaric(CanonicalField::Temperature, 700),
    ];
    let alternate = [FieldSelector::isobaric(
        CanonicalField::RelativeHumidity,
        500,
    )];

    let fresh_primary = extract_field_values_partial_from_model_bytes_at_forecast_hour(
        ModelId::Hrrr,
        &bytes,
        None,
        &primary,
        Some(6),
    )
    .unwrap();
    let fresh_alternate = extract_field_values_partial_from_model_bytes_at_forecast_hour(
        ModelId::Hrrr,
        &bytes,
        None,
        &alternate,
        Some(6),
    )
    .unwrap();
    let parsed = ParsedModelGrib::from_model_bytes(ModelId::Hrrr, &bytes).unwrap();
    let reused_primary = parsed
        .extract_field_values_partial_at_forecast_hour(&primary, Some(6))
        .unwrap();
    let reused_alternate = parsed
        .extract_field_values_partial_at_forecast_hour(&alternate, Some(6))
        .unwrap();
    assert_eq!(
        parsed
            .matching_native_field_selectors_at_forecast_hour(&primary, Some(6))
            .unwrap(),
        vec![primary[0]],
        "inventory probing reports the native 500 hPa field without decoding it"
    );
    assert_eq!(
        parsed
            .matching_native_field_selectors_at_forecast_hour(&alternate, Some(6))
            .unwrap(),
        alternate
    );

    let assert_same = |actual: &PartialValuesExtraction, expected: &PartialValuesExtraction| {
        assert_eq!(actual.missing, expected.missing);
        assert_eq!(actual.grids.len(), expected.grids.len());
        for (actual, expected) in actual.grids.iter().zip(&expected.grids) {
            assert_eq!(actual.grid, expected.grid);
            assert_eq!(actual.projection, expected.projection);
        }
        assert_eq!(actual.extracted.len(), expected.extracted.len());
        for (actual, expected) in actual.extracted.iter().zip(&expected.extracted) {
            assert_eq!(actual.selector, expected.selector);
            assert_eq!(actual.units, expected.units);
            assert_eq!(actual.grid_index, expected.grid_index);
            assert_eq!(
                actual
                    .values
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                expected
                    .values
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>()
            );
        }
    };
    assert_same(&reused_primary, &fresh_primary);
    assert_same(&reused_alternate, &fresh_alternate);
}

fn regional_wind_message(parameter_number: u8, values: &[f32]) -> Grib2Message {
    let mut message = ieee_f32_message(
        ParameterCode {
            discipline: 0,
            category: 2,
            number: parameter_number,
        },
        103,
        10.0,
        values,
        0.0,
        (values.len() - 1) as f64,
    );
    message.grid.template = 1;
    message.grid.dx = 1.0;
    message.grid.south_pole_lat = -90.0;
    message.grid.south_pole_lon = 0.0;
    message.grid.resolution_flags = 0x38;
    message
}

#[test]
fn regional_grid_relative_winds_are_paired_and_rotated_before_publication() {
    let u_selector = FieldSelector::height_agl(CanonicalField::UWind, 10);
    let v_selector = FieldSelector::height_agl(CanonicalField::VWind, 10);
    let grib = Grib2File {
        messages: vec![
            regional_wind_message(2, &[3.0, 4.0, 5.0]),
            regional_wind_message(3, &[4.0, 3.0, 0.0]),
        ],
    };
    let parsed = ParsedModelGrib {
        model: ModelId::Rdps,
        grib,
    };

    let extracted = parsed
        .extract_field_values_partial_at_forecast_hour(&[u_selector, v_selector], None)
        .expect("identity rotated grid has a safe paired wind transform");
    assert!(extracted.missing.is_empty());
    for (actual, expected) in extracted.extracted[0]
        .values
        .iter()
        .zip([3.0_f32, 4.0, 5.0])
    {
        assert!((actual - expected).abs() < 0.03);
    }
    for ((u, v), expected_speed) in extracted.extracted[0]
        .values
        .iter()
        .zip(&extracted.extracted[1].values)
        .zip([5.0_f32, 5.0, 5.0])
    {
        assert!((u.hypot(*v) - expected_speed).abs() < 1.0e-5);
    }

    let error = parsed
        .extract_field_values_partial_at_forecast_hour(&[u_selector], None)
        .expect_err("a native grid-relative U component cannot escape without V");
    assert!(error.to_string().contains("matching 'v_wind@10m_agl'"));
}

#[test]
fn regional_wind_normalization_fails_closed_on_metadata_or_grid_drift() {
    let selectors = [
        FieldSelector::height_agl(CanonicalField::UWind, 10),
        FieldSelector::height_agl(CanonicalField::VWind, 10),
    ];

    let mut no_component_flag = regional_wind_message(2, &[1.0, 1.0]);
    no_component_flag.grid.resolution_flags = 0x30;
    let parsed = ParsedModelGrib {
        model: ModelId::Hrdps,
        grib: Grib2File {
            messages: vec![no_component_flag, regional_wind_message(3, &[1.0, 1.0])],
        },
    };
    assert!(
        parsed
            .extract_field_values_partial_at_forecast_hour(&selectors, None)
            .unwrap_err()
            .to_string()
            .contains("does not declare grid-relative")
    );

    let mut mismatched_v = regional_wind_message(3, &[1.0, 1.0]);
    mismatched_v.grid.south_pole_lon = 1.0;
    let parsed = ParsedModelGrib {
        model: ModelId::Hrdps,
        grib: Grib2File {
            messages: vec![regional_wind_message(2, &[1.0, 1.0]), mismatched_v],
        },
    };
    assert!(
        parsed
            .extract_field_values_partial_at_forecast_hour(&selectors, None)
            .unwrap_err()
            .to_string()
            .contains("different native grids")
    );
}

#[test]
fn normalized_grid_is_the_wind_rotation_authority_and_preserves_magnitude() {
    let grid = LatLonGrid::new(
        GridShape::new(3, 1).unwrap(),
        vec![0.0, 1.0, 2.0],
        vec![0.0, 1.0, 2.0],
    )
    .unwrap();
    let coefficients = grid_i_to_earth_rotation_coefficients(ModelId::Rdps, &grid).unwrap();
    let mut u = vec![10.0; 3];
    let mut v = vec![0.0; 3];
    rotate_grid_relative_wind_pair(
        ModelId::Rdps,
        FieldSelector::height_agl(CanonicalField::UWind, 10),
        &mut u,
        &mut v,
        &coefficients,
    )
    .unwrap();

    assert!(u[1] > 6.9 && u[1] < 7.2, "east component was {}", u[1]);
    assert!(v[1] > 6.9 && v[1] < 7.2, "north component was {}", v[1]);
    for (&earth_u, &earth_v) in u.iter().zip(&v) {
        assert!((earth_u.hypot(earth_v) - 10.0).abs() < 1.0e-5);
    }
}

#[test]
fn regional_wind_tangent_ignores_the_artificial_noncyclic_dateline_seam() {
    // A reduced version of the exact RDPS row where per-row longitude
    // normalization moves the eastern piece ahead of the western piece.
    // The middle adjacency is between the original regional row endpoints,
    // not a physical grid-i step.
    let grid = LatLonGrid::new(
        GridShape::new(4, 1).unwrap(),
        vec![45.231285, 45.154537, 41.015793, 41.09256],
        vec![-4.017726, -3.968054, 176.58026, 176.62663],
    )
    .unwrap();
    let coefficients = grid_i_to_earth_rotation_coefficients(ModelId::Rdps, &grid).unwrap();

    assert!(coefficients[1].0 > 0.3 && coefficients[1].1 < -0.8);
    assert!(coefficients[2].0 > 0.3 && coefficients[2].1 > 0.8);
    assert!(
        coefficients
            .iter()
            .all(|(east, north)| { (east.hypot(*north) - 1.0).abs() < 1.0e-6 })
    );
}

#[test]
#[ignore = "requires the four bounded official ECCC wind objects named in the ingest fixtures"]
fn live_eccc_grid_wind_rotation_matches_provider_speed_and_direction() {
    let fixture_dir = std::env::var("RUSTWX_ECCC_WIND_FIXTURE_DIR")
        .expect("set RUSTWX_ECCC_WIND_FIXTURE_DIR to the bounded fixture directory");
    for (model, prefix) in [(ModelId::Rdps, "rdps"), (ModelId::Hrdps, "hrdps")] {
        let read = |suffix: &str| {
            std::fs::read(PathBuf::from(&fixture_dir).join(format!("{prefix}-{suffix}.grib2")))
                .unwrap_or_else(|error| panic!("read {prefix}-{suffix}: {error}"))
        };
        let mut paired_bytes = read("u10");
        paired_bytes.extend(read("v10"));
        let selectors = [
            FieldSelector::height_agl(CanonicalField::UWind, 10),
            FieldSelector::height_agl(CanonicalField::VWind, 10),
        ];
        let earth = ParsedModelGrib::from_model_bytes(model, &paired_bytes)
            .unwrap()
            .extract_field_values_partial_at_forecast_hour(&selectors, Some(24))
            .unwrap();
        assert!(earth.missing.is_empty(), "{model}");
        assert_eq!(earth.extracted.len(), 2, "{model}");

        let speed_grib = Grib2File::from_bytes(&read("wind")).unwrap();
        let direction_grib = Grib2File::from_bytes(&read("wdir")).unwrap();
        let mut grid_memo = GridMemo::new();
        let speed = build_field_values(
            &speed_grib.messages[0],
            FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
            "m/s",
            &mut grid_memo,
        )
        .unwrap();
        let direction = build_field_values(
            &direction_grib.messages[0],
            FieldSelector::height_agl(CanonicalField::VWind, 10),
            "deg",
            &mut grid_memo,
        )
        .unwrap();
        assert_eq!(speed.grid_index, direction.grid_index, "{model}");
        assert_eq!(
            earth.grids[earth.extracted[0].grid_index].grid,
            grid_memo.slots[speed.grid_index].0.grid,
            "{model} provider reference grid"
        );

        let mut compared = 0_u64;
        let mut squared_error = 0.0_f64;
        let mut max_component_error = 0.0_f32;
        let mut max_error_sample = None;
        let mut cell_errors = Vec::new();
        for (index, (((&earth_u, &earth_v), &speed), &direction_deg)) in earth.extracted[0]
            .values
            .iter()
            .zip(&earth.extracted[1].values)
            .zip(&speed.values)
            .zip(&direction.values)
            .enumerate()
        {
            if !earth_u.is_finite()
                || !earth_v.is_finite()
                || !speed.is_finite()
                || !direction_deg.is_finite()
            {
                continue;
            }
            let direction_rad = direction_deg.to_radians();
            let reference_u = -speed * direction_rad.sin();
            let reference_v = -speed * direction_rad.cos();
            let u_error = (earth_u - reference_u).abs();
            let v_error = (earth_v - reference_v).abs();
            let cell_error = u_error.max(v_error);
            cell_errors.push(cell_error);
            if cell_error > max_component_error {
                max_component_error = cell_error;
                max_error_sample = Some((
                    index,
                    earth_u,
                    earth_v,
                    speed,
                    direction_deg,
                    reference_u,
                    reference_v,
                ));
            }
            squared_error += f64::from(u_error * u_error + v_error * v_error);
            compared += 2;
        }
        let rms_component_error = (squared_error / compared as f64).sqrt();
        cell_errors.sort_by(f32::total_cmp);
        let percentile = |fraction: f64| {
            cell_errors[((cell_errors.len() - 1) as f64 * fraction).round() as usize]
        };
        let (max_index, earth_u, earth_v, speed, direction, reference_u, reference_v) =
            max_error_sample.unwrap();
        let grid = &earth.grids[earth.extracted[0].grid_index].grid;
        eprintln!(
            "{model}: compared {compared} components, RMS error {rms_component_error:.6} m/s, p95/p99/p99.9 {}/{}/{}, max error {max_component_error:.6} m/s at {}x{} ({}, {}): earth=({earth_u},{earth_v}) reference=({reference_u},{reference_v}) speed={speed} direction={direction}",
            percentile(0.95),
            percentile(0.99),
            percentile(0.999),
            max_index % grid.shape.nx,
            max_index / grid.shape.nx,
            grid.lat_deg[max_index],
            grid.lon_deg[max_index],
        );
        assert!(
            compared > 1_000_000,
            "{model}: too few finite reference cells"
        );
        assert!(
            rms_component_error < 0.08,
            "{model}: RMS component mismatch {rms_component_error} m/s"
        );
        assert!(
            max_component_error < 0.25,
            "{model}: max component mismatch {max_component_error} m/s"
        );
    }
}

#[test]
fn component_bundle_is_ordered_source_bound_and_inventory_keyed() {
    use wx_core::grib2::{
        Grib2Writer, GridDefinition as WxGridDefinition, MessageBuilder, PackingMethod,
        ProductDefinition as WxProductDefinition,
    };

    let grid = WxGridDefinition {
        template: 0,
        nx: 2,
        ny: 1,
        lat1: 40.0,
        lon1: -105.0,
        lat2: 40.0,
        lon2: -104.0,
        dx: 1.0,
        dy: 1.0,
        scan_mode: 0,
        ..WxGridDefinition::default()
    };
    let grib = |parameter_number: u8, values: Vec<f64>| {
        Grib2Writer::new()
            .add_message(
                MessageBuilder::new(0, values)
                    .grid(grid.clone())
                    .product(WxProductDefinition {
                        template: 0,
                        parameter_category: 0,
                        parameter_number,
                        generating_process: 2,
                        forecast_time: 0,
                        time_range_unit: 1,
                        level_type: 100,
                        level_value: 50_000.0,
                    })
                    .packing(PackingMethod::Simple { bits_per_value: 16 }),
            )
            .to_bytes()
            .unwrap()
    };
    let first_bytes = grib(0, vec![250.0, 251.0]);
    let second_bytes = grib(1, vec![40.0, 50.0]);
    let first_wmo_bulletin = [
        b"\x01\r\r\n823\r\r\nYTRB50 RUMS 140000\r\r\n".as_slice(),
        first_bytes.as_slice(),
        b"\r\r\n\x03".as_slice(),
    ]
    .concat();

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let cache_root = std::env::temp_dir().join(format!(
        "rustwx-component-bundle-{}-{nonce}",
        std::process::id()
    ));
    let cycle = CycleSpec::new("20260814", 0).unwrap();
    let logical = FetchRequest {
        request: ModelRunRequest::new(ModelId::Gdps, cycle.clone(), 0, "rws-pressure").unwrap(),
        source_override: Some(SourceId::Eccc),
        variable_patterns: Vec::new(),
    };
    let components = [
        ("AirTemp_IsbL-0500", first_wmo_bulletin.clone()),
        ("RelativeHumidity_IsbL-0500", second_bytes.clone()),
    ];
    for (product, bytes) in &components {
        let request = FetchRequest {
            request: ModelRunRequest::new(ModelId::Gdps, cycle.clone(), 0, *product).unwrap(),
            source_override: Some(SourceId::Eccc),
            variable_patterns: Vec::new(),
        };
        store_cached_fetch(
            &cache_root,
            &request,
            &FetchResult {
                source: SourceId::Eccc,
                url: format!("https://dd.weather.gc.ca/{product}.grib2"),
                bytes: bytes.clone(),
            },
        )
        .unwrap();
    }
    let inventory = components
        .iter()
        .map(|(product, _)| (*product).to_string())
        .collect::<Vec<_>>();
    let first = fetch_component_bundle_with_cache(&logical, &inventory, &cache_root, true).unwrap();
    assert!(!first.cache_hit);
    assert_eq!(first.result.source, SourceId::Eccc);
    assert!(first.result.url.starts_with("rws-bundle://eccc/gdps/"));
    assert_eq!(
        first.result.bytes,
        [first_bytes.as_slice(), second_bytes.as_slice()].concat()
    );
    assert_eq!(
        Grib2File::from_bytes(&first.result.bytes)
            .unwrap()
            .messages
            .len(),
        2
    );

    let warm = fetch_component_bundle_with_cache(&logical, &inventory, &cache_root, true).unwrap();
    assert!(warm.cache_hit);
    assert_eq!(warm.result.bytes, first.result.bytes);

    let reversed = inventory.iter().rev().cloned().collect::<Vec<_>>();
    let reordered =
        fetch_component_bundle_with_cache(&logical, &reversed, &cache_root, true).unwrap();
    assert!(
        !reordered.cache_hit,
        "inventory order is part of the cache key"
    );
    assert_eq!(
        reordered.result.bytes,
        [second_bytes.as_slice(), first_bytes.as_slice()].concat()
    );

    let exact_stream = [first_bytes.as_slice(), second_bytes.as_slice()].concat();
    assert_eq!(
        parse_complete_grib2_stream(&exact_stream)
            .unwrap()
            .messages
            .len(),
        2
    );
    for malformed in [
        Vec::new(),
        [b"junk".as_slice(), first_bytes.as_slice()].concat(),
        [first_bytes.as_slice(), b"junk".as_slice()].concat(),
        first_bytes[..first_bytes.len() - 1].to_vec(),
    ] {
        assert!(
            parse_complete_grib2_stream(&malformed).is_err(),
            "component admission must reject non-exact GRIB2 streams"
        );
    }

    assert_eq!(
        grib2_component_payload(&first_wmo_bulletin).unwrap(),
        first_bytes
    );
    for malformed in [
        [b"junk\r\r\n".as_slice(), first_bytes.as_slice()].concat(),
        [
            b"\x01\r\r\n823\r\r\nYTRB50 RUMS 140000\r\r\n".as_slice(),
            first_bytes.as_slice(),
            b"\n\x03".as_slice(),
        ]
        .concat(),
        first_wmo_bulletin[..first_wmo_bulletin.len() - 1].to_vec(),
    ] {
        assert!(
            grib2_component_payload(&malformed).is_err(),
            "only an exact WMO envelope may be stripped"
        );
    }

    let duplicate = vec![inventory[0].clone(), inventory[0].clone()];
    assert!(
        fetch_component_bundle_with_cache(&logical, &duplicate, &cache_root, true)
            .unwrap_err()
            .to_string()
            .contains("duplicate product")
    );
    let _ = std::fs::remove_dir_all(cache_root);
}

#[test]
fn global_specific_humidity_products_synthesize_pressure_level_dewpoint() {
    use wx_core::grib2::{
        Grib2Writer, GridDefinition as WxGridDefinition, MessageBuilder, PackingMethod,
        ProductDefinition as WxProductDefinition,
    };

    let grid = WxGridDefinition {
        template: 0,
        nx: 2,
        ny: 2,
        lat1: 40.0,
        lon1: -105.0,
        lat2: 39.0,
        lon2: -104.0,
        dx: 1.0,
        dy: 1.0,
        scan_mode: 0,
        ..WxGridDefinition::default()
    };
    let q = MessageBuilder::new(0, vec![0.008, 0.009, 0.010, 0.011])
        .grid(grid)
        .product(WxProductDefinition {
            template: 0,
            parameter_category: 1,
            parameter_number: 0,
            generating_process: 2,
            forecast_time: 6,
            time_range_unit: 1,
            level_type: 100,
            level_value: 85_000.0,
        })
        .packing(PackingMethod::Simple { bits_per_value: 24 });
    let bytes = Grib2Writer::new().add_message(q).to_bytes().unwrap();
    let selector = FieldSelector::isobaric(CanonicalField::Dewpoint, 850);

    for model in [
        ModelId::Aigfs,
        ModelId::Aigefs,
        ModelId::Hgefs,
        ModelId::EcmwfOpenData,
        ModelId::Aifs,
    ] {
        let parsed = ParsedModelGrib::from_model_bytes(model, &bytes).unwrap();
        let values = parsed
            .extract_field_values_partial_at_forecast_hour(&[selector], Some(6))
            .unwrap();
        assert!(values.missing.is_empty(), "{model}");
        assert_eq!(values.extracted.len(), 1, "{model}");
        assert_eq!(values.extracted[0].selector, selector);
        assert_eq!(values.extracted[0].units, "K");
        assert!(
            values.extracted[0]
                .values
                .iter()
                .all(|value| value.is_finite() && (275.0..295.0).contains(value)),
            "{model}"
        );

        let fields = extract_fields_partial_from_model_bytes_at_forecast_hour(
            model,
            &bytes,
            None,
            &[selector],
            Some(6),
        )
        .unwrap();
        assert!(fields.missing.is_empty(), "{model}");
        assert_eq!(fields.extracted.len(), 1, "{model}");
        assert_eq!(fields.extracted[0].selector, selector);
        assert_eq!(fields.extracted[0].units, "K");
        assert_eq!(fields.extracted[0].values, values.extracted[0].values);
    }
}

#[test]
fn projection_metadata_is_inferred_from_grib_grid_templates() {
    let lambert = GridDefinition {
        template: 30,
        latin1: 38.5,
        latin2: 38.5,
        lov: 262.5,
        ..Default::default()
    };
    assert_eq!(
        grid_projection_from_grib2_grid(&lambert),
        Some(GridProjection::LambertConformal {
            standard_parallel_1_deg: 38.5,
            standard_parallel_2_deg: 38.5,
            central_meridian_deg: -97.5,
        })
    );

    let polar = GridDefinition {
        template: 20,
        lad: 60.0,
        lov: 210.0,
        projection_center_flag: 1,
        ..Default::default()
    };
    assert_eq!(
        grid_projection_from_grib2_grid(&polar),
        Some(GridProjection::PolarStereographic {
            true_latitude_deg: 60.0,
            central_meridian_deg: -150.0,
            south_pole_on_projection_plane: true,
        })
    );
}

fn sample_pressure_subset_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("proof")
        .join("rustwx_hrrr_20260414_22z_f00_prs_subset.grib2")
}

#[test]
fn candidate_hours_match_model_rules() {
    assert_eq!(candidate_hours(ModelId::Hrrr, 20).last().copied(), Some(18));
    assert_eq!(candidate_hours(ModelId::Hrrr, 18).last().copied(), Some(48));
    assert_eq!(
        candidate_hours(ModelId::RrfsA, 20).last().copied(),
        Some(60)
    );
    // ECMWF open-data 00/12z stream reaches f360; 06/18z stops at f144.
    assert_eq!(
        candidate_hours(ModelId::EcmwfOpenData, 0).last().copied(),
        Some(360)
    );
    assert_eq!(
        candidate_hours(ModelId::EcmwfOpenData, 12).last().copied(),
        Some(360)
    );
    assert_eq!(
        candidate_hours(ModelId::EcmwfOpenData, 6).last().copied(),
        Some(144)
    );
    assert_eq!(
        candidate_hours(ModelId::EcmwfOpenData, 18).last().copied(),
        Some(144)
    );
    let nbm = candidate_hours(ModelId::Nbm, 12);
    assert!(nbm.contains(&36));
    assert!(nbm.contains(&39));
    assert!(nbm.contains(&192));
    assert!(nbm.contains(&198));
    assert_eq!(nbm.last().copied(), Some(264));
    assert!(!nbm.contains(&37));
    assert!(!nbm.contains(&195));
}

#[test]
fn nomads_hour_probes_are_serialized() {
    assert!(!should_parallelize_hour_availability_probes(
        Some(SourceId::Nomads),
        model_summary(ModelId::Hrrr)
    ));
    assert!(should_parallelize_hour_availability_probes(
        Some(SourceId::Aws),
        model_summary(ModelId::Hrrr)
    ));
    assert!(!should_parallelize_hour_availability_probes(
        None,
        model_summary(ModelId::Hrrr)
    ));
}

#[test]
fn aws_fetches_can_use_idx_subsets_and_parallel_whole_file_fallback() {
    assert!(should_use_idx_subset_fetch(SourceId::Aws));
    assert!(should_use_parallel_whole_file_fetch(SourceId::Aws));
}

#[test]
fn nomads_skips_idx_subsets_and_fetches_full_grib_files() {
    assert!(!should_use_idx_subset_fetch(SourceId::Nomads));
    assert!(!should_use_parallel_whole_file_fetch(SourceId::Nomads));
}

#[test]
fn nomads_fetch_strategy_ignores_variable_patterns() {
    let resolved = ResolvedUrl {
        source: SourceId::Nomads,
        grib_url: "https://nomads.ncep.noaa.gov/file.grib2".to_string(),
        idx_url: Some("https://nomads.ncep.noaa.gov/file.grib2.idx".to_string()),
    };

    assert!(!should_use_idx_subset_fetch(resolved.source));
    assert_eq!(resolved.grib_url, "https://nomads.ncep.noaa.gov/file.grib2");
}

#[test]
fn nomads_probe_uses_grib_url_for_availability() {
    let resolved = ResolvedUrl {
        source: SourceId::Nomads,
        grib_url: "https://nomads.ncep.noaa.gov/file.grib2".to_string(),
        idx_url: Some("https://nomads.ncep.noaa.gov/file.grib2.idx".to_string()),
    };
    assert_eq!(
        resolved.availability_probe_url(),
        "https://nomads.ncep.noaa.gov/file.grib2.idx"
    );
    assert_eq!(resolved.grib_url, "https://nomads.ncep.noaa.gov/file.grib2");
}

#[test]
fn source_probe_uses_fallback_sources_in_registry_order() {
    let urls = vec![
        ResolvedUrl {
            source: SourceId::Nomads,
            grib_url: "https://nomads.ncep.noaa.gov/primary.grib2".to_string(),
            idx_url: None,
        },
        ResolvedUrl {
            source: SourceId::Aws,
            grib_url: "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/fallback.grib2".to_string(),
            idx_url: None,
        },
    ];
    let seen = std::sync::Mutex::new(Vec::new());
    let available = any_source_available(&urls, |resolved| {
        seen.lock().unwrap().push(resolved.source);
        matches!(resolved.source, SourceId::Aws)
    });
    assert!(available);
    assert_eq!(*seen.lock().unwrap(), vec![SourceId::Nomads, SourceId::Aws]);
}

#[test]
fn matching_ranges_uses_idx_patterns() {
    let ranges = idx_subset_ranges(SAMPLE_IDX, &["TMP:2 m above ground", "CAPE:surface"])
        .unwrap()
        .expect("idx subset ranges should exist");
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0].0, 0);
    assert_eq!(ranges[1].0, 96542);
}

#[test]
fn matching_ranges_dedupes_duplicate_selector_hits() {
    let ranges = idx_subset_ranges(
        SAMPLE_IDX,
        &["TMP:2 m above ground", "TMP:2 m above ground"],
    )
    .unwrap()
    .expect("idx subset ranges should exist");
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].0, 0);
}

#[test]
fn idx_subset_ranges_coalesces_contiguous_messages_only() {
    let ranges = idx_subset_ranges(
        SAMPLE_IDX,
        &["TMP:2 m above ground", "SPFH:2 m above ground"],
    )
    .unwrap()
    .expect("idx subset ranges should exist");
    assert_eq!(ranges, vec![(0, 96541)]);
}

#[test]
fn idx_subset_ranges_falls_back_when_patterns_do_not_match() {
    assert_eq!(
        idx_subset_ranges(SAMPLE_IDX, &["TMP:850 mb"]).unwrap(),
        None
    );
}

#[test]
fn idx_subset_ranges_falls_back_when_idx_is_unparseable() {
    assert_eq!(
        idx_subset_ranges("not an idx", &["TMP:2 m above ground"]).unwrap(),
        None
    );
}

#[test]
fn aifs_json_index_uses_explicit_offsets_and_lengths() {
    let ranges = idx_subset_ranges(AIFS_INDEX_SAMPLE, &["param=q", "param=2t"])
        .unwrap()
        .expect("AIFS JSON index ranges should exist");
    assert_eq!(
        ranges,
        vec![(2_465_216, 3_112_528), (82_676_279, 83_227_597)]
    );
}

#[test]
fn aifs_json_index_requires_its_exact_key_value_grammar() {
    assert_eq!(
        idx_subset_ranges(AIFS_INDEX_SAMPLE, &["TMP:2 m above ground"]).unwrap(),
        None
    );
    assert_eq!(
        idx_subset_ranges(AIFS_INDEX_SAMPLE, &["unknown=value"]).unwrap(),
        None
    );
}

const NBM_IDX_SAMPLE: &str = "\
1:1000:d=2026072512:TMP:2 m above ground:9 hour fcst:
2:2000:d=2026072512:TMP:2 m above ground:9 hour fcst:ens std dev
3:3000:d=2026072512:DPT:2 m above ground:9 hour fcst:
4:4000:d=2026072512:DPT:2 m above ground:9 hour fcst:ens std dev
5:5000:d=2026072512:PWAT:entire atmosphere (considered as a single layer):9 hour fcst:
6:6000:d=2026072512:PWAT:entire atmosphere (considered as a single layer):9 hour fcst:10% level
7:7000:d=2026072512:PWAT:entire atmosphere (considered as a single layer):9 hour fcst:90% level
8:8000:d=2026072512:APCP:surface:8-9 hour acc fcst:
9:9000:d=2026072512:APCP:surface:8-9 hour acc fcst:prob >0.254:prob fcst 255/255
10:10000:d=2026072512:VIS:surface:9 hour fcst:
";

#[test]
fn idx_subset_without_directive_pulls_probabilistic_companions() {
    let patterns = [
        "TMP:2 m above ground",
        "DPT:2 m above ground",
        "PWAT:entire atmosphere",
        "APCP:surface",
        "VIS:surface",
    ];
    let ranges = idx_subset_ranges(NBM_IDX_SAMPLE, &patterns)
        .expect("subset ok")
        .expect("some ranges");
    assert_eq!(ranges, vec![(1000, u64::MAX)]);
}

#[test]
fn idx_subset_deterministic_only_drops_probabilistic_companions() {
    let patterns = [
        IDX_DETERMINISTIC_ONLY,
        "TMP:2 m above ground",
        "DPT:2 m above ground",
        "PWAT:entire atmosphere",
        "APCP:surface",
        "VIS:surface",
    ];
    let ranges = idx_subset_ranges(NBM_IDX_SAMPLE, &patterns)
        .expect("subset ok")
        .expect("some ranges");
    assert_eq!(
        ranges,
        vec![
            (1000, 1999),
            (3000, 3999),
            (5000, 5999),
            (8000, 8999),
            (10000, u64::MAX),
        ]
    );
}

#[test]
fn idx_deterministic_only_keeps_ensemble_mean() {
    let idx = "\
1:1000:d=2026072512:TMP:2 m above ground:9 hour fcst:ens mean
2:2000:d=2026072512:TMP:2 m above ground:9 hour fcst:ens std dev
";
    let ranges = idx_subset_ranges(idx, &[IDX_DETERMINISTIC_ONLY, "TMP:2 m above ground"])
        .expect("subset ok")
        .expect("some ranges");
    assert_eq!(ranges, vec![(1000, 1999)]);
}

#[test]
fn subset_requests_prefer_subset_capable_sources() {
    let fetch = FetchRequest {
        request: ModelRunRequest::new(
            ModelId::Nbm,
            rustwx_core::CycleSpec::new("20260725", 12).unwrap(),
            12,
            "core/co",
        )
        .unwrap(),
        source_override: None,
        variable_patterns: vec!["TMP:2 m above ground".to_string()],
    };
    let urls = filtered_urls(&fetch).expect("resolve");
    assert!(
        urls.len() >= 2,
        "NBM should resolve AWS and NOMADS: {urls:?}"
    );
    assert!(
        should_use_idx_subset_fetch(urls[0].source),
        "first source must support indexed subsets: {urls:?}"
    );
    assert!(
        urls.iter().any(|url| url.source == SourceId::Nomads),
        "NOMADS must remain a whole-file fallback: {urls:?}"
    );
}

#[test]
fn rtma_urma_subset_requests_prefer_the_indexed_aws_archives() {
    for model in [ModelId::Rtma, ModelId::Urma] {
        let fetch = FetchRequest {
            request: ModelRunRequest::new(
                model,
                rustwx_core::CycleSpec::new("20260810", 17).unwrap(),
                0,
                "2dvaranl_ndfd",
            )
            .unwrap(),
            source_override: None,
            variable_patterns: vec!["TMP:2 m above ground".to_string()],
        };
        let urls = filtered_urls(&fetch).expect("resolve analysis sources");
        assert_eq!(urls[0].source, SourceId::Aws, "{model}: {urls:?}");
        assert!(urls[0].idx_url.is_some(), "{model}: {urls:?}");
        assert!(
            urls.iter().any(|url| url.source == SourceId::Nomads),
            "NOMADS must remain a whole-file fallback for {model}: {urls:?}"
        );
    }
}

#[test]
fn whole_file_requests_keep_registry_source_order() {
    let fetch = FetchRequest {
        request: ModelRunRequest::new(
            ModelId::Nbm,
            rustwx_core::CycleSpec::new("20260725", 12).unwrap(),
            12,
            "core/co",
        )
        .unwrap(),
        source_override: None,
        variable_patterns: Vec::new(),
    };
    let ordered = filtered_urls(&fetch).expect("resolve");
    let baseline = resolve_urls(&fetch.request).expect("baseline");
    assert_eq!(
        ordered.iter().map(|url| url.source).collect::<Vec<_>>(),
        baseline.iter().map(|url| url.source).collect::<Vec<_>>()
    );
}

#[test]
fn subset_request_honors_explicit_source_override() {
    let fetch = FetchRequest {
        request: ModelRunRequest::new(
            ModelId::Nbm,
            rustwx_core::CycleSpec::new("20260725", 12).unwrap(),
            12,
            "core/co",
        )
        .unwrap(),
        source_override: Some(SourceId::Nomads),
        variable_patterns: vec!["TMP:2 m above ground".to_string()],
    };
    let urls = filtered_urls(&fetch).expect("resolve");
    assert_eq!(urls.len(), 1);
    assert_eq!(urls[0].source, SourceId::Nomads);
}

#[test]
fn resolve_fetch_urls_uses_registry_order() {
    let request = ModelRunRequest::new(
        ModelId::RrfsA,
        rustwx_core::CycleSpec::new("20260414", 20).unwrap(),
        2,
        "prs-conus",
    )
    .unwrap();
    let fetch = FetchRequest {
        request,
        source_override: None,
        variable_patterns: Vec::new(),
    };
    let urls = filtered_urls(&fetch).unwrap();
    assert_eq!(urls.len(), 1);
    assert!(urls[0].grib_url.contains("noaa-rrfs-pds.s3.amazonaws.com/rrfs_a/rrfs.20260414/20/rrfs.t20z.prslev.3km.f002.conus.grib2"));
}

#[test]
fn fetch_request_from_timestep_builds_request() {
    let timestep = ModelTimestep::with_source(
        ModelId::Hrrr,
        rustwx_core::CycleSpec::new("20260414", 18).unwrap(),
        3,
        rustwx_core::TimeStamp::new("2026-04-14T21:00:00Z").unwrap(),
        Some(SourceId::Nomads),
    )
    .unwrap();

    let fetch = FetchRequest::from_timestep(
        &timestep,
        "prs",
        timestep.source,
        ["TMP:500 mb", "RH:500 mb"],
    )
    .unwrap();

    assert_eq!(fetch.request.model, ModelId::Hrrr);
    assert_eq!(fetch.request.forecast_hour, 3);
    assert_eq!(fetch.request.product, "prs");
    assert_eq!(fetch.source_override, Some(SourceId::Nomads));
    assert_eq!(
        fetch.variable_patterns,
        vec!["TMP:500 mb".to_string(), "RH:500 mb".to_string()]
    );
}

#[test]
fn structured_selector_matches_supported_upper_air_subset() {
    let height_200 = StructuredMessageSelector::try_from(FieldSelector::isobaric(
        CanonicalField::GeopotentialHeight,
        200,
    ))
    .unwrap();
    let height_200_message =
        ieee_f32_message(PARAMETER_HGT[0], 100, 20_000.0, &[12_040.0], -99.0, -99.0);
    assert!(height_200.matches(&height_200_message));

    let height_250 = StructuredMessageSelector::try_from(FieldSelector::isobaric(
        CanonicalField::GeopotentialHeight,
        250,
    ))
    .unwrap();
    let height_250_message =
        ieee_f32_message(PARAMETER_HGT[0], 100, 25_000.0, &[10_540.0], -99.0, -99.0);
    assert!(height_250.matches(&height_250_message));

    let wind_300 =
        StructuredMessageSelector::try_from(FieldSelector::isobaric(CanonicalField::VWind, 300))
            .unwrap();
    let wind_300_message =
        ieee_f32_message(PARAMETER_VGRD[0], 100, 30_000.0, &[36.0], -99.0, -99.0);
    assert!(wind_300.matches(&wind_300_message));

    let wind_selector =
        StructuredMessageSelector::try_from(FieldSelector::isobaric(CanonicalField::UWind, 850))
            .unwrap();
    let wind_message = ieee_f32_message(
        PARAMETER_UGRD[0],
        100,
        85_000.0,
        &[12.0, 15.0],
        -99.0,
        -98.0,
    );
    assert!(wind_selector.matches(&wind_message));

    let wind_speed_850 = StructuredMessageSelector::try_from(FieldSelector::isobaric(
        CanonicalField::WindSpeed,
        850,
    ))
    .unwrap();
    let wind_speed_850_message = ieee_f32_message(
        PARAMETER_WIND_SPEED[0],
        100,
        85_000.0,
        &[21.0],
        -99.0,
        -99.0,
    );
    assert!(wind_speed_850.matches(&wind_speed_850_message));

    let temp_700 = StructuredMessageSelector::try_from(FieldSelector::isobaric(
        CanonicalField::Temperature,
        700,
    ))
    .unwrap();
    let temp_message = ieee_f32_message(PARAMETER_TMP[0], 100, 70_000.0, &[274.0], -99.0, -99.0);
    assert!(temp_700.matches(&temp_message));
    // Stratospheric 7 hPa (level_value=700 Pa) must NOT alias onto 700 hPa.
    let stratospheric_tmp_message =
        ieee_f32_message(PARAMETER_TMP[0], 100, 700.0, &[210.0], -99.0, -99.0);
    assert!(!temp_700.matches(&stratospheric_tmp_message));

    let rh_700 = StructuredMessageSelector::try_from(FieldSelector::isobaric(
        CanonicalField::RelativeHumidity,
        700,
    ))
    .unwrap();
    let rh_message = ieee_f32_message(PARAMETER_RH[0], 100, 70_000.0, &[61.0], -99.0, -99.0);
    assert!(rh_700.matches(&rh_message));
    // GFS/RRFS carry stratospheric RH at level_value=700 Pa (7 hPa). With the
    // old "divide by 100 only when > 2000" heuristic this collided with 700
    // hPa and the first-match extraction picked up the near-zero
    // stratospheric RH, producing a flat-brown 700 mb render.
    let stratospheric_rh_message =
        ieee_f32_message(PARAMETER_RH[0], 100, 700.0, &[0.1], -99.0, -99.0);
    assert!(!rh_700.matches(&stratospheric_rh_message));

    let dewpoint_850 =
        StructuredMessageSelector::try_from(FieldSelector::isobaric(CanonicalField::Dewpoint, 850))
            .unwrap();
    let dewpoint_message =
        ieee_f32_message(PARAMETER_DPT[0], 100, 85_000.0, &[281.0], -99.0, -99.0);
    assert!(dewpoint_850.matches(&dewpoint_message));

    let dewpoint_700 =
        StructuredMessageSelector::try_from(FieldSelector::isobaric(CanonicalField::Dewpoint, 700))
            .unwrap();
    let dewpoint_700_message =
        ieee_f32_message(PARAMETER_DPT[0], 100, 70_000.0, &[270.0], -99.0, -99.0);
    assert!(dewpoint_700.matches(&dewpoint_700_message));

    let total_precipitation = StructuredMessageSelector::try_from(FieldSelector::surface(
        CanonicalField::TotalPrecipitation,
    ))
    .unwrap();
    let aifs_total_precipitation = ieee_f32_message(
        ParameterCode {
            discipline: 0,
            category: 1,
            number: 52,
        },
        1,
        0.0,
        &[12.5],
        -99.0,
        -99.0,
    );
    assert!(total_precipitation.matches(&aifs_total_precipitation));

    let vorticity_500 = StructuredMessageSelector::try_from(FieldSelector::isobaric(
        CanonicalField::AbsoluteVorticity,
        500,
    ))
    .unwrap();
    let vorticity_message = ieee_f32_message(
        PARAMETER_ABSOLUTE_VORTICITY[0],
        100,
        50_000.0,
        &[0.00012],
        -99.0,
        -99.0,
    );
    assert!(vorticity_500.matches(&vorticity_message));

    let lsm_surface =
        StructuredMessageSelector::try_from(FieldSelector::surface(CanonicalField::LandSeaMask))
            .unwrap();
    let lsm_message = ieee_f32_message(PARAMETER_LANDSEA_MASK[0], 1, 0.0, &[1.0], -99.0, -99.0);
    assert!(lsm_surface.matches(&lsm_message));

    let terrain_surface = StructuredMessageSelector::try_from(FieldSelector::surface(
        CanonicalField::GeopotentialHeight,
    ))
    .unwrap();
    let terrain_message = ieee_f32_message(PARAMETER_HGT[0], 1, 0.0, &[326.0], -99.0, -99.0);
    assert!(terrain_surface.matches(&terrain_message));

    let temp_2m = StructuredMessageSelector::try_from(FieldSelector::height_agl(
        CanonicalField::Temperature,
        2,
    ))
    .unwrap();
    let temp_2m_message = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[293.2], -99.0, -99.0);
    assert!(temp_2m.matches(&temp_2m_message));

    let dewpoint_2m =
        StructuredMessageSelector::try_from(FieldSelector::height_agl(CanonicalField::Dewpoint, 2))
            .unwrap();
    let dewpoint_2m_message = ieee_f32_message(PARAMETER_DPT[0], 103, 2.0, &[286.4], -99.0, -99.0);
    assert!(dewpoint_2m.matches(&dewpoint_2m_message));

    let rh_2m = StructuredMessageSelector::try_from(FieldSelector::height_agl(
        CanonicalField::RelativeHumidity,
        2,
    ))
    .unwrap();
    let rh_2m_message = ieee_f32_message(PARAMETER_RH[0], 103, 2.0, &[64.0], -99.0, -99.0);
    assert!(rh_2m.matches(&rh_2m_message));

    let hybrid_pressure = StructuredMessageSelector::try_from(FieldSelector::hybrid_level(
        CanonicalField::Pressure,
        7,
    ))
    .unwrap();
    let hybrid_pressure_message =
        ieee_f32_message(PARAMETER_PRESSURE[0], 105, 7.0, &[81_500.0], -99.0, -99.0);
    assert!(hybrid_pressure.matches(&hybrid_pressure_message));

    let hybrid_smoke = StructuredMessageSelector::try_from(FieldSelector::hybrid_level(
        CanonicalField::SmokeMassDensity,
        7,
    ))
    .unwrap();
    let hybrid_smoke_message = ieee_f32_message(
        PARAMETER_SMOKE_MASS_DENSITY[0],
        105,
        7.0,
        &[0.000_012],
        -99.0,
        -99.0,
    );
    assert!(hybrid_smoke.matches(&hybrid_smoke_message));
    let wrong_hybrid_smoke_message = ieee_f32_message(
        PARAMETER_SMOKE_MASS_DENSITY[0],
        105,
        8.0,
        &[0.000_012],
        -99.0,
        -99.0,
    );
    assert!(!hybrid_smoke.matches(&wrong_hybrid_smoke_message));

    let smoke_8m = StructuredMessageSelector::try_from(FieldSelector::height_agl(
        CanonicalField::SmokeMassDensity,
        8,
    ))
    .unwrap();
    let smoke_8m_message = ieee_f32_message(
        PARAMETER_SMOKE_MASS_DENSITY[0],
        103,
        8.0,
        &[0.000_025],
        -99.0,
        -99.0,
    );
    assert!(smoke_8m.matches(&smoke_8m_message));

    let smoke_column = StructuredMessageSelector::try_from(FieldSelector::entire_atmosphere(
        CanonicalField::ColumnIntegratedSmoke,
    ))
    .unwrap();
    let smoke_column_message = ieee_f32_message(
        PARAMETER_COLUMN_INTEGRATED_SMOKE[0],
        200,
        0.0,
        &[0.003],
        -99.0,
        -99.0,
    );
    assert!(smoke_column.matches(&smoke_column_message));

    let u_10m =
        StructuredMessageSelector::try_from(FieldSelector::height_agl(CanonicalField::UWind, 10))
            .unwrap();
    let u_10m_message = ieee_f32_message(PARAMETER_UGRD[0], 103, 10.0, &[8.0], -99.0, -99.0);
    assert!(u_10m.matches(&u_10m_message));

    let wind_speed_10m = StructuredMessageSelector::try_from(FieldSelector::height_agl(
        CanonicalField::WindSpeed,
        10,
    ))
    .unwrap();
    let wind_speed_10m_message =
        ieee_f32_message(PARAMETER_WIND_SPEED[0], 103, 10.0, &[12.0], -99.0, -99.0);
    assert!(wind_speed_10m.matches(&wind_speed_10m_message));

    let gust_10m = StructuredMessageSelector::try_from(FieldSelector::height_agl(
        CanonicalField::WindGust,
        10,
    ))
    .unwrap();
    let gust_surface_message =
        ieee_f32_message(PARAMETER_WIND_GUST[0], 1, 0.0, &[18.0], -99.0, -99.0);
    assert!(gust_10m.matches(&gust_surface_message));
    let gust_10m_message =
        ieee_f32_message(PARAMETER_WIND_GUST[0], 103, 10.0, &[18.0], -99.0, -99.0);
    assert!(gust_10m.matches(&gust_10m_message));

    let mslp = StructuredMessageSelector::try_from(FieldSelector::mean_sea_level(
        CanonicalField::PressureReducedToMeanSeaLevel,
    ))
    .unwrap();
    let mslp_message = ieee_f32_message(PARAMETER_MSLP[0], 101, 0.0, &[100_925.0], -99.0, -99.0);
    assert!(mslp.matches(&mslp_message));
    let mslma_message = ieee_f32_message(PARAMETER_MSLP[2], 101, 0.0, &[100_830.0], -99.0, -99.0);
    assert!(mslp.matches(&mslma_message));

    let pwat = StructuredMessageSelector::try_from(FieldSelector::entire_atmosphere(
        CanonicalField::PrecipitableWater,
    ))
    .unwrap();
    let pwat_message = ieee_f32_message(PARAMETER_PWAT[0], 200, 0.0, &[31.0], -99.0, -99.0);
    assert!(pwat.matches(&pwat_message));

    let qpf = StructuredMessageSelector::try_from(FieldSelector::surface(
        CanonicalField::TotalPrecipitation,
    ))
    .unwrap();
    let qpf_message = ieee_f32_message(
        PARAMETER_TOTAL_PRECIPITATION[0],
        1,
        0.0,
        &[12.0],
        -99.0,
        -99.0,
    );
    assert!(qpf.matches(&qpf_message));

    let pop = StructuredMessageSelector::try_from(FieldSelector::surface(
        CanonicalField::ProbabilityOfPrecipitation,
    ))
    .unwrap();
    let pop_message = ieee_f32_message(
        PARAMETER_PROBABILITY_OF_PRECIPITATION[0],
        1,
        0.0,
        &[80.0],
        -99.0,
        -99.0,
    );
    assert!(pop.matches(&pop_message));

    let tcdc = StructuredMessageSelector::try_from(FieldSelector::entire_atmosphere(
        CanonicalField::TotalCloudCover,
    ))
    .unwrap();
    let tcdc_message = ieee_f32_message(
        PARAMETER_TOTAL_CLOUD_COVER[0],
        200,
        0.0,
        &[84.0],
        -99.0,
        -99.0,
    );
    assert!(tcdc.matches(&tcdc_message));

    let surface_tcdc = StructuredMessageSelector::try_from(FieldSelector::surface(
        CanonicalField::TotalCloudCover,
    ))
    .unwrap();
    let surface_tcdc_message = ieee_f32_message(
        PARAMETER_TOTAL_CLOUD_COVER[0],
        1,
        0.0,
        &[76.0],
        -99.0,
        -99.0,
    );
    assert!(surface_tcdc.matches(&surface_tcdc_message));

    let lcdc = StructuredMessageSelector::try_from(FieldSelector::entire_atmosphere(
        CanonicalField::LowCloudCover,
    ))
    .unwrap();
    let lcdc_message = ieee_f32_message(
        PARAMETER_LOW_CLOUD_COVER[0],
        214,
        0.0,
        &[40.0],
        -99.0,
        -99.0,
    );
    assert!(lcdc.matches(&lcdc_message));

    let mcdc = StructuredMessageSelector::try_from(FieldSelector::entire_atmosphere(
        CanonicalField::MiddleCloudCover,
    ))
    .unwrap();
    let mcdc_message = ieee_f32_message(
        PARAMETER_MIDDLE_CLOUD_COVER[0],
        224,
        0.0,
        &[55.0],
        -99.0,
        -99.0,
    );
    assert!(mcdc.matches(&mcdc_message));

    let hcdc = StructuredMessageSelector::try_from(FieldSelector::entire_atmosphere(
        CanonicalField::HighCloudCover,
    ))
    .unwrap();
    let hcdc_message = ieee_f32_message(
        PARAMETER_HIGH_CLOUD_COVER[0],
        234,
        0.0,
        &[70.0],
        -99.0,
        -99.0,
    );
    assert!(hcdc.matches(&hcdc_message));

    let visibility =
        StructuredMessageSelector::try_from(FieldSelector::surface(CanonicalField::Visibility))
            .unwrap();
    let visibility_message =
        ieee_f32_message(PARAMETER_VISIBILITY[0], 1, 0.0, &[16_000.0], -99.0, -99.0);
    assert!(visibility.matches(&visibility_message));

    let simulated_ir = StructuredMessageSelector::try_from(FieldSelector::nominal_top(
        CanonicalField::SimulatedInfraredBrightnessTemperature,
    ))
    .unwrap();
    let simulated_ir_message =
        ieee_f32_message(PARAMETER_SIMULATED_IR[0], 8, 0.0, &[234.5], -99.0, -99.0);
    let simulated_ir_wrong_level =
        ieee_f32_message(PARAMETER_SIMULATED_IR[0], 10, 0.0, &[234.5], -99.0, -99.0);
    assert!(simulated_ir.matches(&simulated_ir_message));
    assert!(!simulated_ir.matches(&simulated_ir_wrong_level));

    let categorical_rain = StructuredMessageSelector::try_from(FieldSelector::surface(
        CanonicalField::CategoricalRain,
    ))
    .unwrap();
    let categorical_rain_message =
        ieee_f32_message(PARAMETER_CATEGORICAL_RAIN[0], 1, 0.0, &[1.0], -99.0, -99.0);
    assert!(categorical_rain.matches(&categorical_rain_message));
    let categorical_rain_hrrr_message =
        ieee_f32_message(PARAMETER_CATEGORICAL_RAIN[1], 1, 0.0, &[1.0], -99.0, -99.0);
    assert!(categorical_rain.matches(&categorical_rain_hrrr_message));

    let categorical_freezing_rain = StructuredMessageSelector::try_from(FieldSelector::surface(
        CanonicalField::CategoricalFreezingRain,
    ))
    .unwrap();
    let categorical_freezing_rain_message = ieee_f32_message(
        PARAMETER_CATEGORICAL_FREEZING_RAIN[0],
        1,
        0.0,
        &[1.0],
        -99.0,
        -99.0,
    );
    assert!(categorical_freezing_rain.matches(&categorical_freezing_rain_message));
    let categorical_freezing_rain_hrrr_message = ieee_f32_message(
        PARAMETER_CATEGORICAL_FREEZING_RAIN[1],
        1,
        0.0,
        &[1.0],
        -99.0,
        -99.0,
    );
    assert!(categorical_freezing_rain.matches(&categorical_freezing_rain_hrrr_message));

    let categorical_ice_pellets = StructuredMessageSelector::try_from(FieldSelector::surface(
        CanonicalField::CategoricalIcePellets,
    ))
    .unwrap();
    let categorical_ice_pellets_message = ieee_f32_message(
        PARAMETER_CATEGORICAL_ICE_PELLETS[0],
        1,
        0.0,
        &[1.0],
        -99.0,
        -99.0,
    );
    assert!(categorical_ice_pellets.matches(&categorical_ice_pellets_message));
    let categorical_ice_pellets_hrrr_message = ieee_f32_message(
        PARAMETER_CATEGORICAL_ICE_PELLETS[1],
        1,
        0.0,
        &[1.0],
        -99.0,
        -99.0,
    );
    assert!(categorical_ice_pellets.matches(&categorical_ice_pellets_hrrr_message));

    let categorical_snow = StructuredMessageSelector::try_from(FieldSelector::surface(
        CanonicalField::CategoricalSnow,
    ))
    .unwrap();
    let categorical_snow_message =
        ieee_f32_message(PARAMETER_CATEGORICAL_SNOW[0], 1, 0.0, &[1.0], -99.0, -99.0);
    assert!(categorical_snow.matches(&categorical_snow_message));
    let categorical_snow_hrrr_message =
        ieee_f32_message(PARAMETER_CATEGORICAL_SNOW[1], 1, 0.0, &[1.0], -99.0, -99.0);
    assert!(categorical_snow.matches(&categorical_snow_hrrr_message));

    let reflectivity_1km = StructuredMessageSelector::try_from(FieldSelector::height_agl(
        CanonicalField::RadarReflectivity,
        1000,
    ))
    .unwrap();
    let reflectivity_message = ieee_f32_message(
        PARAMETER_RADAR_REFLECTIVITY[0],
        103,
        1000.0,
        &[42.0],
        -99.0,
        -99.0,
    );
    assert!(reflectivity_1km.matches(&reflectivity_message));

    let uh_2_5km = StructuredMessageSelector::try_from(FieldSelector::height_layer_agl(
        CanonicalField::UpdraftHelicity,
        2000,
        5000,
    ))
    .unwrap();
    let uh_message = ieee_f32_message(
        PARAMETER_UPDRAFT_HELICITY[0],
        103,
        5000.0,
        &[125.0],
        -99.0,
        -99.0,
    );
    assert!(uh_2_5km.matches(&uh_message));

    // Off-grid isobaric levels (not a 25 hPa multiple in 100..=1000) stay
    // unsupported; 500 mb dewpoint and 925 mb vorticity are now on-grid.
    assert!(matches!(
        StructuredMessageSelector::try_from(FieldSelector::isobaric(CanonicalField::Dewpoint, 510)),
        Err(IoError::UnsupportedStructuredSelector { .. })
    ));
    assert!(matches!(
        StructuredMessageSelector::try_from(FieldSelector::isobaric(
            CanonicalField::AbsoluteVorticity,
            935
        )),
        Err(IoError::UnsupportedStructuredSelector { .. })
    ));
    assert!(matches!(
        StructuredMessageSelector::try_from(FieldSelector::isobaric(
            CanonicalField::RelativeVorticity,
            500
        )),
        Err(IoError::UnsupportedStructuredSelector { .. })
    ));
    assert!(matches!(
        StructuredMessageSelector::try_from(FieldSelector::height_layer_agl(
            CanonicalField::UpdraftHelicity,
            0,
            3000
        )),
        Err(IoError::UnsupportedStructuredSelector { .. })
    ));
    assert!(matches!(
        StructuredMessageSelector::try_from(FieldSelector::hybrid_level(
            CanonicalField::SmokeMassDensity,
            51
        )),
        Err(IoError::UnsupportedStructuredSelector { .. })
    ));
    assert!(matches!(
        StructuredMessageSelector::try_from(FieldSelector::height_agl(
            CanonicalField::SmokeMassDensity,
            2
        )),
        Err(IoError::UnsupportedStructuredSelector { .. })
    ));
    assert!(matches!(
        StructuredMessageSelector::try_from(FieldSelector::entire_atmosphere(
            CanonicalField::SimulatedInfraredBrightnessTemperature
        )),
        Err(IoError::UnsupportedStructuredSelector { .. })
    ));
}

#[test]
fn extract_ignores_stratospheric_pa_alias_of_tropospheric_level() {
    // GFS/RRFS-A carry both 7 hPa (level_value = 700 Pa) and 700 hPa
    // (level_value = 70_000 Pa) messages in the same file. The 7 hPa one
    // appears first. The extractor must return the 700 hPa message.
    let stratospheric = ieee_f32_message(PARAMETER_RH[0], 100, 700.0, &[0.1, 0.2], 261.0, 262.0);
    let tropospheric =
        ieee_f32_message(PARAMETER_RH[0], 100, 70_000.0, &[55.0, 65.0], 261.0, 262.0);
    let grib = Grib2File {
        messages: vec![stratospheric, tropospheric],
    };

    let field =
        extract_pressure_field_from_grib2(&grib, CanonicalField::RelativeHumidity, 700).unwrap();

    assert_eq!(field.values, vec![55.0, 65.0]);
}

#[test]
fn extract_prefers_instantaneous_temperature_over_statistical_alias() {
    // ECMWF Open Data can carry PDT 4.8 statistical 2 m temperature
    // messages before the instantaneous PDT 4.0 2 m temperature message.
    // The statistical fields can be zero at f000, which becomes -273.15 C
    // downstream if we take the first parameter/level match.
    let mut statistical = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[0.0, 0.0], 261.0, 262.0);
    statistical.product.template = 8;
    statistical.product.statistical_process_type = Some(2);
    statistical.product.statistical_time_range_unit = Some(1);
    statistical.product.time_range_length = Some(6);

    let instantaneous = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[280.0, 281.5], 261.0, 262.0);
    let grib = Grib2File {
        messages: vec![statistical, instantaneous],
    };

    let field = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
    )
    .unwrap();

    assert_eq!(field.values, vec![280.0, 281.5]);
}

#[test]
fn partial_extract_at_forecast_hour_uses_requested_lead_time() {
    let mut f003 = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[273.0], -99.0, -99.0);
    f003.product.time_range_unit = 1;
    f003.product.forecast_time = 3;

    let mut f024 = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[294.0], -99.0, -99.0);
    f024.product.time_range_unit = 1;
    f024.product.forecast_time = 24;

    let selector = FieldSelector::height_agl(CanonicalField::Temperature, 2);
    let grib = Grib2File {
        messages: vec![f003, f024],
    };

    let partial =
        extract_fields_from_grib2_partial_at_forecast_hour(&grib, &[selector], 24).unwrap();
    assert!(partial.missing.is_empty());
    assert_eq!(partial.extracted[0].values, vec![294.0]);

    let missing =
        extract_fields_from_grib2_partial_at_forecast_hour(&grib, &[selector], 12).unwrap();
    assert!(missing.extracted.is_empty());
    assert_eq!(missing.missing, vec![selector]);
}

#[test]
fn partial_extract_at_forecast_hour_matches_statistical_window_end() {
    let mut qpf = ieee_f32_message(
        PARAMETER_TOTAL_PRECIPITATION[0],
        1,
        0.0,
        &[7.5],
        -99.0,
        -99.0,
    );
    qpf.product.template = 8;
    qpf.product.time_range_unit = 1;
    qpf.product.forecast_time = 18;
    qpf.product.statistical_time_range_unit = Some(1);
    qpf.product.time_range_length = Some(6);

    let selector = FieldSelector::surface(CanonicalField::TotalPrecipitation);
    let grib = Grib2File {
        messages: vec![qpf],
    };

    let partial =
        extract_fields_from_grib2_partial_at_forecast_hour(&grib, &[selector], 24).unwrap();
    assert!(partial.missing.is_empty());
    assert_eq!(partial.extracted[0].values, vec![7.5]);
}

/// DWD ICON-D2 hourly `TOT_PREC` objects contain four run-total messages at
/// 15-minute endpoints. For the f001 object these are 60/75/90/105 minutes;
/// for f002 they are 120/135/150/165 minutes. RWS stores an integer-hour time
/// axis, so matching must select the exact hourly endpoint and must never
/// truncate or round a later quarter-hour message back to that hour.
#[test]
fn dwd_d2_minute_statistical_endpoints_select_only_the_exact_hour() {
    let make_run_total = |end_minutes: u32, value: f32| {
        let mut message = ieee_f32_message(
            PARAMETER_TOTAL_PRECIPITATION[1],
            1,
            0.0,
            &[value],
            -99.0,
            -99.0,
        );
        message.product.template = 8;
        message.product.time_range_unit = 0;
        message.product.forecast_time = 0;
        message.product.statistical_time_range_unit = Some(0);
        message.product.time_range_length = Some(end_minutes);
        message
    };
    let selector = FieldSelector::surface(CanonicalField::TotalPrecipitation);

    for (expected_hour, endpoints, expected_value) in [
        (1_u16, [60_u32, 75, 90, 105], 60.0_f32),
        (2_u16, [120_u32, 135, 150, 165], 120.0_f32),
    ] {
        let grib = Grib2File {
            messages: endpoints
                .into_iter()
                .map(|minutes| make_run_total(minutes, minutes as f32))
                .collect(),
        };
        let partial =
            extract_fields_from_grib2_partial_at_forecast_hour(&grib, &[selector], expected_hour)
                .unwrap();
        assert!(partial.missing.is_empty());
        assert_eq!(partial.extracted.len(), 1);
        assert_eq!(partial.extracted[0].values, vec![expected_value]);
    }
}

/// REGRESSION (found live on RRFS-A f002, 2026-06-11): a surface file may
/// carry BOTH the run-total (0→h) and the trailing-window ((h−1)→h) APCP
/// accumulation, and both end at hour h so both tie on the end-hour forecast
/// score. Selection of the run total must NOT depend on which message comes
/// first in the file: HRRR orders the run total first (accidentally correct),
/// RRFS-A orders the window first — which silently stored the 1 h window as
/// `apcp_run_total`. The run-total selection must prefer the accumulation
/// that starts at the run start (hour 0) in BOTH file orders, and the
/// trailing re-select at h−1 must still find the window.
#[test]
fn qpf_run_total_prefers_zero_start_accumulation_in_either_file_order() {
    let make_apcp = |start_hour: u32, length: u32, values: &[f32]| {
        let mut message = ieee_f32_message(
            PARAMETER_TOTAL_PRECIPITATION[0],
            1,
            0.0,
            values,
            -99.0,
            -99.0,
        );
        message.product.template = 8;
        message.product.time_range_unit = 1;
        message.product.forecast_time = start_hour;
        message.product.statistical_time_range_unit = Some(1);
        message.product.time_range_length = Some(length);
        message
    };
    // f002: window = 1→2 hour acc, run total = 0→2 hour acc.
    let window = make_apcp(1, 1, &[1.5]);
    let run_total = make_apcp(0, 2, &[9.0]);
    let selector = FieldSelector::surface(CanonicalField::TotalPrecipitation);

    // RRFS-A file order: window FIRST (this is the order that bit live).
    let rrfs_order = Grib2File {
        messages: vec![window.clone(), run_total.clone()],
    };
    let picked = extract_fields_from_grib2_partial_at_forecast_hour(&rrfs_order, &[selector], 2)
        .unwrap()
        .extracted
        .swap_remove(0);
    assert_eq!(
        picked.values,
        vec![9.0],
        "run total must be the 0->2 accumulation even when the window comes first"
    );

    // HRRR file order: run total first (the historical accidental pass).
    let hrrr_order = Grib2File {
        messages: vec![run_total.clone(), window.clone()],
    };
    let picked = extract_fields_from_grib2_partial_at_forecast_hour(&hrrr_order, &[selector], 2)
        .unwrap()
        .extracted
        .swap_remove(0);
    assert_eq!(picked.values, vec![9.0]);

    // The trailing-window re-select at h−1 = 1 must still find the WINDOW
    // (its start hour matches exactly; the run total's start and end both
    // miss) — in both orders.
    for messages in [
        vec![window.clone(), run_total.clone()],
        vec![run_total, window],
    ] {
        let grib = Grib2File { messages };
        let picked = extract_fields_from_grib2_partial_at_forecast_hour(&grib, &[selector], 1)
            .unwrap()
            .extracted
            .swap_remove(0);
        assert_eq!(
            picked.values,
            vec![1.5],
            "the h-1 re-select must still pick the trailing window"
        );
    }
}

#[test]
fn statistical_qpf_run_totals_are_not_selected_by_file_order() {
    let make_percentile = |start_hour: u32, length: u32, value: f32| {
        let mut message = ieee_f32_message(
            PARAMETER_TOTAL_PRECIPITATION[0],
            1,
            0.0,
            &[value],
            -99.0,
            -99.0,
        );
        message.product.template = 10;
        message.product.percentile_value = Some(50);
        message.product.time_range_unit = 1;
        message.product.forecast_time = start_hour;
        message.product.statistical_time_range_unit = Some(1);
        message.product.time_range_length = Some(length);
        message
    };
    let make_probability = |start_hour: u32, length: u32, value: f32| {
        let mut message = ieee_f32_message(
            PARAMETER_TOTAL_PRECIPITATION[0],
            1,
            0.0,
            &[value],
            -99.0,
            -99.0,
        );
        message.product.template = 9;
        message.product.probability_type = Some(3);
        message.product.probability_lower_limit = Some(10.0);
        message.product.time_range_unit = 1;
        message.product.forecast_time = start_hour;
        message.product.statistical_time_range_unit = Some(1);
        message.product.time_range_length = Some(length);
        message
    };

    let cases = [
        (
            FieldSelector::surface(CanonicalField::TotalPrecipitation).with_percentile(50),
            make_percentile(1, 1, 1.5),
            make_percentile(0, 2, 9.0),
            9.0,
        ),
        (
            FieldSelector::surface(CanonicalField::TotalPrecipitation)
                .with_probability(ProbabilitySelection::new(Some(3), Some(10_000), None)),
            make_probability(1, 1, 15.0),
            make_probability(0, 2, 75.0),
            75.0,
        ),
    ];
    for (selector, window, run_total, expected) in cases {
        for messages in [
            vec![window.clone(), run_total.clone()],
            vec![run_total.clone(), window.clone()],
        ] {
            let grib = Grib2File { messages };
            let picked = extract_fields_from_grib2_partial_at_forecast_hour(&grib, &[selector], 2)
                .unwrap()
                .extracted
                .swap_remove(0);
            assert_eq!(picked.values, vec![expected]);
        }
    }
}

#[test]
fn extract_distinguishes_pop_from_accumulated_qpf() {
    let mut probability = ieee_f32_message(
        PARAMETER_TOTAL_PRECIPITATION[0],
        1,
        0.0,
        &[80.0, 90.0],
        -99.0,
        -99.0,
    );
    probability.product.template = 9;

    let mut accumulation = ieee_f32_message(
        PARAMETER_TOTAL_PRECIPITATION[0],
        1,
        0.0,
        &[2.0, 4.0],
        -99.0,
        -99.0,
    );
    accumulation.product.template = 8;

    let grib = Grib2File {
        messages: vec![probability, accumulation],
    };

    let qpf = extract_field_from_grib2(
        &grib,
        FieldSelector::surface(CanonicalField::TotalPrecipitation),
    )
    .unwrap();
    let pop = extract_field_from_grib2(
        &grib,
        FieldSelector::surface(CanonicalField::ProbabilityOfPrecipitation),
    )
    .unwrap();

    assert_eq!(qpf.values, vec![2.0, 4.0]);
    assert_eq!(pop.values, vec![80.0, 90.0]);
    assert_eq!(pop.units, "%");
}

#[test]
fn extract_qmd_percentile_uses_exact_percentile_metadata() {
    let mut p10 = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[270.0], -99.0, -99.0);
    p10.product.template = 6;
    p10.product.percentile_value = Some(10);
    let mut p50 = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[280.0], -99.0, -99.0);
    p50.product.template = 6;
    p50.product.percentile_value = Some(50);
    let mut p90 = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[290.0], -99.0, -99.0);
    p90.product.template = 6;
    p90.product.percentile_value = Some(90);
    let grib = Grib2File {
        messages: vec![p10, p50, p90],
    };

    let field = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2).with_percentile(50),
    )
    .unwrap();

    assert_eq!(field.values, vec![280.0]);
    assert_eq!(
        field.selector,
        FieldSelector::height_agl(CanonicalField::Temperature, 2).with_percentile(50)
    );
}

#[test]
fn extract_qmd_percentile_does_not_fallback_to_wrong_percentile() {
    let mut p50 = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[280.0], -99.0, -99.0);
    p50.product.template = 6;
    p50.product.percentile_value = Some(50);
    let grib = Grib2File {
        messages: vec![p50],
    };

    let err = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2).with_percentile(90),
    )
    .unwrap_err();

    assert!(matches!(err, IoError::FieldNotFound { .. }));
}

#[test]
fn extract_qmd_probability_uses_exact_threshold_metadata() {
    let mut freeze = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[70.0], -99.0, -99.0);
    freeze.product.template = 5;
    freeze.product.probability_type = Some(0);
    freeze.product.probability_lower_limit = Some(273.0);
    let mut hot = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[30.0], -99.0, -99.0);
    hot.product.template = 5;
    hot.product.probability_type = Some(1);
    hot.product.probability_upper_limit = Some(298.8);
    let grib = Grib2File {
        messages: vec![freeze, hot],
    };

    let freezing_probability = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2)
            .with_probability(ProbabilitySelection::below_milli(273_000)),
    )
    .unwrap();
    let hot_probability = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2)
            .with_probability(ProbabilitySelection::above_milli(298_800)),
    )
    .unwrap();

    assert_eq!(freezing_probability.values, vec![70.0]);
    assert_eq!(hot_probability.values, vec![30.0]);
    assert_eq!(freezing_probability.units, "%");
    assert_eq!(hot_probability.units, "%");
}

#[test]
fn extract_qmd_derived_mean_and_stddev_do_not_alias() {
    let mut mean = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[279.0], -99.0, -99.0);
    mean.product.template = 2;
    mean.product.derived_forecast_type = Some(0);
    let mut stddev = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[3.5], -99.0, -99.0);
    stddev.product.template = 2;
    stddev.product.derived_forecast_type = Some(2);
    let grib = Grib2File {
        messages: vec![stddev, mean],
    };

    let mean_field = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2).with_ensemble_mean(),
    )
    .unwrap();
    let stddev_field = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2)
            .with_ensemble_standard_deviation(),
    )
    .unwrap();

    assert_eq!(mean_field.values, vec![279.0]);
    assert_eq!(stddev_field.values, vec![3.5]);
}

#[test]
fn wmo_derived_code_four_is_spread_and_never_standard_deviation() {
    let mut spread = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[7.25], -99.0, -99.0);
    spread.product.template = 2;
    spread.product.derived_forecast_type = Some(4);
    let grib = Grib2File {
        messages: vec![spread],
    };

    let spread_field = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2).with_ensemble_spread(),
    )
    .unwrap();
    assert_eq!(spread_field.values, vec![7.25]);

    let error = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2)
            .with_ensemble_standard_deviation(),
    )
    .expect_err("WMO spread code 4 must not be relabeled as standard deviation");
    assert!(matches!(error, IoError::FieldNotFound { .. }));
}

#[test]
fn ensemble_mean_selector_accepts_weighted_mean_product() {
    let mut weighted_mean = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[281.0], -99.0, -99.0);
    weighted_mean.product.template = 2;
    weighted_mean.product.derived_forecast_type = Some(1);
    let grib = Grib2File {
        messages: vec![weighted_mean],
    };

    let field = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2).with_ensemble_mean(),
    )
    .unwrap();

    assert_eq!(field.values, vec![281.0]);
}

#[test]
fn default_selector_can_fallback_to_ensemble_mean_when_file_is_mean_product() {
    let mut mean = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[279.0], -99.0, -99.0);
    mean.product.template = 2;
    mean.product.derived_forecast_type = Some(0);
    let grib = Grib2File {
        messages: vec![mean],
    };

    let field = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
    )
    .unwrap();

    assert_eq!(field.selector.product, FieldProduct::Default);
    assert_eq!(field.values, vec![279.0]);
}

#[test]
fn default_selector_can_fallback_to_weighted_ensemble_mean_product() {
    let mut mean = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[281.0], -99.0, -99.0);
    mean.product.template = 2;
    mean.product.derived_forecast_type = Some(1);
    let grib = Grib2File {
        messages: vec![mean],
    };

    let field = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
    )
    .unwrap();

    assert_eq!(field.selector.product, FieldProduct::Default);
    assert_eq!(field.values, vec![281.0]);
}

#[test]
fn default_qpf_selector_can_fallback_to_ensemble_mean_accumulation() {
    let mut qpf = ieee_f32_message(
        PARAMETER_TOTAL_PRECIPITATION[0],
        1,
        0.0,
        &[12.7],
        -99.0,
        -99.0,
    );
    qpf.product.template = 8;
    qpf.product.derived_forecast_type = Some(1);
    let grib = Grib2File {
        messages: vec![qpf],
    };

    let field = extract_field_from_grib2(
        &grib,
        FieldSelector::surface(CanonicalField::TotalPrecipitation),
    )
    .unwrap();

    assert_eq!(field.selector.product, FieldProduct::Default);
    assert_eq!(field.values, vec![12.7]);
}

#[test]
fn default_temperature_selector_does_not_fallback_to_qmd_percentiles() {
    let mut percentile = ieee_f32_message(PARAMETER_TMP[0], 103, 2.0, &[280.0], -99.0, -99.0);
    percentile.product.template = 6;
    percentile.product.percentile_value = Some(50);
    let grib = Grib2File {
        messages: vec![percentile],
    };

    let err = extract_field_from_grib2(
        &grib,
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
    )
    .unwrap_err();

    assert!(matches!(err, IoError::FieldNotFound { .. }));
}

#[test]
fn structured_selector_accepts_standard_mslp_parameter_zero() {
    let message = ieee_f32_message(
        PARAMETER_MSLP[0],
        101,
        0.0,
        &[101000.0, 100750.0],
        261.0,
        262.0,
    );
    let grib = Grib2File {
        messages: vec![message],
    };

    let field = extract_field_from_grib2(
        &grib,
        FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel),
    )
    .unwrap();

    assert_eq!(field.values, vec![101000.0, 100750.0]);
}

#[test]
fn extract_field_from_grib2_returns_selector_backed_field() {
    // 500 hPa is encoded as 50_000 Pa per GRIB2 Code Table 4.5 level 100.
    let message = ieee_f32_message(
        PARAMETER_TMP[0],
        100,
        50_000.0,
        &[255.0, 256.5],
        261.0,
        262.0,
    );
    let grib = Grib2File {
        messages: vec![message],
    };

    let field = extract_pressure_field_from_grib2(&grib, CanonicalField::Temperature, 500).unwrap();

    assert_eq!(
        field.selector,
        FieldSelector::isobaric(CanonicalField::Temperature, 500)
    );
    assert_eq!(field.units, "K");
    assert_eq!(field.grid.shape.nx, 2);
    assert_eq!(field.grid.shape.ny, 1);
    assert_eq!(field.grid.lon_deg, vec![-99.0, -98.0]);
    assert_eq!(field.values, vec![255.0, 256.5]);
}

#[test]
fn nbm_speed_direction_messages_synthesize_10m_uv_components() {
    let direction = ieee_f32_message(
        PARAMETER_WIND_DIRECTION[0],
        103,
        10.0,
        &[0.0, 90.0, 180.0, 270.0],
        261.0,
        264.0,
    );
    let speed = ieee_f32_message(
        PARAMETER_WIND_SPEED[0],
        103,
        10.0,
        &[10.0, 10.0, 10.0, 10.0],
        261.0,
        264.0,
    );
    let grib = Grib2File {
        messages: vec![direction, speed],
    };
    let u_selector = FieldSelector::height_agl(CanonicalField::UWind, 10);
    let v_selector = FieldSelector::height_agl(CanonicalField::VWind, 10);

    let mut partial = extract_fields_from_grib2_partial(&grib, &[u_selector, v_selector])
        .expect("standard U/V messages are absent but partial extraction should soft-fail");
    assert_eq!(partial.missing, vec![u_selector, v_selector]);

    synthesize_nbm_10m_wind_components_from_speed_direction(&grib, &mut partial).unwrap();
    assert!(partial.missing.is_empty());

    let u = partial
        .extracted
        .iter()
        .find(|field| field.selector == u_selector)
        .expect("synthesized U component");
    let v = partial
        .extracted
        .iter()
        .find(|field| field.selector == v_selector)
        .expect("synthesized V component");

    assert_component_values(&u.values, &[0.0, -10.0, 0.0, 10.0]);
    assert_component_values(&v.values, &[-10.0, 0.0, 10.0, 0.0]);
}

fn assert_component_values(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected.iter()) {
        assert!(
            (*actual - *expected).abs() < 1.0e-4,
            "actual={actual} expected={expected}"
        );
    }
}

#[test]
fn extract_hybrid_level_volume_from_grib2_stacks_requested_levels() {
    let smoke_level_2 = ieee_f32_message(
        PARAMETER_SMOKE_MASS_DENSITY[0],
        105,
        2.0,
        &[0.3, 0.4],
        -99.0,
        -98.0,
    );
    let smoke_level_1 = ieee_f32_message(
        PARAMETER_SMOKE_MASS_DENSITY[0],
        105,
        1.0,
        &[0.1, 0.2],
        -99.0,
        -98.0,
    );
    let grib = Grib2File {
        messages: vec![smoke_level_2, smoke_level_1],
    };

    let volume =
        extract_hybrid_level_volume_from_grib2(&grib, CanonicalField::SmokeMassDensity, &[1, 2])
            .unwrap();

    assert_eq!(volume.field, CanonicalField::SmokeMassDensity);
    assert_eq!(volume.levels_hybrid, vec![1, 2]);
    assert_eq!(volume.units, "kg/m^3");
    assert_eq!(volume.level_slice(0), Some(&[0.1, 0.2][..]));
    assert_eq!(volume.level_slice(1), Some(&[0.3, 0.4][..]));
    assert_eq!(
        volume.selector_at(0),
        Some(FieldSelector::hybrid_level(
            CanonicalField::SmokeMassDensity,
            1
        ))
    );
}

#[test]
fn extract_hrrr_wrfnat_smoke_fields_returns_surface_column_and_hybrid_pairs() {
    let mut messages = Vec::new();
    for level in 1..=HRRR_WRFNAT_HYBRID_LEVEL_COUNT {
        messages.push(ieee_f32_message(
            PARAMETER_PRESSURE[0],
            105,
            f64::from(level),
            &[80_000.0 - level as f32, 79_000.0 - level as f32],
            -99.0,
            -98.0,
        ));

        let smoke_values = match level {
            1 => vec![0.1, 0.2],
            2 => vec![0.3, 0.4],
            _ => vec![level as f32, level as f32 + 0.5],
        };
        messages.push(ieee_f32_message(
            PARAMETER_SMOKE_MASS_DENSITY[0],
            105,
            f64::from(level),
            &smoke_values,
            -99.0,
            -98.0,
        ));
    }
    messages.push(ieee_f32_message(
        PARAMETER_SMOKE_MASS_DENSITY[0],
        103,
        8.0,
        &[1.5, 2.5],
        -99.0,
        -98.0,
    ));
    messages.push(ieee_f32_message(
        PARAMETER_COLUMN_INTEGRATED_SMOKE[0],
        200,
        0.0,
        &[3.5, 4.5],
        -99.0,
        -98.0,
    ));
    let grib = Grib2File { messages };

    let extracted = extract_hrrr_wrfnat_smoke_fields_from_grib2(&grib).unwrap();

    assert_eq!(extracted.hybrid_smoke.level_count(), 50);
    assert_eq!(extracted.hybrid_pressure.level_count(), 50);
    assert_eq!(
        extracted.near_surface_smoke.selector,
        FieldSelector::height_agl(CanonicalField::SmokeMassDensity, 8)
    );
    assert_eq!(
        extracted.column_smoke.selector,
        FieldSelector::entire_atmosphere(CanonicalField::ColumnIntegratedSmoke)
    );
    assert_eq!(extracted.hybrid_smoke.level_slice(0), Some(&[0.1, 0.2][..]));
    assert_eq!(extracted.hybrid_smoke.level_slice(1), Some(&[0.3, 0.4][..]));
    assert_eq!(
        extracted.hybrid_pressure.selector_at(49),
        Some(FieldSelector::hybrid_level(CanonicalField::Pressure, 50))
    );
    assert_eq!(extracted.near_surface_smoke.values, vec![1.5, 2.5]);
    assert_eq!(extracted.column_smoke.values, vec![3.5, 4.5]);
}

#[test]
fn extract_field_from_real_pressure_bytes_uses_structured_matching() {
    let path = sample_pressure_subset_path();
    if !path.exists() {
        eprintln!(
            "skipping real pressure subset test; fixture is not present at {}",
            path.display()
        );
        return;
    }
    let bytes = std::fs::read(&path).unwrap();

    let temp_500 =
        extract_pressure_field_from_bytes(&bytes, CanonicalField::Temperature, 500).unwrap();
    let temp_700 =
        extract_pressure_field_from_bytes(&bytes, CanonicalField::Temperature, 700).unwrap();
    let hgt_700 =
        extract_pressure_field_from_bytes(&bytes, CanonicalField::GeopotentialHeight, 700).unwrap();
    let hgt_850 =
        extract_pressure_field_from_bytes(&bytes, CanonicalField::GeopotentialHeight, 850).unwrap();
    let u_700 = extract_pressure_field_from_bytes(&bytes, CanonicalField::UWind, 700).unwrap();
    let v_700 = extract_pressure_field_from_bytes(&bytes, CanonicalField::VWind, 700).unwrap();

    assert_eq!(
        temp_500.selector,
        FieldSelector::isobaric(CanonicalField::Temperature, 500)
    );
    assert_eq!(
        temp_700.selector,
        FieldSelector::isobaric(CanonicalField::Temperature, 700)
    );
    assert_eq!(temp_500.units, "K");
    assert_eq!(temp_700.units, "K");
    assert_eq!(hgt_700.units, "gpm");
    assert_eq!(hgt_850.units, "gpm");
    assert_eq!(u_700.units, "m/s");
    assert_eq!(v_700.units, "m/s");
    assert_eq!(temp_700.grid.shape, hgt_700.grid.shape);
    assert_eq!(temp_700.grid.shape, u_700.grid.shape);
    assert_eq!(u_700.grid.shape, v_700.grid.shape);
    assert_eq!(temp_500.grid.shape, hgt_850.grid.shape);
    assert_eq!(temp_500.values.len(), temp_500.grid.shape.len());
    assert_eq!(temp_700.values.len(), temp_700.grid.shape.len());
    assert_eq!(hgt_700.values.len(), hgt_700.grid.shape.len());
    assert_eq!(hgt_850.values.len(), hgt_850.grid.shape.len());
    assert_eq!(u_700.values.len(), u_700.grid.shape.len());
    assert_eq!(v_700.values.len(), v_700.grid.shape.len());
    assert!(temp_500.values.iter().any(|value| value.is_finite()));
    assert!(temp_700.values.iter().any(|value| value.is_finite()));
    assert!(hgt_700.values.iter().any(|value| value.is_finite()));
    assert!(hgt_850.values.iter().any(|value| value.is_finite()));
    assert!(u_700.values.iter().any(|value| value.is_finite()));
    assert!(v_700.values.iter().any(|value| value.is_finite()));
}

#[test]
fn extract_fields_from_real_pressure_bytes_batches_parse_and_matching() {
    let path = sample_pressure_subset_path();
    if !path.exists() {
        eprintln!(
            "skipping real pressure subset batch test; fixture is not present at {}",
            path.display()
        );
        return;
    }
    let bytes = std::fs::read(&path).unwrap();
    let selectors = [
        FieldSelector::isobaric(CanonicalField::Temperature, 500),
        FieldSelector::isobaric(CanonicalField::Temperature, 700),
        FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 700),
        FieldSelector::isobaric(CanonicalField::UWind, 700),
        FieldSelector::isobaric(CanonicalField::VWind, 700),
    ];

    let batched = extract_fields_from_bytes(&bytes, &selectors).unwrap();

    assert_eq!(batched.len(), selectors.len());
    for (selector, field) in selectors.iter().zip(batched.iter()) {
        assert_eq!(&field.selector, selector);
    }

    let single_temp_500 =
        extract_pressure_field_from_bytes(&bytes, CanonicalField::Temperature, 500).unwrap();
    let single_hgt_700 =
        extract_pressure_field_from_bytes(&bytes, CanonicalField::GeopotentialHeight, 700).unwrap();
    let single_u_700 =
        extract_pressure_field_from_bytes(&bytes, CanonicalField::UWind, 700).unwrap();

    assert_eq!(batched[0], single_temp_500);
    assert_eq!(batched[2], single_hgt_700);
    assert_eq!(batched[3], single_u_700);
}

#[test]
fn normalize_and_rotate_longitude_rows_keeps_rows_monotone() {
    let mut lat = vec![40.0, 40.0, 40.0, 40.0, 39.0, 39.0, 39.0, 39.0];
    let mut lon = vec![0.0, 90.0, 180.0, 270.0, 0.0, 90.0, 180.0, 270.0];
    let mut values = vec![1.0, 2.0, 3.0, 4.0, 11.0, 12.0, 13.0, 14.0];

    let row_wraps = normalize_and_rotate_longitude_grid_rows(&mut lat, &mut lon, 4, 2);
    rotate_rows_left(&mut values, 4, &row_wraps);

    assert_eq!(row_wraps, [3, 3]);
    assert_eq!(lon[..4], [-90.0, 0.0, 90.0, 180.0]);
    assert_eq!(lon[4..], [-90.0, 0.0, 90.0, 180.0]);
    assert_eq!(values[..4], [4.0, 1.0, 2.0, 3.0]);
    assert_eq!(values[4..], [14.0, 11.0, 12.0, 13.0]);
    assert_eq!(lat[..4], [40.0, 40.0, 40.0, 40.0]);
    assert_eq!(lat[4..], [39.0, 39.0, 39.0, 39.0]);
}

#[test]
fn icon_ru_dateline_crossing_rows_rotate_coordinates_and_values_together() {
    const NX: usize = 697;
    const NY: usize = 2;
    let mut lat = Vec::with_capacity(NX * NY);
    let mut lon = Vec::with_capacity(NX * NY);
    let mut values = Vec::with_capacity(NX * NY);
    for row in 0..NY {
        for column in 0..NX {
            lat.push(35.0 + row as f64 * 0.25);
            lon.push(19.5 + column as f64 * 0.25);
            values.push((row * 1_000 + column) as f64);
        }
    }

    let row_wraps = normalize_and_rotate_longitude_grid_rows(&mut lat, &mut lon, NX, NY);
    rotate_rows_left(&mut values, NX, &row_wraps);

    assert_eq!(row_wraps, [643, 643]);
    for row in 0..NY {
        let start = row * NX;
        let lon_row = &lon[start..start + NX];
        assert!(lon_row.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(lon_row.first(), Some(&-179.75));
        assert_eq!(lon_row.get(53), Some(&-166.5));
        assert_eq!(lon_row.get(54), Some(&19.5));
        assert_eq!(lon_row.last(), Some(&180.0));
        assert_eq!(values[start], (row * 1_000 + 643) as f64);
        assert_eq!(values[start + 53], (row * 1_000 + 696) as f64);
        assert_eq!(values[start + 54], (row * 1_000) as f64);
        assert_eq!(values[start + NX - 1], (row * 1_000 + 642) as f64);
    }
}

#[test]
fn eccc_cyclic_equal_endpoint_grid_retains_global_longitudes() {
    let grid = GridDefinition {
        template: 0,
        nx: 721,
        ny: 360,
        lat1: -90.0,
        lon1: 180.0,
        lat2: 89.5,
        lon2: 180.0,
        dx: 0.5,
        dy: 0.5,
        scan_mode: 0x40,
        ..Default::default()
    };
    let (mut lat, mut lon) = grid_latlon(&grid);
    flip_rows(&mut lat, 721, 360);
    flip_rows(&mut lon, 721, 360);
    let row_wraps = normalize_and_rotate_longitude_grid_rows(&mut lat, &mut lon, 721, 360);

    assert!(row_wraps.iter().all(|wrap| *wrap == 1));
    assert_eq!(lat[0], 89.5);
    assert_eq!(lat[721 * 359], -90.0);
    assert_eq!(lon[0], -179.5);
    assert_eq!(lon[359], 0.0);
    assert_eq!(lon[719], 180.0);
    assert_eq!(lon[720], 180.0);
    assert!(lon[..721].windows(2).all(|pair| pair[1] >= pair[0]));
    assert_eq!(
        lon[..721]
            .windows(2)
            .filter(|pair| pair[1] > pair[0])
            .count(),
        719
    );
}
