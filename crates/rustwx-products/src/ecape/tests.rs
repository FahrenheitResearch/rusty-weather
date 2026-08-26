//! Regression fixtures for the effective-layer STP published under
//! `EcapeStpExperimental`.
//!
//! The composite is defined on the *effective inflow layer* (Thompson, Mead &
//! Edwards 2007, Wea. Forecasting 22, 102-115). These fixtures pin the wiring:
//! the published field must be the composite built from effective-layer
//! kinematics, and on a profile where the effective layer and the fixed layers
//! disagree it must not coincide with the fixed 0-1 km SRH / 0-6 km shear
//! version of the same formula.

use super::*;
use crate::gridded::{PressureFields, SurfaceFields};
use rustwx_calc::compute_effective_layer_diagnostics;

/// Single-column deep-tropospheric supercell sounding.
///
/// Two properties make it a useful discriminator:
///
/// * the effective inflow layer resolves as 0-1500 m, so effective SRH samples
///   a deeper slab than the fixed 0-1 km layer on a hodograph that veers
///   sharply through the lowest 1.5 km;
/// * the profile reaches 100 hPa, so the MU parcel equilibrium level - and
///   therefore the half-storm-depth EBWD top - resolves inside the sounding
///   instead of falling off the top.
fn deep_supercell_profile() -> (SurfaceFields, PressureFields) {
    let pressure_levels_hpa = vec![
        950.0, 900.0, 850.0, 700.0, 500.0, 300.0, 200.0, 150.0, 100.0,
    ];
    let temperature_c = vec![26.0, 22.0, 18.0, 8.0, -10.0, -38.0, -54.0, -64.0, -62.0];
    let qvapor_kgkg = vec![
        0.016, 0.013, 0.010, 0.005, 0.0015, 0.0003, 0.00008, 0.00004, 0.00002,
    ];
    // Orography is zero, so geopotential height doubles as height AGL.
    let height_m = vec![
        150.0, 800.0, 1500.0, 3000.0, 5600.0, 9200.0, 11_800.0, 13_600.0, 16_200.0,
    ];
    let u_ms = vec![-4.0, 2.0, 9.0, 18.0, 26.0, 33.0, 40.0, 44.0, 46.0];
    let v_ms = vec![10.0, 11.0, 9.0, 6.0, 2.0, -4.0, -8.0, -10.0, -12.0];

    let surface = SurfaceFields {
        lat: vec![35.0],
        lon: vec![-97.0],
        nx: 1,
        ny: 1,
        projection: None,
        psfc_pa: vec![100_000.0],
        orog_m: vec![0.0],
        orog_is_proxy: false,
        t2_k: vec![303.15],
        q2_kgkg: vec![0.018],
        u10_ms: vec![-6.0],
        v10_ms: vec![9.0],
        native_sbcape_jkg: None,
        native_mlcape_jkg: None,
        native_mucape_jkg: None,
        native_pblh_m: None,
    };
    let pressure = PressureFields {
        pressure_levels_hpa,
        pressure_3d_pa: None,
        temperature_c_3d: temperature_c,
        qvapor_kgkg_3d: qvapor_kgkg,
        u_ms_3d: u_ms,
        v_ms_3d: v_ms,
        gh_m_3d: height_m,
        omega_pa_s_3d: None,
        absolute_vorticity_s_3d: None,
        cloud_liquid_kgkg_3d: None,
        cloud_ice_kgkg_3d: None,
        rain_kgkg_3d: None,
        snow_kgkg_3d: None,
        graupel_kgkg_3d: None,
    };
    (surface, pressure)
}

struct ProfileDiagnostics {
    published_stp: Vec<f64>,
    stp_from_effective_layer: Vec<f64>,
    stp_from_fixed_layers: Vec<f64>,
    srh_01km_m2s2: Vec<f64>,
    shear_06km_ms: Vec<f64>,
    effective_srh_m2s2: Vec<f64>,
    effective_bulk_wind_difference_ms: Vec<f64>,
    effective_inflow_layer_shear_ms: Vec<f64>,
}

/// Run the published product path, then rebuild the same composite twice by
/// hand - once from effective-layer kinematics, once from the fixed-layer pair
/// the old wiring used - holding the thermodynamic ingredients fixed so the
/// only difference between the two is the layer definition.
fn diagnose(surface: &SurfaceFields, pressure: &PressureFields) -> ProfileDiagnostics {
    let prepared = prepare_heavy_volume(surface, pressure, false).unwrap();
    let volume = EcapeVolumeInputs {
        pressure_pa: prepared
            .pressure_3d_pa
            .as_deref()
            .unwrap_or(&prepared.pressure_levels_pa),
        temperature_c: &pressure.temperature_c_3d,
        qvapor_kgkg: &pressure.qvapor_kgkg_3d,
        height_agl_m: &prepared.height_agl_3d,
        u_ms: &pressure.u_ms_3d,
        v_ms: &pressure.v_ms_3d,
        nz: prepared.shape.nz,
    };
    let surface_inputs = SurfaceInputs {
        psfc_pa: &surface.psfc_pa,
        t2_k: &surface.t2_k,
        q2_kgkg: &surface.q2_kgkg,
        u10_ms: &surface.u10_ms,
        v10_ms: &surface.v10_ms,
    };

    let wind = compute_wind_diagnostics_bundle(WindGridInputs {
        shape: prepared.shape,
        u_3d_ms: &pressure.u_ms_3d,
        v_3d_ms: &pressure.v_ms_3d,
        height_agl_3d_m: &prepared.height_agl_3d,
    })
    .unwrap();
    let effective =
        compute_effective_layer_diagnostics(prepared.grid, volume, surface_inputs, None).unwrap();
    let ml_classic = compute_mlcape_cin(prepared.grid, volume, surface_inputs, None).unwrap();

    let (fields, _failures) = compute_ecape_map_fields(surface, pressure).unwrap();
    let field = |product: WeatherProduct| -> Vec<f64> {
        fields
            .iter()
            .find(|candidate| candidate.product == product)
            .unwrap_or_else(|| panic!("{product:?} must be published"))
            .values
            .clone()
    };
    // The STP's thermodynamic ingredients are themselves published, so the
    // reconstruction below shares them exactly with the product path.
    let mlcape_jkg = field(WeatherProduct::Mlecape);
    let mlcin_jkg = field(WeatherProduct::Mlecin);

    let stp_with = |srh: &[f64], bwd: &[f64]| {
        compute_stp_effective(EffectiveStpInputs {
            grid: prepared.grid,
            mlcape_jkg: &mlcape_jkg,
            mlcin_jkg: &mlcin_jkg,
            ml_lcl_m: &ml_classic.lcl_m,
            effective_srh_m2s2: srh,
            effective_bulk_wind_difference_ms: bwd,
        })
        .unwrap()
    };

    ProfileDiagnostics {
        published_stp: field(WeatherProduct::EcapeStpExperimental),
        stp_from_effective_layer: stp_with(
            &effective.effective_srh_m2s2,
            &effective.effective_bulk_wind_difference_ms,
        ),
        stp_from_fixed_layers: stp_with(&wind.srh_01km_m2s2, &wind.shear_06km_ms),
        srh_01km_m2s2: wind.srh_01km_m2s2,
        shear_06km_ms: wind.shear_06km_ms,
        effective_srh_m2s2: effective.effective_srh_m2s2,
        effective_bulk_wind_difference_ms: effective.effective_bulk_wind_difference_ms,
        effective_inflow_layer_shear_ms: effective.effective_inflow_layer_shear_ms,
    }
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1.0e-6,
        "expected {expected}, got {actual}"
    );
}

/// The fixture only proves anything if the two layer definitions really do
/// disagree on this sounding. Pin that separately from the wiring assertion so
/// a future profile edit that quietly collapses the difference fails loudly
/// here instead of turning the wiring test into a tautology.
#[test]
fn effective_and_fixed_layer_kinematics_differ_on_the_fixture_profile() {
    let (surface, pressure) = deep_supercell_profile();
    let diagnostics = diagnose(&surface, &pressure);

    assert_close(diagnostics.srh_01km_m2s2[0], 102.832_674_013_883_83);
    assert_close(diagnostics.effective_srh_m2s2[0], 178.341_629_094_700_86);
    assert_close(diagnostics.shear_06km_ms[0], 31.974_719_952_634_19);
    assert_close(
        diagnostics.effective_bulk_wind_difference_ms[0],
        35.280_979_561_827_05,
    );
    // The effective inflow layer's own shear is a third, distinct quantity: it
    // is neither the 0-6 km shear nor the half-storm-depth EBWD, which is why
    // it is reported separately and never fed to a composite.
    assert_close(diagnostics.effective_inflow_layer_shear_ms[0], 15.0);
}

/// `ecape_stp_experimental` must be the effective-layer composite.
///
/// Before the fix this call site handed `compute_stp_effective` the fixed-layer
/// pair (0-1 km SRH, 0-6 km bulk shear), publishing a fixed-layer STP under the
/// effective-layer name.
#[test]
fn published_ecape_stp_is_built_from_effective_layer_kinematics() {
    let (surface, pressure) = deep_supercell_profile();
    let diagnostics = diagnose(&surface, &pressure);

    assert_close(
        diagnostics.stp_from_effective_layer[0],
        2.387_077_509_047_596_6,
    );
    assert_close(
        diagnostics.stp_from_fixed_layers[0],
        1.376_400_813_314_422_8,
    );

    assert_close(
        diagnostics.published_stp[0],
        diagnostics.stp_from_effective_layer[0],
    );
    assert!(
        (diagnostics.published_stp[0] - diagnostics.stp_from_fixed_layers[0]).abs() > 1.0,
        "published effective STP {} must not be the fixed-layer STP {}",
        diagnostics.published_stp[0],
        diagnostics.stp_from_fixed_layers[0]
    );
}
