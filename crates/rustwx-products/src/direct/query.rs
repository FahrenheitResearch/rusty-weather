use std::collections::{HashMap, HashSet};

use rustwx_core::{FieldSelector, ModelId, SelectedField2D, SourceId};
use rustwx_models::{LatestRun, PlotRecipe};

use crate::planner::ExecutionPlan;
use crate::publication::PublishedFetchIdentity;
use crate::runtime::{BundleLoaderConfig, LoadedBundleSet, load_execution_plan};
use crate::source::direct_route_for_recipe_slug;

use super::composite::composite_panel_spec;
use super::planning::{
    build_direct_execution_plan, canonical_fetch_product_for_selectors, group_direct_fetches,
    partition_recipes_by_selector_availability, plan_direct_recipes, recipe_block_reason,
    recipe_slugs_depending_on_group,
};
use super::rendering::render_filled_field;
use super::types::{
    DirectBatchRequest, DirectRecipeBlocker, DirectSampledComponentField,
    DirectSampledCompositeProduct, DirectSampledProductField, DirectSampledProductSet,
};
use super::{
    extract_direct_fetch_group_from_loaded, find_loaded_bytes_for_group, sampling_direct_request,
};

pub(crate) fn required_direct_fetch_products(
    model: ModelId,
    recipe_slugs: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let planned = plan_direct_recipes(model, recipe_slugs)?;
    let request =
        sampling_direct_request(model, SourceId::Aws, 0, std::path::Path::new("."), false);
    Ok(group_direct_fetches(&request, &planned)
        .into_iter()
        .map(|group| group.product)
        .collect())
}

pub(crate) fn load_direct_sampled_fields_from_latest(
    latest: &LatestRun,
    forecast_hour: u16,
    cache_root: &std::path::Path,
    use_cache: bool,
    recipe_slugs: &[String],
) -> Result<DirectSampledProductSet, Box<dyn std::error::Error>> {
    let request = sampling_direct_request(
        latest.model,
        latest.source,
        forecast_hour,
        cache_root,
        use_cache,
    );
    let planned = plan_direct_recipes(latest.model, recipe_slugs)?;
    if planned.is_empty() {
        return Ok(DirectSampledProductSet {
            latest: latest.clone(),
            fields: Vec::new(),
            composites: Vec::new(),
            blockers: Vec::new(),
        });
    }

    let groups = group_direct_fetches(&request, &planned);
    let plan = build_direct_execution_plan(latest, forecast_hour, &groups);
    let loaded = load_execution_plan(
        plan,
        &BundleLoaderConfig::new(cache_root.to_path_buf(), use_cache),
    )?;
    load_direct_sampled_fields_from_loaded_request(&request, &loaded, recipe_slugs)
}

pub(crate) fn build_direct_sampled_execution_plan(
    latest: &LatestRun,
    forecast_hour: u16,
    cache_root: &std::path::Path,
    use_cache: bool,
    recipe_slugs: &[String],
) -> Result<ExecutionPlan, Box<dyn std::error::Error>> {
    let request = sampling_direct_request(
        latest.model,
        latest.source,
        forecast_hour,
        cache_root,
        use_cache,
    );
    let planned = plan_direct_recipes(latest.model, recipe_slugs)?;
    let groups = group_direct_fetches(&request, &planned);
    Ok(build_direct_execution_plan(latest, forecast_hour, &groups))
}

pub(crate) fn load_direct_sampled_fields_from_loaded(
    latest: &LatestRun,
    forecast_hour: u16,
    cache_root: &std::path::Path,
    use_cache: bool,
    recipe_slugs: &[String],
    loaded: &LoadedBundleSet,
) -> Result<DirectSampledProductSet, Box<dyn std::error::Error>> {
    let request = sampling_direct_request(
        latest.model,
        latest.source,
        forecast_hour,
        cache_root,
        use_cache,
    );
    load_direct_sampled_fields_from_loaded_request(&request, loaded, recipe_slugs)
}

fn load_direct_sampled_fields_from_loaded_request(
    request: &DirectBatchRequest,
    loaded: &LoadedBundleSet,
    recipe_slugs: &[String],
) -> Result<DirectSampledProductSet, Box<dyn std::error::Error>> {
    let planned = plan_direct_recipes(request.model, recipe_slugs)?;
    if planned.is_empty() {
        return Ok(DirectSampledProductSet {
            latest: loaded.latest.clone(),
            fields: Vec::new(),
            composites: Vec::new(),
            blockers: Vec::new(),
        });
    }

    let groups = group_direct_fetches(request, &planned);

    let mut extracted = HashMap::<FieldSelector, SelectedField2D>::new();
    let mut missing_selectors = HashSet::<FieldSelector>::new();
    let mut blockers = Vec::<DirectRecipeBlocker>::new();
    let mut fetches_by_product = HashMap::<String, PublishedFetchIdentity>::new();

    for group in &groups {
        let fetched = match find_loaded_bytes_for_group(&loaded, group) {
            Ok(bytes) => bytes,
            Err(err) => {
                let reason = err.to_string();
                for selector in &group.selectors {
                    missing_selectors.insert(*selector);
                }
                for recipe_slug in recipe_slugs_depending_on_group(&planned, group) {
                    blockers.push(DirectRecipeBlocker {
                        recipe_slug,
                        reason: reason.clone(),
                    });
                }
                continue;
            }
        };
        let (fields, unmatched, timing) =
            extract_direct_fetch_group_from_loaded(request, group, fetched, request.use_cache)?;
        extracted.extend(fields.into_iter().map(|field| (field.selector, field)));
        for selector in unmatched {
            missing_selectors.insert(selector);
        }
        fetches_by_product.insert(group.product.clone(), timing.input_fetch.clone());
    }

    let (renderable, selector_blockers) =
        partition_recipes_by_selector_availability(&planned, &missing_selectors);
    blockers.extend(selector_blockers);

    let mut fields = Vec::new();
    let mut composites = Vec::new();
    for item in renderable {
        let canonical_product = canonical_fetch_product_for_selectors(
            request,
            item.plan.product.as_ref(),
            &item.plan.selectors(),
        );
        let input_fetches: Vec<PublishedFetchIdentity> = fetches_by_product
            .get(&canonical_product)
            .cloned()
            .into_iter()
            .collect();
        if let Some(spec) = composite_panel_spec(item.recipe.slug) {
            composites.push(direct_sampled_composite(
                item.recipe,
                spec.rows,
                spec.columns,
                spec.component_slugs,
                &extracted,
                &input_fetches,
            )?);
            continue;
        }
        let Some(filled_selector) = item.recipe.filled.selector else {
            blockers.push(DirectRecipeBlocker {
                recipe_slug: item.recipe.slug.to_string(),
                reason: "direct recipe is missing a filled selector binding".to_string(),
            });
            continue;
        };
        let Some(filled) = extracted.get(&filled_selector) else {
            blockers.push(DirectRecipeBlocker {
                recipe_slug: item.recipe.slug.to_string(),
                reason: format!(
                    "direct recipe '{}' was renderable but missing selector {}",
                    item.recipe.slug,
                    filled_selector.key()
                ),
            });
            continue;
        };
        let field = render_filled_field(item.recipe, filled, &extracted)?;
        let components = direct_sampled_components(item.recipe, &extracted, &input_fetches)?;
        fields.push(DirectSampledProductField {
            recipe_slug: item.recipe.slug.to_string(),
            title: item.recipe.title.to_string(),
            source_route: direct_route_for_recipe_slug(item.recipe.slug),
            field_selector: Some(filled_selector),
            field,
            input_fetches,
            components,
        });
    }

    Ok(DirectSampledProductSet {
        latest: loaded.latest.clone(),
        fields,
        composites,
        blockers,
    })
}

pub(crate) fn load_single_direct_sampled_field_from_latest(
    latest: &LatestRun,
    forecast_hour: u16,
    cache_root: &std::path::Path,
    use_cache: bool,
    recipe_slug: &str,
    allow_composite_filled_field: bool,
) -> Result<DirectSampledProductField, Box<dyn std::error::Error>> {
    let request = sampling_direct_request(
        latest.model,
        latest.source,
        forecast_hour,
        cache_root,
        use_cache,
    );
    let planned = plan_direct_recipes(latest.model, &[recipe_slug.to_string()])?;
    let planned_item = planned
        .first()
        .ok_or_else(|| format!("direct recipe '{recipe_slug}' did not plan"))?;

    let groups = group_direct_fetches(&request, &planned);
    let plan = build_direct_execution_plan(latest, forecast_hour, &groups);
    let loaded = load_execution_plan(
        plan,
        &BundleLoaderConfig::new(cache_root.to_path_buf(), use_cache),
    )?;

    let mut extracted = HashMap::<FieldSelector, SelectedField2D>::new();
    let mut missing_selectors = HashSet::<FieldSelector>::new();
    let mut fetches_by_product = HashMap::<String, PublishedFetchIdentity>::new();

    for group in &groups {
        let fetched = match find_loaded_bytes_for_group(&loaded, group) {
            Ok(bytes) => bytes,
            Err(err) => {
                for selector in &group.selectors {
                    missing_selectors.insert(*selector);
                }
                return Err(format!(
                    "direct recipe '{}' fetch group '{}' failed: {}",
                    recipe_slug, group.product, err
                )
                .into());
            }
        };
        let (fields, unmatched, timing) =
            extract_direct_fetch_group_from_loaded(&request, group, fetched, use_cache)?;
        extracted.extend(fields.into_iter().map(|field| (field.selector, field)));
        for selector in unmatched {
            missing_selectors.insert(selector);
        }
        fetches_by_product.insert(group.product.clone(), timing.input_fetch.clone());
    }

    if let Some(reason) = recipe_block_reason(planned_item.recipe, &missing_selectors) {
        return Err(format!(
            "direct sampled field '{}' is blocked: {}",
            recipe_slug, reason
        )
        .into());
    }

    if composite_panel_spec(planned_item.recipe.slug).is_some() && !allow_composite_filled_field {
        return Err(format!(
            "direct recipe '{}' is composite and does not expose a single sampled filled field by default",
            recipe_slug
        )
        .into());
    }

    let Some(filled_selector) = planned_item.recipe.filled.selector else {
        return Err(format!(
            "direct recipe '{}' is missing a filled selector binding",
            recipe_slug
        )
        .into());
    };
    let Some(filled) = extracted.get(&filled_selector) else {
        return Err(format!(
            "direct recipe '{}' did not resolve filled selector {}",
            recipe_slug,
            filled_selector.key()
        )
        .into());
    };
    let field = render_filled_field(planned_item.recipe, filled, &extracted)?;
    let canonical_product = canonical_fetch_product_for_selectors(
        &request,
        planned_item.plan.product.as_ref(),
        &planned_item.plan.selectors(),
    );
    let input_fetches: Vec<PublishedFetchIdentity> = fetches_by_product
        .get(&canonical_product)
        .cloned()
        .into_iter()
        .collect();
    let components = direct_sampled_components(planned_item.recipe, &extracted, &input_fetches)?;
    Ok(DirectSampledProductField {
        recipe_slug: planned_item.recipe.slug.to_string(),
        title: planned_item.recipe.title.to_string(),
        source_route: direct_route_for_recipe_slug(planned_item.recipe.slug),
        field_selector: Some(filled_selector),
        field,
        input_fetches,
        components,
    })
}

fn direct_sampled_components(
    recipe: &PlotRecipe,
    extracted: &HashMap<FieldSelector, SelectedField2D>,
    input_fetches: &[PublishedFetchIdentity],
) -> Result<Vec<DirectSampledComponentField>, Box<dyn std::error::Error>> {
    let mut components = Vec::new();

    if let Some(spec) = &recipe.contours {
        if let Some(selector) = spec.selector {
            if let Some(field) = extracted.get(&selector) {
                components.push(DirectSampledComponentField {
                    product_slug: direct_component_slug(recipe.slug, "contour"),
                    title: format!("{} Contour", recipe.title),
                    field: field.clone().into_field2d(),
                    input_fetches: input_fetches.to_vec(),
                });
            }
        }
    }

    if let (Some(u_spec), Some(v_spec)) = (&recipe.barbs_u, &recipe.barbs_v) {
        if let (Some(u_selector), Some(v_selector)) = (u_spec.selector, v_spec.selector) {
            if let Some(u) = extracted.get(&u_selector) {
                components.push(DirectSampledComponentField {
                    product_slug: direct_component_slug(recipe.slug, "wind_u"),
                    title: format!("{} Wind U", recipe.title),
                    field: u.clone().into_field2d(),
                    input_fetches: input_fetches.to_vec(),
                });
            }
            if let Some(v) = extracted.get(&v_selector) {
                components.push(DirectSampledComponentField {
                    product_slug: direct_component_slug(recipe.slug, "wind_v"),
                    title: format!("{} Wind V", recipe.title),
                    field: v.clone().into_field2d(),
                    input_fetches: input_fetches.to_vec(),
                });
            }
        }
    }

    Ok(components)
}

fn direct_sampled_composite(
    recipe: &PlotRecipe,
    rows: u32,
    columns: u32,
    component_slugs: &[&str],
    extracted: &HashMap<FieldSelector, SelectedField2D>,
    input_fetches: &[PublishedFetchIdentity],
) -> Result<DirectSampledCompositeProduct, Box<dyn std::error::Error>> {
    let mut components = Vec::with_capacity(component_slugs.len());
    for component_slug in component_slugs {
        let component = rustwx_models::plot_recipe(component_slug)
            .ok_or_else(|| format!("missing composite component recipe '{component_slug}'"))?;
        let selector = component.filled.selector.ok_or_else(|| {
            format!("composite component recipe '{component_slug}' is missing a filled selector")
        })?;
        let field = extracted
            .get(&selector)
            .ok_or_else(|| format!("missing composite component selector {}", selector.key()))?;
        components.push(DirectSampledComponentField {
            product_slug: direct_component_slug(recipe.slug, component_slug),
            title: component.title.to_string(),
            field: field.clone().into_field2d(),
            input_fetches: input_fetches.to_vec(),
        });
    }

    Ok(DirectSampledCompositeProduct {
        recipe_slug: recipe.slug.to_string(),
        title: recipe.title.to_string(),
        rows,
        columns,
        components,
    })
}

pub(crate) fn direct_component_slug(recipe_slug: &str, role: &str) -> String {
    format!("{recipe_slug}__{role}")
}
