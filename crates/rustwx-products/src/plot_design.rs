use rustwx_core::{CanonicalField, FieldSelector, VerticalSelector};
use rustwx_models::{PlotRecipe, RenderStyle};
use rustwx_render::{
    Color, ColorScale, ContourLayer, ContourLinePattern, DiscreteColorScale, DomainFrame,
    DomainFrameSource, ExtendMode, LegendControls, LegendMode, LevelDensity, MapRenderRequest,
    ProductVisualMode, RenderDensity, WindStreamlineStyle,
    weather::{
        WeatherPalette, dewpoint_palette_celsius_for_levels, temperature_palette_cropped_f,
        weather_palette, winds_palette_segments,
    },
};

#[derive(Debug, Clone, Copy)]
pub struct StaticPlotDesign {
    pub bounds: (f64, f64, f64, f64),
    pub visual_mode: ProductVisualMode,
    pub overlay_only: bool,
}

impl StaticPlotDesign {
    pub fn new(bounds: (f64, f64, f64, f64), visual_mode: ProductVisualMode) -> Self {
        Self {
            bounds,
            visual_mode,
            overlay_only: false,
        }
    }

    pub fn overlay_only(mut self, overlay_only: bool) -> Self {
        self.overlay_only = overlay_only;
        self
    }

    pub fn apply_to_request(self, request: &mut MapRenderRequest) {
        apply_static_map_design(request, self.bounds, self.visual_mode, self.overlay_only);
    }
}

pub fn longitude_bounds_span_deg(bounds: (f64, f64, f64, f64)) -> f64 {
    let raw_span = (bounds.1 - bounds.0).abs();
    if raw_span >= 359.0 {
        return raw_span.min(360.0);
    }

    let west = normalize_longitude_for_bounds(bounds.0);
    let east = normalize_longitude_for_bounds(bounds.1);
    if west <= east {
        east - west
    } else {
        east + 360.0 - west
    }
}

pub fn is_global_scale_domain(bounds: (f64, f64, f64, f64)) -> bool {
    let lat_span = (bounds.3 - bounds.2).abs();
    lat_span >= 100.0 && longitude_bounds_span_deg(bounds) >= 300.0
}

pub fn static_domain_frame_for_bounds(bounds: (f64, f64, f64, f64)) -> Option<DomainFrame> {
    if is_global_scale_domain(bounds) {
        None
    } else if straight_western_domain_frame_enabled(bounds) {
        Some(static_map_viewport_domain_frame())
    } else {
        Some(static_model_data_domain_frame())
    }
}

fn static_map_viewport_domain_frame() -> DomainFrame {
    DomainFrame {
        inset_px: 2,
        outline_width: 2,
        source: DomainFrameSource::MapViewport,
        ..DomainFrame::map_viewport_default()
    }
}

fn static_model_data_domain_frame() -> DomainFrame {
    DomainFrame {
        inset_px: 2,
        outline_width: 2,
        source: DomainFrameSource::ProjectedGrid,
        ..DomainFrame::map_viewport_default()
    }
}

fn straight_western_domain_frame_enabled(bounds: (f64, f64, f64, f64)) -> bool {
    let default = is_straight_western_domain_frame_candidate(bounds);
    std::env::var("RUSTWX_STRAIGHT_WEST_PROJECTION")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on" | "mercator" | "straight" | "northup"
            )
        })
        .unwrap_or(default)
}

fn is_straight_western_domain_frame_candidate(bounds: (f64, f64, f64, f64)) -> bool {
    let west = normalize_longitude_for_bounds(bounds.0);
    let east = normalize_longitude_for_bounds(bounds.1);
    if west > east {
        return false;
    }
    let lat_span = (bounds.3 - bounds.2).abs();
    let lon_span = longitude_bounds_span_deg(bounds);
    bounds.2 >= 25.0
        && bounds.3 <= 55.0
        && west >= -130.0
        && west <= -115.0
        && east >= -123.0
        && east <= -104.0
        && lat_span >= 4.0
        && lon_span <= 28.0
}

pub fn apply_static_map_design(
    request: &mut MapRenderRequest,
    bounds: (f64, f64, f64, f64),
    visual_mode: ProductVisualMode,
    overlay_only: bool,
) {
    request.visual_mode = visual_mode;
    request.render_density = RenderDensity {
        fill: high_detail_fill_density(),
        palette_multiplier: 4,
    };
    request.legend = LegendControls {
        density: LevelDensity::default(),
        mode: LegendMode::SmoothRamp,
    };
    if is_global_scale_domain(bounds) && !overlay_only {
        request.render_density = RenderDensity::default();
        request.legend = LegendControls {
            density: LevelDensity::default(),
            mode: LegendMode::SmoothRamp,
        };
    }
    request.domain_frame = static_domain_frame_for_bounds(bounds);
}

fn high_detail_fill_density() -> LevelDensity {
    LevelDensity {
        multiplier: 4,
        min_source_level_count: 2,
    }
}

pub fn operational_fill_scale_for_recipe(
    recipe: &PlotRecipe,
    filled_selector: FieldSelector,
) -> ColorScale {
    if recipe.slug == "mslp_10m_winds" || recipe.slug == "gefs_avg_mslp_10m_winds" {
        return ColorScale::Discrete(ten_meter_wind_speed_scale());
    }

    if filled_selector.field == CanonicalField::SmokeMassDensity {
        // Near-surface smoke is PM2.5 in ug/m^3 (see `direct_fill_unit_conversion`),
        // which is exactly what the EPA AQI breakpoints are defined on — so the
        // scale is those categories rather than a tuned ramp. Requested by an NWS
        // met who could not tell from the old smooth gradient which side of a
        // threshold he was looking at.
        //
        // What the previous empirical ramp learned is kept: the 2 ug/m^3 floor so
        // a plume's leading edge still draws, and a ceiling near observed values
        // rather than far above them. Both are now pinned to published numbers
        // instead of to a frame someone looked at once.
        //
        // Column smoke below deliberately does NOT get this treatment.
        return ColorScale::Discrete(epa_pm25_surface_scale());
    }
    if filled_selector.field == CanonicalField::ColumnIntegratedSmoke {
        // NOT the EPA scale, on purpose. This is mg/m^2 integrated through the
        // whole column — a different quantity from what anyone breathes. AQI
        // colors here would assert a health category the number cannot support:
        // a dense plume aloft that never reaches the ground would read
        // "Hazardous" while the surface air is clean. It keeps the tuned ramp.
        return ColorScale::Discrete(DiscreteColorScale {
            // Fine geometric ramp (was 5 coarse doublings → blocky) so the
            // palette lerps smoothly across smoke's heavy-tailed range.
            //
            // Top raised 720 -> 1500 mg/m^2, which is what a real plume core
            // measures: the CONUS frame from 20260727 03z F006 peaked at 1500
            // over the Idaho/Montana fires while the ramp stopped at 720, so
            // the whole core — the part a reader most wants structure in —
            // collapsed into one flat saturated blob.
            //
            // 1500 and not higher, learned by trying 3000 and looking: the ten
            // palette stops stretch across whatever range they are given, so a
            // top far above the data pushes ordinary 100-500 values down into
            // the blues and the map loses the mid-range contrast it had. Match
            // the ceiling to values that actually occur.
            levels: geometric_levels(20.0, 1.12, 1500.0),
            colors: smoke_scale_colors(),
            extend: ExtendMode::Max,
            mask_below: Some(20.0),
        });
    }

    let discrete = match recipe.style {
        RenderStyle::WeatherTemperature => {
            let (lo, hi, step, crop_f) = match filled_selector.vertical {
                VerticalSelector::HeightAboveGroundMeters(2) => {
                    (-60.0, 120.0, 1.0, Some((-60.0, 120.0)))
                }
                VerticalSelector::IsobaricHpa(200) => (-70.0, -29.0, 1.0, Some((-40.0, 70.0))),
                VerticalSelector::IsobaricHpa(250) => (-70.0, -29.0, 1.0, Some((-40.0, 70.0))),
                VerticalSelector::IsobaricHpa(300) => (-70.0, -29.0, 1.0, Some((-40.0, 70.0))),
                VerticalSelector::IsobaricHpa(500) => (-50.0, 6.0, 1.0, Some((-40.0, 70.0))),
                VerticalSelector::IsobaricHpa(700) => (-40.0, 26.0, 1.0, Some((-40.0, 90.0))),
                VerticalSelector::IsobaricHpa(850) => (-40.0, 40.0, 5.0, Some((-40.0, 110.0))),
                _ => (-50.0, 50.5, 0.5, Some((-40.0, 120.0))),
            };
            DiscreteColorScale {
                levels: range_step(lo, hi, step),
                colors: temperature_palette_cropped_f(
                    crop_f,
                    (((hi - lo) / step).round() as usize).max(2),
                ),
                extend: ExtendMode::Both,
                mask_below: None,
            }
        }
        RenderStyle::WeatherReflectivity | RenderStyle::WeatherRadarReflectivity => {
            reflectivity_dbz_scale()
        }
        RenderStyle::WeatherRh => relative_humidity_scale_for_selector(filled_selector),
        RenderStyle::WeatherProbability => DiscreteColorScale {
            levels: range_step(0.0, 101.0, 1.0),
            colors: weather_palette(WeatherPalette::Rh),
            extend: ExtendMode::Both,
            mask_below: None,
        },
        RenderStyle::WeatherVorticity => DiscreteColorScale {
            levels: range_step(-40.0, 60.1, 1.0),
            colors: weather_palette(WeatherPalette::RelVort),
            extend: ExtendMode::Both,
            mask_below: None,
        },
        RenderStyle::WeatherDewpoint => dewpoint_scale_for_selector(filled_selector),
        RenderStyle::WeatherPressure => mslp_pressure_fill_scale(),
        RenderStyle::WeatherHeight => DiscreteColorScale {
            levels: match filled_selector.vertical {
                VerticalSelector::IsobaricHpa(200) | VerticalSelector::IsobaricHpa(250) => {
                    range_step(50.0, 170.0, 5.0)
                }
                VerticalSelector::IsobaricHpa(300) => range_step(20.0, 160.0, 5.0),
                VerticalSelector::IsobaricHpa(500) => range_step(20.0, 140.0, 5.0),
                VerticalSelector::IsobaricHpa(700) => range_step(20.0, 80.0, 5.0),
                VerticalSelector::IsobaricHpa(850) | VerticalSelector::IsobaricHpa(925) => {
                    range_step(20.0, 80.0, 5.0)
                }
                _ => range_step(10.0, 71.0, 1.0),
            },
            colors: match filled_selector.vertical {
                VerticalSelector::IsobaricHpa(200) | VerticalSelector::IsobaricHpa(250) => {
                    winds_palette_segments(120)
                }
                VerticalSelector::IsobaricHpa(300) => winds_palette_segments(100),
                VerticalSelector::IsobaricHpa(500) => winds_palette_segments(100),
                VerticalSelector::IsobaricHpa(700)
                | VerticalSelector::IsobaricHpa(850)
                | VerticalSelector::IsobaricHpa(925) => winds_palette_segments(70),
                _ => winds_palette_segments(60),
            },
            extend: ExtendMode::Both,
            mask_below: Some(match filled_selector.vertical {
                VerticalSelector::IsobaricHpa(200) | VerticalSelector::IsobaricHpa(250) => 50.0,
                VerticalSelector::IsobaricHpa(300)
                | VerticalSelector::IsobaricHpa(500)
                | VerticalSelector::IsobaricHpa(700)
                | VerticalSelector::IsobaricHpa(850)
                | VerticalSelector::IsobaricHpa(925) => 20.0,
                _ => 10.0,
            }),
        },
        RenderStyle::WeatherWindGust | RenderStyle::WeatherWinds => {
            wind_speed_scale_for_selector(filled_selector)
        }
        RenderStyle::WeatherUh => DiscreteColorScale {
            levels: {
                let mut levels = range_step(0.0, 200.0, 5.0);
                levels.extend(range_step(200.0, 401.0, 10.0).into_iter().skip(1));
                levels
            },
            colors: weather_palette(WeatherPalette::Uh),
            extend: ExtendMode::Both,
            mask_below: Some(0.0),
        },
        RenderStyle::WeatherCloudCover => cloud_cover_scale(),
        RenderStyle::WeatherPrecipitableWater => precipitable_water_inches_scale(),
        RenderStyle::WeatherQpf => crate::qpf::qpf_inches_scale(),
        RenderStyle::WeatherCategorical => DiscreteColorScale {
            levels: vec![0.0, 0.5, 1.0],
            colors: vec![
                Color::rgba(242, 242, 242, 255),
                Color::rgba(216, 34, 34, 255),
            ],
            extend: ExtendMode::Neither,
            mask_below: Some(0.5),
        },
        RenderStyle::WeatherVisibility => DiscreteColorScale {
            levels: range_step(0.0, 10.5, 0.5),
            colors: weather_palette(WeatherPalette::MlMetric),
            extend: ExtendMode::Both,
            mask_below: None,
        },
        RenderStyle::WeatherSatellite => DiscreteColorScale {
            levels: range_step(170.0, 321.0, 2.0),
            colors: weather_palette(WeatherPalette::SimIr),
            extend: ExtendMode::Both,
            mask_below: None,
        },
        RenderStyle::WeatherLightning => DiscreteColorScale {
            levels: range_step(0.0, 20.5, 0.5),
            colors: weather_palette(WeatherPalette::Uh),
            extend: ExtendMode::Max,
            mask_below: Some(0.5),
        },
        _ => DiscreteColorScale {
            levels: range_step(-50.0, 5.0, 1.0),
            colors: weather_palette(WeatherPalette::Temperature),
            extend: ExtendMode::Both,
            mask_below: None,
        },
    };
    ColorScale::Discrete(discrete)
}

pub fn operational_contour_layer_for_values(
    selector: FieldSelector,
    values: &[f32],
) -> Option<ContourLayer> {
    let data = if selector.field == CanonicalField::GeopotentialHeight {
        values.iter().map(|value| value * 0.1).collect()
    } else if selector.field == CanonicalField::PressureReducedToMeanSeaLevel {
        values.iter().map(|value| value * 0.01).collect()
    } else {
        values.to_vec()
    };
    let (levels, color, width, labels, major_every, major_width, show_extrema) = match selector {
        FieldSelector {
            field: CanonicalField::GeopotentialHeight,
            vertical: rustwx_core::VerticalSelector::IsobaricHpa(200),
            ..
        } => operational_height_contour_policy(range_step(1020.0, 1321.0, 6.0)),
        FieldSelector {
            field: CanonicalField::GeopotentialHeight,
            vertical: rustwx_core::VerticalSelector::IsobaricHpa(300),
            ..
        } => operational_height_contour_policy(range_step(700.0, 1101.0, 6.0)),
        FieldSelector {
            field: CanonicalField::GeopotentialHeight,
            vertical: rustwx_core::VerticalSelector::IsobaricHpa(250),
            ..
        } => operational_height_contour_policy(range_step(900.0, 1201.0, 6.0)),
        FieldSelector {
            field: CanonicalField::GeopotentialHeight,
            vertical: rustwx_core::VerticalSelector::IsobaricHpa(500),
            ..
        } => operational_height_contour_policy(range_step(450.0, 651.0, 6.0)),
        FieldSelector {
            field: CanonicalField::GeopotentialHeight,
            vertical: rustwx_core::VerticalSelector::IsobaricHpa(700),
            ..
        } => operational_height_contour_policy(range_step(100.0, 401.0, 6.0)),
        FieldSelector {
            field: CanonicalField::GeopotentialHeight,
            vertical: rustwx_core::VerticalSelector::IsobaricHpa(850),
            ..
        } => operational_height_contour_policy(range_step(0.0, 201.0, 6.0)),
        FieldSelector {
            field: CanonicalField::PressureReducedToMeanSeaLevel,
            ..
        } => operational_pressure_contour_policy(range_step(960.0, 1045.0, 2.0)),
        FieldSelector {
            field: CanonicalField::UpdraftHelicity,
            vertical:
                rustwx_core::VerticalSelector::HeightAboveGroundLayerMeters {
                    bottom_m: 2000,
                    top_m: 5000,
                },
            ..
        } => (vec![75.0], Color::BLACK, 1, false, None, None, false),
        _ => (
            range_step(0.0, 200.0, 10.0),
            Color::BLACK,
            1,
            true,
            Some(2),
            Some(2),
            false,
        ),
    };

    Some(ContourLayer {
        data,
        levels,
        color,
        width,
        labels,
        show_extrema,
        pattern: ContourLinePattern::Solid,
        major_every,
        major_width,
    })
}

pub fn operational_wind_streamline_style(stride_x: usize, stride_y: usize) -> WindStreamlineStyle {
    WindStreamlineStyle {
        stride_x: stride_x.max(1),
        stride_y: stride_y.max(1),
        color: Color::rgba(18, 24, 32, 72),
        width: 1,
        max_steps: 14,
        step_cells: 0.80,
        min_speed: 2.5,
    }
}

fn operational_height_contour_policy(
    levels: Vec<f64>,
) -> (Vec<f64>, Color, u32, bool, Option<usize>, Option<u32>, bool) {
    (
        levels,
        Color::rgba(0, 0, 0, 220),
        1,
        true,
        Some(2),
        Some(2),
        false,
    )
}

fn operational_pressure_contour_policy(
    levels: Vec<f64>,
) -> (Vec<f64>, Color, u32, bool, Option<usize>, Option<u32>, bool) {
    (levels, Color::BLACK, 1, true, Some(2), Some(2), true)
}

/// The catalog fill scale for MSLP values in hPa (the `WeatherPressure`
/// recipe arm, e.g. the HREF MSLP mean fill). NOTE: the production
/// `mslp_10m_winds` plot fills the companion 10 m wind speed and only
/// CONTOURS mslp at 2 hPa — no single-model production colorbar shows
/// these pressure values, which is why the store viewer resolver keeps the
/// generic ramp for the stored `mslp` plane instead of this scale.
pub(crate) fn mslp_pressure_fill_scale() -> DiscreteColorScale {
    DiscreteColorScale {
        levels: range_step(960.0, 1045.0, 2.0),
        colors: weather_palette(WeatherPalette::Winds),
        extend: ExtendMode::Both,
        mask_below: None,
    }
}

pub(crate) fn reflectivity_dbz_scale() -> DiscreteColorScale {
    DiscreteColorScale {
        levels: vec![
            10.0, 15.0, 20.0, 25.0, 30.0, 35.0, 40.0, 45.0, 50.0, 55.0, 60.0, 65.0, 70.0,
        ],
        colors: vec![
            Color::rgba(242, 246, 252, 255),
            Color::rgba(150, 183, 232, 255),
            Color::rgba(55, 105, 195, 255),
            Color::rgba(20, 94, 133, 255),
            Color::rgba(45, 126, 76, 255),
            Color::rgba(132, 169, 80, 255),
            Color::rgba(246, 226, 82, 255),
            Color::rgba(237, 143, 42, 255),
            Color::rgba(211, 32, 28, 255),
            Color::rgba(147, 5, 21, 255),
            Color::rgba(132, 34, 157, 255),
            Color::rgba(178, 178, 178, 255),
        ],
        extend: ExtendMode::Max,
        mask_below: Some(10.0),
    }
}

fn wind_speed_scale_for_selector(selector: FieldSelector) -> DiscreteColorScale {
    let levels = match selector.vertical {
        VerticalSelector::IsobaricHpa(200) | VerticalSelector::IsobaricHpa(250) => {
            range_step(50.0, 170.0, 5.0)
        }
        VerticalSelector::IsobaricHpa(500) => range_step(20.0, 140.0, 5.0),
        VerticalSelector::IsobaricHpa(700)
        | VerticalSelector::IsobaricHpa(850)
        | VerticalSelector::IsobaricHpa(925) => range_step(20.0, 80.0, 5.0),
        VerticalSelector::HeightAboveGroundMeters(10) => range_step(10.0, 60.0, 5.0),
        _ => range_step(10.0, 80.0, 5.0),
    };
    DiscreteColorScale {
        levels,
        colors: winds_palette_segments(90),
        extend: ExtendMode::Max,
        mask_below: Some(match selector.vertical {
            VerticalSelector::IsobaricHpa(200) | VerticalSelector::IsobaricHpa(250) => 50.0,
            VerticalSelector::IsobaricHpa(500)
            | VerticalSelector::IsobaricHpa(700)
            | VerticalSelector::IsobaricHpa(850)
            | VerticalSelector::IsobaricHpa(925) => 20.0,
            VerticalSelector::HeightAboveGroundMeters(10) => 10.0,
            _ => 10.0,
        }),
    }
}

fn ten_meter_wind_speed_scale() -> DiscreteColorScale {
    DiscreteColorScale {
        levels: range_step(10.0, 60.0, 5.0),
        colors: winds_palette_segments(60),
        extend: ExtendMode::Max,
        mask_below: Some(10.0),
    }
}

fn dewpoint_scale_for_selector(selector: FieldSelector) -> DiscreteColorScale {
    match selector.vertical {
        VerticalSelector::HeightAboveGroundMeters(2) => {
            let levels = range_step(-40.0, 90.0, 1.0);
            DiscreteColorScale {
                colors: surface_dewpoint_colors(),
                levels,
                extend: ExtendMode::Both,
                mask_below: None,
            }
        }
        VerticalSelector::IsobaricHpa(_) => {
            let levels = range_step(-40.0, 31.0, 1.0);
            DiscreteColorScale {
                colors: dewpoint_palette_celsius_for_levels(&levels),
                levels,
                extend: ExtendMode::Both,
                mask_below: None,
            }
        }
        _ => {
            let levels = range_step(-40.0, 90.0, 1.0);
            DiscreteColorScale {
                colors: surface_dewpoint_colors(),
                levels,
                extend: ExtendMode::Both,
                mask_below: None,
            }
        }
    }
}

fn relative_humidity_scale_for_selector(selector: FieldSelector) -> DiscreteColorScale {
    match selector.vertical {
        VerticalSelector::HeightAboveGroundMeters(2) => DiscreteColorScale {
            levels: range_step(0.0, 100.0, 5.0),
            colors: surface_relative_humidity_colors(),
            extend: ExtendMode::Max,
            mask_below: None,
        },
        _ => DiscreteColorScale {
            levels: range_step(0.0, 101.0, 1.0),
            colors: weather_palette(WeatherPalette::Rh),
            extend: ExtendMode::Both,
            mask_below: None,
        },
    }
}

fn surface_dewpoint_colors() -> Vec<Color> {
    let mut colors = weather_palette(WeatherPalette::Dewpoint);
    if colors.len() <= 1 {
        return colors;
    }

    colors.remove(0);
    if let Some(last) = colors.last().copied() {
        colors.push(last);
    }
    colors
}

fn surface_relative_humidity_colors() -> Vec<Color> {
    vec![
        Color::rgba(140, 45, 4, 255),
        Color::rgba(204, 76, 2, 255),
        Color::rgba(236, 112, 20, 255),
        Color::rgba(254, 153, 41, 255),
        Color::rgba(254, 196, 79, 255),
        Color::rgba(255, 247, 188, 255),
        Color::rgba(224, 243, 219, 255),
        Color::rgba(168, 221, 181, 255),
        Color::rgba(67, 162, 202, 255),
        Color::rgba(8, 104, 172, 255),
    ]
}

pub(crate) fn cloud_cover_scale() -> DiscreteColorScale {
    DiscreteColorScale {
        levels: range_step(10.0, 100.0, 10.0),
        colors: vec![
            Color::rgba(255, 255, 255, 255),
            Color::rgba(222, 222, 222, 255),
            Color::rgba(178, 178, 178, 255),
            Color::rgba(128, 128, 128, 255),
            Color::rgba(70, 80, 100, 255),
            Color::rgba(35, 68, 122, 255),
            Color::rgba(38, 111, 166, 255),
            Color::rgba(103, 177, 209, 255),
            Color::rgba(189, 232, 241, 255),
        ],
        extend: ExtendMode::Both,
        mask_below: None,
    }
}

fn precipitable_water_inches_scale() -> DiscreteColorScale {
    DiscreteColorScale {
        levels: vec![
            0.25, 0.50, 0.75, 1.00, 1.25, 1.50, 1.75, 2.00, 2.25, 2.50, 2.75, 3.00,
        ],
        colors: vec![
            Color::rgba(70, 55, 44, 255),
            Color::rgba(118, 108, 94, 255),
            Color::rgba(213, 211, 189, 255),
            Color::rgba(183, 224, 175, 255),
            Color::rgba(105, 191, 105, 255),
            Color::rgba(32, 137, 67, 255),
            Color::rgba(16, 111, 101, 255),
            Color::rgba(39, 124, 158, 255),
            Color::rgba(63, 95, 168, 255),
            Color::rgba(116, 74, 165, 255),
            Color::rgba(191, 127, 177, 255),
        ],
        extend: ExtendMode::Both,
        mask_below: None,
    }
}

/// Smoke palette: ten stops, every one a different HUE.
///
/// Two rounds of getting this wrong, both worth recording. The original low end
/// was alpha 42/78 — over a light basemap that is nearly invisible, so the thin
/// leading edge of a plume did not read at all. Raising the alpha alone then made
/// it worse: stops 1 and 2 were both light blue (82,185,226) and (84,210,238), so
/// a wider, more opaque low end painted a big flat BLUE BLANKET over half the
/// map with no gradation in it.
///
/// Opacity was never the problem — hue resolution was. The low half now moves
/// pale steel → blue → cyan-teal → green, so light haze, noticeable smoke and
/// unhealthy smoke are different COLORS rather than shades of one, and the top
/// half keeps the familiar amber → orange → red → purple escalation.
pub(crate) fn smoke_scale_colors() -> Vec<Color> {
    vec![
        Color::rgba(170, 212, 238, 105),
        Color::rgba(96, 178, 232, 148),
        Color::rgba(56, 196, 206, 182),
        Color::rgba(96, 214, 130, 205),
        Color::rgba(196, 228, 74, 222),
        Color::rgba(252, 205, 52, 234),
        Color::rgba(248, 141, 35, 242),
        Color::rgba(232, 58, 40, 248),
        Color::rgba(158, 12, 128, 252),
        Color::rgba(74, 0, 112, 255),
    ]
}

/// EPA PM2.5 AQI category boundaries, µg/m³, **2024 revision**.
///
/// Requested by an NWS forecaster: "apply the EPA thresholds to that color scale
/// so I can tell what I'm looking at". These are the numbers on the AirNow map
/// open next to ours. The 2024 PM NAAQS reconsideration moved Good/Moderate from
/// 12.0 down to 9.0 (effective May 2024), so a scale built on the older table
/// would disagree with every other AQI product a met has in front of them.
///
/// EPA publishes each category's UPPER edge (9.0, 35.4, …) because it reports
/// concentrations rounded to 0.1 µg/m³, which leaves 9.0–9.1 undefined. A model
/// field is continuous and has no such gap, so here a boundary is the number
/// itself and the bands are half-open: below 9.0 is Good, 9.0 and up is
/// Moderate. Everything in this file depends on that convention.
const EPA_PM25_BOUNDARIES: [f64; 5] = [9.0, 35.4, 55.4, 125.4, 225.4];

/// Where the surface-smoke ladder starts drawing. Below this is transparent.
///
/// Kept at the value the earlier empirical tuning landed on: light smoke is the
/// difference between a clear day and a hazy one, and masking at 10 erased the
/// whole leading edge of a plume. It sits INSIDE the Good band, so the visible
/// Good class is 2–9, not 0–9 — otherwise every clean pixel in the domain would
/// be painted EPA green.
const EPA_PM25_FLOOR: f64 = 2.0;

/// Top of the ladder: EPA's AQI-400 break.
///
/// Above this everything is the deepest maroon anyway (EPA colors AQI 301–400
/// and 401–500 identically), so carrying the ladder higher buys no information
/// and costs real width — the colorbar is drawn LINEAR in concentration, so a
/// top of 500 would spend over half the bar on air that is essentially never
/// observed and squeeze Good/Moderate/Sensitive into a tenth of it. 325.4 is
/// both a published break and close to the 250 the old empirical ceiling found.
const EPA_PM25_LADDER_TOP: f64 = 325.4;

/// Band edges, lowest first, closed with [`EPA_PM25_LADDER_TOP`].
fn epa_pm25_band_edges() -> [f64; 7] {
    let [b1, b2, b3, b4, b5] = EPA_PM25_BOUNDARIES;
    [0.0, b1, b2, b3, b4, b5, EPA_PM25_LADDER_TOP]
}

/// The official AirNow AQI category colors, Good → Hazardous.
///
/// **Not our choices, and that is the point.** A forecaster who has read ten
/// thousand AQI maps knows what orange means without consulting a legend, so
/// matching EPA's published RGB exactly is worth more than any palette we could
/// tune. Alpha is ours: these sit over a terrain basemap that EPA's flat
/// category maps do not have, and they ramp up with severity so a thin veil of
/// Good smoke lets the ground show through while a Hazardous core does not.
fn epa_pm25_category_colors() -> [Color; 6] {
    [
        Color::rgba(0, 228, 0, 132),
        Color::rgba(255, 255, 0, 178),
        Color::rgba(255, 126, 0, 208),
        Color::rgba(255, 0, 0, 230),
        Color::rgba(143, 63, 151, 245),
        Color::rgba(126, 0, 35, 255),
    ]
}

/// Deepen a band's color as concentration rises through it.
///
/// Within-band shading is what keeps a plume from going flat: a 60 and a 120 are
/// both Unhealthy, but they are not the same air. Two constraints shaped it.
///
/// **It must not drift toward a neighbouring anchor.** The first attempt mixed
/// 22% white into each band's foot, which DESATURATES — and on a dark anchor that
/// travels a long way. The foot of Hazardous came out (154, 56, 83), measurably
/// closer to the Very Unhealthy purple (143, 63, 151) than to its own maroon;
/// `every_epa_threshold_changes_category_exactly_where_it_should` caught it.
/// Scaling brightness leaves the channel RATIOS alone, so a hue cannot wander
/// into its neighbour's.
///
/// **It must stay quieter than the hue step between bands**, or it becomes a
/// second signal saying the same thing badly — a reader cannot tell "darker
/// orange" from "a new category" at a glance. Hence darkening ONLY, which also
/// means the lightest shade in every band is EPA's exact published color: the one
/// a forecaster is matching against the AirNow legend.
fn epa_pm25_shade_within_band(anchor: Color, position: f64) -> Color {
    /// Brightness at the top of a band, relative to its anchor.
    ///
    /// Bounded below by the SAME requirement, and not by taste: darken yellow
    /// far enough and it becomes olive, which is nearer EPA's orange than its own
    /// yellow. Solving that for the Moderate/Sensitive pair — the tightest of the
    /// six, since yellow sits between orange and nothing — gives a floor of
    /// 0.747. 0.80 clears it with margin and still moves 255 to 204, which is a
    /// visible amount of structure.
    const DEEPEST: f64 = 0.80;
    let position = position.clamp(0.0, 1.0);
    let factor = 1.0 - (1.0 - DEEPEST) * position;
    let channel = |value: u8| (f64::from(value) * factor).round().clamp(0.0, 255.0) as u8;
    Color::rgba(
        channel(anchor.r),
        channel(anchor.g),
        channel(anchor.b),
        anchor.a,
    )
}

/// The EPA category color for one concentration, shaded by depth into its band.
fn epa_pm25_color_at(value: f64) -> Color {
    let edges = epa_pm25_band_edges();
    // Half-open bands: a value AT a boundary belongs to the band ABOVE it.
    let band = EPA_PM25_BOUNDARIES
        .iter()
        .filter(|boundary| value >= **boundary)
        .count();
    let anchor = epa_pm25_category_colors()[band];
    let (lo, hi) = (edges[band], edges[band + 1]);
    let position = if hi > lo { (value - lo) / (hi - lo) } else { 0.0 };
    epa_pm25_shade_within_band(anchor, position)
}

/// Palette entries used to express the category step function.
///
/// The renderer picks a bin's color by NUMERIC POSITION across the level span —
/// `floor(t * palette.len())` — not by bin index (see
/// `rustwx_render::colormap::sample_palette_for_levels`). So a palette listed one
/// color per band does NOT put one color per band on a nonlinear ladder; it
/// smears them across it, and Good would share a color with Moderate. Expressing
/// the step function as a fine lookup table over the same numeric axis makes
/// every bin land on its own category color however the levels are spaced.
const EPA_PM25_PALETTE_RESOLUTION: usize = 4096;

/// The category step function as a numeric-position lookup table over
/// `lo..=hi`.
fn epa_pm25_palette(lo: f64, hi: f64) -> Vec<Color> {
    let span = hi - lo;
    (0..EPA_PM25_PALETTE_RESOLUTION)
        .map(|index| {
            // Each cell is classified by its UPPER edge, which is what puts a
            // threshold exactly on a bin edge. A bin whose lower edge IS a
            // boundary lands in the cell straddling that boundary, and that
            // cell's upper edge is strictly above the boundary — so the bin gets
            // the band above the threshold, per the half-open convention.
            // Classifying by the centre or the lower edge instead colors the
            // 9.0 bin Good, and the transition the met asked for lands one bin
            // late.
            let upper = lo + span * (index + 1) as f64 / EPA_PM25_PALETTE_RESOLUTION as f64;
            epa_pm25_color_at(upper)
        })
        .collect()
}

/// The surface-smoke level ladder: every EPA boundary an exact bin edge, with
/// sub-steps inside each band.
///
/// The boundaries MUST be exact edges. A bin straddling 55.4 would draw the
/// Unhealthy transition wherever that bin happened to start, which is the one
/// thing this scale exists to get right.
fn epa_pm25_levels() -> Vec<f64> {
    /// Bins per band. Three is enough for a plume to show structure and few
    /// enough that the hue steps stay the loudest thing on the map.
    const SUB_STEPS: usize = 3;
    let edges = epa_pm25_band_edges();
    let mut levels = vec![EPA_PM25_FLOOR];
    for pair in edges.windows(2) {
        let (lo, hi) = (pair[0].max(EPA_PM25_FLOOR), pair[1]);
        if hi <= lo {
            continue;
        }
        for step in 1..=SUB_STEPS {
            if step == SUB_STEPS {
                // The band edge goes in VERBATIM. A geometric round-trip lands
                // a hair off 35.4, and "a hair off" is a threshold in the wrong
                // place.
                levels.push(hi);
            } else {
                // Geometric inside the band: smoke is heavy-tailed, so equal
                // ratios read more evenly than equal differences across a band
                // as wide as 55.4–125.4.
                let t = step as f64 / SUB_STEPS as f64;
                let value = lo * (hi / lo).powf(t);
                levels.push((value * 10.0).round() / 10.0);
            }
        }
    }
    levels.dedup();
    levels
}

/// The near-surface smoke scale: EPA PM2.5 AQI categories.
///
/// Shared with the windowed lane so the 0–24/24–48/0–48 h maxima cannot drift
/// away from the hourly map the way the hand-copied geometric ladders once did.
pub(crate) fn epa_pm25_surface_scale() -> DiscreteColorScale {
    DiscreteColorScale {
        levels: epa_pm25_levels(),
        colors: epa_pm25_palette(EPA_PM25_FLOOR, EPA_PM25_LADDER_TOP),
        extend: ExtendMode::Max,
        mask_below: Some(EPA_PM25_FLOOR),
    }
}

/// Colorbar ticks for the EPA smoke scale: the thresholds, and nothing else.
///
/// Evenly spaced ticks on this ladder would label arbitrary concentrations and
/// leave the reader to guess where a category changed. These are the only
/// numbers on the bar that mean anything.
pub(crate) fn epa_pm25_colorbar_ticks() -> Vec<f64> {
    let mut ticks = vec![EPA_PM25_FLOOR];
    ticks.extend_from_slice(&EPA_PM25_BOUNDARIES);
    ticks
}

/// Geometric (log-spaced) level ladder — fine steps for heavy-tailed fields
/// like smoke, so the palette lerps into a fluid gradient instead of a few
/// hard doublings. `factor` ~1.1 gives a smooth ramp.
pub(crate) fn geometric_levels(start: f64, factor: f64, max: f64) -> Vec<f64> {
    let mut out = Vec::new();
    let mut value = start;
    while value <= max + 1e-6 {
        out.push((value * 10.0).round() / 10.0);
        value *= factor;
    }
    out
}

fn range_step(start: f64, stop: f64, step: f64) -> Vec<f64> {
    let mut out = Vec::new();
    let mut value = start;
    while value <= stop + 1e-9 {
        out.push(value);
        value += step;
    }
    out
}

fn normalize_longitude_for_bounds(lon: f64) -> f64 {
    let mut lon = lon % 360.0;
    if lon > 180.0 {
        lon -= 360.0;
    } else if lon <= -180.0 {
        lon += 360.0;
    }
    lon
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::{CanonicalField, Field2D, FieldSelector, GridShape, LatLonGrid, ProductKey};
    use rustwx_render::{ColorScale, DiscreteColorScale, ExtendMode};

    fn sample_request() -> MapRenderRequest {
        let shape = GridShape::new(2, 2).unwrap();
        let grid = LatLonGrid::new(
            shape,
            vec![35.0, 35.0, 36.0, 36.0],
            vec![-100.0, -99.0, -100.0, -99.0],
        )
        .unwrap();
        let field = Field2D::new(
            ProductKey::named("sample"),
            "unit",
            grid,
            vec![0.0, 1.0, 2.0, 3.0],
        )
        .unwrap();
        MapRenderRequest::new(
            field.into(),
            ColorScale::Discrete(DiscreteColorScale {
                levels: vec![0.0, 1.0, 2.0, 3.0],
                colors: vec![
                    rustwx_render::Color::rgba(0, 0, 255, 255),
                    rustwx_render::Color::rgba(255, 0, 0, 255),
                ],
                extend: ExtendMode::Neither,
                mask_below: None,
            }),
        )
    }

    #[test]
    fn regional_static_design_uses_projected_grid_frame_and_smooth_legend() {
        let mut request = sample_request();

        StaticPlotDesign::new(
            (-125.0, -66.0, 24.0, 50.0),
            ProductVisualMode::FilledMeteorology,
        )
        .apply_to_request(&mut request);

        assert_eq!(request.visual_mode, ProductVisualMode::FilledMeteorology);
        assert_eq!(
            request.domain_frame.map(|frame| frame.source),
            Some(DomainFrameSource::ProjectedGrid)
        );
        assert_eq!(request.legend.mode, LegendMode::SmoothRamp);
        assert_eq!(request.render_density.fill, high_detail_fill_density());
        assert_eq!(request.render_density.palette_multiplier, 4);
    }

    #[test]
    fn straight_west_static_design_uses_viewport_frame() {
        let mut request = sample_request();

        StaticPlotDesign::new(
            (-124.9, -113.8, 31.9, 42.5),
            ProductVisualMode::FilledMeteorology,
        )
        .apply_to_request(&mut request);

        assert_eq!(
            request.domain_frame.map(|frame| frame.source),
            Some(DomainFrameSource::MapViewport)
        );

        let mut west_request = sample_request();
        StaticPlotDesign::new(
            (-125.7, -110.5, 30.5, 49.0),
            ProductVisualMode::FilledMeteorology,
        )
        .apply_to_request(&mut west_request);
        assert_eq!(
            west_request.domain_frame.map(|frame| frame.source),
            Some(DomainFrameSource::MapViewport)
        );
    }

    #[test]
    fn rockies_static_design_keeps_projected_grid_frame() {
        let mut request = sample_request();

        StaticPlotDesign::new(
            (-112.0, -96.0, 37.0, 49.5),
            ProductVisualMode::FilledMeteorology,
        )
        .apply_to_request(&mut request);

        assert_eq!(
            request.domain_frame.map(|frame| frame.source),
            Some(DomainFrameSource::ProjectedGrid)
        );
    }

    #[test]
    fn global_filled_static_design_uses_smooth_legend_without_viewport_frame() {
        let mut request = sample_request();

        apply_static_map_design(
            &mut request,
            (-180.0, 179.999, -90.0, 90.0),
            ProductVisualMode::FilledMeteorology,
            false,
        );

        assert!(request.domain_frame.is_none());
        assert_eq!(request.legend.mode, LegendMode::SmoothRamp);
        assert_eq!(request.render_density, RenderDensity::default());
    }

    #[test]
    fn global_overlay_static_design_keeps_stepped_legend_policy() {
        let mut request = sample_request();

        apply_static_map_design(
            &mut request,
            (-180.0, 179.999, -90.0, 90.0),
            ProductVisualMode::OverlayAnalysis,
            true,
        );

        assert!(request.domain_frame.is_none());
        assert_eq!(request.legend.mode, LegendMode::SmoothRamp);
        assert_eq!(request.render_density.fill, high_detail_fill_density());
        assert_eq!(request.render_density.palette_multiplier, 4);
    }

    #[test]
    fn operational_pressure_contours_convert_units_and_mark_extrema() {
        let layer = operational_contour_layer_for_values(
            FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel),
            &[100000.0, 100200.0, 100400.0, 100600.0],
        )
        .expect("pressure contour layer");

        assert_eq!(layer.data[0], 1000.0);
        assert_eq!(layer.levels.first().copied(), Some(960.0));
        assert_eq!(layer.width, 1);
        assert_eq!(layer.major_every, Some(2));
        assert_eq!(layer.major_width, Some(2));
        assert_eq!(layer.pattern, ContourLinePattern::Solid);
        assert!(layer.labels);
        assert!(layer.show_extrema);
    }

    #[test]
    fn operational_height_contours_convert_to_decameters_without_extrema() {
        let layer = operational_contour_layer_for_values(
            FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 500),
            &[5400.0, 5460.0, 5520.0, 5580.0],
        )
        .expect("height contour layer");

        assert_eq!(layer.data[0], 540.0);
        assert_eq!(layer.levels.first().copied(), Some(450.0));
        assert_eq!(layer.levels.get(1).copied(), Some(456.0));
        assert_eq!(layer.color, Color::rgba(0, 0, 0, 220));
        assert_eq!(layer.major_every, Some(2));
        assert_eq!(layer.major_width, Some(2));
        assert!(layer.labels);
        assert!(!layer.show_extrema);
    }

    #[test]
    fn operational_wind_streamlines_are_subtle_dense_flow_texture() {
        let style = operational_wind_streamline_style(9, 7);

        assert_eq!(style.stride_x, 9);
        assert_eq!(style.stride_y, 7);
        assert_eq!(style.width, 1);
        assert!(style.color.a < 160);
        assert!(style.max_steps >= 12);
        assert!(style.step_cells > 0.0);
    }

    #[test]
    fn operational_fill_scale_masks_sparse_signal_products() {
        let reflectivity = rustwx_models::plot_recipe("composite_reflectivity").unwrap();
        let ColorScale::Discrete(reflectivity_scale) = operational_fill_scale_for_recipe(
            reflectivity,
            FieldSelector::surface(CanonicalField::CompositeReflectivity),
        ) else {
            panic!("expected reflectivity discrete scale");
        };
        assert_eq!(reflectivity_scale.levels.first().copied(), Some(10.0));
        assert_eq!(reflectivity_scale.levels.last().copied(), Some(70.0));
        assert_eq!(reflectivity_scale.extend, ExtendMode::Max);
        assert_eq!(reflectivity_scale.mask_below, Some(10.0));

        let mslp_winds = rustwx_models::plot_recipe("mslp_10m_winds").unwrap();
        let ColorScale::Discrete(mslp_wind_scale) = operational_fill_scale_for_recipe(
            mslp_winds,
            FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel),
        ) else {
            panic!("expected MSLP/10m wind discrete scale");
        };
        assert_eq!(mslp_wind_scale.levels.first().copied(), Some(10.0));
        assert_eq!(mslp_wind_scale.mask_below, Some(10.0));

        let qpf = rustwx_models::plot_recipe("1h_qpf").unwrap();
        let ColorScale::Discrete(qpf_scale) = operational_fill_scale_for_recipe(
            qpf,
            FieldSelector::surface(CanonicalField::TotalPrecipitation),
        ) else {
            panic!("expected QPF discrete scale");
        };
        assert_eq!(qpf_scale.mask_below, Some(0.01));

        let categorical = rustwx_models::plot_recipe("categorical_snow").unwrap();
        let ColorScale::Discrete(categorical_scale) = operational_fill_scale_for_recipe(
            categorical,
            FieldSelector::surface(CanonicalField::CategoricalSnow),
        ) else {
            panic!("expected categorical discrete scale");
        };
        assert_eq!(categorical_scale.extend, ExtendMode::Neither);
        assert_eq!(categorical_scale.mask_below, Some(0.5));

        let surface_smoke = rustwx_models::plot_recipe("smoke_pm25_native").unwrap();
        let ColorScale::Discrete(surface_smoke_scale) = operational_fill_scale_for_recipe(
            surface_smoke,
            FieldSelector::height_agl(CanonicalField::SmokeMassDensity, 8),
        ) else {
            panic!("expected surface smoke discrete scale");
        };
        // Light smoke must be BOTH unmasked and actually visible: a 2 ug/m^3
        // floor, and a first stop opaque enough to see over a light basemap.
        assert_eq!(surface_smoke_scale.levels.first().copied(), Some(2.0));
        assert_eq!(surface_smoke_scale.mask_below, Some(2.0));
        assert!(
            surface_smoke_scale.colors[0].a >= 100,
            "the faintest smoke stop is invisible again: alpha {}",
            surface_smoke_scale.colors[0].a
        );

        let column_smoke = rustwx_models::plot_recipe("smoke_column").unwrap();
        let ColorScale::Discrete(column_smoke_scale) = operational_fill_scale_for_recipe(
            column_smoke,
            FieldSelector::entire_atmosphere(CanonicalField::ColumnIntegratedSmoke),
        ) else {
            panic!("expected column smoke discrete scale");
        };
        assert_eq!(column_smoke_scale.levels.first().copied(), Some(20.0));
        assert_eq!(column_smoke_scale.mask_below, Some(20.0));
        // The ramp has to reach a real plume core; 720 saturated at 1500.
        assert!(
            column_smoke_scale.levels.last().copied().unwrap_or(0.0) >= 1400.0,
            "column ramp tops out at {:?} — dense plumes will flatten",
            column_smoke_scale.levels.last()
        );
        assert!(column_smoke_scale.colors[0].a >= 100);
    }

    /// Which EPA category a rendered color belongs to, by nearest published
    /// anchor.
    ///
    /// Deliberately does NOT compare against `epa_pm25_color_at` — that would
    /// only prove the palette agrees with itself. Nearest-anchor asks the
    /// question a reader asks ("what color is that?"), and it survives the
    /// within-band shading: the light and dark ends of a band both stay far
    /// closer to their own anchor than to any neighbour.
    fn epa_category_of(color: rustwx_render::Rgba) -> usize {
        epa_pm25_category_colors()
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                let distance = |c: &Color| {
                    let d = |x: u8, y: u8| (f64::from(x) - f64::from(y)).powi(2);
                    d(c.r, color.r) + d(c.g, color.g) + d(c.b, color.b)
                };
                distance(a).total_cmp(&distance(b))
            })
            .map(|(index, _)| index)
            .expect("six anchors")
    }

    /// The whole point of the scale: crossing an EPA threshold must change the
    /// category, ON THE RIGHT SIDE of the number.
    ///
    /// Asserted through `LeveledColormap::map`, which is the exact path the
    /// renderer colors pixels with — including the numeric-position palette
    /// sampling that makes a naive one-color-per-band palette wrong. A unit test
    /// of the level ladder alone would have passed while the map drew Good smoke
    /// in Moderate yellow.
    #[test]
    fn every_epa_threshold_changes_category_exactly_where_it_should() {
        let scale = epa_pm25_surface_scale();
        let cmap = rustwx_render::build_colormap(
            &ColorScale::Discrete(scale.clone()),
            rustwx_render::ColormapBuildOptions::default(),
        );
        for (index, boundary) in EPA_PM25_BOUNDARIES.iter().enumerate() {
            // Half-open bands: AT the boundary is the category ABOVE it.
            let above = epa_category_of(cmap.map(*boundary));
            let below = epa_category_of(cmap.map(boundary - 0.05));
            assert_eq!(
                above,
                index + 1,
                "{boundary} ug/m^3 should read as category {} but reads as {above}",
                index + 1
            );
            assert_eq!(
                below,
                index,
                "just under {boundary} should still read as category {index}, got {below}"
            );
        }
        // The extremes: a visible wisp is Good, and a core past the ladder top
        // is still Hazardous rather than wrapping or clipping to purple.
        assert_eq!(epa_category_of(cmap.map(EPA_PM25_FLOOR)), 0);
        assert_eq!(epa_category_of(cmap.map(2_000.0)), 5);
        // Below the floor draws nothing at all — a clean domain must not be
        // painted EPA green.
        assert_eq!(cmap.map(EPA_PM25_FLOOR - 0.01).a, 0);
    }

    #[test]
    fn the_epa_thresholds_are_exact_bin_edges() {
        let scale = epa_pm25_surface_scale();
        for boundary in EPA_PM25_BOUNDARIES {
            assert!(
                scale.levels.contains(&boundary),
                "{boundary} is not a bin edge, so its transition lands wherever \
                 the surrounding bin happens to start; levels: {:?}",
                scale.levels
            );
        }
        // Sub-steps inside each band, or a plume core goes flat.
        assert!(
            scale.levels.len() > EPA_PM25_BOUNDARIES.len() * 2,
            "no within-band structure: {:?}",
            scale.levels
        );
        // Monotone, and starting at the visible floor.
        assert_eq!(scale.levels.first().copied(), Some(EPA_PM25_FLOOR));
        assert!(scale.levels.windows(2).all(|pair| pair[1] > pair[0]));
        // Ticks label the thresholds and nothing else.
        assert_eq!(
            epa_pm25_colorbar_ticks(),
            vec![EPA_PM25_FLOOR, 9.0, 35.4, 55.4, 125.4, 225.4]
        );
    }

    /// Column smoke is mg/m^2 through the whole atmosphere, not what anyone
    /// breathes, so it must NOT wear AQI colors — a plume aloft would claim a
    /// health category the number cannot support.
    #[test]
    fn column_smoke_keeps_its_own_ramp() {
        let recipe = rustwx_models::plot_recipe("smoke_column").unwrap();
        let ColorScale::Discrete(column) = operational_fill_scale_for_recipe(
            recipe,
            FieldSelector::entire_atmosphere(CanonicalField::ColumnIntegratedSmoke),
        ) else {
            panic!("expected a discrete scale");
        };
        assert_eq!(column.colors, smoke_scale_colors());
        assert!(
            !column.levels.contains(&35.4),
            "column ladder picked up an AQI boundary"
        );
    }
}
