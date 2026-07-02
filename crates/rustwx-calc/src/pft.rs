//! Pyrocumulonimbus Firepower Threshold (PFT), Tory & Kepert (2021, WAF
//! 36, 439-456): the minimum total sensible-heat flux (GW) a fire must
//! feed into its plume base for the plume to reach free moist convection
//! deep enough to make a pyroCb, given one atmospheric column. LOWER PFT
//! = more favorable atmosphere. Full derivation, constants, validation
//! anchors, and ambiguity decisions: docs/PFT_SPEC.md.
//!
//! Moist-path thermodynamics use the vendored Wobus pair
//! (`wx_math::thermo::satlift` via one inverse bisection) — the spec
//! allows any self-consistent pseudoadiabat pair.

use rayon::prelude::*;
use rustwx_core::GridShape;
use wx_math::thermo::{drylift, satlift};

use crate::ecape::{EcapeVolumeInputs, SurfaceInputs};
use crate::error::CalcError;

const CPD: f64 = 1005.7; // J kg^-1 K^-1
const RD: f64 = 287.04; // J kg^-1 K^-1
const KAPPA: f64 = 0.2854; // Rd/Cpd
const CPV: f64 = 1870.0; // J kg^-1 K^-1
const EPS: f64 = 0.622; // Rd/Rv
const ZEROC_K: f64 = 273.15;
/// Fire moisture-to-heat ratio: 1 g/kg of q per 15 K of dtheta
/// (Luderer et al. 2009; Table 2 of TK21).
const PHI_KGKG_PER_K: f64 = 6.67e-5;
/// 1 + a'*beta' with a' = 0.32, beta' = 0.4 (Briggs internal plume).
const ONE_PLUS_AB: f64 = 1.128;
/// pi * Cpd * (beta'/(1 + a'*beta'))^2 — the Eq. (25) constant bracket.
const PFT_CONST: f64 = 397.3; // J kg^-1 K^-1

#[derive(Debug, Clone, Copy)]
pub struct PftOptions {
    /// Minimum (parcel) cloud-top temperature, deg C. TK21: -20 C
    /// ("conservative electrification level").
    pub min_cloud_top_c: f64,
    /// Buoyancy buffer delta-theta_b (K): the moist path must stay at
    /// least this much warmer (virtual) than the environment.
    pub buoyancy_buffer_k: f64,
    /// Minimum entrained-mixed-layer depth (hPa) before the ML search
    /// may terminate (reference-implementation guard).
    pub min_ml_depth_hpa: f64,
    /// Upper end of the beta (= dtheta/theta_ML) scan. TK21 plots reach
    /// ~0.1; 0.20 is the reference implementation's ceiling.
    pub beta_max: f64,
}

impl Default for PftOptions {
    fn default() -> Self {
        Self {
            min_cloud_top_c: -20.0,
            buoyancy_buffer_k: 0.5,
            min_ml_depth_hpa: 50.0,
            beta_max: 0.20,
        }
    }
}

/// One column's PFT solution. All heights AGL of the column's surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PftSolution {
    pub pft_gw: f64,
    pub z_fc_m: f64,
    pub dtheta_fc_k: f64,
    pub u_ml_ms: f64,
    pub p_fc_hpa: f64,
    pub theta_ml_k: f64,
    pub beta_star: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PftOutcome {
    /// A marginal freely-convecting path exists at beta* <= beta_max.
    Solution(PftSolution),
    /// No plume buoyancy within the scan supports a pyroCb-deep path —
    /// the atmosphere is (very) unfavorable, not "missing data".
    NoPyroCb,
    /// The column could not be evaluated (too few levels, no -20 C
    /// level in data, ML never crosses its LCL, ...).
    Unevaluable,
}

/// Grid output planes (row-major, grid.len()). `pft_gw` carries
/// `NO_PYROCB_SENTINEL_GW` where the scan found no path (renderers bin
/// it into the top ">1024 GW" class) and NaN where unevaluable;
/// component planes are NaN in both non-solution cases.
#[derive(Debug, Clone)]
pub struct PftFields {
    pub pft_gw: Vec<f64>,
    pub z_fc_m: Vec<f64>,
    pub dtheta_fc_k: Vec<f64>,
    pub u_ml_ms: Vec<f64>,
    pub no_pyrocb_count: usize,
    pub unevaluable_count: usize,
}

/// "Off the top of the operational log2 16..1024 GW scale" marker for
/// no-path columns (kept finite so maps stay dense).
pub const NO_PYROCB_SENTINEL_GW: f64 = 4096.0;

#[derive(Debug, Clone, Copy)]
struct Level {
    p_hpa: f64,
    h_m: f64,
    t_c: f64,
    q_kgkg: f64,
    u_ms: f64,
    v_ms: f64,
}

impl Level {
    fn theta_k(&self) -> f64 {
        (self.t_c + ZEROC_K) * (1000.0 / self.p_hpa).powf(KAPPA)
    }
}

/// Bolton saturation vapor pressure (hPa) over water, T in deg C.
fn es_hpa(t_c: f64) -> f64 {
    6.112 * (17.67 * t_c / (t_c + 243.5)).exp()
}

/// Vapor pressure (hPa) from specific humidity (kg/kg) and pressure (hPa).
fn e_from_q(q: f64, p_hpa: f64) -> f64 {
    p_hpa / ((1.0 - EPS) + EPS / q.max(1e-9))
}

/// Dewpoint (deg C) from specific humidity and pressure (Bolton inverse).
fn dewpoint_c_from_q(q: f64, p_hpa: f64) -> f64 {
    let e = e_from_q(q, p_hpa).max(1e-6);
    let ln_ratio = (e / 6.112).ln();
    243.5 * ln_ratio / (17.67 - ln_ratio)
}

/// Virtual temperature (K) from T (deg C), specific humidity (kg/kg).
fn tv_k(t_c: f64, q: f64) -> f64 {
    (t_c + ZEROC_K) * (1.0 + 0.61 * q)
}

/// Saturation specific humidity (kg/kg) at (T deg C, p hPa).
fn qs_kgkg(t_c: f64, p_hpa: f64) -> f64 {
    let e = es_hpa(t_c);
    EPS * e / (p_hpa - (1.0 - EPS) * e).max(1e-3)
}

/// theta-w (deg C at 1000 hPa) of the moist adiabat passing through
/// (p_hpa, t_c): inverse of `satlift` by bisection (monotone).
fn theta_w_c_through(p_hpa: f64, t_c: f64) -> Option<f64> {
    let (mut lo, mut hi) = (-70.0f64, 70.0f64);
    if satlift(p_hpa, lo) > t_c || satlift(p_hpa, hi) < t_c {
        return None;
    }
    for _ in 0..48 {
        let mid = 0.5 * (lo + hi);
        if satlift(p_hpa, mid) < t_c {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(0.5 * (lo + hi))
}

/// Height AGL at pressure `p_hpa` by log-p interpolation of the column.
fn height_at_pressure(column: &[Level], p_hpa: f64) -> Option<f64> {
    if p_hpa >= column[0].p_hpa {
        return Some(column[0].h_m);
    }
    for pair in column.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if p_hpa <= a.p_hpa && p_hpa >= b.p_hpa {
            let f = (a.p_hpa.ln() - p_hpa.ln()) / (a.p_hpa.ln() - b.p_hpa.ln()).max(1e-12);
            return Some(a.h_m + f * (b.h_m - a.h_m));
        }
    }
    None
}

/// Environment (T deg C, q kg/kg) at pressure `p_hpa`, linear in log-p.
fn env_at_pressure(column: &[Level], p_hpa: f64) -> Option<(f64, f64)> {
    if p_hpa >= column[0].p_hpa {
        return Some((column[0].t_c, column[0].q_kgkg));
    }
    for pair in column.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if p_hpa <= a.p_hpa && p_hpa >= b.p_hpa {
            let f = (a.p_hpa.ln() - p_hpa.ln()) / (a.p_hpa.ln() - b.p_hpa.ln()).max(1e-12);
            return Some((a.t_c + f * (b.t_c - a.t_c), a.q_kgkg + f * (b.q_kgkg - a.q_kgkg)));
        }
    }
    None
}

/// Step 1: entrained mixed layer — linearly-height-weighted theta/q
/// means grown level by level until the ML's own LCL falls inside it.
/// Returns (theta_ML K, q_ML kg/kg).
fn entrained_mixed_layer(column: &[Level], opts: &PftOptions) -> Option<(f64, f64)> {
    let psfc = column[0].p_hpa;
    let mut last_valid: Option<(f64, f64)> = None;
    for k in 1..column.len() {
        let top_h = column[k].h_m;
        if top_h <= 0.0 {
            continue;
        }
        // Height-weighted trapezoid of theta*h and q*h over [0, top_h].
        let (mut int_theta, mut int_q) = (0.0, 0.0);
        for pair in column[..=k].windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let dh = b.h_m - a.h_m;
            if dh <= 0.0 {
                continue;
            }
            int_theta += (a.theta_k() * a.h_m + b.theta_k() * b.h_m) * 0.5 * dh;
            int_q += (a.q_kgkg * a.h_m + b.q_kgkg * b.h_m) * 0.5 * dh;
        }
        let norm = top_h * top_h / 2.0;
        let theta_ml = int_theta / norm;
        let q_ml = int_q / norm;

        // ML parcel at the surface -> its LCL.
        let t_parcel_c = theta_ml * (psfc / 1000.0).powf(KAPPA) - ZEROC_K;
        let td_parcel_c = dewpoint_c_from_q(q_ml, psfc);
        if td_parcel_c >= t_parcel_c {
            // Saturated at the surface: no dry-plume layer to speak of.
            return None;
        }
        let (p_lcl, _t_lcl) = drylift(psfc, t_parcel_c, td_parcel_c);
        let deep_enough = psfc - column[k].p_hpa >= opts.min_ml_depth_hpa;
        if column[k].p_hpa > p_lcl {
            // ML top still below its own LCL: candidate remains valid.
            if deep_enough {
                last_valid = Some((theta_ml, q_ml));
            }
        } else if last_valid.is_some() {
            // First crossing after a valid candidate: take the last one.
            return last_valid;
        } else if deep_enough {
            // Crossed immediately at the first deep-enough candidate.
            return Some((theta_ml, q_ml));
        }
    }
    last_valid
}

/// SP point for buoyancy parameter beta (Tory et al. 2018 Eqs. 14-17):
/// (theta_SP K, q_SP kg/kg, T_SP deg C, P_SP hPa).
fn sp_point(theta_ml_k: f64, q_ml: f64, beta: f64) -> Option<(f64, f64, f64, f64)> {
    let theta_sp = (1.0 + beta) * theta_ml_k;
    let q_sp = q_ml + beta * PHI_KGKG_PER_K * theta_ml_k;
    let t_pl_k = theta_sp; // parcel T at 1000 hPa
    let e_pl = e_from_q(q_sp, 1000.0);
    if e_pl <= 0.0 {
        return None;
    }
    let t_sp_k = 2840.0 / (3.5 * t_pl_k.ln() - e_pl.ln() - 4.805) + 55.0;
    let r_pl = q_sp / (1.0 - q_sp);
    let k_exp = (CPD / RD) * (1.0 + r_pl * (CPV / CPD)) / (1.0 + r_pl / EPS);
    let p_sp = 1000.0 * (t_sp_k / theta_sp).powf(k_exp);
    if !(100.0..=1080.0).contains(&p_sp) {
        return None;
    }
    Some((theta_sp, q_sp, t_sp_k - ZEROC_K, p_sp))
}

/// Free-convection test for one beta: minimum virtual-temperature excess
/// of the saturated path from the SP point up to where the PARCEL cools
/// to the cloud-top threshold. None = path cannot be evaluated.
fn min_path_buoyancy_k(column: &[Level], t_sp_c: f64, p_sp_hpa: f64, opts: &PftOptions) -> Option<f64> {
    let theta_w = theta_w_c_through(p_sp_hpa, t_sp_c)?;
    let mut min_buoy = f64::INFINITY;

    // At the SP point itself (env interpolated).
    let (t_env, q_env) = env_at_pressure(column, p_sp_hpa)?;
    let q_sat_sp = qs_kgkg(t_sp_c, p_sp_hpa);
    min_buoy = min_buoy.min(tv_k(t_sp_c, q_sat_sp) - tv_k(t_env, q_env));

    let mut reached_cloud_top = false;
    for level in column.iter().filter(|l| l.p_hpa < p_sp_hpa) {
        let t_parcel = satlift(level.p_hpa, theta_w);
        let q_parcel = qs_kgkg(t_parcel, level.p_hpa);
        min_buoy = min_buoy.min(tv_k(t_parcel, q_parcel) - tv_k(level.t_c, level.q_kgkg));
        if t_parcel <= opts.min_cloud_top_c {
            reached_cloud_top = true;
            break;
        }
        if min_buoy < opts.buoyancy_buffer_k - 25.0 {
            // Hopelessly negative already; the caller only needs "fails".
            return Some(min_buoy);
        }
    }
    reached_cloud_top.then_some(min_buoy)
}

/// Trapezoid vector-mean wind magnitude over 0 <= h <= z_top.
fn mean_wind_ms(column: &[Level], z_top_m: f64) -> f64 {
    if z_top_m <= 0.0 {
        return (column[0].u_ms.powi(2) + column[0].v_ms.powi(2)).sqrt();
    }
    let (mut su, mut sv, mut covered) = (0.0, 0.0, 0.0);
    for pair in column.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a.h_m >= z_top_m {
            break;
        }
        let seg_top = b.h_m.min(z_top_m);
        let dh = seg_top - a.h_m;
        if dh <= 0.0 {
            continue;
        }
        // Interpolate the segment-top wind when the layer cuts through.
        let f = ((seg_top - a.h_m) / (b.h_m - a.h_m).max(1e-9)).clamp(0.0, 1.0);
        let (ut, vt) = (a.u_ms + f * (b.u_ms - a.u_ms), a.v_ms + f * (b.v_ms - a.v_ms));
        su += (a.u_ms + ut) * 0.5 * dh;
        sv += (a.v_ms + vt) * 0.5 * dh;
        covered += dh;
    }
    if covered <= 0.0 {
        return (column[0].u_ms.powi(2) + column[0].v_ms.powi(2)).sqrt();
    }
    ((su / covered).powi(2) + (sv / covered).powi(2)).sqrt()
}

/// Full single-column PFT (docs/PFT_SPEC.md §4).
fn pft_column(column: &[Level], opts: &PftOptions) -> PftOutcome {
    if column.len() < 8 {
        return PftOutcome::Unevaluable;
    }
    let Some((theta_ml, q_ml)) = entrained_mixed_layer(column, opts) else {
        return PftOutcome::Unevaluable;
    };

    let buoy_at = |beta: f64| -> Option<f64> {
        let (_, _, t_sp_c, p_sp) = sp_point(theta_ml, q_ml, beta)?;
        min_path_buoyancy_k(column, t_sp_c, p_sp, opts)
    };

    // Bracket the marginal beta on a coarse scan, then bisect.
    const COARSE_STEP: f64 = 0.005;
    let mut prev_beta = 0.0;
    let Some(buoy0) = buoy_at(0.0) else {
        return PftOutcome::Unevaluable;
    };
    let mut bracket: Option<(f64, f64)> = None;
    if buoy0 >= opts.buoyancy_buffer_k {
        // Ordinary deep convection already possible from the ML.
        bracket = Some((0.0, 0.0));
    } else {
        let mut beta = COARSE_STEP;
        while beta <= opts.beta_max + 1e-12 {
            match buoy_at(beta) {
                Some(buoy) if buoy >= opts.buoyancy_buffer_k => {
                    bracket = Some((prev_beta, beta));
                    break;
                }
                _ => {}
            }
            prev_beta = beta;
            beta += COARSE_STEP;
        }
    }
    let Some((mut lo, mut hi)) = bracket else {
        return PftOutcome::NoPyroCb;
    };
    // Bisection refine (test is monotone in beta).
    while hi - lo > 1e-4 {
        let mid = 0.5 * (lo + hi);
        match buoy_at(mid) {
            Some(buoy) if buoy >= opts.buoyancy_buffer_k => hi = mid,
            _ => lo = mid,
        }
    }
    let beta_star = hi;

    let Some((theta_sp, _q_sp, _t_sp_c, p_fc)) = sp_point(theta_ml, q_ml, beta_star) else {
        return PftOutcome::Unevaluable;
    };
    let Some(z_fc) = height_at_pressure(column, p_fc) else {
        return PftOutcome::Unevaluable;
    };
    if z_fc <= 0.0 {
        return PftOutcome::Unevaluable;
    }
    let u_ml = mean_wind_ms(column, z_fc);
    let dtheta_fc = beta_star * theta_ml;
    let psfc = column[0].p_hpa;
    let pc = psfc - (psfc - p_fc) / ONE_PLUS_AB; // Eq. 28
    let rho0 = (100.0 * pc) / (RD * theta_sp) * (1000.0 / pc).powf(KAPPA); // Eq. 26
    let pft_w = PFT_CONST * rho0 * z_fc * z_fc * u_ml * dtheta_fc; // Eq. 25
    PftOutcome::Solution(PftSolution {
        pft_gw: pft_w / 1e9,
        z_fc_m: z_fc,
        dtheta_fc_k: dtheta_fc,
        u_ml_ms: u_ml,
        p_fc_hpa: p_fc,
        theta_ml_k: theta_ml,
        beta_star,
    })
}

/// Manual/quick PFT, TK21 Eq. (31): fixed rho0 = 0.755 kg m^-3. The
/// published case-study anchors use exactly this form.
pub fn manual_pft_gw(z_fc_km: f64, u_ml_ms: f64, dtheta_fc_k: f64) -> f64 {
    0.3 * z_fc_km * z_fc_km * u_ml_ms * dtheta_fc_k
}

/// Grid driver: one PFT evaluation per cell, rayon-parallel, mirroring
/// the ECAPE lane's input layout (pressure may be per-level [nz] or
/// full 3D [nz * len2d]).
pub fn compute_pft_grid(
    grid: GridShape,
    volume: EcapeVolumeInputs<'_>,
    surface: SurfaceInputs<'_>,
    opts: PftOptions,
) -> Result<PftFields, CalcError> {
    let len2d = grid.len();
    let nz = volume.nz;
    let pressure_is_3d = volume.pressure_pa.len() == nz * len2d;
    if !pressure_is_3d && volume.pressure_pa.len() != nz {
        return Err(CalcError::LengthMismatch {
            field: "pressure_pa",
            expected: nz * len2d,
            actual: volume.pressure_pa.len(),
        });
    }
    for (name, slice) in [
        ("temperature_c", volume.temperature_c),
        ("qvapor_kgkg", volume.qvapor_kgkg),
        ("height_agl_m", volume.height_agl_m),
        ("u_ms", volume.u_ms),
        ("v_ms", volume.v_ms),
    ] {
        if slice.len() != nz * len2d {
            return Err(CalcError::LengthMismatch {
                field: name,
                expected: nz * len2d,
                actual: slice.len(),
            });
        }
    }
    for (name, slice) in [
        ("psfc_pa", surface.psfc_pa),
        ("t2_k", surface.t2_k),
        ("q2_kgkg", surface.q2_kgkg),
        ("u10_ms", surface.u10_ms),
        ("v10_ms", surface.v10_ms),
    ] {
        if slice.len() != len2d {
            return Err(CalcError::LengthMismatch {
                field: name,
                expected: len2d,
                actual: slice.len(),
            });
        }
    }

    let outcomes: Vec<PftOutcome> = (0..len2d)
        .into_par_iter()
        .map(|cell| {
            let psfc_hpa = surface.psfc_pa[cell] / 100.0;
            let t2_c = surface.t2_k[cell] - ZEROC_K;
            if !(psfc_hpa.is_finite() && t2_c.is_finite()) {
                return PftOutcome::Unevaluable;
            }
            let mut column = Vec::with_capacity(nz + 1);
            column.push(Level {
                p_hpa: psfc_hpa,
                h_m: 0.0,
                t_c: t2_c,
                q_kgkg: surface.q2_kgkg[cell].max(1e-9),
                u_ms: surface.u10_ms[cell],
                v_ms: surface.v10_ms[cell],
            });
            for k in 0..nz {
                let idx = k * len2d + cell;
                let p_hpa = if pressure_is_3d {
                    volume.pressure_pa[idx] / 100.0
                } else {
                    volume.pressure_pa[k] / 100.0
                };
                let h_m = volume.height_agl_m[idx];
                let t_c = volume.temperature_c[idx];
                let q = volume.qvapor_kgkg[idx];
                if !(p_hpa.is_finite() && h_m.is_finite() && t_c.is_finite() && q.is_finite()) {
                    continue;
                }
                // Below-ground pressure levels have h <= 0 / p >= psfc.
                if p_hpa >= psfc_hpa - 0.5 || h_m <= 2.0 {
                    continue;
                }
                column.push(Level {
                    p_hpa,
                    h_m,
                    t_c,
                    q_kgkg: q.max(1e-9),
                    u_ms: volume.u_ms[idx],
                    v_ms: volume.v_ms[idx],
                });
            }
            column.sort_by(|a, b| b.p_hpa.partial_cmp(&a.p_hpa).unwrap_or(std::cmp::Ordering::Equal));
            pft_column(&column, &opts)
        })
        .collect();

    let mut fields = PftFields {
        pft_gw: vec![f64::NAN; len2d],
        z_fc_m: vec![f64::NAN; len2d],
        dtheta_fc_k: vec![f64::NAN; len2d],
        u_ml_ms: vec![f64::NAN; len2d],
        no_pyrocb_count: 0,
        unevaluable_count: 0,
    };
    for (cell, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            PftOutcome::Solution(s) => {
                fields.pft_gw[cell] = s.pft_gw;
                fields.z_fc_m[cell] = s.z_fc_m;
                fields.dtheta_fc_k[cell] = s.dtheta_fc_k;
                fields.u_ml_ms[cell] = s.u_ml_ms;
            }
            PftOutcome::NoPyroCb => {
                fields.pft_gw[cell] = NO_PYROCB_SENTINEL_GW;
                fields.no_pyrocb_count += 1;
            }
            PftOutcome::Unevaluable => {
                fields.unevaluable_count += 1;
            }
        }
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TK21 Table of published case studies — Eq. (31) transcription
    /// check (docs/PFT_SPEC.md §6).
    #[test]
    fn manual_formula_reproduces_all_published_anchors() {
        let cases: [(f64, f64, f64, f64); 6] = [
            (4.8, 20.0, 9.0, 1240.0), // Black Saturday 1000 LST
            (3.5, 17.0, 8.0, 500.0),  // Black Saturday 2200 LST
            (4.0, 20.0, 3.0, 290.0),  // Black Saturday pre-change estimate
            (3.3, 20.0, 8.0, 520.0),  // Chisholm 0600 LST
            (2.7, 18.0, 2.5, 100.0),  // Chisholm 1800 LST
            (4.4, 3.0, 8.0, 140.0),   // Bald Fire
        ];
        for (z, u, dth, published) in cases {
            let got = manual_pft_gw(z, u, dth);
            assert!(
                (got - published).abs() / published < 0.02,
                "z={z} u={u} dth={dth}: got {got}, published {published}"
            );
        }
        // Sedgerly Rd: 12 GW from 2.8 km, 5 m/s, 1 K.
        assert!((manual_pft_gw(2.8, 5.0, 1.0) - 11.76).abs() < 0.01);
    }

    #[test]
    fn pft_constant_bracket_matches_its_definition() {
        let bracket = std::f64::consts::PI * CPD * (0.4f64 / ONE_PLUS_AB).powi(2);
        assert!((bracket - PFT_CONST).abs() < 0.2, "bracket {bracket}");
    }

    /// The beta = 0 SP point must coincide with the ML parcel's LCL —
    /// the SP curve's anchor point (internal consistency of Eqs. 14-17
    /// vs the drylift LCL routine).
    #[test]
    fn sp_point_at_beta_zero_is_the_ml_lcl() {
        let theta_ml = 310.0; // K
        let q_ml = 0.008; // kg/kg
        let psfc = 1000.0;
        let (_, _, _t_sp, p_sp) = sp_point(theta_ml, q_ml, 0.0).expect("SP point");
        let t_parcel_c = theta_ml * (psfc / 1000.0f64).powf(KAPPA) - ZEROC_K;
        let td_parcel_c = dewpoint_c_from_q(q_ml, psfc);
        let (p_lcl, _) = drylift(psfc, t_parcel_c, td_parcel_c);
        assert!(
            (p_sp - p_lcl).abs() < 6.0,
            "SP(beta=0) {p_sp} hPa vs drylift LCL {p_lcl} hPa"
        );
    }

    /// Idealized dry-summer column: deep dry-adiabatic ML with sharp
    /// drying, stable above, cold enough aloft. The solver must find a
    /// finite positive PFT with plausible components, and PFT must scale
    /// linearly with the wind (Eq. 25 is linear in U_ML; the thermo
    /// search does not see the wind).
    #[test]
    fn idealized_column_solves_and_scales_linearly_with_wind() {
        let column = idealized_column(1.0);
        let outcome = pft_column(&column, &PftOptions::default());
        let PftOutcome::Solution(sol) = outcome else {
            panic!("expected a solution, got {outcome:?}");
        };
        assert!(sol.pft_gw > 1.0 && sol.pft_gw < 5000.0, "PFT {}", sol.pft_gw);
        assert!(sol.z_fc_m > 500.0 && sol.z_fc_m < 9000.0, "z_fc {}", sol.z_fc_m);
        assert!(sol.dtheta_fc_k >= 0.0 && sol.dtheta_fc_k < 40.0);
        assert!(sol.beta_star > 0.0, "dry column should need fire buoyancy");

        let doubled = idealized_column(2.0);
        let PftOutcome::Solution(sol2) = pft_column(&doubled, &PftOptions::default()) else {
            panic!("doubled-wind column must also solve");
        };
        assert!(
            (sol2.pft_gw / sol.pft_gw - 2.0).abs() < 0.02,
            "PFT should double with wind: {} -> {}",
            sol.pft_gw,
            sol2.pft_gw
        );
        assert!((sol2.z_fc_m - sol.z_fc_m).abs() < 1.0, "thermo unchanged");
    }

    /// A profile whose data top is warmer than -20 C cannot verify cloud
    /// depth: Unevaluable, never a fake solution.
    #[test]
    fn shallow_warm_profile_is_unevaluable() {
        let mut column = idealized_column(1.0);
        column.retain(|l| l.t_c > -10.0);
        assert_eq!(pft_column(&column, &PftOptions::default()), PftOutcome::Unevaluable);
    }

    /// Build a plausible interior-West summer column. `wind_scale`
    /// multiplies the wind profile only.
    fn idealized_column(wind_scale: f64) -> Vec<Level> {
        let psfc = 950.0;
        let theta_ml = 315.0; // hot, deep dry boundary layer
        let mut column = vec![Level {
            p_hpa: psfc,
            h_m: 0.0,
            t_c: theta_ml * (psfc / 1000.0f64).powf(KAPPA) - ZEROC_K,
            q_kgkg: 0.006,
            u_ms: 8.0 * wind_scale,
            v_ms: 2.0 * wind_scale,
        }];
        let mut h = 0.0;
        let mut p = psfc;
        while p > 200.0 {
            p -= 25.0;
            // Crude hypsometric step with T ~ 0 C mean.
            h += 287.04 * 260.0 / 9.8 * ((p + 25.0) / p).ln();
            let t_c = if p > 600.0 {
                // Dry-adiabatic ML up to ~600 hPa.
                theta_ml * (p / 1000.0f64).powf(KAPPA) - ZEROC_K
            } else {
                // Slightly stable above: theta grows 3 K / 50 hPa.
                let theta = theta_ml + (600.0 - p) * 0.06;
                theta * (p / 1000.0f64).powf(KAPPA) - ZEROC_K
            };
            let q = if p > 600.0 { 0.006 } else { 0.002 };
            column.push(Level {
                p_hpa: p,
                h_m: h,
                t_c,
                q_kgkg: q,
                u_ms: (8.0 + (psfc - p) * 0.02) * wind_scale,
                v_ms: 2.0 * wind_scale,
            });
        }
        column
    }
}
