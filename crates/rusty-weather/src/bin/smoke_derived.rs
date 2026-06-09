use std::fs;
use std::path::PathBuf;

#[path = "../contour_mode.rs"]
mod contour_mode;
#[path = "../domain.rs"]
mod domain;
#[path = "../region.rs"]
mod region;

use clap::{Parser, ValueEnum};
use contour_mode::ContourModeArg;
use domain::{domain_from_region_or_country, requested_domain_slug};
use region::RegionPreset;
use rustwx_core::{ModelId, SourceId};
use rustwx_models::model_summary;
use rustwx_products::cache::{default_proof_cache_dir, ensure_dir};
use rustwx_products::derived::{
    DerivedBatchRequest, run_derived_batch, supported_derived_recipe_slugs,
};
use rustwx_products::places::{PlaceLabelDensityTier, default_place_label_overlay_for_domain};
use rustwx_products::publication::{
    ArtifactPublicationState, PublishedArtifactRecord, RunPublicationManifest, atomic_write_json,
    canonical_run_slug, finalize_and_publish_run_manifest, publish_failure_manifest,
};
use rustwx_products::source::ProductSourceMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum PngCompressionArg {
    Default,
    Fast,
    Fastest,
}

impl From<PngCompressionArg> for rustwx_render::PngCompressionMode {
    fn from(value: PngCompressionArg) -> Self {
        match value {
            PngCompressionArg::Default => Self::Default,
            PngCompressionArg::Fast => Self::Fast,
            PngCompressionArg::Fastest => Self::Fastest,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "derived-batch",
    about = "Generate multiple derived RustWX plots from one shared full-file thermodynamic load"
)]
struct Args {
    #[arg(long, default_value = "hrrr")]
    model: ModelId,
    #[arg(long, default_value = "20260414")]
    date: String,
    #[arg(long)]
    cycle: Option<u8>,
    #[arg(long, default_value_t = 0)]
    forecast_hour: u16,
    #[arg(long)]
    source: Option<SourceId>,
    #[arg(long, value_enum, default_value_t = RegionPreset::Midwest)]
    region: RegionPreset,
    #[arg(
        long,
        help = "Country crop by ISO alpha-2/alpha-3 code or normalized country name, e.g. usa, us, japan"
    )]
    country: Option<String>,
    #[arg(long = "recipe", value_delimiter = ',', num_args = 1..)]
    recipes: Vec<String>,
    #[arg(long, default_value_t = false)]
    all_supported: bool,
    #[arg(long)]
    surface_product: Option<String>,
    #[arg(long)]
    pressure_product: Option<String>,
    #[arg(long, default_value = "out")]
    out_dir: PathBuf,
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    no_cache: bool,
    #[arg(long = "source-mode", alias = "thermo-path", value_enum, default_value_t = SourceModeArg::Canonical)]
    source_mode: SourceModeArg,
    #[arg(
        long,
        default_value_t = false,
        help = "Allow very large heavy ECAPE domains instead of refusing the run"
    )]
    allow_large_heavy_domain: bool,
    #[arg(long, value_enum, default_value_t = ContourModeArg::Automatic)]
    contour_mode: ContourModeArg,
    #[arg(long, default_value_t = 1)]
    native_fill_level_multiplier: usize,
    #[arg(long = "png-compression", value_enum, default_value_t = PngCompressionArg::Fast)]
    png_compression: PngCompressionArg,
    #[arg(long = "place-label-density", default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=3))]
    place_label_density: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SourceModeArg {
    Canonical,
    Fastest,
}

impl From<SourceModeArg> for ProductSourceMode {
    fn from(value: SourceModeArg) -> Self {
        match value {
            SourceModeArg::Canonical => Self::Canonical,
            SourceModeArg::Fastest => Self::Fastest,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let failure_slug = canonical_run_slug(
        &args.model.as_str().replace('-', "_"),
        &args.date,
        args.cycle,
        args.forecast_hour,
        &requested_domain_slug(args.region, args.country.as_deref()),
        "derived",
    );
    let failure_out_dir = args.out_dir.clone();
    if let Err(err) = run(&args) {
        let _ = publish_failure_manifest(
            "derived_batch",
            &failure_slug,
            &failure_out_dir,
            &failure_slug,
            err.to_string(),
        );
        return Err(err);
    }
    Ok(())
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(&args.out_dir)?;
    let cache_root = args
        .cache_dir
        .clone()
        .unwrap_or_else(|| default_proof_cache_dir(&args.out_dir));
    if !args.no_cache {
        ensure_dir(&cache_root)?;
    }

    let source = args
        .source
        .unwrap_or(model_summary(args.model).sources[0].id);
    let recipes = if args.all_supported {
        let supported = supported_derived_recipe_slugs(args.model);
        if supported.is_empty() {
            return Err(format!(
                "no derived products are currently supported for {}",
                args.model
            )
            .into());
        }
        supported
    } else if args.recipes.is_empty() {
        return Err("pass at least one --recipe or use --all-supported".into());
    } else {
        args.recipes.clone()
    };

    let domain = domain_from_region_or_country(args.region, args.country.as_deref())?;
    let request = DerivedBatchRequest {
        model: args.model,
        date_yyyymmdd: args.date.clone(),
        cycle_override_utc: args.cycle,
        forecast_hour: args.forecast_hour,
        source,
        domain: domain.clone(),
        out_dir: args.out_dir.clone(),
        cache_root: cache_root.clone(),
        use_cache: !args.no_cache,
        recipe_slugs: recipes,
        surface_product_override: args.surface_product.clone(),
        pressure_product_override: args.pressure_product.clone(),
        source_mode: args.source_mode.into(),
        allow_large_heavy_domain: args.allow_large_heavy_domain,
        contour_mode: args.contour_mode.into(),
        native_fill_level_multiplier: args.native_fill_level_multiplier.max(1),
        output_width: static_output_dimension("RUSTWX_STATIC_OUTPUT_WIDTH", 1600),
        output_height: static_output_dimension("RUSTWX_STATIC_OUTPUT_HEIGHT", 900),
        png_compression: args.png_compression.into(),
        place_label_overlay: default_place_label_overlay_for_domain(
            &domain,
            PlaceLabelDensityTier::from_numeric(args.place_label_density),
        ),
    };
    let report = run_derived_batch(&request)?;

    let model_slug = report.model.as_str().replace('-', "_");
    let stem = format!(
        "rustwx_{}_{}_{}z_f{:03}_{}_derived",
        model_slug,
        report.date_yyyymmdd,
        report.cycle_utc,
        report.forecast_hour,
        report.domain.slug
    );
    let manifest_path = args.out_dir.join(format!("{stem}_manifest.json"));
    let timing_path = args.out_dir.join(format!("{stem}_timing.json"));
    atomic_write_json(&manifest_path, &report)?;
    atomic_write_json(&timing_path, &report)?;
    let mut run_manifest =
        RunPublicationManifest::new("derived_batch", stem.clone(), args.out_dir.clone())
            .with_run_metadata(
                report.model.as_str(),
                report.date_yyyymmdd.clone(),
                report.cycle_utc,
                report.forecast_hour,
                report.source.as_str(),
                report.domain.slug.clone(),
            )
            .with_input_fetches(report.input_fetches.clone())
            .with_artifacts(
                request
                    .recipe_slugs
                    .iter()
                    .map(|slug| {
                        PublishedArtifactRecord::planned(
                            slug.clone(),
                            expected_output_relative_path(
                                report.model.as_str(),
                                &report.date_yyyymmdd,
                                report.cycle_utc,
                                report.forecast_hour,
                                &report.domain.slug,
                                slug,
                            ),
                        )
                    })
                    .collect(),
            );
    for recipe in &report.recipes {
        run_manifest.update_artifact_state(
            &recipe.recipe_slug,
            ArtifactPublicationState::Complete,
            Some(format!(
                "source_mode={} source_route={}",
                report.source_mode.as_str(),
                recipe.source_route.as_str()
            )),
        );
        run_manifest.update_artifact_identity(&recipe.recipe_slug, recipe.content_identity.clone());
        run_manifest
            .update_artifact_input_fetch_keys(&recipe.recipe_slug, recipe.input_fetch_keys.clone());
    }
    for blocker in &report.blockers {
        run_manifest.update_artifact_state(
            &blocker.recipe_slug,
            ArtifactPublicationState::Blocked,
            Some(format!(
                "source_mode={} source_route={} {}",
                report.source_mode.as_str(),
                blocker.source_route.as_str(),
                blocker.reason
            )),
        );
    }
    let (canonical_manifest, attempt_manifest) =
        finalize_and_publish_run_manifest(&mut run_manifest, &args.out_dir, &stem)?;

    for recipe in &report.recipes {
        println!("{}", recipe.output_path.display());
    }
    if !report.blockers.is_empty() {
        eprintln!("blocked derived products:");
        for blocker in &report.blockers {
            eprintln!(
                "  {} [{}]: {}",
                blocker.recipe_slug,
                blocker.source_route.as_str(),
                blocker.reason
            );
        }
    }
    println!("{}", manifest_path.display());
    println!("{}", timing_path.display());
    println!("{}", canonical_manifest.display());
    println!("{}", attempt_manifest.display());
    Ok(())
}

fn static_output_dimension(name: &str, fallback: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value >= 320)
        .unwrap_or(fallback)
}

fn expected_output_relative_path(
    model_slug: &str,
    date_yyyymmdd: &str,
    cycle_utc: u8,
    forecast_hour: u16,
    domain_slug: &str,
    product_slug: &str,
) -> PathBuf {
    PathBuf::from(format!(
        "rustwx_{}_{}_{}z_f{:03}_{}_{}.png",
        model_slug.replace('-', "_"),
        date_yyyymmdd,
        cycle_utc,
        forecast_hour,
        domain_slug,
        product_slug
    ))
}
