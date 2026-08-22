from pathlib import Path
import re

root = Path.cwd()


def load(path: str) -> str:
    return (root / path).read_text(encoding="utf-8")


def save(path: str, text: str) -> None:
    (root / path).write_text(text, encoding="utf-8")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if new in text:
        return text
    if old not in text:
        raise RuntimeError(f"{label}: anchor missing")
    return text.replace(old, new, 1)


# Product catalog categories must be exclusive; favorites are represented by
# ordering, not by duplicating products across impossible match arms.
path = "crates/rw-sat/src/product.rs"
text = load(path)
old = '''    pub const fn category(self) -> SatelliteProductCategory {
        match self {
            Self::GeoColor | Self::CleanInfrared | Self::EnhancedInfrared | Self::MidWaterVapor => {
                SatelliteProductCategory::Favorites
            }
            Self::TrueColor => SatelliteProductCategory::Visible,
            Self::CleanInfrared
            | Self::EnhancedInfrared
            | Self::ShortwaveInfrared
            | Self::CloudPhase
            | Self::Ozone
            | Self::LongwaveInfrared
            | Self::DirtyInfrared
            | Self::Co2Infrared => SatelliteProductCategory::Infrared,
            Self::UpperWaterVapor | Self::MidWaterVapor | Self::LowerWaterVapor => {
                SatelliteProductCategory::WaterVapor
            }
            Self::AirMass
            | Self::Dust
            | Self::DayCloudPhase
            | Self::DayNightCloudMicrophysics
            | Self::Sandwich => SatelliteProductCategory::RgbComposite,
            Self::FireTemperature => SatelliteProductCategory::Fire,
            Self::RawChannel(_) => SatelliteProductCategory::Advanced,
        }
    }
'''
new = '''    pub const fn category(self) -> SatelliteProductCategory {
        match self {
            Self::GeoColor => SatelliteProductCategory::Favorites,
            Self::TrueColor => SatelliteProductCategory::Visible,
            Self::CleanInfrared
            | Self::EnhancedInfrared
            | Self::ShortwaveInfrared
            | Self::CloudPhase
            | Self::Ozone
            | Self::LongwaveInfrared
            | Self::DirtyInfrared
            | Self::Co2Infrared => SatelliteProductCategory::Infrared,
            Self::UpperWaterVapor | Self::MidWaterVapor | Self::LowerWaterVapor => {
                SatelliteProductCategory::WaterVapor
            }
            Self::AirMass
            | Self::Dust
            | Self::DayCloudPhase
            | Self::DayNightCloudMicrophysics
            | Self::Sandwich => SatelliteProductCategory::RgbComposite,
            Self::FireTemperature => SatelliteProductCategory::Fire,
            Self::RawChannel(_) => SatelliteProductCategory::Advanced,
        }
    }
'''
text = replace_once(text, old, new, "product category")
save(path, text)

# Frame IDs are untrusted URL input. Avoid slicing at a non-UTF8 boundary and
# remove the deprecated DateTime parser.
path = "crates/rw-sat/src/archive.rs"
text = load(path)
old = '''fn valid_frame_id(value: &str) -> bool {
    value.len() == 13
        && value.as_bytes()[8] == b'T'
        && value[..8].bytes().all(|byte| byte.is_ascii_digit())
        && value[9..].bytes().all(|byte| byte.is_ascii_digit())
        && Utc
            .datetime_from_str(value, "%Y%m%dT%H%M")
            .is_ok()
}
'''
new = '''fn valid_frame_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 13
        && bytes[8] == b'T'
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[9..].iter().all(u8::is_ascii_digit)
        && chrono::NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M").is_ok()
}
'''
text = replace_once(text, old, new, "safe frame id validation")
save(path, text)

path = "crates/rw-sat/src/tile.rs"
text = load(path).replace(
    "use crate::archive::{NativeSatelliteFrame, resolve_native_frame};",
    "use crate::archive::resolve_native_frame;",
    1,
)
save(path, text)

# Native source files become authoritative; `.rws` is a bounded preview.
path = "crates/rw-sat/src/follow.rs"
text = load(path)
text = replace_once(
    text,
    "use crate::abi::read_goes_abi_field;\n",
    "use crate::abi::read_goes_abi_field;\nuse crate::archive::{archive_goes_source, automatic_preview_stride, prune_native_archive};\n",
    "follow archive import",
)
text = replace_once(
    text,
    "    /// Per-band stride decimation before storing (1 = native).\n    pub downsample: usize,",
    "    /// Preview stride: 0 chooses an automatic bounded preview; native source is always retained.\n    pub downsample: usize,",
    "follow downsample docs",
)
text = replace_once(text, "            downsample: 1,", "            downsample: 0,", "follow automatic default")
text = replace_once(
    text,
    '''    let field = read_goes_abi_field(&download.path, "CMI").map_err(to_send_sync)?;
    let field = downsample_field(field, downsample);
    let frame = write_band_frame(store_root, &field, written_unix).map_err(to_send_sync)?;
''',
    '''    let field = read_goes_abi_field(&download.path, "CMI").map_err(to_send_sync)?;
    archive_goes_source(store_root, &download.path, &field.scene, &object.key)
        .map_err(to_send_sync)?;
    let preview_stride = if downsample == 0 {
        automatic_preview_stride(
            field.scene.fixed_grid.nx,
            field.scene.fixed_grid.ny,
            8_000_000,
        )
    } else {
        downsample
    };
    let field = downsample_field(field, preview_stride);
    let frame = write_band_frame(store_root, &field, written_unix).map_err(to_send_sync)?;
''',
    "native source archive before preview",
)
eviction_anchor = '''                    match enforce_window(
                        &config.store_root,
                        &frame.model,
                        &run_prefix,
                        Utc::now(),
                        &config.window,
                    ) {
                        Ok(report) if report.removed_frames > 0 => {
                            summary.evicted_frames += report.removed_frames;
                            summary.evicted_bytes += report.removed_bytes;
                            sink(SatEvent::Evicted {
                                model: frame.model.clone(),
                                frames: report.removed_frames,
                                bytes: report.removed_bytes,
                            });
                        }
                        Ok(_) => {}
                        Err(err) => sink(SatEvent::Warning {
                            message: format!("window eviction: {err}"),
                        }),
                    }
                    Ok(())
'''
eviction_new = '''                    match enforce_window(
                        &config.store_root,
                        &frame.model,
                        &run_prefix,
                        Utc::now(),
                        &config.window,
                    ) {
                        Ok(report) if report.removed_frames > 0 => {
                            summary.evicted_frames += report.removed_frames;
                            summary.evicted_bytes += report.removed_bytes;
                            sink(SatEvent::Evicted {
                                model: frame.model.clone(),
                                frames: report.removed_frames,
                                bytes: report.removed_bytes,
                            });
                        }
                        Ok(_) => {}
                        Err(err) => sink(SatEvent::Warning {
                            message: format!("window eviction: {err}"),
                        }),
                    }
                    let archive_max_bytes = config.window.max_bytes.map(|bytes| {
                        bytes.saturating_mul(config.bands.len().max(1) as u64)
                    });
                    match prune_native_archive(
                        &config.store_root,
                        &frame.model,
                        config.sector.slug(),
                        Utc::now(),
                        config.window.max_age_minutes,
                        archive_max_bytes,
                    ) {
                        Ok(report) if report.removed_frames > 0 => sink(SatEvent::Info {
                            message: format!(
                                "native archive pruned: {} frame(s), {} bytes",
                                report.removed_frames, report.removed_bytes
                            ),
                        }),
                        Ok(_) => {}
                        Err(err) => sink(SatEvent::Warning {
                            message: format!("native archive eviction: {err}"),
                        }),
                    }
                    Ok(())
'''
text = replace_once(text, eviction_anchor, eviction_new, "native archive retention")
save(path, text)

# CLI defaults should never silently turn Full Disk into a quarter-resolution
# source. The optional composite stride now affects only a quicklook.
path = "crates/rw-sat/src/bin/rw_sat.rs"
text = load(path)
text = replace_once(
    text,
    "    /// Stride-decimate frames before storing (1 = native resolution).\n    #[arg(long, default_value_t = 1)]\n    downsample: usize,",
    "    /// Preview stride (0 = automatic bounded preview; native source is retained).\n    #[arg(long, default_value_t = 0)]\n    downsample: usize,",
    "CLI automatic preview",
)
text = replace_once(
    text,
    "        /// Extra stride decimation applied to the composite base grid.\n        #[arg(long, default_value_t = 4)]",
    "        /// Optional extra stride for a one-off composite quicklook.\n        #[arg(long, default_value_t = 1)]",
    "CLI composite default",
)
save(path, text)

# Remove the public fraction selector from the pure UI panel.
path = "crates/rw-ui/src/panels/satellite.rs"
text = load(path)
text = replace_once(
    text,
    '''    /// Layer slug: a band ("c13") or an RGB composite ("geocolor" — the
    /// host expands it to the required bands).''',
    '    /// User-facing product slug such as "geocolor", "clean_ir", or "c13".',
    "UI product docs",
)
text = replace_once(
    text,
    "    /// Stride decimation before storing (1 = native resolution).\n    pub downsample: usize,",
    "    /// Internal preview stride (0 = automatic); native source is retained separately.\n    pub downsample: usize,",
    "UI downsample docs",
)
text = replace_once(text, '            layer: "c13".to_string(),', '            layer: "geocolor".to_string(),', "UI GeoColor default")
text = replace_once(text, "            downsample: 1,", "            downsample: 0,", "UI automatic preview default")
text = text.replace(".width(120.0)", ".width(180.0)", 1)
detail_pattern = re.compile(
    r'''            ui\.label\("Detail"\);\n            ComboBox::from_id_salt\("rw-ui-sat-downsample"\).*?                \);\n''',
    re.S,
)
detail_replacement = '''            ui.label(
                RichText::new("Native source retained · preview optimized automatically")
                    .small()
                    .weak(),
            )
            .on_hover_text(
                "Full-resolution NetCDF remains available to rw-server; the desktop preview chooses a bounded stride automatically.",
            );
'''
text, count = detail_pattern.subn(detail_replacement, text, count=1)
if count != 1:
    raise RuntimeError(f"UI detail control replacement count={count}")
text, count = re.subn(
    r'''\nfn downsample_label\(step: usize\) -> String \{.*?\n\}\n''',
    "\n",
    text,
    count=1,
    flags=re.S,
)
if count != 1:
    raise RuntimeError(f"downsample label removal count={count}")
save(path, text)

# Desktop host product catalog and shared enhancement path.
path = "crates/rusty-weather-ui/src/sat_worker.rs"
text = load(path)
text = text.replace("use rw_sat::composite::GoesAbiRgbCompositeStyle;\n", "")
text = text.replace(
    "use rw_sat::palette::{anchor_color, band_anchors};",
    "use rw_sat::palette::band_color;\nuse rw_sat::{GoesAbiProduct, product_catalog};",
    1,
)
text = text.replace('Sector::Conus => "CONUS".to_string(),', 'Sector::Conus => "CONUS · 5 minute".to_string(),', 1)
text = text.replace('Sector::FullDisk => "Full disk".to_string(),', 'Sector::FullDisk => "Full Disk · 10 minute".to_string(),', 1)
text = text.replace('Sector::Meso1 => "Meso 1".to_string(),', 'Sector::Meso1 => "Mesoscale 1 · 1 minute".to_string(),', 1)
text = text.replace('Sector::Meso2 => "Meso 2".to_string(),', 'Sector::Meso2 => "Mesoscale 2 · 1 minute".to_string(),', 1)
catalog_pattern = re.compile(
    r'''/// ABI band display names.*?/// Layer slug -> the ABI bands it follows, plus a description for the\n/// summary line\. Bands: "c13"; composites by slug \("geocolor"\)\.\nfn resolve_layer\(layer: &str\) -> Result<\(Vec<u8>, String\), String> \{.*?\n\}\n''',
    re.S,
)
catalog_replacement = '''/// User-facing product picker. Required channels stay an implementation detail.
pub fn layer_options() -> Vec<SatLayerOption> {
    product_catalog(true)
        .into_iter()
        .map(|product| {
            let daylight = if product.daylight_only { " · daylight" } else { " · 24 hour" };
            SatLayerOption {
                slug: product.id,
                label: product.title,
                note: format!(
                    "{} · {:.1} km native{}",
                    product.description, product.native_resolution_km, daylight
                ),
            }
        })
        .collect()
}

fn resolve_layer(layer: &str) -> Result<(Vec<u8>, String), String> {
    let product = GoesAbiProduct::parse(layer)
        .ok_or_else(|| format!("unknown satellite product '{layer}'"))?;
    Ok((product.required_channels().to_vec(), product.title()))
}
'''
text, count = catalog_pattern.subn(catalog_replacement, text, count=1)
if count != 1:
    raise RuntimeError(f"satellite product catalog replacement count={count}")
text = replace_once(
    text,
    '''    if ![1usize, 2, 4].contains(&spec.downsample) {
        return Err(format!("unsupported detail stride {}", spec.downsample));
    }''',
    '''    if spec.downsample > 16 {
        return Err(format!(
            "preview stride {} exceeds the supported maximum of 16",
            spec.downsample
        ));
    }''',
    "worker preview validation",
)
text = replace_once(
    text,
    '''    let detail = match spec.downsample {
        1 => String::new(),
        step => format!(" · 1/{step} res"),
    };
''',
    '''    let detail = match spec.downsample {
        0 => " · native source retained; preview optimized automatically".to_string(),
        1 => " · native preview".to_string(),
        step => format!(" · explicit 1/{step} preview"),
    };
''',
    "worker summary detail",
)
text = replace_once(
    text,
    '''    let anchors = band_anchors(band);
    let mut pixels = Vec::with_capacity(nx * ny);
''',
    "    let mut pixels = Vec::with_capacity(nx * ny);\n",
    "worker remove anchors",
)
text = replace_once(
    text,
    "            let [r, g, b, a] = anchor_color(value, anchors);",
    "            let [r, g, b, a] = band_color(band, value);",
    "worker shared palette",
)
save(path, text)

# Server dependency/module/router integration while preserving the newer APIs.
path = "crates/rw-server/Cargo.toml"
text = load(path)
text = replace_once(
    text,
    'rw-scheduler = { path = "../rw-scheduler" }\n',
    'rw-scheduler = { path = "../rw-scheduler" }\nrw-sat = { path = "../rw-sat" }\n',
    "server rw-sat dependency",
)
save(path, text)

path = "crates/rw-server/src/lib.rs"
text = load(path)
text = replace_once(text, "pub mod observations;\n", "pub mod observations;\npub mod satellite;\n", "server satellite module")
save(path, text)

path = "crates/rw-server/src/routes.rs"
text = load(path)
text = replace_once(
    text,
    "        .merge(crate::observations::read_router())\n",
    "        .merge(crate::observations::read_router())\n        .merge(crate::satellite::read_router())\n",
    "server satellite routes",
)
for preserved in (
    '.route("/v1/models/{model}/latest-run", get(latest_run))',
    '.route("/v1/profile-cycle", post(profile_cycle))',
):
    if preserved not in text:
        raise RuntimeError(f"lost newer server API: {preserved}")
save(path, text)

# Close the non-Send boxed-error boundary before returning from a heavy worker.
path = "crates/rw-server/src/satellite.rs"
text = load(path)
text = replace_once(
    text,
    '''            render_native_xyz_tile(
                &store_root,
                &platform,
                &sector_slug,
                product,
                &frame,
                path.z,
                path.x,
                path.y,
                DEFAULT_TILE_SIZE,
            )
''',
    '''            render_native_xyz_tile(
                &store_root,
                &platform,
                &sector_slug,
                product,
                &frame,
                path.z,
                path.x,
                path.y,
                DEFAULT_TILE_SIZE,
            )
            .map_err(|error| error.to_string())
''',
    "server Send-safe tile error",
)
text = replace_once(
    text,
    '''fn satellite_render_problem(error: Box<dyn std::error::Error>, request_id: uuid::Uuid) -> Response {
    if let Some(error) = error.downcast_ref::<io::Error>() {
        return satellite_io_problem(io::Error::new(error.kind(), error.to_string()), request_id);
    }
    error!(%request_id, %error, "satellite tile render failed");
    ProblemDetails::internal(request_id).into_response()
}
''',
    '''fn satellite_render_problem(error: String, request_id: uuid::Uuid) -> Response {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("not found")
        || normalized.contains("no complete satellite frame")
        || normalized.contains("is incomplete")
        || normalized.contains("has no abi")
    {
        return ProblemDetails::not_found(request_id).into_response();
    }
    if normalized.contains("invalid")
        || normalized.contains("outside")
        || normalized.contains("exceeds")
        || normalized.contains("must be")
    {
        return problem(
            StatusCode::BAD_REQUEST,
            "INVALID_SATELLITE_TILE",
            "The satellite tile request is invalid.",
            request_id,
        );
    }
    error!(%request_id, %error, "satellite tile render failed");
    ProblemDetails::internal(request_id).into_response()
}
''',
    "server tile error mapper",
)
save(path, text)

for required in (
    "crates/rw-observations/src/lib.rs",
    "crates/rw-sat/src/archive.rs",
    "crates/rw-sat/src/product.rs",
    "crates/rw-sat/src/tile.rs",
    "crates/rw-server/src/observations.rs",
    "crates/rw-server/src/satellite.rs",
):
    if not (root / required).is_file():
        raise RuntimeError(f"missing normal source file: {required}")
if (root / ".github/bootstrap").exists():
    raise RuntimeError("encoded bootstrap directory must not exist")

print("satellite v2 source integration complete")
