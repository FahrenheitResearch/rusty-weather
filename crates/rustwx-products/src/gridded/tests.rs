use super::*;

#[test]
fn hrrr_defaults_to_split_surface_and_pressure_products() {
    let (surface, pressure) = thermo_bundles(ModelId::Hrrr, None, None);
    assert_eq!(surface.bundle, CanonicalBundleDescriptor::SurfaceAnalysis);
    assert_eq!(surface.family, CanonicalDataFamily::Surface);
    assert_eq!(surface.native_product, "sfc");
    assert_eq!(pressure.bundle, CanonicalBundleDescriptor::PressureAnalysis);
    assert_eq!(pressure.family, CanonicalDataFamily::Pressure);
    assert_eq!(pressure.native_product, "prs");
}

#[test]
fn global_models_default_to_single_full_family_product() {
    let (gfs_surface, gfs_pressure) = thermo_bundles(ModelId::Gfs, None, None);
    assert_eq!(gfs_surface.native_product, "pgrb2.0p25");
    assert_eq!(gfs_pressure.native_product, "pgrb2.0p25");

    let (ecmwf_surface, ecmwf_pressure) = thermo_bundles(ModelId::EcmwfOpenData, None, None);
    assert_eq!(ecmwf_surface.native_product, "oper");
    assert_eq!(ecmwf_pressure.native_product, "oper");

    let (rrfs_surface, rrfs_pressure) = thermo_bundles(ModelId::RrfsA, None, None);
    assert_eq!(rrfs_surface.native_product, "nat-na");
    assert_eq!(rrfs_pressure.native_product, "prs-na");
}

#[test]
fn thermo_bundle_fetch_patterns_use_idx_subsetting() {
    assert_eq!(
        bundle_fetch_variable_patterns(
            ModelId::RrfsA,
            CanonicalBundleDescriptor::SurfaceAnalysis,
            "nat-na"
        ),
        surface_analysis_fetch_patterns(ModelId::RrfsA)
    );
    assert_eq!(
        bundle_fetch_variable_patterns(
            ModelId::RrfsA,
            CanonicalBundleDescriptor::PressureAnalysis,
            "prs-na"
        ),
        pressure_analysis_fetch_patterns(ModelId::RrfsA)
    );
    assert_eq!(
        bundle_fetch_variable_patterns(
            ModelId::Hrrr,
            CanonicalBundleDescriptor::SurfaceAnalysis,
            "sfc"
        ),
        surface_analysis_fetch_patterns(ModelId::Hrrr)
    );
    assert_eq!(
        bundle_fetch_variable_patterns(
            ModelId::Hrrr,
            CanonicalBundleDescriptor::PressureAnalysis,
            "prs"
        ),
        pressure_analysis_fetch_patterns(ModelId::Hrrr)
    );
    assert_eq!(
        bundle_fetch_variable_patterns(
            ModelId::Gfs,
            CanonicalBundleDescriptor::PressureAnalysis,
            "pgrb2.0p25"
        ),
        pressure_analysis_fetch_patterns(ModelId::Gfs)
    );
}

#[test]
fn hrrr_native_surface_patterns_include_visibility() {
    let patterns = bundle_fetch_variable_patterns(
        ModelId::Hrrr,
        CanonicalBundleDescriptor::NativeAnalysis,
        "sfc",
    );
    assert!(patterns.contains(&"VIS:surface".to_string()));
}

#[test]
fn rap_pressure_bundle_uses_full_family_fetch() {
    assert!(
        bundle_fetch_variable_patterns(
            ModelId::Rap,
            CanonicalBundleDescriptor::PressureAnalysis,
            "awp130pgrb",
        )
        .is_empty()
    );
}

#[test]
fn rap_same_file_surface_pressure_merge_uses_full_fetch() {
    assert!(
        merge_variable_patterns([
            surface_analysis_fetch_patterns(ModelId::Rap),
            pressure_analysis_fetch_patterns(ModelId::Rap),
        ])
        .is_empty()
    );
}

#[test]
fn product_overrides_replace_defaults() {
    let (surface, pressure) = thermo_bundles(ModelId::RrfsA, Some("prs-na"), Some("prs-na"));
    assert_eq!(surface.native_product, "prs-na");
    assert_eq!(pressure.native_product, "prs-na");
}

#[test]
fn mixing_ratio_fallbacks_produce_positive_values() {
    let dewpoint = mixing_ratio_from_dewpoint_k(1000.0, 293.15);
    let rh = mixing_ratio_from_relative_humidity(1000.0, 298.15, 65.0);
    assert!(dewpoint > 0.0);
    assert!(rh > 0.0);
}

#[test]
fn pressure_optional_decode_env_parser_defaults_on() {
    assert!(pressure_optional_decode_enabled_from_env_value(None));
    assert!(pressure_optional_decode_enabled_from_env_value(Some(
        "1".to_string()
    )));
    assert!(pressure_optional_decode_enabled_from_env_value(Some(
        "true".to_string()
    )));
    assert!(!pressure_optional_decode_enabled_from_env_value(Some(
        "0".to_string()
    )));
    assert!(!pressure_optional_decode_enabled_from_env_value(Some(
        "false".to_string()
    )));
    assert!(!pressure_optional_decode_enabled_from_env_value(Some(
        " off ".to_string()
    )));
}

#[test]
fn hrrr_core_pressure_fetch_patterns_omit_optional_volumes() {
    let core = hrrr_pressure_analysis_fetch_patterns(false);
    assert_eq!(core, vec!["HGT", "TMP", "SPFH", "UGRD", "VGRD"]);

    let full = hrrr_pressure_analysis_fetch_patterns(true);
    assert!(full.contains(&"VVEL"));
    assert!(full.contains(&"ABSV"));
    assert!(full.contains(&"CLWMR"));
    assert!(full.len() > core.len());
}

#[test]
fn pressure_decode_cache_name_includes_optional_policy() {
    assert_eq!(
        pressure_decode_cache_name_from_optional_enabled(false),
        "pressure_core"
    );
    assert_eq!(
        pressure_decode_cache_name_from_optional_enabled(true),
        "pressure_optional"
    );
}

#[test]
fn cropped_row_window_rotation_matches_full_rotate_then_crop() {
    let nx = 5usize;
    let crop = GridCrop {
        x_start: 1,
        x_end: 4,
        y_start: 0,
        y_end: 2,
    };
    let row_wraps = vec![2usize, 1usize];
    let mut full = (0..10).map(|value| value as f64).collect::<Vec<_>>();
    full[0..5].rotate_left(row_wraps[0]);
    full[5..10].rotate_left(row_wraps[1]);
    let expected = crop_2d_values(&full, nx, crop);

    let mut window = (0..10).map(|value| value as f64).collect::<Vec<_>>();
    rotate_window_values_to_normalized_longitude_rows(
        &mut window,
        nx,
        crop.y_start,
        crop.y_end,
        &row_wraps,
    );
    let actual = crop_window_x_values(&window, nx, crop);

    assert_eq!(actual, expected);
}

fn synthetic_store_thermo_files(scan_mode: u8) -> (Grib2File, Grib2File) {
    use grib_core::grib2::{DataRepresentation, ProductDefinition};

    let grid = GridDefinition {
        nx: 5,
        ny: 4,
        lat1: 42.0,
        lon1: 170.0,
        lat2: 30.0,
        lon2: -150.0,
        scan_mode,
        num_data_points: 20,
        ..GridDefinition::default()
    };
    let make = |category: u8,
                number: u8,
                level_type: u8,
                level_value: f64,
                reference_value: f32,
                decimal_scale: i16,
                seed: u8| {
        let bitmap: Vec<bool> = (0..grid.num_data_points)
            // Source x=3/4 rotate into the cropped x=1/2 columns on this
            // date-line-crossing grid; both source rows stay inside the crop
            // under either north/south scan direction.
            .map(|index| index != 8 && index != 14)
            .collect();
        let raw_data = (0..grid.num_data_points)
            .filter(|&index| bitmap[index as usize])
            .map(|offset| seed + offset as u8)
            .collect();
        Grib2Message {
            discipline: 0,
            reference_time: chrono::NaiveDate::from_ymd_opt(2026, 6, 11)
                .unwrap()
                .and_hms_opt(16, 0, 0)
                .unwrap(),
            grid: grid.clone(),
            product: ProductDefinition {
                parameter_category: category,
                parameter_number: number,
                level_type,
                level_value,
                ..ProductDefinition::default()
            },
            data_rep: DataRepresentation {
                reference_value,
                decimal_scale,
                bits_per_value: 8,
                section5_num_data_points: grid.num_data_points,
                ..DataRepresentation::default()
            },
            bitmap: Some(bitmap),
            raw_data,
        }
    };
    let surface = Grib2File {
        messages: vec![
            make(3, 0, 1, 0.0, 90_000.0, 0, 1),
            make(3, 5, 1, 0.0, 100.0, 0, 2),
            make(0, 0, 103, 2.0, 270.0, 0, 3),
            make(1, 0, 103, 2.0, 0.0, 4, 4),
            make(2, 2, 103, 10.0, -20.0, 0, 5),
            make(2, 3, 103, 10.0, -10.0, 0, 6),
            make(7, 6, 1, 0.0, 1_000.0, 0, 7),
            make(3, 18, 1, 0.0, 500.0, 0, 8),
        ],
    };
    let mut pressure = Vec::new();
    for (index, level) in [100_000.0, 90_000.0].into_iter().enumerate() {
        let seed = 20 + index as u8 * 20;
        pressure.extend([
            make(0, 0, 100, level, 250.0, 0, seed),
            make(1, 0, 100, level, 0.0, 4, seed + 1),
            make(2, 2, 100, level, -40.0, 0, seed + 2),
            make(2, 3, 100, level, -30.0, 0, seed + 3),
            make(3, 5, 100, level, 0.0, 0, seed + 4),
        ]);
    }
    (surface, Grib2File { messages: pressure })
}

fn assert_f64_bits_eq(actual: &[f64], expected: &[f64], context: &str) {
    assert_eq!(actual.len(), expected.len(), "{context}: length");
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(
            actual.to_bits(),
            expected.to_bits(),
            "{context}: value {index}"
        );
    }
}

#[test]
fn cropped_store_thermo_decode_matches_full_decode_then_crop_across_scan_modes() {
    let crop = GridCrop {
        x_start: 1,
        x_end: 4,
        y_start: 1,
        y_end: 3,
    };
    for scan_mode in [0x00, 0x40] {
        let context = format!("scan mode 0x{scan_mode:02x}");
        let (surface_file, pressure_file) = synthetic_store_thermo_files(scan_mode);
        let full_surface = decode_surface_file(surface_file.clone()).unwrap();
        let (full_pressure, _, _) =
            decode_pressure_file_with_shape_opts(pressure_file.clone(), false).unwrap();
        let expected = crop_heavy_domain_with(&full_surface, &full_pressure, crop).unwrap();
        let actual_surface = decode_surface_file_cropped(surface_file, crop).unwrap();
        let (actual_pressure, nx, ny) =
            decode_pressure_file_cropped_with_shape_opts(pressure_file, crop, false).unwrap();

        assert_eq!((nx, ny), (crop.width(), crop.height()), "{context}");
        assert_eq!(
            actual_surface.projection, expected.surface.projection,
            "{context}"
        );
        assert!(
            actual_surface.psfc_pa.iter().any(|value| value.is_nan()),
            "{context}"
        );
        assert!(
            actual_pressure
                .temperature_c_3d
                .iter()
                .any(|value| value.is_nan()),
            "{context}"
        );
        for (name, actual, expected) in [
            ("lat", &actual_surface.lat, &expected.surface.lat),
            ("lon", &actual_surface.lon, &expected.surface.lon),
            ("psfc", &actual_surface.psfc_pa, &expected.surface.psfc_pa),
            ("orog", &actual_surface.orog_m, &expected.surface.orog_m),
            ("t2", &actual_surface.t2_k, &expected.surface.t2_k),
            ("q2", &actual_surface.q2_kgkg, &expected.surface.q2_kgkg),
            ("u10", &actual_surface.u10_ms, &expected.surface.u10_ms),
            ("v10", &actual_surface.v10_ms, &expected.surface.v10_ms),
            (
                "sbcape",
                actual_surface.native_sbcape_jkg.as_ref().unwrap(),
                expected.surface.native_sbcape_jkg.as_ref().unwrap(),
            ),
            (
                "pblh",
                actual_surface.native_pblh_m.as_ref().unwrap(),
                expected.surface.native_pblh_m.as_ref().unwrap(),
            ),
        ] {
            assert_f64_bits_eq(actual, expected, &format!("{context}: {name}"));
        }
        assert_f64_bits_eq(
            &actual_pressure.pressure_levels_hpa,
            &expected.pressure.pressure_levels_hpa,
            &format!("{context}: levels"),
        );
        for (name, actual, expected) in [
            (
                "temperature",
                &actual_pressure.temperature_c_3d,
                &expected.pressure.temperature_c_3d,
            ),
            (
                "moisture",
                &actual_pressure.qvapor_kgkg_3d,
                &expected.pressure.qvapor_kgkg_3d,
            ),
            ("u", &actual_pressure.u_ms_3d, &expected.pressure.u_ms_3d),
            ("v", &actual_pressure.v_ms_3d, &expected.pressure.v_ms_3d),
            (
                "height",
                &actual_pressure.gh_m_3d,
                &expected.pressure.gh_m_3d,
            ),
        ] {
            assert_f64_bits_eq(actual, expected, &format!("{context}: {name}"));
        }
        assert!(actual_pressure.omega_pa_s_3d.is_none(), "{context}");
        assert!(actual_pressure.cloud_liquid_kgkg_3d.is_none(), "{context}");
    }
}

#[test]
fn projected_crop_uses_projected_extent_with_padding() {
    let nx = 4usize;
    let ny = 4usize;
    let len = nx * ny;
    let surface = SurfaceFields {
        lat: vec![35.0; len],
        lon: vec![-97.0; len],
        nx,
        ny,
        projection: Some(GridProjection::LambertConformal {
            standard_parallel_1_deg: 38.5,
            standard_parallel_2_deg: 38.5,
            central_meridian_deg: -97.5,
        }),
        psfc_pa: vec![100000.0; len],
        orog_m: vec![300.0; len],
        orog_is_proxy: false,
        t2_k: vec![295.0; len],
        q2_kgkg: vec![0.012; len],
        u10_ms: vec![10.0; len],
        v10_ms: vec![5.0; len],
        native_sbcape_jkg: None,
        native_mlcape_jkg: None,
        native_mucape_jkg: None,
        native_pblh_m: None,
    };
    let pressure = PressureFields {
        pressure_levels_hpa: vec![1000.0],
        pressure_3d_pa: None,
        temperature_c_3d: vec![20.0; len],
        qvapor_kgkg_3d: vec![0.010; len],
        u_ms_3d: vec![10.0; len],
        v_ms_3d: vec![5.0; len],
        gh_m_3d: vec![1500.0; len],
        omega_pa_s_3d: None,
        absolute_vorticity_s_3d: None,
        cloud_liquid_kgkg_3d: None,
        cloud_ice_kgkg_3d: None,
        rain_kgkg_3d: None,
        snow_kgkg_3d: None,
        graupel_kgkg_3d: None,
    };
    let projected_x = vec![
        0.0, 1.0, 2.0, 3.0, 0.0, 1.0, 2.0, 3.0, 0.0, 1.0, 2.0, 3.0, 0.0, 1.0, 2.0, 3.0,
    ];
    let projected_y = vec![
        0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0, 3.0, 3.0, 3.0, 3.0,
    ];
    let extent = ProjectedExtent {
        x_min: 1.0,
        x_max: 1.0,
        y_min: 1.0,
        y_max: 1.0,
    };

    let cropped = crop_heavy_domain_for_projected_extent(
        &surface,
        &pressure,
        &projected_x,
        &projected_y,
        &extent,
        1,
    )
    .expect("crop should succeed")
    .expect("crop should reduce to a padded subset");

    assert_eq!(cropped.surface.nx, 3);
    assert_eq!(cropped.surface.ny, 3);
    assert_eq!(cropped.grid.shape.nx, 3);
    assert_eq!(cropped.grid.shape.ny, 3);
}
