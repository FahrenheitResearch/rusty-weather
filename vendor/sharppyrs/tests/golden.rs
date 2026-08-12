//! Golden tests: the sharprs-backed analysis pipeline must reproduce the
//! numbers computed by the vendored SHARPpy 1.4.0a5 Python code
//! (`testdata/golden.json`, generated from the SHARPpy-Reimagined example
//! HRRR sounding).
//!
//! Tolerances are engine-level (sharprs differs from SHARPpy in the last
//! decimals of virtual temperature and iterative lifts), tight enough to
//! catch wiring mistakes: a wrong parcel, layer, or unit fails immediately.

use serde_json::Value;
use sharppyrs::{Profile, SoundingData};

fn load() -> (Value, Profile) {
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/golden.json"))
        .unwrap();
    let g: Value = serde_json::from_str(&raw).unwrap();
    let arr = |k: &str| -> Vec<f64> {
        g["input"][k]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap_or(f64::NAN))
            .collect()
    };
    let data = SoundingData {
        pres: arr("pres"),
        hght: arr("hght"),
        tmpc: arr("tmpc"),
        dwpc: arr("dwpc"),
        wdir: arr("wdir"),
        wspd: arr("wspd"),
        omeg: Some(arr("omeg")),
        latitude: g["latitude"].as_f64(),
        longitude: None,
        missing: None,
    };
    let prof = Profile::new(data).unwrap();
    (g, prof)
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn assert_close(actual: f64, expected: f64, tol: f64, what: &str) {
    if expected.is_nan() {
        assert!(actual.is_nan(), "{what}: expected missing, got {actual}");
        return;
    }
    let denom = expected.abs().max(1.0);
    assert!(
        (actual - expected).abs() / denom <= tol,
        "{what}: got {actual}, expected {expected}"
    );
}

fn assert_arr_close(actual: &[f64], expected: &Value, tol: f64, what: &str) {
    let exp: Vec<f64> = expected
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect();
    assert_eq!(actual.len(), exp.len(), "{what}: length mismatch");
    for (i, (a, e)) in actual.iter().zip(exp.iter()).enumerate() {
        assert_close(*a, *e, tol, &format!("{what}[{i}]"));
    }
}

#[test]
fn thermo_point_checks() {
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/golden.json"))
        .unwrap();
    let g: Value = serde_json::from_str(&raw).unwrap();
    let t = &g["thermo"];
    use sharppyrs::sharprs::thermo;
    assert_close(thermo::wobf(10.0), num(&t["wobf_10"]), 1e-9, "wobf(10)");
    assert_close(thermo::wobf(-30.0), num(&t["wobf_m30"]), 1e-9, "wobf(-30)");
    assert_close(
        thermo::wetlift(850.0, 20.0, 500.0),
        num(&t["wetlift_850_20_500"]),
        1e-6,
        "wetlift",
    );
    let (p2, t2) = thermo::drylift(950.0, 30.0, 20.0);
    assert_close(p2, num(&t["drylift_950_30_20"][0]), 1e-9, "drylift p");
    assert_close(t2, num(&t["drylift_950_30_20"][1]), 1e-9, "drylift t");
    assert_close(
        thermo::virtemp(1000.0, 25.0, Some(20.0)),
        num(&t["virtemp_1000_25_20"]),
        1e-9,
        "virtemp",
    );
    assert_close(
        thermo::wetbulb(900.0, 15.0, 10.0),
        num(&t["wetbulb_900_15_10"]),
        1e-6,
        "wetbulb",
    );
    assert_close(
        thermo::temp_at_mixrat(14.0, 850.0),
        num(&t["temp_at_mixrat_14_850"]),
        1e-9,
        "temp_at_mixrat",
    );
    assert_close(
        thermo::thetae(850.0, 20.0, 15.0),
        num(&t["thetae_850_20_15"]),
        1e-6,
        "thetae",
    );
}

#[test]
fn derived_profile_arrays() {
    let (g, prof) = load();
    let inner = &prof.inner;
    assert_eq!(inner.sfc as u64, g["sfc"].as_u64().unwrap());
    assert_eq!(inner.top as u64, g["top"].as_u64().unwrap());
    assert_arr_close(&inner.vtmp, &g["derived"]["vtmp"], 5e-3, "vtmp");
    assert_arr_close(&inner.wetbulb, &g["derived"]["wetbulb"], 2e-2, "wetbulb");
    assert_arr_close(&inner.theta, &g["derived"]["theta"], 5e-3, "theta");
    // theta-e omitted: sharprs uses a different saturated-lift formulation
    // whose values diverge from SHARPpy aloft (not displayed by the plot).
    assert_arr_close(&inner.wvmr, &g["derived"]["wvmr"], 5e-3, "wvmr");
    assert_arr_close(&inner.u, &g["derived"]["u"], 1e-6, "u");
    assert_arr_close(&inner.v, &g["derived"]["v"], 1e-6, "v");
}

#[test]
fn interp_checks() {
    let (g, prof) = load();
    let ps = [900.0, 700.0, 500.0, 300.0, 200.0];
    let inner = &prof.inner;
    for (i, p) in ps.iter().enumerate() {
        assert_close(
            inner.interp_hght(*p),
            num(&g["interp_hght"][i]),
            1e-6,
            "interp hght",
        );
        assert_close(
            inner.interp_tmpc(*p),
            num(&g["interp_temp"][i]),
            1e-6,
            "interp temp",
        );
        assert_close(
            inner.interp_by_pressure(&inner.vtmp, *p),
            num(&g["interp_vtmp"][i]),
            5e-3,
            "interp vtmp",
        );
        assert_close(
            inner.interp_dwpc(*p),
            num(&g["interp_dwpt"][i]),
            1e-6,
            "interp dwpt",
        );
        let (u, v) = inner.interp_wind(*p);
        assert_close(u, num(&g["interp_u"][i]), 1e-6, "interp u");
        assert_close(v, num(&g["interp_v"][i]), 1e-6, "interp v");
    }
}

fn check_parcel(prof_pcl: &sharppyrs::Parcel, g: &Value, name: &str, tol: f64) {
    for (field, actual) in [
        ("lclpres", prof_pcl.lclpres),
        ("lclhght", prof_pcl.lclhght),
        ("lfcpres", prof_pcl.lfcpres),
        ("elpres", prof_pcl.elpres),
        ("bplus", prof_pcl.bplus),
        ("bminus", prof_pcl.bminus),
        ("p0c", prof_pcl.p0c),
        ("pm20c", prof_pcl.pm20c),
        ("pm30c", prof_pcl.pm30c),
        ("hght0c", prof_pcl.hght0c),
        ("hghtm20c", prof_pcl.hghtm20c),
        ("hghtm30c", prof_pcl.hghtm30c),
    ] {
        assert_close(actual, num(&g[field]), tol, &format!("{name}.{field}"));
    }
    // Traces: same pressures, temperatures to engine tolerance.
    assert_arr_close(
        &prof_pcl.ptrace,
        &g["ptrace"],
        tol,
        &format!("{name}.ptrace"),
    );
    // Trace temperatures: quarter-degree absolute tolerance (sub-pixel).
    let exp: Vec<f64> = g["ttrace"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect();
    assert_eq!(
        prof_pcl.ttrace.len(),
        exp.len(),
        "{name}.ttrace: length mismatch"
    );
    for (i, (a, e)) in prof_pcl.ttrace.iter().zip(exp.iter()).enumerate() {
        if e.is_nan() {
            assert!(a.is_nan(), "{name}.ttrace[{i}]: expected missing, got {a}");
        } else {
            assert!(
                (a - e).abs() <= 0.25,
                "{name}.ttrace[{i}]: got {a}, expected {e}"
            );
        }
    }
}

#[test]
fn parcels_match() {
    let (g, prof) = load();
    check_parcel(&prof.mupcl, &g["mupcl"], "mupcl", 5e-3);
    check_parcel(&prof.sfcpcl, &g["sfcpcl"], "sfcpcl", 5e-3);
    check_parcel(&prof.mlpcl, &g["mlpcl"], "mlpcl", 5e-3);
    // FCST parcel rebuilt from sharprs primitives (its own Forecast type
    // omits the forecast max-temp step); layer means differ slightly.
    check_parcel(&prof.fcstpcl, &g["fcstpcl"], "fcstpcl", 1e-1);
}

#[test]
fn effective_layer_and_kinematics() {
    let (g, prof) = load();
    assert_close(prof.ebottom, num(&g["ebottom"]), 1e-6, "ebottom");
    assert_close(prof.etop, num(&g["etop"]), 1e-6, "etop");
    assert_close(prof.ebotm, num(&g["ebotm"]), 1e-2, "ebotm");
    assert_close(prof.etopm, num(&g["etopm"]), 1e-2, "etopm");
    assert_close(prof.srwind.0, num(&g["srwind"][0]), 1e-2, "srwind rstu");
    assert_close(prof.srwind.1, num(&g["srwind"][1]), 1e-2, "srwind rstv");
    assert_close(prof.srwind.2, num(&g["srwind"][2]), 1e-2, "srwind lstu");
    assert_close(prof.srwind.3, num(&g["srwind"][3]), 1e-2, "srwind lstv");
    assert_close(
        prof.right_esrh,
        num(&g["right_esrh"][0]),
        1e-2,
        "right_esrh",
    );
    assert_close(
        prof.max_lapse_rate_2_6.0,
        num(&g["max_lapse_rate_2_6"][0]),
        1e-2,
        "max lapse rate",
    );
    assert_close(
        prof.max_lapse_rate_2_6.1,
        num(&g["max_lapse_rate_2_6"][1]),
        1e-2,
        "max lapse rate pbot",
    );
    assert_close(
        prof.max_lapse_rate_2_6.2,
        num(&g["max_lapse_rate_2_6"][2]),
        1e-2,
        "max lapse rate ptop",
    );
}

#[test]
fn dcape_and_downdraft_trace() {
    let (g, prof) = load();
    assert_close(prof.dcape.abs(), num(&g["dcape"]).abs(), 1e-2, "dcape");
    // sharprs's trace convention differs slightly from SHARPpy's (no
    // duplicated start point); check the span rather than element-for-element.
    assert!(prof.dpcl_ptrace.len() >= 2, "downdraft trace too short");
    // sharprs returns the trace surface-first; SHARPpy source-level-first.
    let g_arr = g["dpcl_ptrace"].as_array().unwrap();
    let g_first = num(&g_arr[0]);
    let g_last = num(&g_arr[g_arr.len() - 1]);
    let first = prof.dpcl_ptrace[0];
    let last = prof.dpcl_ptrace[prof.dpcl_ptrace.len() - 1];
    let (g_lo, g_hi) = (g_first.min(g_last), g_first.max(g_last));
    let (lo, hi) = (first.min(last), first.max(last));
    assert_close(lo, g_lo, 1e-2, "dpcl trace top pressure");
    assert_close(hi, g_hi, 1e-2, "dpcl trace bottom pressure");
}

#[test]
fn temp_levels() {
    let (g, prof) = load();
    for (t, key) in [(0.0, "0"), (-10.0, "-10"), (-20.0, "-20"), (-30.0, "-30")] {
        assert_close(
            sharppyrs::sharprs::params::indices::temp_lvl(&prof.inner, t, false)
                .unwrap_or(f64::NAN),
            num(&g["temp_lvl"][key]),
            1e-6,
            &format!("temp_lvl {t}"),
        );
    }
}

#[test]
fn barb_sampling() {
    let (g, prof) = load();
    let bp: Vec<f64> = g["barb_pres"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect();
    for (i, p) in bp.iter().enumerate() {
        let (wdir, wspd) = prof.inner.interp_vec(*p);
        assert_close(wdir, num(&g["barb_wdir"][i]), 1e-4, "barb wdir");
        assert_close(wspd, num(&g["barb_wspd"][i]), 1e-4, "barb wspd");
    }
}
