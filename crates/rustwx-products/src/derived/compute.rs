use rayon::prelude::*;
use rustwx_calc::{
    CalcError, EcapeVolumeInputs, FixedStpInputs, GridShape as CalcGridShape, SurfaceInputs,
    TemperatureAdvectionInputs, VolumeShape, WindGridInputs, compute_dcape, compute_ehi_01km,
    compute_ehi_03km, compute_lapse_rate_0_3km, compute_lapse_rate_700_500, compute_lifted_index,
    compute_mlcape_cin, compute_mucape_cin, compute_sbcape_cin, compute_shear_01km,
    compute_shear_06km, compute_srh_01km_hemispheric, compute_srh_03km_hemispheric,
    compute_stp_fixed, compute_surface_thermo,
};

use crate::gridded::{
    PressureFields as GenericPressureFields, SurfaceFields as GenericSurfaceFields,
};

use super::KNOTS_PER_MS;
use super::recipes::{DerivedRecipe, DerivedRequirements, derived_compute_recipes_need_pressure};

pub(super) trait SurfaceFieldSet {
    fn lat(&self) -> &[f64];
    fn lon(&self) -> &[f64];
    fn nx(&self) -> usize;
    fn ny(&self) -> usize;
    fn projection(&self) -> Option<&rustwx_core::GridProjection>;
    fn orog_m(&self) -> &[f64];
    fn psfc_pa(&self) -> &[f64];
    fn t2_k(&self) -> &[f64];
    fn q2_kgkg(&self) -> &[f64];
    fn u10_ms(&self) -> &[f64];
    fn v10_ms(&self) -> &[f64];
}

pub(super) trait PressureFieldSet {
    fn pressure_levels_hpa(&self) -> &[f64];
    fn pressure_3d_pa(&self) -> Option<&[f64]>;
    fn temperature_c_3d(&self) -> &[f64];
    fn qvapor_kgkg_3d(&self) -> &[f64];
    fn u_ms_3d(&self) -> &[f64];
    fn v_ms_3d(&self) -> &[f64];
    fn gh_m_3d(&self) -> &[f64];
}

impl SurfaceFieldSet for GenericSurfaceFields {
    fn lat(&self) -> &[f64] {
        &self.lat
    }

    fn lon(&self) -> &[f64] {
        &self.lon
    }

    fn nx(&self) -> usize {
        self.nx
    }

    fn ny(&self) -> usize {
        self.ny
    }

    fn projection(&self) -> Option<&rustwx_core::GridProjection> {
        self.projection.as_ref()
    }

    fn orog_m(&self) -> &[f64] {
        &self.orog_m
    }

    fn psfc_pa(&self) -> &[f64] {
        &self.psfc_pa
    }

    fn t2_k(&self) -> &[f64] {
        &self.t2_k
    }

    fn q2_kgkg(&self) -> &[f64] {
        &self.q2_kgkg
    }

    fn u10_ms(&self) -> &[f64] {
        &self.u10_ms
    }

    fn v10_ms(&self) -> &[f64] {
        &self.v10_ms
    }
}

impl PressureFieldSet for GenericPressureFields {
    fn pressure_levels_hpa(&self) -> &[f64] {
        &self.pressure_levels_hpa
    }

    fn pressure_3d_pa(&self) -> Option<&[f64]> {
        self.pressure_3d_pa.as_deref()
    }

    fn temperature_c_3d(&self) -> &[f64] {
        &self.temperature_c_3d
    }

    fn qvapor_kgkg_3d(&self) -> &[f64] {
        &self.qvapor_kgkg_3d
    }

    fn u_ms_3d(&self) -> &[f64] {
        &self.u_ms_3d
    }

    fn v_ms_3d(&self) -> &[f64] {
        &self.v_ms_3d
    }

    fn gh_m_3d(&self) -> &[f64] {
        &self.gh_m_3d
    }
}

struct EmptyPressureFields;

impl PressureFieldSet for EmptyPressureFields {
    fn pressure_levels_hpa(&self) -> &[f64] {
        &[]
    }

    fn pressure_3d_pa(&self) -> Option<&[f64]> {
        None
    }

    fn temperature_c_3d(&self) -> &[f64] {
        &[]
    }

    fn qvapor_kgkg_3d(&self) -> &[f64] {
        &[]
    }

    fn u_ms_3d(&self) -> &[f64] {
        &[]
    }

    fn v_ms_3d(&self) -> &[f64] {
        &[]
    }

    fn gh_m_3d(&self) -> &[f64] {
        &[]
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct DerivedComputedFields {
    pub(super) sbcape_jkg: Option<Vec<f64>>,
    pub(super) sbcin_jkg: Option<Vec<f64>>,
    pub(super) sblcl_m: Option<Vec<f64>>,
    pub(super) mlcape_jkg: Option<Vec<f64>>,
    pub(super) mlcin_jkg: Option<Vec<f64>>,
    pub(super) mucape_jkg: Option<Vec<f64>>,
    pub(super) mucin_jkg: Option<Vec<f64>>,
    pub(super) dcape_jkg: Option<Vec<f64>>,
    pub(super) theta_e_2m_k: Option<Vec<f64>>,
    pub(super) vpd_2m_hpa: Option<Vec<f64>>,
    pub(super) dewpoint_depression_2m_c: Option<Vec<f64>>,
    pub(super) wetbulb_2m_c: Option<Vec<f64>>,
    pub(super) fire_weather_composite: Option<Vec<f64>>,
    pub(super) apparent_temperature_2m_c: Option<Vec<f64>>,
    pub(super) heat_index_2m_c: Option<Vec<f64>>,
    pub(super) wind_chill_2m_c: Option<Vec<f64>>,
    pub(super) surface_u10_ms: Option<Vec<f64>>,
    pub(super) surface_v10_ms: Option<Vec<f64>>,
    pub(super) lifted_index_c: Option<Vec<f64>>,
    pub(super) lapse_rate_700_500_cpkm: Option<Vec<f64>>,
    pub(super) lapse_rate_0_3km_cpkm: Option<Vec<f64>>,
    pub(super) shear_01km_kt: Option<Vec<f64>>,
    pub(super) shear_06km_kt: Option<Vec<f64>>,
    pub(super) srh_01km_m2s2: Option<Vec<f64>>,
    pub(super) srh_03km_m2s2: Option<Vec<f64>>,
    pub(super) ehi_01km: Option<Vec<f64>>,
    pub(super) ehi_03km: Option<Vec<f64>>,
    pub(super) stp_fixed: Option<Vec<f64>>,
    pub(super) scp_mu_03km_06km_proxy: Option<Vec<f64>>,
    pub(super) temperature_advection_700mb_cph: Option<Vec<f64>>,
    pub(super) temperature_advection_850mb_cph: Option<Vec<f64>>,
}

pub(super) fn compute_derived_fields_generic<S, P>(
    surface: &S,
    pressure: &P,
    recipes: &[DerivedRecipe],
) -> Result<DerivedComputedFields, Box<dyn std::error::Error>>
where
    S: SurfaceFieldSet,
    P: PressureFieldSet,
{
    fn missing_dependency(name: &str) -> std::io::Error {
        std::io::Error::other(format!(
            "derived compute missing required dependency: {name}"
        ))
    }

    fn require_option_ref<'a, T>(
        option: &'a Option<T>,
        name: &str,
    ) -> Result<&'a T, Box<dyn std::error::Error>> {
        option
            .as_ref()
            .ok_or_else(|| missing_dependency(name))
            .map_err(Into::into)
    }

    fn require_option_copy<T: Copy>(
        option: Option<T>,
        name: &str,
    ) -> Result<T, Box<dyn std::error::Error>> {
        option
            .ok_or_else(|| missing_dependency(name))
            .map_err(Into::into)
    }

    let requirements = DerivedRequirements::from_recipes(recipes);
    let grid = CalcGridShape::new(surface.nx(), surface.ny())?;
    let mut computed = DerivedComputedFields::default();

    let surface_inputs = SurfaceInputs {
        psfc_pa: surface.psfc_pa(),
        t2_k: surface.t2_k(),
        q2_kgkg: surface.q2_kgkg(),
        u10_ms: surface.u10_ms(),
        v10_ms: surface.v10_ms(),
    };

    let shape = if requirements.needs_height_agl() {
        Some(VolumeShape::new(
            grid,
            pressure.pressure_levels_hpa().len(),
        )?)
    } else {
        None
    };
    let pressure_pa = if requirements.needs_volume() {
        Some(
            pressure
                .pressure_3d_pa()
                .map(|values| values.to_vec())
                .unwrap_or_else(|| {
                    pressure
                        .pressure_levels_hpa()
                        .iter()
                        .map(|level_hpa| level_hpa * 100.0)
                        .collect()
                }),
        )
    } else {
        None
    };
    let height_agl_3d = if requirements.needs_height_agl() {
        Some(compute_height_agl_3d_generic(
            surface,
            pressure,
            grid,
            require_option_copy(shape, "volume shape for height_agl")?,
        ))
    } else {
        None
    };

    let make_volume = || -> Result<EcapeVolumeInputs<'_>, Box<dyn std::error::Error>> {
        Ok(EcapeVolumeInputs {
            pressure_pa: require_option_ref(
                &pressure_pa,
                "pressure volume for derived thermodynamics",
            )?,
            temperature_c: pressure.temperature_c_3d(),
            qvapor_kgkg: pressure.qvapor_kgkg_3d(),
            height_agl_m: require_option_ref(
                &height_agl_3d,
                "height_agl for derived thermodynamics",
            )?,
            u_ms: pressure.u_ms_3d(),
            v_ms: pressure.v_ms_3d(),
            nz: require_option_copy(shape, "volume shape for derived thermodynamics")?.nz,
        })
    };
    let make_wind = || -> Result<WindGridInputs<'_>, Box<dyn std::error::Error>> {
        Ok(WindGridInputs {
            shape: require_option_copy(shape, "volume shape for wind diagnostics")?,
            u_3d_ms: pressure.u_ms_3d(),
            v_3d_ms: pressure.v_ms_3d(),
            height_agl_3d_m: require_option_ref(&height_agl_3d, "height_agl for wind diagnostics")?,
        })
    };

    let sb = if requirements.sb {
        Some(compute_sbcape_cin(
            grid,
            make_volume()?,
            surface_inputs,
            None,
        )?)
    } else {
        None
    };
    let ml = if requirements.ml {
        Some(compute_mlcape_cin(
            grid,
            make_volume()?,
            surface_inputs,
            None,
        )?)
    } else {
        None
    };
    let mu = if requirements.mu {
        Some(compute_mucape_cin(
            grid,
            make_volume()?,
            surface_inputs,
            None,
        )?)
    } else {
        None
    };

    if let Some(sb) = sb.as_ref() {
        computed.sbcape_jkg = Some(sb.cape_jkg.clone());
        computed.sbcin_jkg = Some(sb.cin_jkg.clone());
        computed.sblcl_m = Some(sb.lcl_m.clone());
    }
    if let Some(ml) = ml.as_ref() {
        computed.mlcape_jkg = Some(ml.cape_jkg.clone());
        computed.mlcin_jkg = Some(ml.cin_jkg.clone());
    }
    if let Some(mu) = mu.as_ref() {
        computed.mucape_jkg = Some(mu.cape_jkg.clone());
        computed.mucin_jkg = Some(mu.cin_jkg.clone());
    }
    if requirements.dcape {
        computed.dcape_jkg = Some(compute_dcape(grid, make_volume()?, surface_inputs)?);
    }

    if requirements.surface_thermo {
        let surface_thermo = compute_surface_thermo(grid, surface_inputs)?;
        if recipes.contains(&DerivedRecipe::ThetaE2m10mWinds) {
            computed.theta_e_2m_k = Some(surface_thermo.theta_e_2m_k);
            computed.surface_u10_ms = Some(surface.u10_ms().to_vec());
            computed.surface_v10_ms = Some(surface.v10_ms().to_vec());
        }
        if recipes.contains(&DerivedRecipe::Vpd2m) {
            computed.vpd_2m_hpa = Some(surface_thermo.vpd_2m_hpa);
        }
        if recipes.contains(&DerivedRecipe::DewpointDepression2m) {
            computed.dewpoint_depression_2m_c = Some(surface_thermo.dewpoint_depression_2m_c);
        }
        if recipes.contains(&DerivedRecipe::Wetbulb2m) {
            computed.wetbulb_2m_c = Some(surface_thermo.wetbulb_2m_c);
        }
        if recipes.contains(&DerivedRecipe::FireWeatherComposite) {
            computed.fire_weather_composite = Some(surface_thermo.fire_weather_composite);
        }
        if recipes.contains(&DerivedRecipe::ApparentTemperature2m) {
            computed.apparent_temperature_2m_c = Some(surface_thermo.apparent_temperature_2m_c);
        }
        if recipes.contains(&DerivedRecipe::HeatIndex2m) {
            computed.heat_index_2m_c = Some(surface_thermo.heat_index_2m_c);
        }
        if recipes.contains(&DerivedRecipe::WindChill2m) {
            computed.wind_chill_2m_c = Some(surface_thermo.wind_chill_2m_c);
        }
    }

    if requirements.lifted_index {
        computed.lifted_index_c = Some(compute_lifted_index(grid, make_volume()?, surface_inputs)?);
    }
    if requirements.lapse_rate_700_500 {
        computed.lapse_rate_700_500_cpkm = Some(compute_lapse_rate_700_500(grid, make_volume()?)?);
    }
    if requirements.lapse_rate_0_3km {
        computed.lapse_rate_0_3km_cpkm = Some(compute_lapse_rate_0_3km(
            grid,
            make_volume()?,
            surface_inputs,
        )?);
    }

    let shear_01km_ms = if requirements.shear_01km {
        Some(compute_shear_01km(make_wind()?)?)
    } else {
        None
    };
    let shear_06km_ms = if requirements.shear_06km {
        Some(compute_shear_06km(make_wind()?)?)
    } else {
        None
    };
    let srh_01km_m2s2 = if requirements.srh_01km {
        Some(compute_srh_01km_hemispheric(make_wind()?, surface.lat())?)
    } else {
        None
    };
    let srh_03km_m2s2 = if requirements.srh_03km {
        Some(compute_srh_03km_hemispheric(make_wind()?, surface.lat())?)
    } else {
        None
    };

    if let Some(values) = shear_01km_ms {
        computed.shear_01km_kt = Some(
            values
                .into_iter()
                .map(|value| value * KNOTS_PER_MS)
                .collect(),
        );
    }
    if let Some(values) = shear_06km_ms.as_ref() {
        computed.shear_06km_kt = Some(
            values
                .iter()
                .copied()
                .map(|value| value * KNOTS_PER_MS)
                .collect(),
        );
    }
    if let Some(values) = srh_01km_m2s2.as_ref() {
        computed.srh_01km_m2s2 = Some(values.clone());
    }
    if let Some(values) = srh_03km_m2s2.as_ref() {
        computed.srh_03km_m2s2 = Some(values.clone());
    }

    if requirements.ehi_01km {
        let sb = require_option_ref(&sb, "surface-based CAPE/CIN outputs for EHI 0-1 km")?;
        let srh_01km = require_option_ref(&srh_01km_m2s2, "0-1 km SRH for EHI 0-1 km")?;
        computed.ehi_01km = Some(compute_ehi_01km(grid, &sb.cape_jkg, srh_01km)?);
    }
    if requirements.ehi_03km {
        let sb = require_option_ref(&sb, "surface-based CAPE/CIN outputs for EHI 0-3 km")?;
        let srh_03km = require_option_ref(&srh_03km_m2s2, "0-3 km SRH for EHI 0-3 km")?;
        computed.ehi_03km = Some(compute_ehi_03km(grid, &sb.cape_jkg, srh_03km)?);
    }
    if requirements.stp_fixed {
        let sb = require_option_ref(&sb, "surface-based CAPE/CIN outputs for STP fixed")?;
        let srh_01km = require_option_ref(&srh_01km_m2s2, "0-1 km SRH for STP fixed")?;
        let shear_06km = require_option_ref(&shear_06km_ms, "0-6 km shear for STP fixed")?;
        computed.stp_fixed = Some(compute_stp_fixed(FixedStpInputs {
            grid,
            sbcape_jkg: &sb.cape_jkg,
            lcl_m: &sb.lcl_m,
            srh_1km_m2s2: srh_01km,
            shear_6km_ms: shear_06km,
        })?);
    }
    if requirements.scp_mu_03km_06km_proxy {
        let mu = require_option_ref(&mu, "most-unstable CAPE/CIN outputs for SCP proxy")?;
        let srh_03km = require_option_ref(&srh_03km_m2s2, "0-3 km SRH for SCP proxy")?;
        let shear_06km = require_option_ref(&shear_06km_ms, "0-6 km shear for SCP proxy")?;
        computed.scp_mu_03km_06km_proxy = Some(rustwx_calc::compute_scp(
            grid,
            &mu.cape_jkg,
            srh_03km,
            shear_06km,
        )?);
    }

    if requirements.needs_grid_spacing() {
        let (dx_m, dy_m) = estimate_grid_spacing_m(surface)?;
        if requirements.temperature_advection_700mb {
            let t700 = pressure_level_slice_or_interp(
                pressure,
                pressure.temperature_c_3d(),
                700.0,
                grid.len(),
            )
            .ok_or("missing 700 mb temperature slice in HRRR pressure bundle")?;
            let u700 =
                pressure_level_slice_or_interp(pressure, pressure.u_ms_3d(), 700.0, grid.len())
                    .ok_or("missing 700 mb u-wind slice in HRRR pressure bundle")?;
            let v700 =
                pressure_level_slice_or_interp(pressure, pressure.v_ms_3d(), 700.0, grid.len())
                    .ok_or("missing 700 mb v-wind slice in HRRR pressure bundle")?;
            computed.temperature_advection_700mb_cph = Some(
                rustwx_calc::compute_temperature_advection_700mb(TemperatureAdvectionInputs {
                    grid,
                    temperature_2d: &t700,
                    u_2d_ms: &u700,
                    v_2d_ms: &v700,
                    dx_m,
                    dy_m,
                })?
                .into_iter()
                .map(|value| value * 3600.0)
                .collect(),
            );
        }
        if requirements.temperature_advection_850mb {
            let t850 = pressure_level_slice_or_interp(
                pressure,
                pressure.temperature_c_3d(),
                850.0,
                grid.len(),
            )
            .ok_or("missing 850 mb temperature slice in HRRR pressure bundle")?;
            let u850 =
                pressure_level_slice_or_interp(pressure, pressure.u_ms_3d(), 850.0, grid.len())
                    .ok_or("missing 850 mb u-wind slice in HRRR pressure bundle")?;
            let v850 =
                pressure_level_slice_or_interp(pressure, pressure.v_ms_3d(), 850.0, grid.len())
                    .ok_or("missing 850 mb v-wind slice in HRRR pressure bundle")?;
            computed.temperature_advection_850mb_cph = Some(
                rustwx_calc::compute_temperature_advection_850mb(TemperatureAdvectionInputs {
                    grid,
                    temperature_2d: &t850,
                    u_2d_ms: &u850,
                    v_2d_ms: &v850,
                    dx_m,
                    dy_m,
                })?
                .into_iter()
                .map(|value| value * 3600.0)
                .collect(),
            );
        }
    }

    Ok(computed)
}

pub(super) fn compute_surface_only_derived_fields<S>(
    surface: &S,
    recipes: &[DerivedRecipe],
) -> Result<DerivedComputedFields, Box<dyn std::error::Error>>
where
    S: SurfaceFieldSet,
{
    if derived_compute_recipes_need_pressure(recipes) {
        return Err("surface-only derived compute received a pressure-dependent recipe".into());
    }
    compute_derived_fields_generic(surface, &EmptyPressureFields, recipes)
}

fn level_slice<'a>(
    values_3d: &'a [f64],
    levels_hpa: &[f64],
    target_hpa: f64,
    nxy: usize,
) -> Option<&'a [f64]> {
    let level_idx = levels_hpa
        .iter()
        .position(|level| (level - target_hpa).abs() < 0.25)?;
    let start = level_idx * nxy;
    let end = start + nxy;
    values_3d.get(start..end)
}

pub(super) fn pressure_level_slice_or_interp<P>(
    pressure: &P,
    values_3d: &[f64],
    target_hpa: f64,
    nxy: usize,
) -> Option<Vec<f64>>
where
    P: PressureFieldSet,
{
    if let Some(slice) = level_slice(values_3d, pressure.pressure_levels_hpa(), target_hpa, nxy) {
        return Some(slice.to_vec());
    }

    let pressure_3d_pa = pressure.pressure_3d_pa()?;
    let nz = pressure.pressure_levels_hpa().len();
    if nz == 0 || values_3d.len() != pressure_3d_pa.len() || values_3d.len() != nxy * nz {
        return None;
    }

    let log_target = target_hpa.ln();
    Some(
        (0..nxy)
            .into_par_iter()
            .map(|ij| {
                for k in 0..nz.saturating_sub(1) {
                    let idx0 = k * nxy + ij;
                    let idx1 = (k + 1) * nxy + ij;
                    let p0 = pressure_3d_pa[idx0] / 100.0;
                    let p1 = pressure_3d_pa[idx1] / 100.0;
                    if !p0.is_finite() || !p1.is_finite() || p0 <= 0.0 || p1 <= 0.0 {
                        continue;
                    }
                    if (p0 >= target_hpa && p1 <= target_hpa)
                        || (p0 <= target_hpa && p1 >= target_hpa)
                    {
                        let v0 = values_3d[idx0];
                        let v1 = values_3d[idx1];
                        let log0 = p0.ln();
                        let log1 = p1.ln();
                        let denom = log1 - log0;
                        if denom.abs() < 1.0e-12 {
                            return 0.5 * (v0 + v1);
                        }
                        let frac = (log_target - log0) / denom;
                        return v0 + frac * (v1 - v0);
                    }
                }
                f64::NAN
            })
            .collect(),
    )
}

fn compute_height_agl_3d_generic<S, P>(
    surface: &S,
    pressure: &P,
    grid: CalcGridShape,
    shape: VolumeShape,
) -> Vec<f64>
where
    S: SurfaceFieldSet,
    P: PressureFieldSet,
{
    let mut height_agl_3d = pressure
        .gh_m_3d()
        .iter()
        .enumerate()
        .map(|(idx, &value)| {
            let ij = idx % grid.len();
            (value - surface.orog_m()[ij]).max(0.0)
        })
        .collect::<Vec<_>>();

    for k in 1..shape.nz {
        let level_offset = k * grid.len();
        let prev_offset = (k - 1) * grid.len();
        for ij in 0..grid.len() {
            let min_height = height_agl_3d[prev_offset + ij] + 1.0;
            if height_agl_3d[level_offset + ij] < min_height {
                height_agl_3d[level_offset + ij] = min_height;
            }
        }
    }

    height_agl_3d
}

fn estimate_grid_spacing_m<S>(surface: &S) -> Result<(f64, f64), CalcError>
where
    S: SurfaceFieldSet,
{
    if surface.nx() < 2 || surface.ny() < 2 {
        return Err(CalcError::LengthMismatch {
            field: "grid_spacing",
            expected: 4,
            actual: surface.nx() * surface.ny(),
        });
    }

    let mut dx_sum = 0.0;
    let mut dx_count = 0usize;
    for y in 0..surface.ny() {
        let row_offset = y * surface.nx();
        for x in 0..(surface.nx() - 1) {
            let left = row_offset + x;
            let right = left + 1;
            let distance = haversine_m(
                surface.lat()[left],
                surface.lon()[left],
                surface.lat()[right],
                surface.lon()[right],
            );
            if distance.is_finite() && distance > 0.0 {
                dx_sum += distance;
                dx_count += 1;
            }
        }
    }

    let mut dy_sum = 0.0;
    let mut dy_count = 0usize;
    for y in 0..(surface.ny() - 1) {
        let row_offset = y * surface.nx();
        let next_row_offset = (y + 1) * surface.nx();
        for x in 0..surface.nx() {
            let top = row_offset + x;
            let bottom = next_row_offset + x;
            let distance = haversine_m(
                surface.lat()[top],
                surface.lon()[top],
                surface.lat()[bottom],
                surface.lon()[bottom],
            );
            if distance.is_finite() && distance > 0.0 {
                dy_sum += distance;
                dy_count += 1;
            }
        }
    }

    if dx_count == 0 || dy_count == 0 {
        return Err(CalcError::LengthMismatch {
            field: "grid_spacing",
            expected: 2,
            actual: 0,
        });
    }

    Ok((dx_sum / dx_count as f64, dy_sum / dy_count as f64))
}

pub(super) fn haversine_m(lat1_deg: f64, lon1_deg: f64, lat2_deg: f64, lon2_deg: f64) -> f64 {
    let lat1 = lat1_deg.to_radians();
    let lon1 = lon1_deg.to_radians();
    let lat2 = lat2_deg.to_radians();
    let lon2 = lon2_deg.to_radians();
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let a = (dlat * 0.5).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon * 0.5).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    6_371_000.0 * c
}
