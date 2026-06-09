use rustwx_core::{CanonicalBundleDescriptor, Field2D, ModelId, ProductKey};
use rustwx_models::{LatestRun, resolve_canonical_bundle_product};
use serde::{Deserialize, Serialize};

use crate::gridded::{
    PressureFields as GenericPressureFields, SurfaceFields as GenericSurfaceFields,
};
use crate::planner::ExecutionPlanBuilder;
use crate::publication::PublishedFetchIdentity;
use crate::runtime::{BundleLoaderConfig, LoadedBundleSet, load_execution_plan};
use crate::severe::build_planned_input_fetches;
use crate::source::ProductSourceRoute;

use super::compute::{DerivedComputedFields, compute_derived_fields_generic};
use super::planning::{build_derived_execution_plan, plan_derived_recipes};
use super::recipes::DerivedRecipe;
use super::types::DerivedRecipeBlocker;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DerivedQueryField {
    pub recipe_slug: String,
    pub title: String,
    pub units: String,
    pub values: Vec<f64>,
    pub nx: usize,
    pub ny: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct DerivedSampledProductField {
    pub recipe_slug: String,
    pub source_route: ProductSourceRoute,
    pub field: Field2D,
    pub input_fetches: Vec<PublishedFetchIdentity>,
}

#[derive(Debug, Clone)]
pub(crate) struct DerivedSampledProductSet {
    pub fields: Vec<DerivedSampledProductField>,
    pub blockers: Vec<DerivedRecipeBlocker>,
}

pub(crate) fn required_derived_fetch_products(
    model: ModelId,
    recipe_slugs: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let recipes = plan_derived_recipes(recipe_slugs)?;
    if recipes.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![
        resolve_canonical_bundle_product(model, CanonicalBundleDescriptor::SurfaceAnalysis, None)
            .native_product,
        resolve_canonical_bundle_product(model, CanonicalBundleDescriptor::PressureAnalysis, None)
            .native_product,
    ])
}

pub(crate) fn load_derived_sampled_fields_from_latest(
    latest: &LatestRun,
    forecast_hour: u16,
    cache_root: &std::path::Path,
    use_cache: bool,
    recipe_slugs: &[String],
) -> Result<DerivedSampledProductSet, Box<dyn std::error::Error>> {
    let recipes = plan_derived_recipes(recipe_slugs)?;
    if recipes.is_empty() {
        return Ok(DerivedSampledProductSet {
            fields: Vec::new(),
            blockers: Vec::new(),
        });
    }

    let plan =
        build_derived_execution_plan(latest, forecast_hour, None, None, true, true, &Vec::new());
    let loaded = load_execution_plan(
        plan,
        &BundleLoaderConfig::new(cache_root.to_path_buf(), use_cache),
    )?;
    load_derived_sampled_fields_from_loaded(recipe_slugs, &loaded)
}

pub(crate) fn build_derived_sampled_execution_plan(
    latest: &LatestRun,
    forecast_hour: u16,
    recipe_slugs: &[String],
) -> Result<crate::planner::ExecutionPlan, Box<dyn std::error::Error>> {
    let recipes = plan_derived_recipes(recipe_slugs)?;
    if recipes.is_empty() {
        return Ok(ExecutionPlanBuilder::new(latest, forecast_hour).build());
    }
    Ok(build_derived_execution_plan(
        latest,
        forecast_hour,
        None,
        None,
        true,
        true,
        &Vec::new(),
    ))
}

pub(crate) fn load_derived_sampled_fields_from_loaded(
    recipe_slugs: &[String],
    loaded: &LoadedBundleSet,
) -> Result<DerivedSampledProductSet, Box<dyn std::error::Error>> {
    let recipes = plan_derived_recipes(recipe_slugs)?;
    if recipes.is_empty() {
        return Ok(DerivedSampledProductSet {
            fields: Vec::new(),
            blockers: Vec::new(),
        });
    }

    let (_, surface_decode, _, pressure_decode) = loaded
        .require_surface_pressure_pair()
        .map_err(|err| format!("derived sampling surface/pressure pair unavailable: {err}"))?;
    let input_fetches = build_planned_input_fetches(loaded);
    let mut fields = Vec::new();
    let mut blockers = Vec::new();

    match compute_derived_fields_generic(&surface_decode.value, &pressure_decode.value, &recipes) {
        Ok(computed) => {
            for recipe in recipes {
                match derived_query_field_from_computed(
                    surface_decode.value.nx,
                    surface_decode.value.ny,
                    recipe,
                    &computed,
                ) {
                    Ok(query) => {
                        let field = Field2D::new(
                            ProductKey::named(query.recipe_slug.clone()),
                            query.units.clone(),
                            surface_decode.value.core_grid()?,
                            query.values.into_iter().map(|value| value as f32).collect(),
                        )?;
                        fields.push(DerivedSampledProductField {
                            recipe_slug: query.recipe_slug,
                            source_route: ProductSourceRoute::CanonicalDerived,
                            field,
                            input_fetches: input_fetches.clone(),
                        });
                    }
                    Err(err) => blockers.push(DerivedRecipeBlocker {
                        recipe_slug: recipe.slug().to_string(),
                        source_route: ProductSourceRoute::CanonicalDerived,
                        reason: err.to_string(),
                    }),
                }
            }
        }
        Err(shared_err) => {
            for recipe in recipes {
                match compute_derived_query_field(
                    &surface_decode.value,
                    &pressure_decode.value,
                    recipe.slug(),
                ) {
                    Ok(query) => {
                        let field = Field2D::new(
                            ProductKey::named(query.recipe_slug.clone()),
                            query.units.clone(),
                            surface_decode.value.core_grid()?,
                            query.values.into_iter().map(|value| value as f32).collect(),
                        )?;
                        fields.push(DerivedSampledProductField {
                            recipe_slug: query.recipe_slug,
                            source_route: ProductSourceRoute::CanonicalDerived,
                            field,
                            input_fetches: input_fetches.clone(),
                        });
                    }
                    Err(err) => blockers.push(DerivedRecipeBlocker {
                        recipe_slug: recipe.slug().to_string(),
                        source_route: ProductSourceRoute::CanonicalDerived,
                        reason: format!(
                            "shared derived compute failed: {shared_err}; recipe failed: {err}"
                        ),
                    }),
                }
            }
        }
    }

    Ok(DerivedSampledProductSet { fields, blockers })
}

pub(crate) fn compute_derived_query_field(
    surface: &GenericSurfaceFields,
    pressure: &GenericPressureFields,
    recipe_slug: &str,
) -> Result<DerivedQueryField, Box<dyn std::error::Error>> {
    let recipe = DerivedRecipe::parse(recipe_slug).map_err(std::io::Error::other)?;
    if recipe.is_heavy() {
        return Err(format!(
            "heavy derived recipe '{}' is not exposed through the lightweight query path",
            recipe.slug()
        )
        .into());
    }

    let computed = compute_derived_fields_generic(surface, pressure, &[recipe])?;
    derived_query_field_from_computed(surface.nx, surface.ny, recipe, &computed)
}

fn derived_query_field_from_computed(
    nx: usize,
    ny: usize,
    recipe: DerivedRecipe,
    computed: &DerivedComputedFields,
) -> Result<DerivedQueryField, Box<dyn std::error::Error>> {
    fn take_values(
        values: &Option<Vec<f64>>,
        recipe: DerivedRecipe,
        field_name: &str,
    ) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
        values.clone().ok_or_else(|| {
            format!(
                "derived field '{field_name}' was not computed for requested recipe '{}'",
                recipe.slug()
            )
            .into()
        })
    }

    let (values, units) = match recipe {
        DerivedRecipe::Sbcape => (
            take_values(&computed.sbcape_jkg, recipe, "sbcape_jkg")?,
            "J/kg",
        ),
        DerivedRecipe::Sbcin => (
            take_values(&computed.sbcin_jkg, recipe, "sbcin_jkg")?,
            "J/kg",
        ),
        DerivedRecipe::Sblcl => (take_values(&computed.sblcl_m, recipe, "sblcl_m")?, "m"),
        DerivedRecipe::Mlcape => (
            take_values(&computed.mlcape_jkg, recipe, "mlcape_jkg")?,
            "J/kg",
        ),
        DerivedRecipe::Mlcin => (
            take_values(&computed.mlcin_jkg, recipe, "mlcin_jkg")?,
            "J/kg",
        ),
        DerivedRecipe::Mucape => (
            take_values(&computed.mucape_jkg, recipe, "mucape_jkg")?,
            "J/kg",
        ),
        DerivedRecipe::Mucin => (
            take_values(&computed.mucin_jkg, recipe, "mucin_jkg")?,
            "J/kg",
        ),
        DerivedRecipe::Dcape => (
            take_values(&computed.dcape_jkg, recipe, "dcape_jkg")?,
            "J/kg",
        ),
        DerivedRecipe::ThetaE2m10mWinds => (
            take_values(&computed.theta_e_2m_k, recipe, "theta_e_2m_k")?,
            "K",
        ),
        DerivedRecipe::Vpd2m => (
            take_values(&computed.vpd_2m_hpa, recipe, "vpd_2m_hpa")?,
            "hPa",
        ),
        DerivedRecipe::DewpointDepression2m => (
            take_values(
                &computed.dewpoint_depression_2m_c,
                recipe,
                "dewpoint_depression_2m_c",
            )?,
            "degC",
        ),
        DerivedRecipe::Wetbulb2m => (
            take_values(&computed.wetbulb_2m_c, recipe, "wetbulb_2m_c")?,
            "degC",
        ),
        DerivedRecipe::FireWeatherComposite => (
            take_values(
                &computed.fire_weather_composite,
                recipe,
                "fire_weather_composite",
            )?,
            "index",
        ),
        DerivedRecipe::ApparentTemperature2m => (
            take_values(
                &computed.apparent_temperature_2m_c,
                recipe,
                "apparent_temperature_2m_c",
            )?,
            "degC",
        ),
        DerivedRecipe::HeatIndex2m => (
            take_values(&computed.heat_index_2m_c, recipe, "heat_index_2m_c")?,
            "degC",
        ),
        DerivedRecipe::WindChill2m => (
            take_values(&computed.wind_chill_2m_c, recipe, "wind_chill_2m_c")?,
            "degC",
        ),
        DerivedRecipe::LiftedIndex => (
            take_values(&computed.lifted_index_c, recipe, "lifted_index_c")?,
            "degC",
        ),
        DerivedRecipe::LapseRate700500 => (
            take_values(
                &computed.lapse_rate_700_500_cpkm,
                recipe,
                "lapse_rate_700_500_cpkm",
            )?,
            "degC/km",
        ),
        DerivedRecipe::LapseRate03km => (
            take_values(
                &computed.lapse_rate_0_3km_cpkm,
                recipe,
                "lapse_rate_0_3km_cpkm",
            )?,
            "degC/km",
        ),
        DerivedRecipe::BulkShear01km => (
            take_values(&computed.shear_01km_kt, recipe, "shear_01km_kt")?,
            "kt",
        ),
        DerivedRecipe::BulkShear06km => (
            take_values(&computed.shear_06km_kt, recipe, "shear_06km_kt")?,
            "kt",
        ),
        DerivedRecipe::Srh01km => (
            take_values(&computed.srh_01km_m2s2, recipe, "srh_01km_m2s2")?,
            "m^2/s^2",
        ),
        DerivedRecipe::Srh03km => (
            take_values(&computed.srh_03km_m2s2, recipe, "srh_03km_m2s2")?,
            "m^2/s^2",
        ),
        DerivedRecipe::Ehi01km => (
            take_values(&computed.ehi_01km, recipe, "ehi_01km")?,
            "dimensionless",
        ),
        DerivedRecipe::Ehi03km => (
            take_values(&computed.ehi_03km, recipe, "ehi_03km")?,
            "dimensionless",
        ),
        DerivedRecipe::StpFixed => (
            take_values(&computed.stp_fixed, recipe, "stp_fixed")?,
            "dimensionless",
        ),
        DerivedRecipe::ScpMu03km06kmProxy => (
            take_values(
                &computed.scp_mu_03km_06km_proxy,
                recipe,
                "scp_mu_03km_06km_proxy",
            )?,
            "dimensionless",
        ),
        DerivedRecipe::TemperatureAdvection700mb => (
            take_values(
                &computed.temperature_advection_700mb_cph,
                recipe,
                "temperature_advection_700mb_cph",
            )?,
            "degC/hr",
        ),
        DerivedRecipe::TemperatureAdvection850mb => (
            take_values(
                &computed.temperature_advection_850mb_cph,
                recipe,
                "temperature_advection_850mb_cph",
            )?,
            "degC/hr",
        ),
        DerivedRecipe::Sbecape
        | DerivedRecipe::Mlecape
        | DerivedRecipe::Muecape
        | DerivedRecipe::SbEcapeDerivedCapeRatio
        | DerivedRecipe::MlEcapeDerivedCapeRatio
        | DerivedRecipe::MuEcapeDerivedCapeRatio
        | DerivedRecipe::SbEcapeNativeCapeRatio
        | DerivedRecipe::MlEcapeNativeCapeRatio
        | DerivedRecipe::MuEcapeNativeCapeRatio
        | DerivedRecipe::Sbncape
        | DerivedRecipe::Sbecin
        | DerivedRecipe::Mlecin
        | DerivedRecipe::EcapeScp
        | DerivedRecipe::EcapeEhi01km
        | DerivedRecipe::EcapeEhi03km
        | DerivedRecipe::EcapeStp => unreachable!("heavy recipes are blocked above"),
    };

    Ok(DerivedQueryField {
        recipe_slug: recipe.slug().to_string(),
        title: recipe.title().to_string(),
        units: units.to_string(),
        values,
        nx,
        ny,
    })
}
