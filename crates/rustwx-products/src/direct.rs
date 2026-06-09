use crate::derived::NativeContourRenderMode;
#[cfg(test)]
use rustwx_core::CanonicalField;
use rustwx_core::{FieldSelector, ModelId, SelectedField2D, SourceId};
use rustwx_models::{LatestRun, PlotRecipe, plot_recipe};
#[cfg(test)]
use rustwx_models::{PlotRecipeFetchMode, plot_recipe_fetch_plan};
use rustwx_render::{
    Color, ContourLayer, PanelGridLayout, PanelPadding, PngCompressionMode, PngWriteOptions,
    ProductVisualMode, ProjectedMap, RenderImageTiming, RenderStateTiming, WindBarbLayer,
    WindStreamlineLayer, draw_centered_text_line, render_panel_grid, save_png_profile_with_options,
    save_rgba_png_profile_with_options,
};
#[cfg(test)]
use rustwx_render::{ColorScale, ExtendMode, MapRenderRequest, RasterSampleMode};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::Instant;

use crate::publication::artifact_identity_from_path;
use crate::shared_context::{
    DomainSpec, ProjectedMapProvider, model_time_subtitle, source_subtitle,
};
use crate::source::direct_route_for_recipe_slug;

mod batch;
mod composite;
mod domain;
mod fetch;
mod planning;
mod projection;
mod query;
mod rendering;
mod titles;
mod types;
pub(crate) use batch::{
    prepare_direct_batch_from_loaded, run_direct_batch_from_loaded, run_direct_batch_from_prepared,
};
pub use batch::{run_direct_batch, run_hrrr_direct_batch};
use composite::{CompositePanelSpec, composite_panel_spec};
#[cfg(test)]
use domain::{
    DirectGridCrop, crop_for_direct_grid, crop_latlon_grid_for_direct,
    crop_selected_field_for_domain, is_global_scale_domain, longitude_bounds_span_deg,
    point_in_geographic_bounds,
};
use domain::{
    crop_bounds_for_direct_request, crop_direct_fields_for_domain, render_bounds_for_direct_field,
};
use fetch::{extract_direct_fetch_group_from_loaded, find_loaded_bytes_for_group};
pub use planning::{FetchGroup, supported_direct_recipe_slugs};
use planning::{
    PlannedDirectRecipe, canonical_fetch_product_for_selectors, group_direct_fetches,
    plan_direct_recipes,
};
#[cfg(test)]
use planning::{canonical_fetch_product, should_attach_direct_idx_patterns};
use projection::direct_map_frame_aspect_ratio;
pub(crate) use projection::inverse_raster_projection_for_grid;
#[cfg(test)]
use projection::{
    PIVOTAL_CONUS_CENTRAL_MERIDIAN_DEG, PIVOTAL_CONUS_REFERENCE_LATITUDE_DEG,
    PIVOTAL_CONUS_STANDARD_PARALLEL_1_DEG, PIVOTAL_CONUS_STANDARD_PARALLEL_2_DEG,
    ProjectionPresentationVariant, center_longitude_for_bounds,
    full_domain_projected_frame_default, inverse_raster_clip_bounds,
    presentation_frame_bounds_for_grid, presentation_projection_for_bounds,
    reference_latitude_for_projection_variant,
};
pub use projection::{
    build_projected_map, build_projected_map_with_projection,
    build_requested_projected_map_with_projection, model_data_domain_frame_for_projection,
};
pub(crate) use query::{
    build_direct_sampled_execution_plan, direct_component_slug,
    load_direct_sampled_fields_from_latest, load_direct_sampled_fields_from_loaded,
    load_single_direct_sampled_field_from_latest, required_direct_fetch_products,
};
#[cfg(test)]
use rendering::{
    StreamlineSetting, barb_target_columns_rows, convert_filled_field, render_filled_field,
    scale_for_filled_selector, scale_for_recipe, streamlines_enabled_for_grid,
};
use rendering::{
    apply_source_raster_policy, build_render_request, sanitize_output_suffix,
    should_render_overlay_only, visual_mode_for_direct_recipe,
};
#[cfg(test)]
use titles::{apply_native_stat_title_prefix, native_stat_label_for_request};
use titles::{direct_panel_title_for_request, direct_title_for_planned_product};
pub(crate) use types::PreparedDirectBatch;
pub use types::{
    DirectBatchReport, DirectBatchRequest, DirectFetchRuntimeInfo, DirectFetchTiming,
    DirectRecipeBlocker, DirectRecipeTiming, DirectRenderedRecipe, HrrrDirectBatchReport,
    HrrrDirectBatchRequest, HrrrDirectFetchRuntimeInfo, HrrrDirectFetchTiming,
    HrrrDirectRecipeBlocker, HrrrDirectRecipeTiming, HrrrDirectRenderedRecipe,
};
use types::{DirectRequestBuildTiming, OUTPUT_HEIGHT, OUTPUT_WIDTH};

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectCompositePanelLayout {
    pub(crate) rows: u32,
    pub(crate) columns: u32,
    pub(crate) panel_width: u32,
    pub(crate) panel_height: u32,
    pub(crate) top_padding: u32,
    pub(crate) component_slugs: &'static [&'static str],
}

pub(crate) fn direct_composite_component_slugs(slug: &str) -> Option<&'static [&'static str]> {
    composite_panel_spec(slug).map(|spec| spec.component_slugs)
}

pub(crate) fn direct_composite_panel_layout(
    slug: &str,
    output_width: u32,
    output_height: u32,
) -> Option<DirectCompositePanelLayout> {
    let spec = composite_panel_spec(slug)?.scaled_for_output(output_width, output_height);
    Some(DirectCompositePanelLayout {
        rows: spec.rows,
        columns: spec.columns,
        panel_width: spec.panel_width,
        panel_height: spec.panel_height,
        top_padding: spec.top_padding,
        component_slugs: spec.component_slugs,
    })
}

fn direct_data_layer_draw_ms(image_timing: &RenderImageTiming) -> u128 {
    image_timing.polygon_fill_ms
        + image_timing.projected_pixel_ms
        + image_timing.rasterize_ms
        + image_timing.raster_blit_ms
}

fn direct_overlay_draw_ms(image_timing: &RenderImageTiming) -> u128 {
    image_timing.linework_ms + image_timing.contour_ms + image_timing.barb_ms
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BarbStrideCacheKey {
    u_selector: FieldSelector,
    v_selector: FieldSelector,
    bounds_bits: [u64; 4],
}

type SharedContourLayerCache =
    Arc<Mutex<HashMap<(FieldSelector, usize, usize), Option<ContourLayer>>>>;
type SharedBarbStrideCache = Arc<Mutex<HashMap<BarbStrideCacheKey, (usize, usize)>>>;
type SharedBarbLayerCache = Arc<Mutex<HashMap<BarbStrideCacheKey, Vec<WindBarbLayer>>>>;
type SharedStreamlineLayerCache = Arc<Mutex<HashMap<BarbStrideCacheKey, Vec<WindStreamlineLayer>>>>;
type ProjectedMapCacheKey = (u32, u32, u8, usize, usize, String);
type SharedProjectedMapCache = Arc<Mutex<HashMap<ProjectedMapCacheKey, ProjectedMap>>>;
type PreparedProjectedMaps = Arc<HashMap<ProjectedMapCacheKey, ProjectedMap>>;

impl DirectBatchRequest {
    fn from_hrrr(request: &HrrrDirectBatchRequest) -> Self {
        Self {
            model: ModelId::Hrrr,
            date_yyyymmdd: request.date_yyyymmdd.clone(),
            cycle_override_utc: request.cycle_override_utc,
            forecast_hour: request.forecast_hour,
            source: request.source,
            domain: request.domain.clone(),
            out_dir: request.out_dir.clone(),
            cache_root: request.cache_root.clone(),
            use_cache: request.use_cache,
            recipe_slugs: request.recipe_slugs.clone(),
            product_overrides: HashMap::new(),
            contour_mode: request.contour_mode,
            native_fill_level_multiplier: request.native_fill_level_multiplier,
            output_width: request.output_width,
            output_height: request.output_height,
            png_compression: request.png_compression,
            place_label_overlay: request.place_label_overlay.clone(),
            output_suffix: None,
            subtitle_left_override: None,
            subtitle_right_override: None,
        }
    }

    /// Public planner-side conversion: lets the unified non-ECAPE-hour
    /// runner build a `DirectBatchRequest` from the HRRR-pinned variant
    /// so it can ask the direct lane to plan its fetch groups before
    /// loading bundles.
    pub fn from_hrrr_for_planner(request: &HrrrDirectBatchRequest) -> Self {
        Self::from_hrrr(request)
    }
}

impl DirectBatchRequest {
    fn png_write_options(&self) -> PngWriteOptions {
        PngWriteOptions {
            compression: self.png_compression,
        }
    }
}

fn sampling_direct_request(
    model: ModelId,
    source: SourceId,
    forecast_hour: u16,
    cache_root: &std::path::Path,
    use_cache: bool,
) -> DirectBatchRequest {
    DirectBatchRequest {
        model,
        date_yyyymmdd: String::new(),
        cycle_override_utc: None,
        forecast_hour,
        source,
        domain: DomainSpec::new("sampling", (-180.0, 180.0, -90.0, 90.0)),
        out_dir: PathBuf::new(),
        cache_root: cache_root.to_path_buf(),
        use_cache,
        recipe_slugs: Vec::new(),
        product_overrides: HashMap::new(),
        contour_mode: NativeContourRenderMode::Automatic,
        native_fill_level_multiplier: 1,
        output_width: OUTPUT_WIDTH,
        output_height: OUTPUT_HEIGHT,
        png_compression: PngCompressionMode::Default,
        place_label_overlay: None,
        output_suffix: None,
        subtitle_left_override: None,
        subtitle_right_override: None,
    }
}

/// Plan the direct lane's fetch groups without running the loader. The
/// unified non-ECAPE-hour runner uses this to build a single execution
/// plan that covers direct + derived (+ severe/ECAPE if requested).
pub fn plan_direct_fetch_groups(
    request: &DirectBatchRequest,
) -> Result<Vec<FetchGroup>, Box<dyn std::error::Error>> {
    let planned = plan_direct_recipes(request.model, &request.recipe_slugs)?;
    Ok(group_direct_fetches(request, &planned))
}

pub fn render_direct_recipe_from_selected_fields(
    request: &DirectBatchRequest,
    latest: &LatestRun,
    recipe_slug: &str,
    extracted: &HashMap<FieldSelector, SelectedField2D>,
    fetched_product: impl Into<String>,
    resolved_url: impl Into<String>,
    fetch_key: impl Into<String>,
) -> Result<DirectRenderedRecipe, Box<dyn std::error::Error>> {
    let mut rendered = render_direct_recipes_from_selected_fields(
        request,
        latest,
        &[recipe_slug.to_string()],
        extracted,
        fetched_product,
        resolved_url,
        fetch_key,
    )?;
    rendered
        .pop()
        .ok_or_else(|| "direct recipe rendered no outputs".into())
}

pub fn render_direct_recipes_from_selected_fields(
    request: &DirectBatchRequest,
    latest: &LatestRun,
    recipe_slugs: &[String],
    extracted: &HashMap<FieldSelector, SelectedField2D>,
    fetched_product: impl Into<String>,
    resolved_url: impl Into<String>,
    fetch_key: impl Into<String>,
) -> Result<Vec<DirectRenderedRecipe>, Box<dyn std::error::Error>> {
    fs::create_dir_all(&request.out_dir)?;
    let planned = plan_direct_recipes(request.model, recipe_slugs)?;
    let groups = group_direct_fetches(request, &planned);
    let fetched_product = fetched_product.into();
    let resolved_url = resolved_url.into();
    let fetch_key = fetch_key.into();
    let mut fetch_truth_by_actual_product = HashMap::<String, DirectFetchRuntimeInfo>::new();
    for group in &groups {
        fetch_truth_by_actual_product.insert(
            group.product.clone(),
            DirectFetchRuntimeInfo {
                fetch_key: fetch_key.clone(),
                planned_product: group.product.clone(),
                fetched_product: fetched_product.clone(),
                planned_family_aliases: group.planned_family_aliases.iter().cloned().collect(),
                requested_source: request.source,
                resolved_source: latest.source,
                resolved_url: resolved_url.clone(),
            },
        );
    }

    let missing = planned
        .iter()
        .flat_map(|item| item.plan.selectors())
        .filter(|selector| !extracted.contains_key(selector))
        .collect::<HashSet<_>>();
    if !missing.is_empty() {
        return Err(format!("missing selected fields for direct render: {:?}", missing).into());
    }

    render_direct_recipes(
        request,
        latest,
        &planned,
        extracted,
        &fetch_truth_by_actual_product,
        None,
    )
}

fn render_direct_recipes(
    request: &DirectBatchRequest,
    latest: &LatestRun,
    planned: &[PlannedDirectRecipe],
    extracted: &HashMap<FieldSelector, SelectedField2D>,
    fetch_truth_by_actual_product: &HashMap<String, DirectFetchRuntimeInfo>,
    shared_context: Option<&dyn ProjectedMapProvider>,
) -> Result<Vec<DirectRenderedRecipe>, Box<dyn std::error::Error>> {
    if planned.is_empty() {
        return Ok(Vec::new());
    }

    let crop_bounds = crop_bounds_for_direct_request(request, planned, extracted);
    let domain_extracted = crop_direct_fields_for_domain(extracted, crop_bounds)?;
    let extracted = &domain_extracted;
    let contour_layer_cache = Arc::new(Mutex::new(HashMap::new()));
    let barb_layer_cache = Arc::new(Mutex::new(HashMap::new()));
    let streamline_layer_cache = Arc::new(Mutex::new(HashMap::new()));
    let barb_stride_cache = Arc::new(Mutex::new(HashMap::new()));
    let projected_map_cache = Arc::new(Mutex::new(HashMap::new()));
    let prepared_projected_maps = build_prepared_projected_maps(request, planned, extracted)?;
    if prepared_projected_maps.is_empty() {
        return Ok(Vec::new());
    }
    let worker_count = render_worker_count(planned.len());
    if worker_count <= 1 {
        return planned
            .iter()
            .map(|item| {
                render_direct_recipe(
                    request,
                    latest,
                    item,
                    extracted,
                    fetch_truth_by_actual_product,
                    shared_context,
                    &contour_layer_cache,
                    &barb_layer_cache,
                    &streamline_layer_cache,
                    &barb_stride_cache,
                    &projected_map_cache,
                    &prepared_projected_maps,
                )
            })
            .collect();
    }

    let next_index = AtomicUsize::new(0);
    let mut rendered = vec![None; planned.len()];

    thread::scope(|scope| -> Result<(), std::io::Error> {
        let mut handles = Vec::new();
        for _ in 0..worker_count {
            let barb_stride_cache = Arc::clone(&barb_stride_cache);
            let contour_layer_cache = Arc::clone(&contour_layer_cache);
            let barb_layer_cache = Arc::clone(&barb_layer_cache);
            let streamline_layer_cache = Arc::clone(&streamline_layer_cache);
            let projected_map_cache = Arc::clone(&projected_map_cache);
            let prepared_projected_maps = Arc::clone(&prepared_projected_maps);
            let next_index = &next_index;
            handles.push(scope.spawn(
                move || -> Result<Vec<(usize, DirectRenderedRecipe)>, std::io::Error> {
                    let mut worker_rendered = Vec::new();
                    loop {
                        let index = next_index.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = planned.get(index) else {
                            break;
                        };
                        let rendered = render_direct_recipe(
                            request,
                            latest,
                            item,
                            extracted,
                            fetch_truth_by_actual_product,
                            shared_context,
                            &contour_layer_cache,
                            &barb_layer_cache,
                            &streamline_layer_cache,
                            &barb_stride_cache,
                            &projected_map_cache,
                            &prepared_projected_maps,
                        )
                        .map_err(|err| {
                            std::io::Error::other(format!(
                                "failed rendering recipe '{}': {err}",
                                item.recipe.slug
                            ))
                        })?;
                        worker_rendered.push((index, rendered));
                    }
                    Ok(worker_rendered)
                },
            ));
        }

        for handle in handles {
            let chunk_rendered = handle
                .join()
                .map_err(|_| std::io::Error::other("parallel direct render worker panicked"))??;
            for (index, recipe) in chunk_rendered {
                rendered[index] = Some(recipe);
            }
        }
        Ok(())
    })?;

    let mut completed = Vec::with_capacity(planned.len());
    for recipe in rendered {
        completed.push(recipe.ok_or_else(|| {
            std::io::Error::other("parallel direct render worker dropped a recipe result")
        })?);
    }
    Ok(completed)
}

fn render_worker_count(recipe_count: usize) -> usize {
    if recipe_count <= 1 {
        return 1;
    }

    let override_threads = std::env::var("RUSTWX_RENDER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0);

    thread::available_parallelism()
        .map(|count| override_threads.unwrap_or((count.get() / 2).max(1)))
        .unwrap_or(1)
        .min(recipe_count)
}

fn visual_mode_cache_key(mode: ProductVisualMode) -> u8 {
    match mode {
        ProductVisualMode::FilledMeteorology => 0,
        ProductVisualMode::UpperAirAnalysis => 1,
        ProductVisualMode::OverlayAnalysis => 2,
        ProductVisualMode::SevereDiagnostic => 3,
        ProductVisualMode::PanelMember => 4,
        ProductVisualMode::ComparisonPanel => 5,
    }
}

fn standard_projected_key(
    request: &DirectBatchRequest,
    recipe: &PlotRecipe,
) -> Option<(u32, u32, u8)> {
    let filled_selector = recipe.filled.selector?;
    let overlay_only = should_render_overlay_only(filled_selector, recipe.contours.is_some());
    let visual_mode = visual_mode_for_direct_recipe(recipe, filled_selector, overlay_only);
    Some((
        request.output_width,
        request.output_height,
        visual_mode_cache_key(visual_mode),
    ))
}

fn projected_map_cache_key(
    width: u32,
    height: u32,
    mode_key: u8,
    field: &SelectedField2D,
) -> ProjectedMapCacheKey {
    (
        width,
        height,
        mode_key,
        field.grid.shape.nx,
        field.grid.shape.ny,
        format!("{:?}", field.projection),
    )
}

fn visual_mode_for_key(mode_key: u8) -> ProductVisualMode {
    match mode_key {
        0 => ProductVisualMode::FilledMeteorology,
        1 => ProductVisualMode::UpperAirAnalysis,
        2 => ProductVisualMode::OverlayAnalysis,
        3 => ProductVisualMode::SevereDiagnostic,
        4 => ProductVisualMode::PanelMember,
        5 => ProductVisualMode::ComparisonPanel,
        _ => ProductVisualMode::FilledMeteorology,
    }
}

fn projected_sample_selector(item: &PlannedDirectRecipe) -> Option<FieldSelector> {
    if let Some(selector) = item.recipe.filled.selector {
        return Some(selector);
    }
    composite_panel_spec(item.recipe.slug).and_then(|spec| {
        spec.component_slugs.iter().find_map(|component_slug| {
            plot_recipe(component_slug).and_then(|component| component.filled.selector)
        })
    })
}

fn build_prepared_projected_maps(
    request: &DirectBatchRequest,
    planned: &[PlannedDirectRecipe],
    extracted: &HashMap<FieldSelector, SelectedField2D>,
) -> Result<PreparedProjectedMaps, Box<dyn std::error::Error>> {
    let mut requested = Vec::<(ProjectedMapCacheKey, ProductVisualMode, &SelectedField2D)>::new();
    for item in planned {
        if let Some(spec) = composite_panel_spec(item.recipe.slug) {
            let spec = spec.scaled_for_request(request);
            let Some(first_field) =
                projected_sample_selector(item).and_then(|selector| extracted.get(&selector))
            else {
                continue;
            };
            requested.push((
                projected_map_cache_key(
                    spec.panel_width,
                    spec.panel_height,
                    visual_mode_cache_key(ProductVisualMode::PanelMember),
                    first_field,
                ),
                ProductVisualMode::PanelMember,
                first_field,
            ));
        } else if let Some((width, height, mode_key)) = standard_projected_key(request, item.recipe)
        {
            let Some(filled) = item
                .recipe
                .filled
                .selector
                .and_then(|selector| extracted.get(&selector))
            else {
                continue;
            };
            requested.push((
                projected_map_cache_key(width, height, mode_key, filled),
                visual_mode_for_key(mode_key),
                filled,
            ));
        }
    }
    if requested.is_empty() {
        return Ok(Arc::new(HashMap::new()));
    }

    let mut prepared = HashMap::new();
    for (cache_key, visual_mode, sample_field) in requested {
        if prepared.contains_key(&cache_key) {
            continue;
        }
        let (width, height, _, _, _, _) = cache_key.clone();
        let target_ratio = direct_map_frame_aspect_ratio(
            visual_mode,
            width,
            height,
            sample_field.projection.as_ref(),
        );
        let projected = build_projected_map_with_projection(
            &sample_field.grid.lat_deg,
            &sample_field.grid.lon_deg,
            sample_field.projection.as_ref(),
            request.domain.bounds,
            target_ratio,
        )?;
        prepared.insert(cache_key, projected);
    }
    Ok(Arc::new(prepared))
}

fn render_direct_recipe(
    request: &DirectBatchRequest,
    latest: &LatestRun,
    item: &PlannedDirectRecipe,
    extracted: &HashMap<FieldSelector, SelectedField2D>,
    fetch_truth_by_actual_product: &HashMap<String, DirectFetchRuntimeInfo>,
    shared_context: Option<&dyn ProjectedMapProvider>,
    contour_layer_cache: &SharedContourLayerCache,
    barb_layer_cache: &SharedBarbLayerCache,
    streamline_layer_cache: &SharedStreamlineLayerCache,
    barb_stride_cache: &SharedBarbStrideCache,
    projected_map_cache: &SharedProjectedMapCache,
    prepared_projected_maps: &PreparedProjectedMaps,
) -> Result<DirectRenderedRecipe, Box<dyn std::error::Error>> {
    let render_start = Instant::now();
    let suffix = request
        .output_suffix
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("_{}", sanitize_output_suffix(value)))
        .unwrap_or_default();
    let output_path = request.out_dir.join(format!(
        "rustwx_{}_{}_{}z_f{:03}_{}_{}{}.png",
        request.model.as_str().replace('-', "_"),
        request.date_yyyymmdd,
        latest.cycle.hour_utc,
        request.forecast_hour,
        request.domain.slug,
        item.recipe.slug,
        suffix
    ));
    let canonical_product = canonical_fetch_product_for_selectors(
        request,
        item.plan.product.as_ref(),
        &item.plan.selectors(),
    );
    let runtime_fetch = fetch_truth_by_actual_product
        .get::<str>(canonical_product.as_str())
        .ok_or_else(|| {
            format!(
                "missing direct fetch runtime truth for canonical family '{}'",
                canonical_product
            )
        })?;
    let (
        project_ms,
        field_prepare_ms,
        contour_prepare_ms,
        barb_prepare_ms,
        request_build_ms,
        render_state_prep_ms,
        png_encode_ms,
        file_write_ms,
        state_timing,
        image_timing,
    ) = if let Some(spec) = composite_panel_spec(item.recipe.slug) {
        render_direct_composite_panel(
            item.recipe,
            spec.scaled_for_request(request),
            request,
            latest,
            extracted,
            &output_path,
            shared_context,
            contour_layer_cache,
            barb_layer_cache,
            streamline_layer_cache,
            barb_stride_cache,
            projected_map_cache,
            prepared_projected_maps,
        )?
    } else {
        let filled_selector = item
            .recipe
            .filled
            .selector
            .ok_or("recipe filled field missing selector binding")?;
        let filled = extracted
            .get(&filled_selector)
            .ok_or_else(|| format!("missing filled selector {:?}", filled_selector))?;

        let project_start = Instant::now();
        let overlay_only =
            should_render_overlay_only(filled_selector, item.recipe.contours.is_some());
        let visual_mode = visual_mode_for_direct_recipe(item.recipe, filled_selector, overlay_only);
        let target_ratio = direct_map_frame_aspect_ratio(
            visual_mode,
            request.output_width,
            request.output_height,
            filled.projection.as_ref(),
        );
        let render_bounds = render_bounds_for_direct_field(
            request.domain.bounds,
            filled,
            visual_mode,
            request.output_width,
            request.output_height,
        );
        let cache_key = projected_map_cache_key(
            request.output_width,
            request.output_height,
            visual_mode_cache_key(visual_mode),
            filled,
        );
        let projected = if let Some(projected) = shared_context.and_then(|ctx| {
            ctx.projected_map(request.output_width, request.output_height)
                .cloned()
        }) {
            projected
        } else if let Some(projected) = prepared_projected_maps.get(&cache_key).cloned() {
            projected
        } else if let Some(projected) = projected_map_cache
            .lock()
            .expect("projected map cache poisoned")
            .get(&cache_key)
            .cloned()
        {
            projected
        } else {
            let projected = build_projected_map_with_projection(
                &filled.grid.lat_deg,
                &filled.grid.lon_deg,
                filled.projection.as_ref(),
                request.domain.bounds,
                target_ratio,
            )?;
            projected_map_cache
                .lock()
                .expect("projected map cache poisoned")
                .insert(cache_key, projected.clone());
            projected
        };
        let project_ms = project_start.elapsed().as_millis();

        let request_build_start = Instant::now();
        let (mut render_request, build_timing) = build_render_request(
            item.recipe,
            filled,
            extracted,
            projected,
            render_bounds,
            request.output_width,
            request.output_height,
            contour_layer_cache,
            barb_layer_cache,
            streamline_layer_cache,
            barb_stride_cache,
            request.contour_mode,
            request.native_fill_level_multiplier,
        )?;
        let request_build_ms = request_build_start.elapsed().as_millis();
        apply_source_raster_policy(latest.source, &mut render_request);
        render_request.title = Some(direct_title_for_planned_product(
            request,
            item.plan.product.as_ref(),
            item.recipe.title,
        ));
        render_request.subtitle_left =
            Some(request.subtitle_left_override.clone().unwrap_or_else(|| {
                model_time_subtitle(
                    request.model,
                    &request.date_yyyymmdd,
                    latest.cycle.hour_utc,
                    request.forecast_hour,
                )
            }));
        render_request.subtitle_right = Some(
            request
                .subtitle_right_override
                .clone()
                .unwrap_or_else(|| source_subtitle(latest.source)),
        );
        if let Some(overlay) = request.place_label_overlay.as_ref() {
            crate::apply_place_label_overlay_with_density_styling(
                &mut render_request,
                overlay,
                &request.domain,
                &filled.grid.lat_deg,
                &filled.grid.lon_deg,
                filled.projection.as_ref(),
            )?;
        }
        let save_timing = save_png_profile_with_options(
            &render_request,
            &output_path,
            &request.png_write_options(),
        )?;
        (
            project_ms,
            build_timing.field_prepare_ms,
            build_timing.contour_prepare_ms,
            build_timing.barb_prepare_ms,
            request_build_ms,
            save_timing.state_timing.state_prep_ms,
            save_timing.png_timing.png_encode_ms,
            save_timing.file_write_ms,
            save_timing.state_timing,
            save_timing.png_timing.image_timing,
        )
    };
    let content_identity = artifact_identity_from_path(&output_path)?;
    let total_ms = render_start.elapsed().as_millis();

    let panel_compose_ms = if composite_panel_spec(item.recipe.slug).is_some() {
        image_timing.total_ms
    } else {
        0
    };

    Ok(DirectRenderedRecipe {
        recipe_slug: item.recipe.slug.to_string(),
        title: direct_title_for_planned_product(
            request,
            item.plan.product.as_ref(),
            item.recipe.title,
        ),
        source_route: direct_route_for_recipe_slug(item.recipe.slug),
        grib_product: item.plan.product.to_string(),
        fetched_grib_product: runtime_fetch.fetched_product.clone(),
        resolved_source: runtime_fetch.resolved_source,
        resolved_url: runtime_fetch.resolved_url.clone(),
        output_path,
        content_identity,
        input_fetch_keys: vec![runtime_fetch.fetch_key.clone()],
        timing: DirectRecipeTiming {
            render_to_image_ms: image_timing.total_ms,
            data_layer_draw_ms: direct_data_layer_draw_ms(&image_timing),
            overlay_draw_ms: direct_overlay_draw_ms(&image_timing),
            panel_compose_ms,
            project_ms,
            field_prepare_ms,
            contour_prepare_ms,
            barb_prepare_ms,
            request_build_ms,
            render_state_prep_ms,
            png_encode_ms,
            file_write_ms,
            render_ms: total_ms.saturating_sub(project_ms),
            total_ms,
            state_timing,
            image_timing,
        },
    })
}

fn render_direct_composite_panel(
    recipe: &PlotRecipe,
    spec: CompositePanelSpec,
    request: &DirectBatchRequest,
    latest: &LatestRun,
    extracted: &HashMap<FieldSelector, SelectedField2D>,
    output_path: &std::path::Path,
    shared_context: Option<&dyn ProjectedMapProvider>,
    contour_layer_cache: &SharedContourLayerCache,
    barb_layer_cache: &SharedBarbLayerCache,
    streamline_layer_cache: &SharedStreamlineLayerCache,
    barb_stride_cache: &SharedBarbStrideCache,
    projected_map_cache: &SharedProjectedMapCache,
    prepared_projected_maps: &PreparedProjectedMaps,
) -> Result<
    (
        u128,
        u128,
        u128,
        u128,
        u128,
        u128,
        u128,
        u128,
        RenderStateTiming,
        RenderImageTiming,
    ),
    Box<dyn std::error::Error>,
> {
    let first_component = plot_recipe(spec.component_slugs[0])
        .ok_or_else(|| format!("missing component recipe '{}'", spec.component_slugs[0]))?;
    let first_selector = first_component
        .filled
        .selector
        .ok_or("component recipe filled field missing selector binding")?;
    let first_field = extracted
        .get(&first_selector)
        .ok_or_else(|| format!("missing component selector {:?}", first_selector))?;

    let project_start = Instant::now();
    let cache_key = projected_map_cache_key(
        spec.panel_width,
        spec.panel_height,
        visual_mode_cache_key(ProductVisualMode::PanelMember),
        first_field,
    );
    let panel_target_ratio = direct_map_frame_aspect_ratio(
        ProductVisualMode::PanelMember,
        spec.panel_width,
        spec.panel_height,
        first_field.projection.as_ref(),
    );
    let projected = if let Some(projected) = shared_context.and_then(|ctx| {
        ctx.projected_map(spec.panel_width, spec.panel_height)
            .cloned()
    }) {
        projected
    } else if let Some(projected) = prepared_projected_maps.get(&cache_key).cloned() {
        projected
    } else if let Some(projected) = projected_map_cache
        .lock()
        .expect("projected map cache poisoned")
        .get(&cache_key)
        .cloned()
    {
        projected
    } else {
        let projected = build_projected_map_with_projection(
            &first_field.grid.lat_deg,
            &first_field.grid.lon_deg,
            first_field.projection.as_ref(),
            request.domain.bounds,
            panel_target_ratio,
        )?;
        projected_map_cache
            .lock()
            .expect("projected map cache poisoned")
            .insert(cache_key, projected.clone());
        projected
    };
    let project_ms = project_start.elapsed().as_millis();

    let request_build_start = Instant::now();
    let mut build_timing = DirectRequestBuildTiming::default();
    let mut panel_requests = Vec::with_capacity(spec.component_slugs.len());
    for component_slug in spec.component_slugs {
        let component_recipe = plot_recipe(component_slug)
            .ok_or_else(|| format!("missing component recipe '{component_slug}'"))?;
        let selector = component_recipe
            .filled
            .selector
            .ok_or("component recipe filled field missing selector binding")?;
        let filled = extracted
            .get(&selector)
            .ok_or_else(|| format!("missing component selector {:?}", selector))?;
        let panel_render_bounds = render_bounds_for_direct_field(
            request.domain.bounds,
            filled,
            ProductVisualMode::PanelMember,
            spec.panel_width,
            spec.panel_height,
        );
        let (mut panel_request, panel_timing) = build_render_request(
            component_recipe,
            filled,
            extracted,
            projected.clone(),
            panel_render_bounds,
            spec.panel_width,
            spec.panel_height,
            contour_layer_cache,
            barb_layer_cache,
            streamline_layer_cache,
            barb_stride_cache,
            request.contour_mode,
            request.native_fill_level_multiplier,
        )?;
        build_timing.field_prepare_ms += panel_timing.field_prepare_ms;
        build_timing.contour_prepare_ms += panel_timing.contour_prepare_ms;
        build_timing.barb_prepare_ms += panel_timing.barb_prepare_ms;
        apply_source_raster_policy(latest.source, &mut panel_request);
        panel_request.width = spec.panel_width;
        panel_request.height = spec.panel_height;
        panel_request.visual_mode = ProductVisualMode::PanelMember;
        panel_request.subtitle_left = None;
        panel_request.subtitle_right = None;
        if let Some(overlay) = request.place_label_overlay.as_ref() {
            crate::apply_place_label_overlay_with_density_styling(
                &mut panel_request,
                overlay,
                &request.domain,
                &filled.grid.lat_deg,
                &filled.grid.lon_deg,
                filled.projection.as_ref(),
            )?;
        }
        panel_requests.push(panel_request);
    }
    let request_build_ms = request_build_start.elapsed().as_millis();

    let layout =
        PanelGridLayout::new(spec.rows, spec.columns, spec.panel_width, spec.panel_height)?
            .with_padding(PanelPadding {
                top: spec.top_padding,
                ..Default::default()
            });
    let render_start = Instant::now();
    let mut canvas = render_panel_grid(&layout, &panel_requests)?;
    let render_ms = render_start.elapsed().as_millis();
    let title = direct_panel_title_for_request(request, recipe.title);
    draw_centered_text_line(&mut canvas, &title, 10, Color::BLACK, 2);
    draw_centered_text_line(
        &mut canvas,
        &format!(
            "{} | {}",
            request.subtitle_left_override.clone().unwrap_or_else(|| {
                model_time_subtitle(
                    request.model,
                    &request.date_yyyymmdd,
                    latest.cycle.hour_utc,
                    request.forecast_hour,
                )
            }),
            request
                .subtitle_right_override
                .clone()
                .unwrap_or_else(|| source_subtitle(latest.source))
        ),
        35,
        Color::BLACK,
        1,
    );
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let save_timing =
        save_rgba_png_profile_with_options(&canvas, output_path, &request.png_write_options())?;
    Ok((
        project_ms,
        build_timing.field_prepare_ms,
        build_timing.contour_prepare_ms,
        build_timing.barb_prepare_ms,
        request_build_ms,
        save_timing.state_timing.state_prep_ms,
        save_timing.png_timing.png_encode_ms,
        save_timing.file_write_ms,
        save_timing.state_timing,
        RenderImageTiming {
            total_ms: render_ms,
            ..RenderImageTiming::default()
        },
    ))
}

#[cfg(test)]
mod tests;
