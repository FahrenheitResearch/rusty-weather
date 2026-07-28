//! Render every stored product from one rw-store hour to PNG, through the
//! EXACT render paths the GRIB-lane smoke bins use (proven pixel-identical
//! for representative direct and derived products — see the Task 4 parity
//! matrix in the commit). The flow lives in the shared `render_all` module
//! (also driven per-hour by `rw_batch`): direct recipes resolve their
//! fetch-plan `FieldSelector`s against the stored variable metadata;
//! derived/heavy recipes read their precomputed slug-named grids; windowed
//! products accumulate across the run's stored hours. Products whose
//! inputs are not in the store are reported as unresolvable with the
//! missing selector — a failure only when requested explicitly.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[path = "../contour_mode.rs"]
mod contour_mode;
#[path = "../plot_geo.rs"]
mod plot_geo;
#[path = "../region.rs"]
mod region;
#[path = "../render_all.rs"]
mod render_all;
use rw_ingest::throttle;

use clap::{Parser, ValueEnum};
use contour_mode::ContourModeArg;
use plot_geo::{Geo, GeoProjection};
use rayon::prelude::*;
use region::RegionPreset;
use render_all::{StoreFieldSource, StoreRenderConfig, StoreRenderSkip};
use rustwx_core::{ModelId, SourceId};
use rustwx_models::model_summary;
use rustwx_products::places::{PlaceLabelDensityTier, default_place_label_overlay_for_domain};
use rustwx_products::shared_context::DomainSpec;
use serde::Serialize;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum ImageOutputFormatArg {
    Png,
    Webp,
    PngWebp,
}

impl ImageOutputFormatArg {
    fn writes_webp(self) -> bool {
        matches!(self, Self::Webp | Self::PngWebp)
    }

    fn keeps_png(self) -> bool {
        matches!(self, Self::Png | Self::PngWebp)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Webp => "webp",
            Self::PngWebp => "png-webp",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "rw-render",
    about = "Render stored rw-store products to PNG through the existing render paths"
)]
struct Args {
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long, default_value = "hrrr")]
    model: ModelId,
    #[arg(long, help = "Run slug as stored, e.g. 20260608_00z")]
    run: String,
    #[arg(long, help = "Forecast hour of the stored .rws file")]
    hour: u16,
    #[arg(
        long,
        default_value = "all",
        help = "all | none | direct | derived | heavy | windowed | fuels | cafire-core | \
                cafire-all | cafire-expanded | cafire-with-fuels | cafire-fuels | \
                comma-separated product slugs (windowed products span the \
                run's stored hours, anchored at the max hour; 'all' includes them only when \
                more than one hour is stored)"
    )]
    products: String,
    #[arg(long, value_enum, default_value_t = RegionPreset::Midwest)]
    region: RegionPreset,
    #[arg(
        long,
        help = "Custom render domain bounds as west,east,south,north; overrides --region/--regions"
    )]
    domain_bounds: Option<String>,
    #[arg(
        long,
        default_value = "custom",
        help = "Slug/name for --domain-bounds output paths and manifest entries"
    )]
    domain_slug: String,
    #[arg(
        long,
        help = "Comma-separated region presets for one-pass domain batching; accepts slugs like california,pacific_northwest,west-coast"
    )]
    regions: Option<String>,
    #[arg(
        long,
        default_value_t = false,
        help = "Shortcut for --regions california,pacific_northwest,west_coast,rockies_high_plains,sierra_nevada"
    )]
    western_api_domains: bool,
    #[arg(long, default_value = "out/rw_render")]
    out_dir: PathBuf,
    #[arg(
        long,
        help = "Source stamped into provenance subtitles; defaults to the model's primary source \
                (the store does not record the fetch source)"
    )]
    source: Option<SourceId>,
    #[arg(long, value_enum, default_value_t = ContourModeArg::Automatic)]
    contour_mode: ContourModeArg,
    #[arg(long, default_value_t = 1)]
    native_fill_level_multiplier: usize,
    #[arg(long = "png-compression", value_enum, default_value_t = PngCompressionArg::Fast)]
    png_compression: PngCompressionArg,
    #[arg(
        long = "output-format",
        value_enum,
        default_value_t = ImageOutputFormatArg::Png,
        help = "png keeps existing output, webp emits lossless API WebP only, png-webp keeps both"
    )]
    output_format: ImageOutputFormatArg,
    #[arg(long = "place-label-density", default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=4))]
    place_label_density: u8,
    #[arg(
        long,
        help = "Rayon thread count (default: all cores minus 2 in polite mode, all cores with --full-throttle)"
    )]
    threads: Option<usize>,
    #[arg(
        long,
        default_value_t = false,
        help = "Dedicated-node mode: keep normal process priority and use every core"
    )]
    full_throttle: bool,
}

/// `YYYYMMDD_CCz` -> (date, cycle).
fn parse_run_slug(run: &str) -> Result<(String, u8), Box<dyn std::error::Error>> {
    let (date, cycle) = run
        .split_once('_')
        .ok_or_else(|| format!("--run '{run}' is not of the form YYYYMMDD_CCz"))?;
    if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("--run '{run}': '{date}' is not a YYYYMMDD date").into());
    }
    let cycle = cycle
        .strip_suffix('z')
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| *value < 24)
        .ok_or_else(|| format!("--run '{run}': '{cycle}' is not a cycle of the form CCz"))?;
    Ok((date.to_string(), cycle))
}

fn static_output_dimension(name: &str, fallback: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value >= 320)
        .unwrap_or(fallback)
}

fn western_api_domain_presets() -> Vec<RegionPreset> {
    vec![
        RegionPreset::California,
        RegionPreset::PacificNorthwest,
        RegionPreset::WestCoast,
        RegionPreset::RockiesHighPlains,
        RegionPreset::SierraNevada,
    ]
}

fn parse_region_list(spec: &str) -> Result<Vec<RegionPreset>, Box<dyn std::error::Error>> {
    let mut regions = Vec::new();
    for part in spec
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let region = RegionPreset::from_slug_or_name(part)
            .ok_or_else(|| format!("unknown region preset '{part}' in --regions"))?;
        if !regions.iter().any(|existing| *existing == region) {
            regions.push(region);
        }
    }
    if regions.is_empty() {
        return Err("--regions must include at least one region preset".into());
    }
    Ok(regions)
}

fn selected_regions(args: &Args) -> Result<Vec<RegionPreset>, Box<dyn std::error::Error>> {
    if let Some(spec) = &args.regions {
        return parse_region_list(spec);
    }
    if args.western_api_domains {
        return Ok(western_api_domain_presets());
    }
    Ok(vec![args.region])
}

fn selected_domains(args: &Args) -> Result<Vec<DomainSpec>, Box<dyn std::error::Error>> {
    if let Some(bounds) = &args.domain_bounds {
        return Ok(vec![DomainSpec::new(
            sanitize_domain_slug(&args.domain_slug),
            parse_domain_bounds(bounds)?,
        )]);
    }
    selected_regions(args).map(|regions| {
        regions
            .into_iter()
            .map(|region| DomainSpec::new(region.slug(), region.bounds()))
            .collect()
    })
}

fn parse_domain_bounds(spec: &str) -> Result<(f64, f64, f64, f64), Box<dyn std::error::Error>> {
    let parts = spec
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("--domain-bounds must be west,east,south,north: {err}"))?;
    let [west, east, south, north]: [f64; 4] = parts
        .try_into()
        .map_err(|_| "--domain-bounds must contain exactly four comma-separated numbers")?;
    if !west.is_finite() || !east.is_finite() || !south.is_finite() || !north.is_finite() {
        return Err("--domain-bounds values must be finite".into());
    }
    if west < -360.0 || east > 360.0 || south < -90.0 || north > 90.0 || south >= north {
        return Err(format!(
            "--domain-bounds out of range: west/east must be within +/-360, south < north within +/-90; got {west},{east},{south},{north}"
        )
        .into());
    }
    Ok((west, east, south, north))
}

fn sanitize_domain_slug(value: &str) -> String {
    let slug = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if slug.is_empty() {
        "custom".to_string()
    } else {
        slug
    }
}

#[derive(Debug, Clone)]
struct PublishedWebp {
    slug: String,
    webp_path: PathBuf,
    png_bytes: u64,
    webp_bytes: u64,
    encode_ms: u128,
    removed_png: bool,
}

#[derive(Debug, Serialize)]
struct ApiRenderManifest {
    build: &'static str,
    model: String,
    run: String,
    hour: u16,
    output_format: &'static str,
    output_width: u32,
    output_height: u32,
    plot_style: Option<String>,
    domains: Vec<ApiDomainManifest>,
}

#[derive(Debug, Serialize)]
struct ApiDomainManifest {
    slug: String,
    bounds: [f64; 4],
    product_count: usize,
    total_webp_bytes: u64,
    /// The projection every product in this domain was drawn in, as the renderer
    /// resolved it. Domain-level because one domain is one projected mesh — the
    /// per-product `geo` blocks below repeat it, since each also carries the plot
    /// rect, which a vertical colorbar or a missing title moves per image.
    #[serde(skip_serializing_if = "Option::is_none")]
    projection: Option<GeoProjection>,
    products: Vec<ApiProductManifest>,
}

#[derive(Debug, Serialize)]
struct ApiProductManifest {
    slug: String,
    file: String,
    bytes: u64,
    source_png_bytes: u64,
    encode_ms: u128,
    /// Omitted when the renderer could not state the mapping exactly (panels,
    /// and any lane that never told it which projection made the mesh), so a
    /// client keeps its measurement fallback exactly where it needs one.
    #[serde(skip_serializing_if = "Option::is_none")]
    geo: Option<Geo>,
}

fn webp_path_for_png(path: &Path) -> PathBuf {
    path.with_extension("webp")
}

fn api_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

fn publish_lossless_webp(
    product: &render_all::RenderedProduct,
    keep_png: bool,
) -> Result<PublishedWebp, String> {
    let png_path = product.output_path.clone();
    let webp_path = webp_path_for_png(&png_path);
    let png_bytes = fs::metadata(&png_path)
        .map_err(|err| format!("{}: metadata failed: {err}", png_path.display()))?
        .len();
    let started = Instant::now();
    let image = image::ImageReader::open(&png_path)
        .map_err(|err| format!("{}: open failed: {err}", png_path.display()))?
        .with_guessed_format()
        .map_err(|err| format!("{}: format probe failed: {err}", png_path.display()))?
        .decode()
        .map_err(|err| format!("{}: decode failed: {err}", png_path.display()))?
        .to_rgba8();
    let mut writer = BufWriter::new(
        File::create(&webp_path)
            .map_err(|err| format!("{}: create failed: {err}", webp_path.display()))?,
    );
    image::codecs::webp::WebPEncoder::new_lossless(&mut writer)
        .encode(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|err| format!("{}: encode failed: {err}", webp_path.display()))?;
    writer
        .flush()
        .map_err(|err| format!("{}: flush failed: {err}", webp_path.display()))?;
    let webp_bytes = fs::metadata(&webp_path)
        .map_err(|err| format!("{}: metadata failed: {err}", webp_path.display()))?
        .len();
    if !keep_png {
        fs::remove_file(&png_path)
            .map_err(|err| format!("{}: png cleanup failed: {err}", png_path.display()))?;
    }
    Ok(PublishedWebp {
        slug: product.slug.clone(),
        webp_path,
        png_bytes,
        webp_bytes,
        encode_ms: started.elapsed().as_millis(),
        removed_png: !keep_png,
    })
}

fn publish_webp_outputs(
    rendered: &[render_all::RenderedProduct],
    output_format: ImageOutputFormatArg,
) -> Result<Vec<PublishedWebp>, Box<dyn std::error::Error>> {
    if !output_format.writes_webp() || rendered.is_empty() {
        return Ok(Vec::new());
    }
    let keep_png = output_format.keeps_png();
    let mut published = rendered
        .par_iter()
        .map(|product| publish_lossless_webp(product, keep_png))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("webp publish failed: {err}"))?;
    published.sort_by(|left, right| left.slug.cmp(&right.slug));
    Ok(published)
}

fn print_webp_summary(domain_slug: &str, published: &[PublishedWebp]) {
    if published.is_empty() {
        return;
    }
    let png_bytes: u64 = published.iter().map(|item| item.png_bytes).sum();
    let webp_bytes: u64 = published.iter().map(|item| item.webp_bytes).sum();
    let encode_ms: u128 = published.iter().map(|item| item.encode_ms).sum();
    let ratio = if png_bytes == 0 {
        0.0
    } else {
        (webp_bytes as f64 / png_bytes as f64) * 100.0
    };
    let png_state = if published.iter().any(|item| item.removed_png) {
        "png removed"
    } else {
        "png kept"
    };
    let sample_path = published
        .first()
        .map(|item| item.webp_path.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    println!(
        "webp {:<20} {:>3} lossless files | encode {:>6} ms | png {:>8.2} MB -> webp {:>8.2} MB ({:>5.1}%) | {} | sample {}",
        domain_slug,
        published.len(),
        encode_ms,
        png_bytes as f64 / 1_048_576.0,
        webp_bytes as f64 / 1_048_576.0,
        ratio,
        png_state,
        sample_path,
    );
}

/// One domain's manifest entry, built from what was RENDERED rather than from
/// what was published as webp.
///
/// It used to be built from the webp list alone, which meant an
/// `--output-format png` run wrote no manifest at all and the API had no
/// georeferencing to hand back for the exact format the site's PNG mode asks
/// for. The webp publish result is now a join: present, it names the served file
/// and its encode cost; absent, the rendered PNG is the served file.
fn domain_manifest(
    domain_slug: &str,
    bounds: (f64, f64, f64, f64),
    output_root: &Path,
    rendered: &[render_all::RenderedProduct],
    published: &[PublishedWebp],
) -> ApiDomainManifest {
    let products: Vec<ApiProductManifest> = rendered
        .iter()
        .map(|product| {
            let webp = published.iter().find(|item| item.slug == product.slug);
            let geo = product
                .plot_geometry
                .as_ref()
                .map(|geometry| Geo::from_plot_geometry(geometry, bounds));
            match webp {
                Some(webp) => ApiProductManifest {
                    slug: product.slug.clone(),
                    file: api_relative_path(output_root, &webp.webp_path),
                    bytes: webp.webp_bytes,
                    source_png_bytes: webp.png_bytes,
                    encode_ms: webp.encode_ms,
                    geo,
                },
                None => {
                    // No webp leg, so the PNG is what gets served. Its size is
                    // read here rather than tracked through the render lanes.
                    let bytes = fs::metadata(&product.output_path)
                        .map(|meta| meta.len())
                        .unwrap_or(0);
                    ApiProductManifest {
                        slug: product.slug.clone(),
                        file: api_relative_path(output_root, &product.output_path),
                        bytes,
                        source_png_bytes: bytes,
                        encode_ms: 0,
                        geo,
                    }
                }
            }
        })
        .collect();
    let mut products = products;
    // Slug order, not render order: the manifest is a document a client diffs.
    products.sort_by(|left, right| left.slug.cmp(&right.slug));
    let projection = products
        .iter()
        .filter_map(|product| product.geo.as_ref())
        .map(|geo| geo.projection.clone())
        .next();
    ApiDomainManifest {
        slug: domain_slug.to_string(),
        bounds: [bounds.0, bounds.1, bounds.2, bounds.3],
        product_count: products.len(),
        total_webp_bytes: published.iter().map(|item| item.webp_bytes).sum(),
        projection,
        products,
    }
}

fn write_api_manifest(
    path: &Path,
    manifest: &ApiRenderManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(manifest)?;
    fs::write(path, bytes)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // Scheduling policy must land before anything touches rayon (the
    // global pool is built lazily on first use and cannot be resized).
    throttle::apply(args.threads, args.full_throttle);
    run(&args)
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let total_started = Instant::now();
    let (date, cycle) = parse_run_slug(&args.run)?;
    let request = render_all::partition_products(&args.products, args.model)?;
    let domains = selected_domains(args)?;
    let multi_domain = domains.len() > 1;
    let model_slug = args.model.as_str().replace('-', "_");
    let source = args
        .source
        .unwrap_or(model_summary(args.model).sources[0].id);
    let output_width = static_output_dimension("RUSTWX_STATIC_OUTPUT_WIDTH", 1600);
    let output_height = static_output_dimension("RUSTWX_STATIC_OUTPUT_HEIGHT", 900);

    let open_started = Instant::now();
    let store = StoreFieldSource::open(&args.store_root, &model_slug, &args.run, args.hour)?;
    let open_ms = open_started.elapsed().as_millis();
    println!(
        "rw_render build {} | store {} | {} products requested ({} direct, {} derived/heavy, {} fuel, {} windowed) | {} domain(s) | output {:?} | open {} ms",
        env!("RW_BUILD_SHA"),
        store.hour_path().display(),
        request.direct.len() + request.derived.len() + request.fuel.len() + request.windowed.len(),
        request.direct.len(),
        request.derived.len(),
        request.fuel.len(),
        request.windowed.len(),
        domains.len(),
        args.output_format,
        open_ms,
    );

    let mut timings: Vec<(String, u128)> = Vec::new();
    let mut skipped: Vec<(String, StoreRenderSkip)> = Vec::new();
    let mut api_domains: Vec<ApiDomainManifest> = Vec::new();

    for domain in &domains {
        let domain_slug = domain.slug.as_str();
        let domain_started = Instant::now();
        let bounds = domain.bounds;
        let out_dir = if multi_domain {
            args.out_dir.join(domain_slug)
        } else {
            args.out_dir.clone()
        };
        let config = StoreRenderConfig {
            model: args.model,
            date_yyyymmdd: date.clone(),
            cycle_utc: cycle,
            source,
            domain: domain.clone(),
            out_dir,
            contour_mode: args.contour_mode.into(),
            native_fill_level_multiplier: args.native_fill_level_multiplier.max(1),
            output_width,
            output_height,
            png_compression: args.png_compression.into(),
            place_label_overlay: default_place_label_overlay_for_domain(
                &domain,
                PlaceLabelDensityTier::from_numeric(args.place_label_density),
            ),
        };
        println!(
            "domain {:<20} bounds [{:.2}, {:.2}, {:.2}, {:.2}] -> {}",
            domain_slug,
            bounds.0,
            bounds.1,
            bounds.2,
            bounds.3,
            config.out_dir.display()
        );

        let mut domain_rendered: Vec<render_all::RenderedProduct> = Vec::new();
        let hour_outcome = render_all::render_hour_products(
            &config,
            &store,
            args.hour,
            &request.direct,
            &request.derived,
            &request.fuel,
            // Solo render: nothing else competes for memory, no chunk gate.
            None,
        )?;
        for product in &hour_outcome.rendered {
            println!(
                "{:>8} ms  {:<20} {}  {}",
                product.total_ms,
                domain_slug,
                product.slug,
                product.output_path.display()
            );
            timings.push((format!("{domain_slug}/{}", product.slug), product.total_ms));
        }
        domain_rendered.extend(hour_outcome.rendered);
        skipped.extend(
            hour_outcome
                .skipped
                .into_iter()
                .map(|skip| (domain_slug.to_string(), skip)),
        );

        if !request.windowed.is_empty() {
            match render_all::render_windowed_products(
                &config,
                &store,
                &args.store_root,
                &model_slug,
                &args.run,
                &request.windowed,
                request.windowed_auto,
                // Interactive render: anchor the accumulation window at the
                // requested hour so QPF honors the hour the user picked
                // (batch/pipeline passes None = anchor at the max stored hour).
                Some(args.hour),
            )? {
                None => println!(
                    "windowed {:<15} skipped (single stored hour; 'all' includes windowed products only when more than one hour is stored)",
                    domain_slug
                ),
                Some(outcome) => {
                    println!(
                        "windowed {:<15} {} realized, {} blocked | anchor F{:03} over {} stored hour(s) | compute {} ms",
                        domain_slug,
                        outcome.rendered.len(),
                        outcome.blocked.len(),
                        outcome.anchor_hour,
                        outcome.stored_hours,
                        outcome.compute_ms,
                    );
                    for product in &outcome.rendered {
                        println!(
                            "{:>8} ms  {:<20} {}  {}",
                            product.total_ms,
                            domain_slug,
                            product.slug,
                            product.output_path.display()
                        );
                        timings.push((format!("{domain_slug}/{}", product.slug), product.total_ms));
                    }
                    skipped.extend(
                        outcome
                            .blocked
                            .into_iter()
                            .map(|skip| (domain_slug.to_string(), skip)),
                    );
                    domain_rendered.extend(outcome.rendered);
                }
            }
        }

        if !request.climo.is_empty() {
            // Reference-map requests (climo_ref:...) render stored anchor
            // grids directly and don't need the forecast fold.
            let (ref_slugs, anomaly_slugs): (Vec<String>, Vec<String>) = request
                .climo
                .iter()
                .cloned()
                .partition(|slug| render_all::climo_products::parse_climo_ref(slug).is_some());
            if !ref_slugs.is_empty() {
                let requests: Vec<_> = ref_slugs
                    .iter()
                    .filter_map(|slug| render_all::climo_products::parse_climo_ref(slug))
                    .collect();
                let (ref_rendered, ref_skipped) =
                    render_all::climo_products::render_climo_reference_maps(
                        &config,
                        &args.store_root,
                        &model_slug,
                        &args.run,
                        &requests,
                    )?;
                println!(
                    "climo-ref {:<14} {} rendered, {} blocked",
                    domain_slug,
                    ref_rendered.len(),
                    ref_skipped.len(),
                );
                for product in &ref_rendered {
                    println!(
                        "{:>8} ms  {:<20} {}  {}",
                        product.total_ms,
                        domain_slug,
                        product.slug,
                        product.output_path.display()
                    );
                    timings.push((format!("{domain_slug}/{}", product.slug), product.total_ms));
                }
                skipped.extend(
                    ref_skipped
                        .into_iter()
                        .map(|skip| (domain_slug.to_string(), skip)),
                );
                domain_rendered.extend(ref_rendered);
            }
            let outcome = render_all::climo_products::render_climo_products_from_store(
                &config,
                &args.store_root,
                &model_slug,
                &args.run,
                &anomaly_slugs,
            )?;
            println!(
                "climo {:<18} {} rendered, {} blocked | DOY {:03} anchored F{:03}",
                domain_slug,
                outcome.rendered.len(),
                outcome.skipped.len(),
                outcome.doy,
                outcome.anchor_hour,
            );
            for product in &outcome.rendered {
                println!(
                    "{:>8} ms  {:<20} {}  {}",
                    product.total_ms,
                    domain_slug,
                    product.slug,
                    product.output_path.display()
                );
                timings.push((format!("{domain_slug}/{}", product.slug), product.total_ms));
            }
            skipped.extend(
                outcome
                    .skipped
                    .into_iter()
                    .map(|skip| (domain_slug.to_string(), skip)),
            );
            domain_rendered.extend(outcome.rendered);
        }

        let published = publish_webp_outputs(&domain_rendered, args.output_format)?;
        print_webp_summary(domain_slug, &published);
        if !domain_rendered.is_empty() {
            api_domains.push(domain_manifest(
                domain_slug,
                bounds,
                &args.out_dir,
                &domain_rendered,
                &published,
            ));
        }
        println!(
            "domain {:<20} done | {} product(s) | wall {} ms",
            domain_slug,
            domain_rendered.len(),
            domain_started.elapsed().as_millis()
        );
    }

    let total_ms = total_started.elapsed().as_millis();
    if !timings.is_empty() {
        let mut sorted: Vec<u128> = timings.iter().map(|(_, ms)| *ms).collect();
        sorted.sort_unstable();
        println!(
            "rendered {} products | per-product ms min {} / median {} / max {} | total wall {} ms",
            timings.len(),
            sorted[0],
            sorted[sorted.len() / 2],
            sorted[sorted.len() - 1],
            total_ms,
        );
    } else {
        println!("rendered 0 products | total wall {total_ms} ms");
    }
    // Any domain that rendered anything, in ANY output format. This used to
    // depend on the webp publish step, so `--output-format png` wrote no
    // manifest and the API had nothing to georeference PNG jobs with.
    if !api_domains.is_empty() {
        let manifest = ApiRenderManifest {
            build: env!("RW_BUILD_SHA"),
            model: args.model.as_str().to_string(),
            run: args.run.clone(),
            hour: args.hour,
            output_format: args.output_format.as_str(),
            output_width,
            output_height,
            plot_style: std::env::var("RUSTWX_PLOT_STYLE").ok(),
            domains: api_domains,
        };
        let manifest_path = args.out_dir.join("api_manifest.json");
        write_api_manifest(&manifest_path, &manifest)?;
        println!("api manifest {}", manifest_path.display());
    }
    if !skipped.is_empty() {
        eprintln!("unresolvable products ({}):", skipped.len());
        for (domain, skip) in &skipped {
            eprintln!("  {}/{}: {}", domain, skip.slug, skip.reason);
        }
        if request.strict {
            return Err(format!(
                "{} explicitly requested product(s) could not render from the store",
                skipped.len()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_slug_parses_date_and_cycle() {
        assert_eq!(
            parse_run_slug("20260608_00z").unwrap(),
            ("20260608".to_string(), 0)
        );
        assert_eq!(
            parse_run_slug("20251231_23z").unwrap(),
            ("20251231".to_string(), 23)
        );
    }

    #[test]
    fn run_slug_rejects_malformed_inputs_naming_the_flag() {
        for bad in ["20260608", "2026060_00z", "20260608_24z", "20260608_0", ""] {
            let err = parse_run_slug(bad).unwrap_err().to_string();
            assert!(
                err.contains("--run"),
                "error for '{bad}' must name --run: {err}"
            );
        }
    }

    #[test]
    fn region_lists_accept_slugs_and_kebab_case() {
        let regions =
            parse_region_list("california,pacific-northwest,west_coast,sierra-nevada").unwrap();
        assert_eq!(
            regions,
            vec![
                RegionPreset::California,
                RegionPreset::PacificNorthwest,
                RegionPreset::WestCoast,
                RegionPreset::SierraNevada,
            ]
        );
    }

    #[test]
    fn region_lists_deduplicate_in_order() {
        let regions = parse_region_list("california,california,west-coast").unwrap();
        assert_eq!(
            regions,
            vec![RegionPreset::California, RegionPreset::WestCoast]
        );
    }

    #[test]
    fn domain_bounds_parse_for_web_drawn_boxes() {
        assert_eq!(
            parse_domain_bounds("-123.5,-120.25,37.0,39.5").unwrap(),
            (-123.5, -120.25, 37.0, 39.5)
        );
    }

    #[test]
    fn domain_bounds_reject_wrong_shape_and_inverted_latitude() {
        assert!(parse_domain_bounds("-123,-120,37").is_err());
        assert!(parse_domain_bounds("-123,-120,40,37").is_err());
    }

    #[test]
    fn custom_domain_slugs_are_path_safe() {
        assert_eq!(
            sanitize_domain_slug("CAFire Box 12 / Napa"),
            "cafire_box_12_napa"
        );
        assert_eq!(sanitize_domain_slug(" "), "custom");
    }

    fn geometry_fixture() -> rustwx_render::PlotGeometry {
        rustwx_render::PlotGeometry {
            image_w: 1600,
            image_h: 889,
            plot_x: 16,
            plot_y: 74,
            plot_w: 1568,
            plot_h: 742,
            projected: rustwx_render::ProjectedExtent {
                x_min: -467_281.5,
                x_max: 467_281.5,
                y_min: -546_733.6,
                y_max: 552_466.0,
            },
            projection: rustwx_render::ProjectionSpec::Mercator {
                latitude_of_true_scale_deg: 37.2,
                central_meridian_deg: -119.75,
            },
            reference_latitude_deg: None,
            reference_longitude_deg: Some(-119.75),
            earth_radius_m: 6_370_000.0,
            geographic: None,
        }
    }

    fn rendered(slug: &str, path: &Path, geometry: bool) -> render_all::RenderedProduct {
        render_all::RenderedProduct {
            slug: slug.to_string(),
            total_ms: 12,
            output_path: path.to_path_buf(),
            plot_geometry: geometry.then(geometry_fixture),
        }
    }

    /// The bug this fixes: the manifest was built from the webp publish result,
    /// so `--output-format png` produced no manifest at all and the API had no
    /// georeferencing for PNG jobs. A png-only domain must still describe every
    /// rendered file — naming the PNG, since that is what gets served.
    #[test]
    fn a_png_only_domain_still_gets_a_manifest_naming_its_png_files() {
        let root = Path::new("out").join("api");
        let png = root.join("rustwx_hrrr_20260726_12z_f006_california_vpd_2m.png");
        let manifest = domain_manifest(
            "california",
            (-124.5, -113.5, 32.0, 42.0),
            &root,
            &[rendered("vpd_2m", &png, true)],
            &[],
        );
        assert_eq!(manifest.product_count, 1);
        assert_eq!(manifest.total_webp_bytes, 0);
        assert_eq!(
            manifest.products[0].file,
            "rustwx_hrrr_20260726_12z_f006_california_vpd_2m.png"
        );
        let geo = manifest.products[0]
            .geo
            .as_ref()
            .expect("a georeferenced product must publish geo in png mode too");
        assert_eq!(geo.plot_px.width, 1568);
        assert_eq!(geo.requested_bounds.west, -124.5);
        assert_eq!(geo.requested_bounds.north, 42.0);
        // The domain block repeats the projection once for the whole mesh.
        assert_eq!(
            manifest.projection.as_ref().map(|proj| proj.earth_radius_m),
            Some(6_370_000.0)
        );
    }

    /// A product the renderer could not georeference (a composite panel, or any
    /// lane that never stated its projection) must be listed WITHOUT `geo` so the
    /// client keeps its measurement fallback exactly there.
    #[test]
    fn products_without_geometry_are_listed_without_a_geo_block() {
        let root = Path::new("out").join("api");
        let panel = root.join("rustwx_hrrr_20260726_12z_f006_california_cloud_cover_levels.png");
        let manifest = domain_manifest(
            "california",
            (-124.5, -113.5, 32.0, 42.0),
            &root,
            &[rendered("cloud_cover_levels", &panel, false)],
            &[],
        );
        assert!(manifest.products[0].geo.is_none());
        assert!(
            manifest.projection.is_none(),
            "a domain with nothing georeferenced states no projection"
        );
        let json = serde_json::to_value(&manifest).unwrap();
        assert!(
            json["products"][0].get("geo").is_none(),
            "geo must be omitted, not null: {json}"
        );
    }

    /// With a webp leg the served file is the webp, and its geo is the same rect:
    /// the webp is a lossless re-encode of those exact pixels.
    #[test]
    fn a_published_webp_is_the_named_file_and_keeps_the_same_rect() {
        let root = Path::new("out").join("api");
        let png = root.join("rustwx_hrrr_20260726_12z_f006_california_hdw.png");
        let manifest = domain_manifest(
            "california",
            (-124.5, -113.5, 32.0, 42.0),
            &root,
            &[rendered("hdw", &png, true)],
            &[PublishedWebp {
                slug: "hdw".to_string(),
                webp_path: webp_path_for_png(&png),
                png_bytes: 900_000,
                webp_bytes: 400_000,
                encode_ms: 7,
                removed_png: true,
            }],
        );
        assert_eq!(
            manifest.products[0].file,
            "rustwx_hrrr_20260726_12z_f006_california_hdw.webp"
        );
        assert_eq!(manifest.products[0].bytes, 400_000);
        assert_eq!(manifest.products[0].source_png_bytes, 900_000);
        assert_eq!(manifest.total_webp_bytes, 400_000);
        assert_eq!(
            manifest.products[0].geo.as_ref().map(|geo| geo.plot_px),
            Some(plot_geo::RectPx {
                x: 16,
                y: 74,
                width: 1568,
                height: 742
            })
        );
    }

    #[test]
    fn api_relative_paths_use_forward_slashes() {
        let root = Path::new("out").join("api");
        let path = root
            .join("california")
            .join("rustwx_hrrr_20260629_5z_f000_california_2m_temperature.webp");
        assert_eq!(
            api_relative_path(&root, &path),
            "california/rustwx_hrrr_20260629_5z_f000_california_2m_temperature.webp"
        );
    }
}
