//! Small numerics the display needs that `sharprs` does not (yet) provide.

use sharprs::Profile;
use sharprs::params::cape::{self, LiftedParcelLevel, ParcelResult, ParcelType};
use sharprs::params::indices;
use sharprs::thermo;
use sharprs::winds;

use crate::utils::qc;

/// SHARPpy-style forecast surface parcel: forecast max temperature (100-hPa
/// mixed layer warmed 2 K) with the mean boundary-layer mixing ratio.
/// sharprs's own `ParcelType::Forecast` uses the current surface temperature
/// instead; the original renderer's FCST row uses this definition.
pub fn forecast_parcel(inner: &Profile, cape_prof: &cape::Profile) -> ParcelResult {
    let pres = inner.pres[inner.sfc];
    let tmpc = indices::max_temp(inner, Some(100.0)).unwrap_or(f64::NAN);
    let mmr = indices::mean_mixratio(inner, Some(pres), Some(pres - 100.0)).unwrap_or(f64::NAN);
    let dwpc = thermo::temp_at_mixrat(mmr, pres);
    let lpl = LiftedParcelLevel {
        pres,
        tmpc,
        dwpc,
        parcel_type: ParcelType::UserDefined { pres, tmpc, dwpc },
    };
    cape::parcelx(cape_prof, &lpl, None, None)
}

/// Parcel-based Bunkers storm motion `(rstu, rstv, lstu, lstv)` in kts —
/// the Bunkers et al. 2014 method the original renderer uses (effective
/// inflow base to 65% of the MU parcel EL, pressure-weighted mean wind),
/// falling back to the non-parcel method (port of SHARPpy
/// `params.bunkers_storm_motion`).
pub fn bunkers_storm_motion(
    prof: &Profile,
    mupcl: &ParcelResult,
    pbot: f64,
) -> (f64, f64, f64, f64) {
    let d = 7.5 * 1.94384449; // 7.5 m/s deviation, in kts
    let mucape = mupcl.bplus;
    let muel = mupcl.elhght;
    let base = prof.to_agl(prof.interp_hght(pbot));
    if mucape > 100.0 && qc(muel) && qc(base) {
        let depth = muel - base;
        let htop = base + depth * (65.0 / 100.0);
        let ptop = prof.pres_at_height(prof.to_msl(htop));
        let (mnu, mnv) =
            winds::mean_wind(prof, pbot, ptop, -1.0, 0.0, 0.0).unwrap_or((f64::NAN, f64::NAN));
        let (sru, srv) = winds::wind_shear(prof, pbot, ptop).unwrap_or((f64::NAN, f64::NAN));
        let srmag = (sru * sru + srv * srv).sqrt();
        let uchg = d / srmag * srv;
        let vchg = d / srmag * sru;
        (mnu + uchg, mnv - vchg, mnu - uchg, mnv + vchg)
    } else {
        winds::non_parcel_bunkers_motion(prof).unwrap_or((f64::NAN, f64::NAN, f64::NAN, f64::NAN))
    }
}

/// Storm-relative helicity (m2/s2) over `lower..upper` m AGL for storm motion
/// `(stu, stv)` kts — SHARPpy's exact path, tolerant of missing wind levels
/// and of the surface-boundary float round-trip that makes
/// `sharprs::winds::helicity` return an error on some soundings (the
/// height->pressure interpolation can land a hair above the surface
/// pressure; here the boundary is clamped back into the profile).
/// Returns `(total, positive, negative)`.
pub fn helicity(prof: &Profile, lower: f64, upper: f64, stu: f64, stv: f64) -> (f64, f64, f64) {
    let nan3 = (f64::NAN, f64::NAN, f64::NAN);
    if !qc(lower) || !qc(upper) || !qc(stu) || !qc(stv) {
        return nan3;
    }
    if lower == upper {
        return (0.0, 0.0, 0.0);
    }
    let sfc_pres = prof.pres[prof.sfc];
    let mut plower = prof.pres_at_height(prof.to_msl(lower));
    let pupper = prof.pres_at_height(prof.to_msl(upper));
    // Clamp the boundaries into the profile's pressure range (float
    // round-trips can put them an ulp outside it).
    if plower.is_finite() {
        plower = plower.min(sfc_pres);
    } else if lower.abs() < 1.0 {
        plower = sfc_pres;
    }
    if !qc(plower) || !qc(pupper) {
        return nan3;
    }
    let interp_uv = |p: f64| -> (f64, f64) {
        (
            prof.interp_by_pressure(&prof.u, p),
            prof.interp_by_pressure(&prof.v, p),
        )
    };
    let mut us: Vec<f64> = Vec::new();
    let mut vs: Vec<f64> = Vec::new();
    let (u1, v1) = interp_uv(plower);
    if qc(u1) && qc(v1) {
        us.push(u1);
        vs.push(v1);
    }
    for i in 0..prof.pres.len() {
        let p = prof.pres[i];
        if p.is_finite() && p < plower && p > pupper && qc(prof.u[i]) && qc(prof.v[i]) {
            us.push(prof.u[i]);
            vs.push(prof.v[i]);
        }
    }
    let (u2, v2) = interp_uv(pupper);
    if qc(u2) && qc(v2) {
        us.push(u2);
        vs.push(v2);
    }
    if us.len() < 2 {
        return nan3;
    }
    const KTS2MS: f64 = 0.514444;
    let mut phel = 0.0;
    let mut nhel = 0.0;
    for k in 0..us.len() - 1 {
        let sru0 = (us[k] - stu) * KTS2MS;
        let srv0 = (vs[k] - stv) * KTS2MS;
        let sru1 = (us[k + 1] - stu) * KTS2MS;
        let srv1 = (vs[k + 1] - stv) * KTS2MS;
        let layer = sru1 * srv0 - sru0 * srv1;
        if layer.is_nan() {
            continue;
        }
        if layer > 0.0 {
            phel += layer;
        } else if layer < 0.0 {
            nhel += layer;
        }
    }
    (phel + nhel, phel, nhel)
}

/// Virtual-temperature lapse rate (C/km) over an AGL layer, boundary-clamped
/// like [`helicity`]. This is the vendored SHARPpy `params.lapse_rate`
/// convention (sharpmod's SFC-500m/SFC-1km rows used plain temperature — a
/// quirk deliberately not reproduced; see PORTING.md).
pub fn lapse_rate_agl(prof: &Profile, lower_m: f64, upper_m: f64) -> f64 {
    let z1 = prof.to_msl(lower_m);
    let z2 = prof.to_msl(upper_m);
    let mut p1 = prof.pres_at_height(z1);
    let p2 = prof.pres_at_height(z2);
    if p1.is_finite() {
        p1 = p1.min(prof.pres[prof.sfc]);
    } else if lower_m.abs() < 1.0 {
        p1 = prof.pres[prof.sfc];
    }
    if !qc(p1) || !qc(p2) {
        return f64::NAN;
    }
    let tv1 = prof.interp_by_pressure(&prof.vtmp, p1);
    let tv2 = prof.interp_by_pressure(&prof.vtmp, p2);
    let dz = z2 - z1;
    if !qc(tv1) || !qc(tv2) || dz.abs() < 1.0 {
        return f64::NAN;
    }
    (tv2 - tv1) / dz * -1000.0
}
