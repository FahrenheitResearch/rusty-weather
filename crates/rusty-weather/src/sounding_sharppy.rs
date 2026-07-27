//! The SPC-style sounding window (`sharppyrs`) rendered offscreen to a PNG.
//!
//! `sharppyrs` is an **egui widget** — the SHARPpy window ported to Rust, and the
//! same one BowEcho draws — so serving it from a headless HTTP service means
//! running egui with no window. `egui_kittest` is exactly that harness: it is
//! what sharppyrs' own snapshot test uses to produce its reference PNG, so this
//! module is the service-side version of that test rather than a new renderer.
//! Nothing here draws anything; all the geometry lives in sharppyrs.
//!
//! Two things this module owns:
//!
//! * **Units.** The store keeps wind as u/v in m/s; `SoundingData` wants a
//!   meteorological direction in degrees and a speed in KNOTS. Getting that
//!   wrong silently rotates every barb, so the conversion is one function with
//!   its own test.
//! * **Serialization.** Each render builds a wgpu device, which is expensive and
//!   not something to do several of at once on a shared box, so renders take a
//!   permit. The API's own render gate covers map jobs, not this path.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Window size sharppyrs is laid out for; its own snapshot renders at this size,
/// and the panel arrangement assumes roughly this aspect.
const WINDOW_W: f32 = 1630.0;
const WINDOW_H: f32 = 1100.0;

/// Our default panel arrangement, as sharppyrs layout tokens.
///
/// Both marginal box plots are dropped, which is the whole change from the
/// upstream default
/// (`...|slinky,thetae,srwinds,locationmap|convectiveindices,kinematics,ship,severeindices,streamwiseness,hidden|250`):
///
/// * **No SHIP box.**
/// * **No effective-layer STP bar** — upstream already moved off the STP default,
///   and BowEcho migrates old saves away from it.
///
/// Streamwiseness moves up to share the third column with the severe indices, and
/// the two trailing bottom columns are hidden, so the reclaimed width goes to the
/// parcel and kinematics tables. That widening is also what un-squishes the
/// NCAPE/ECAPE line.
///
/// Bottom slots in order: two full-height columns, two sharing the third column
/// vertically, then two more full-height columns.
///
/// Two things learned by rendering rather than reasoning, both worth keeping:
///
/// * The hidden slots must be the TRAILING ones. Hiding a middle slot hands its
///   space to the panel sharing its column, whose text then scales past its box
///   and overlaps into unreadable garbage.
/// * "The el stp plot is kinda usefull" meant marginal, not useful. Reading it the
///   other way put STP in a full column here for one deploy.
pub const DEFAULT_LAYOUT_TOKENS: &str = "speed,advection|hodograph|\
     slinky,thetae,srwinds,locationmap|\
     convectiveindices,kinematics,streamwiseness,severeindices,hidden,hidden|250";

/// Resolve a layout from caller tokens, falling back to our default.
///
/// Malformed tokens fall back rather than erroring: a stale shared link should
/// still draw a sounding.
pub fn layout_or_default(tokens: Option<&str>) -> sharppyrs::SoundingLayout {
    tokens
        .and_then(sharppyrs::SoundingLayout::from_tokens)
        .or_else(|| sharppyrs::SoundingLayout::from_tokens(DEFAULT_LAYOUT_TOKENS))
        .unwrap_or_default()
}

const MS_TO_KNOTS: f64 = 1.943_844;

/// Wind as `SoundingData` wants it: direction the wind blows FROM in degrees
/// clockwise from north, and speed in knots.
///
/// The meteorological convention is the one that bites: a wind blowing TOWARD
/// the north-east (u and v both positive) is a south-westerly, reported as ~225°.
/// Hence the negated components.
fn wind_dir_speed(u_ms: f64, v_ms: f64) -> (f64, f64) {
    let speed = u_ms.hypot(v_ms) * MS_TO_KNOTS;
    // Calm: direction is meaningless, and atan2(0,0) is 0 anyway — say north so
    // the barb renders as a calm circle rather than a random shaft.
    if speed < 1.0e-6 {
        return (0.0, 0.0);
    }
    let dir = (-u_ms).atan2(-v_ms).to_degrees().rem_euclid(360.0);
    (dir, speed)
}

/// Build sharppyrs' input from the profile arrays the store already gave us.
///
/// Arrays arrive surface-first with descending pressure, which is what sharprs
/// expects, so nothing is reordered here.
pub fn sounding_data(
    pressure_hpa: &[f64],
    height_m_msl: &[f64],
    temperature_c: &[f64],
    dewpoint_c: &[f64],
    u_ms: &[f64],
    v_ms: &[f64],
    latitude: f64,
    longitude: f64,
) -> sharppyrs::SoundingData {
    let mut wdir = Vec::with_capacity(u_ms.len());
    let mut wspd = Vec::with_capacity(u_ms.len());
    for (&u, &v) in u_ms.iter().zip(v_ms.iter()) {
        let (dir, speed) = wind_dir_speed(u, v);
        wdir.push(dir);
        wspd.push(speed);
    }
    sharppyrs::SoundingData {
        pres: pressure_hpa.to_vec(),
        hght: height_m_msl.to_vec(),
        tmpc: temperature_c.to_vec(),
        dwpc: dewpoint_c.to_vec(),
        wdir,
        wspd,
        // The store does not carry isobaric omega, so the omega meter stays
        // empty rather than being fed a fabricated profile.
        omeg: None,
        latitude: Some(latitude),
        longitude: Some(longitude),
        missing: None,
    }
}

/// One render at a time: a wgpu device per render is costly, and several at once
/// on a box that may be falling back to software Vulkan is worse.
fn render_permit() -> MutexGuard<'static, ()> {
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Render the SPC-style window to PNG bytes.
///
/// `title` is the single header line sharppyrs draws across the top; `brand` is
/// the small credit in its corner, following the same rule as everything else
/// here — an empty string draws none.
pub fn render_png(
    data: sharppyrs::SoundingData,
    title: &str,
    brand: &str,
    layout: sharppyrs::SoundingLayout,
) -> Result<Vec<u8>, String> {
    // Profile::new returns None when the arrays cannot make a sounding at all
    // (too few levels, non-monotonic pressure); say so rather than unwrapping.
    let profile = sharppyrs::Profile::new(data)
        .ok_or("sharppyrs rejected the profile (too few or inconsistent levels)")?;
    let derived = sharppyrs::DerivedParams::compute(&profile);

    let _permit = render_permit();
    let title = title.to_string();
    let brand = brand.to_string();
    // The first pass installs the bundled Space Grotesk face and asks for a
    // repaint; fonts must be in the context before any text is laid out, or the
    // window renders in egui's default face.
    let mut fonts_installed = false;
    // The widget reads its layout from egui memory — that is how the in-app gear
    // button edits it — so store ours under an id and hand the widget that id.
    let layout_id = egui::Id::new("rustwx_sharppy_layout");
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(WINDOW_W, WINDOW_H))
        .build_ui(move |ui| {
            if !fonts_installed {
                sharppyrs::install_fonts(ui.ctx());
                fonts_installed = true;
                ui.ctx().request_repaint();
                return;
            }
            sharppyrs::store_layout(ui.ctx(), layout_id, &layout);
            egui::Frame::new()
                .fill(egui::Color32::BLACK)
                .show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    ui.add(
                        sharppyrs::SoundingView::new(&profile, &derived)
                            .layout_memory_id(layout_id)
                            .title(&title)
                            .brand(&brand)
                            .style(sharppyrs::SkewTStyle::space_grotesk()),
                    );
                });
        });
    harness.run();

    let image = harness
        .render()
        .map_err(|err| format!("offscreen render failed ({err:?}); is a Vulkan driver present?"))?;
    let mut png: Vec<u8> = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|err| format!("encode sounding png: {err}"))?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Meteorological direction is the one thing here that can be silently
    /// wrong: every barb would point the wrong way and the hodograph would be
    /// mirrored, with no error anywhere.
    #[test]
    fn wind_converts_to_met_direction_and_knots() {
        // Blowing toward the north (v positive) is a SOUTHERLY, 180 degrees.
        let (dir, speed) = wind_dir_speed(0.0, 10.0);
        assert!((dir - 180.0).abs() < 1.0e-6, "dir {dir}");
        assert!((speed - 19.438_44).abs() < 1.0e-3, "speed {speed}");
        // Toward the east: a westerly, 270.
        let (dir, _) = wind_dir_speed(10.0, 0.0);
        assert!((dir - 270.0).abs() < 1.0e-6, "dir {dir}");
        // Toward the north-east: a south-westerly, 225.
        let (dir, _) = wind_dir_speed(5.0, 5.0);
        assert!((dir - 225.0).abs() < 1.0e-6, "dir {dir}");
        // Toward the south: a northerly, 0/360.
        let (dir, _) = wind_dir_speed(0.0, -10.0);
        assert!(dir.abs() < 1.0e-6 || (dir - 360.0).abs() < 1.0e-6, "dir {dir}");
        // Calm stays calm instead of inventing a shaft direction.
        assert_eq!(wind_dir_speed(0.0, 0.0), (0.0, 0.0));
    }

    /// The default layout is a token string, so a typo would silently fall back
    /// to the upstream default and quietly undo all three changes.
    #[test]
    fn our_default_layout_parses_and_makes_the_intended_swaps() {
        let layout = sharppyrs::SoundingLayout::from_tokens(DEFAULT_LAYOUT_TOKENS)
            .expect("default layout tokens must parse");
        for marginal in [sharppyrs::PanelKind::Ship, sharppyrs::PanelKind::Stp] {
            assert!(
                !layout.bottom.contains(&marginal) && !layout.insets.contains(&marginal),
                "{marginal:?} should be gone: bottom {:?} insets {:?}",
                layout.bottom,
                layout.insets
            );
        }
        assert_eq!(
            layout.insets[3],
            sharppyrs::PanelKind::LocationMap,
            "the location map belongs in the small inset"
        );
        // Hidden slots must be the TRAILING ones: hiding a middle slot hands its
        // space to the panel sharing its column, whose text then scales past its
        // box and overlaps.
        assert_eq!(
            [layout.bottom[4], layout.bottom[5]],
            [sharppyrs::PanelKind::Hidden, sharppyrs::PanelKind::Hidden]
        );
        assert_eq!(
            sharppyrs::SoundingLayout::from_tokens(&layout.to_tokens()),
            Some(layout)
        );
    }

    #[test]
    fn a_bad_layout_falls_back_instead_of_failing() {
        let ours = layout_or_default(None);
        assert!(!ours.bottom.contains(&sharppyrs::PanelKind::Stp));
        assert_eq!(layout_or_default(Some("not-a-layout")), ours);
        let upstream = layout_or_default(Some(
            "speed,advection|hodograph|slinky,thetae,srwinds,locationmap|convectiveindices,kinematics,ship,severeindices,streamwiseness,hidden|250",
        ));
        assert!(upstream.bottom.contains(&sharppyrs::PanelKind::Ship));
    }

    #[test]
    fn sounding_data_carries_the_arrays_through_unreordered() {
        let data = sounding_data(
            &[1000.0, 900.0, 800.0],
            &[100.0, 1000.0, 2000.0],
            &[20.0, 15.0, 10.0],
            &[15.0, 10.0, 5.0],
            &[0.0, 5.0, 10.0],
            &[10.0, 5.0, 0.0],
            38.5,
            -121.5,
        );
        assert_eq!(data.pres, vec![1000.0, 900.0, 800.0]);
        assert_eq!(data.tmpc.first().copied(), Some(20.0));
        assert_eq!(data.latitude, Some(38.5));
        assert_eq!(data.longitude, Some(-121.5));
        assert!(data.omeg.is_none(), "no fabricated omega");
        assert_eq!(data.wdir.len(), 3);
        // Surface wind blows toward the north here: a southerly.
        assert!((data.wdir[0] - 180.0).abs() < 1.0e-6);
    }
}
