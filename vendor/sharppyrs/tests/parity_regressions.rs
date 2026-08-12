//! Focused release gates for SHARPpy-equivalent edge semantics.

use sharppyrs::sharprs::params::cape::{LiftedParcelLevel, ParcelType};
use sharppyrs::sharprs::thermo;
use sharppyrs::{DerivedParams, Profile, SoundingData};

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/testdata/hrrr_example.rs"
));

fn close(actual: f64, expected: f64, tolerance: f64, label: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{label}: got {actual}, expected {expected} +/- {tolerance}"
    );
}

fn bufkit_case(stim: u32) -> SoundingData {
    let raw = include_str!("testdata/hrrr_kbvo_20260625_06z.buf");
    let marker = format!("STIM = {stim}");
    let block = raw
        .split(&marker)
        .nth(1)
        .expect("requested BUFKIT forecast block")
        .split("\nSTID =")
        .next()
        .unwrap();
    let mut lines = block.lines();
    while lines.next().is_some_and(|line| line.trim() != "CFRL HGHT") {}

    let mut pres = Vec::new();
    let mut hght = Vec::new();
    let mut tmpc = Vec::new();
    let mut dwpc = Vec::new();
    let mut wdir = Vec::new();
    let mut wspd = Vec::new();
    let mut omeg = Vec::new();
    loop {
        let Some(first) = lines.next() else { break };
        if first.trim().is_empty() {
            break;
        }
        let values: Vec<f64> = first
            .split_whitespace()
            .map(|value| value.parse().unwrap())
            .collect();
        if values.len() != 8 {
            break;
        }
        let height_line = lines.next().expect("BUFKIT CFRL/HGHT row");
        let height: Vec<f64> = height_line
            .split_whitespace()
            .map(|value| value.parse().unwrap())
            .collect();
        pres.push(values[0]);
        tmpc.push(values[1]);
        dwpc.push(values[3]);
        wdir.push(values[5]);
        wspd.push(values[6]);
        omeg.push(values[7]);
        hght.push(height[1]);
    }
    assert_eq!(pres.len(), 50);
    SoundingData {
        pres,
        hght,
        tmpc,
        dwpc,
        wdir,
        wspd,
        omeg: Some(omeg),
        latitude: Some(36.76),
        longitude: Some(-96.01),
        missing: Some(-9999.0),
    }
}

#[test]
fn hrrr_mpl_and_public_thermodynamics_match_legacy() {
    let profile = Profile::new(hrrr_example()).unwrap();
    close(profile.sfcpcl.mplpres, 93.0, 1.0e-9, "SFC MPL pressure");
    close(profile.mupcl.mplpres, 93.0, 1.0e-9, "MU MPL pressure");
    close(profile.mlpcl.mplpres, 94.0, 1.0e-9, "ML MPL pressure");
    close(profile.fcstpcl.mplpres, 74.0, 1.0e-9, "FCST MPL pressure");

    let top = profile.inner.top;
    close(profile.inner.vtmp[top], -58.01839416565872, 1.0e-9, "vtmp");
    close(profile.inner.theta[top], 506.318709645886, 1.0e-9, "theta");
    close(
        profile.inner.thetae[top],
        481.4473886260743,
        1.0e-9,
        "thetae",
    );
    close(
        profile.inner.wvmr[top],
        0.011613879645246603,
        1.0e-12,
        "wvmr",
    );
    close(profile.inner.relh[top], 3.823807641140326, 1.0e-9, "relh");
    close(
        profile.inner.wetbulb[top],
        -59.043616414069454,
        1.0e-9,
        "wetbulb",
    );
}

#[test]
fn sparse_upper_bufkit_keeps_el_mpl_and_effective_layer() {
    let profile = Profile::new(bufkit_case(41)).unwrap();
    close(profile.ebottom, 982.7, 1.0e-9, "effective bottom");
    close(profile.etop, 807.2, 1.0e-9, "effective top");
    close(profile.sfcpcl.elpres, 149.1, 1.0e-9, "SFC EL");
    close(profile.sfcpcl.mplpres, 80.7, 1.0e-9, "SFC MPL");
    close(profile.mlpcl.elpres, 154.1, 1.0e-9, "ML EL");
    close(profile.mlpcl.mplpres, 82.7, 1.0e-9, "ML MPL");
    close(profile.fcstpcl.elpres, 144.1, 1.0e-9, "FCST EL");
    close(profile.fcstpcl.mplpres, 72.3, 1.0e-9, "FCST MPL");

    let inner = &profile.inner;
    let mean_theta =
        sharppyrs::extras::sharppy_mean_theta(inner, profile.ebottom, profile.etop).unwrap();
    let mean_mixratio =
        sharppyrs::extras::sharppy_mean_mixratio(inner, profile.ebottom, profile.etop).unwrap();
    let pres = (profile.ebottom + profile.etop) / 2.0;
    let tmpc = thermo::theta(1000.0, mean_theta, pres);
    let dwpc = thermo::temp_at_mixrat(mean_mixratio, pres);
    close(dwpc, 19.54875852653862, 1.0e-9, "effective dewpoint");
    let cape_profile = sharppyrs::extras::cape_profile(inner);
    let level = LiftedParcelLevel {
        pres,
        tmpc,
        dwpc,
        parcel_type: ParcelType::UserDefined { pres, tmpc, dwpc },
    };
    let effective = sharppyrs::extras::parcelx_sharppy(&cape_profile, &level, None, None);
    close(effective.elpres, 163.0, 1.0e-9, "effective EL");
    close(effective.mplpres, 93.6, 1.0e-9, "effective MPL");
    assert!(effective.mplhght.is_finite());
    assert!(
        effective.bplus > 2_100.0,
        "effective CAPE unexpectedly lost"
    );

    let derived = DerivedParams::compute(&profile);
    close(derived.conv_t_f, 86.432, 1.0e-9, "convective temperature");
}

#[test]
fn single_level_effective_layer_keeps_its_effective_parcel() {
    // F026 has exactly one qualifying effective-inflow level. NumPy's
    // arange(pbot, ptop - 1, -1) still emits that one pressure when the bounds
    // are equal; rejecting a zero-depth grid silently substituted SFC parcel.
    let profile = Profile::new(bufkit_case(26)).unwrap();
    close(profile.ebottom, 951.9, 1.0e-9, "effective bottom");
    close(profile.etop, 951.9, 1.0e-9, "effective top");

    let inner = &profile.inner;
    let mean_theta = sharppyrs::extras::sharppy_mean_theta(inner, profile.ebottom, profile.etop)
        .expect("single-level mean theta");
    let mean_mixratio =
        sharppyrs::extras::sharppy_mean_mixratio(inner, profile.ebottom, profile.etop)
            .expect("single-level mean mixing ratio");
    let pres = (profile.ebottom + profile.etop) / 2.0;
    let tmpc = thermo::theta(1000.0, mean_theta, pres);
    let dwpc = thermo::temp_at_mixrat(mean_mixratio, pres);
    close(pres, 951.9, 1.0e-9, "effective pressure");
    close(tmpc, 21.54, 1.0e-9, "effective temperature");
    close(dwpc, 18.529284819365557, 1.0e-9, "effective dewpoint");

    let cape_profile = sharppyrs::extras::cape_profile(inner);
    let level = LiftedParcelLevel {
        pres,
        tmpc,
        dwpc,
        parcel_type: ParcelType::UserDefined { pres, tmpc, dwpc },
    };
    let effective = sharppyrs::extras::parcelx_sharppy(&cape_profile, &level, None, None);
    close(
        effective.bplus,
        161.51001436817492,
        1.0e-6,
        "effective CAPE",
    );
    close(
        effective.bminus,
        -223.55488965696202,
        1.0e-6,
        "effective CIN",
    );
    close(
        effective.lclpres,
        910.3833165048057,
        1.0e-6,
        "effective LCL",
    );
    close(effective.elpres, 410.7, 1.0e-9, "effective EL");

    let derived = DerivedParams::compute(&profile);
    let single_level_wind = profile.inner.interp_wind(profile.ebottom);
    close(
        derived.mean_eff.0,
        single_level_wind.0,
        1.0e-9,
        "single-level effective mean u",
    );
    close(
        derived.mean_eff.1,
        single_level_wind.1,
        1.0e-9,
        "single-level effective mean v",
    );
    close(
        derived.srw_eff.0,
        single_level_wind.0 - profile.srwind.0,
        1.0e-9,
        "single-level effective storm-relative u",
    );
    close(
        derived.srw_eff.1,
        single_level_wind.1 - profile.srwind.1,
        1.0e-9,
        "single-level effective storm-relative v",
    );
    close(
        derived.conv_t_f,
        92.912,
        1.0e-9,
        "truncated-CAPE convective temperature",
    );
}

#[test]
fn no_effective_layer_masks_critical_angle_and_preserves_moshe_legacy() {
    let profile = Profile::new(bufkit_case(25)).unwrap();
    assert!(!profile.ebottom.is_finite());
    assert!(!profile.etop.is_finite());

    let derived = DerivedParams::compute(&profile);
    assert_eq!(derived.stp_cin, 0.0);
    assert!(!derived.right_critical_angle.is_finite());
    assert!(!derived.ebwd.0.is_finite() && !derived.ebwd.1.is_finite());
    assert!(!derived.mean_eff.0.is_finite() && !derived.mean_eff.1.is_finite());
    assert!(!derived.mean_ebw.0.is_finite() && !derived.mean_ebw.1.is_finite());
    assert!(!derived.srw_eff.0.is_finite() && !derived.srw_eff.1.is_finite());
    assert!(!derived.srw_ebw.0.is_finite() && !derived.srw_ebw.1.is_finite());
    close(
        derived.modified_sherbe,
        -96.86579886250205,
        1.0e-3,
        "no-effective-layer modified SHERBE",
    );
}

#[test]
fn southern_hemisphere_stp_preserves_legacy_signs() {
    let mut data = hrrr_example();
    data.latitude = Some(-36.675);
    let profile = Profile::new(data).unwrap();
    let derived = DerivedParams::compute(&profile);
    assert_eq!(derived.stp_cin, 0.0);
    assert!(derived.stp_cin.is_sign_negative());
    close(
        derived.stp_fixed,
        3.3052229135683477,
        5.0e-5,
        "SH fixed STP",
    );
}

#[test]
fn constant_wind_column_preserves_legacy_zero_composites() {
    let mut data = hrrr_example();
    data.wdir.fill(0.0);
    data.wspd.fill(0.0);
    let profile = Profile::new(data).unwrap();
    assert!(sharppyrs::extras::has_constant_wind(&profile.inner));
    assert!(profile.srwind.0.is_nan() && profile.srwind.1.is_nan());

    let derived = DerivedParams::compute(&profile);
    assert_eq!(derived.right_scp, 0.0);
    assert_eq!(derived.left_scp, 0.0);
    assert_eq!(derived.stp_cin, 0.0);
    assert_eq!(derived.stp_fixed, 0.0);
    assert_eq!(derived.lscp, 0.0);
}

#[test]
fn truncated_upper_moisture_does_not_invent_an_mpl_height() {
    let mut data = hrrr_example();
    for dewpoint in data.dwpc.iter_mut().skip(30) {
        *dewpoint = f64::NAN;
    }
    let profile = Profile::new(data).unwrap();
    for parcel in [
        &profile.sfcpcl,
        &profile.fcstpcl,
        &profile.mupcl,
        &profile.mlpcl,
    ] {
        if !parcel.mplpres.is_finite() {
            assert!(!parcel.mplhght.is_finite());
        }
    }
}
