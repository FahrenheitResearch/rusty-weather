//! The full SPC sounding window: skew-T + speed/advection strips, hodograph
//! with locator inset, storm slinky / theta-e / SR-winds / hazard row, and
//! the bottom index-board band — laid out like the vendored `SPCWidget` grid
//! with the SHARPpy-Reimagined modifications (see PORTING.md).

use egui::{Align2, Rect, Response, Sense, Ui, Vec2, Widget};

use crate::derived::DerivedParams;
use crate::panels;

/// What to draw in the fourth inset cell (upper-right row).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CornerPanel {
    /// Location map with the sounding point (default).
    #[default]
    LocationMap,
    /// The original SHARPpy "Psbl Haz. Type" watch box.
    HazardType,
}

/// Every swappable panel of the window. Any cell (except the skew-T) can
/// hold any of these, or be hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    Speed,
    Advection,
    Hodograph,
    Slinky,
    ThetaE,
    SrWinds,
    LocationMap,
    HazardType,
    IndexBoard,
    Ship,
    Streamwiseness,
    Stp,
    Hidden,
}

impl PanelKind {
    pub const ALL: [PanelKind; 13] = [
        PanelKind::Speed,
        PanelKind::Advection,
        PanelKind::Hodograph,
        PanelKind::Slinky,
        PanelKind::ThetaE,
        PanelKind::SrWinds,
        PanelKind::LocationMap,
        PanelKind::HazardType,
        PanelKind::IndexBoard,
        PanelKind::Ship,
        PanelKind::Streamwiseness,
        PanelKind::Stp,
        PanelKind::Hidden,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PanelKind::Speed => "Wind speed",
            PanelKind::Advection => "Temp advection",
            PanelKind::Hodograph => "Hodograph",
            PanelKind::Slinky => "Storm slinky",
            PanelKind::ThetaE => "Theta-E v. pres",
            PanelKind::SrWinds => "SR wind v. height",
            PanelKind::LocationMap => "Location map",
            PanelKind::HazardType => "Psbl haz. type",
            PanelKind::IndexBoard => "Index board",
            PanelKind::Ship => "SHIP box",
            PanelKind::Streamwiseness => "Streamwiseness",
            PanelKind::Stp => "Effective STP",
            PanelKind::Hidden => "(hidden)",
        }
    }

    /// Stable serialization token for this panel (see
    /// [`SoundingLayout::to_tokens`]). Lowercase, no separators; new
    /// variants get new tokens and existing tokens never change.
    pub fn token(self) -> &'static str {
        match self {
            PanelKind::Speed => "speed",
            PanelKind::Advection => "advection",
            PanelKind::Hodograph => "hodograph",
            PanelKind::Slinky => "slinky",
            PanelKind::ThetaE => "thetae",
            PanelKind::SrWinds => "srwinds",
            PanelKind::LocationMap => "locationmap",
            PanelKind::HazardType => "hazardtype",
            PanelKind::IndexBoard => "indexboard",
            PanelKind::Ship => "ship",
            PanelKind::Streamwiseness => "streamwiseness",
            PanelKind::Stp => "stp",
            PanelKind::Hidden => "hidden",
        }
    }

    /// Inverse of [`PanelKind::token`]; `None` for unknown tokens.
    pub fn from_token(token: &str) -> Option<PanelKind> {
        PanelKind::ALL.into_iter().find(|k| k.token() == token)
    }

    fn draw(
        self,
        painter: &egui::Painter,
        rect: Rect,
        prof: &Profile,
        dv: &DerivedParams,
        st: &SkewTStyle,
        hodo_zoom: f64,
    ) {
        match self {
            PanelKind::Speed => panels::speed::draw(painter, rect, prof, dv, st),
            PanelKind::Advection => panels::advection::draw(painter, rect, prof, dv, st),
            PanelKind::Hodograph => {
                panels::hodo::draw_zoomed(painter, rect, prof, dv, st, hodo_zoom)
            }
            PanelKind::Slinky => panels::slinky::draw(painter, rect, prof, dv, st),
            PanelKind::ThetaE => panels::thetae::draw(painter, rect, prof, dv, st),
            PanelKind::SrWinds => panels::srwinds::draw(painter, rect, prof, dv, st),
            PanelKind::LocationMap => panels::locator::draw(painter, rect, prof, dv, st),
            PanelKind::HazardType => panels::hazard::draw(painter, rect, prof, dv, st),
            PanelKind::IndexBoard => panels::index_board::draw(painter, rect, prof, dv, st),
            PanelKind::Ship => panels::ship_inset::draw(painter, rect, prof, dv, st),
            PanelKind::Streamwiseness => panels::streamwiseness::draw(painter, rect, prof, dv, st),
            PanelKind::Stp => panels::stp::draw(painter, rect, prof, dv, st),
            PanelKind::Hidden => {}
        }
    }
}

/// User-adjustable window layout: which panel lives in each cell of the SPC
/// grid (the skew-T cell is fixed), plus the hodograph zoom. Kept in egui
/// memory per widget id; edited in-app via the gear button.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundingLayout {
    /// The two narrow strips right of the skew-T.
    pub strips: [PanelKind; 2],
    /// The large upper-right cell.
    pub main: PanelKind,
    /// The four inset cells under it.
    pub insets: [PanelKind; 4],
    /// The three bottom-band cells.
    pub bottom: [PanelKind; 3],
    /// Hodograph window width (kts across).
    pub hodo_zoom_kts: f64,
}

impl Default for SoundingLayout {
    fn default() -> Self {
        SoundingLayout {
            strips: [PanelKind::Speed, PanelKind::Advection],
            main: PanelKind::Hodograph,
            insets: [
                PanelKind::Slinky,
                PanelKind::ThetaE,
                PanelKind::SrWinds,
                PanelKind::LocationMap,
            ],
            bottom: [
                PanelKind::IndexBoard,
                PanelKind::Streamwiseness,
                PanelKind::Hidden,
            ],
            hodo_zoom_kts: panels::hodo::DEFAULT_ZOOM_KTS,
        }
    }
}

impl SoundingLayout {
    /// Serialize to a compact, dependency-free token string a host can stash
    /// in its own settings (JSON, ini, ...). Format — five `|`-separated
    /// sections, panel tokens comma-separated within a section:
    ///
    /// ```text
    /// strips(2) | main(1) | insets(4) | bottom(3) | hodo_zoom_kts
    /// ```
    ///
    /// e.g. the default layout is
    /// `"speed,advection|hodograph|slinky,thetae,srwinds,locationmap|indexboard,streamwiseness,stp|250"`.
    /// Panel tokens come from [`PanelKind::token`]; the zoom is a plain
    /// decimal in knots. Parse it back with [`SoundingLayout::from_tokens`].
    pub fn to_tokens(&self) -> String {
        let csv = |kinds: &[PanelKind]| {
            kinds
                .iter()
                .map(|k| k.token())
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "{}|{}|{}|{}|{}",
            csv(&self.strips),
            self.main.token(),
            csv(&self.insets),
            csv(&self.bottom),
            self.hodo_zoom_kts,
        )
    }

    /// Parse a [`SoundingLayout::to_tokens`] string. Whitespace around
    /// tokens is tolerated; the zoom is clamped to the interactive range
    /// (80–500 kts). Returns `None` for wrong section/panel counts,
    /// unknown panel tokens, or a non-finite zoom.
    pub fn from_tokens(s: &str) -> Option<SoundingLayout> {
        fn cells<const N: usize>(section: &str) -> Option<[PanelKind; N]> {
            let mut out = [PanelKind::Hidden; N];
            let mut it = section.split(',');
            for slot in &mut out {
                *slot = PanelKind::from_token(it.next()?.trim())?;
            }
            it.next().is_none().then_some(out)
        }
        let mut sections = s.split('|');
        let strips = cells::<2>(sections.next()?)?;
        let [main] = cells::<1>(sections.next()?)?;
        let insets = cells::<4>(sections.next()?)?;
        let bottom = cells::<3>(sections.next()?)?;
        let zoom: f64 = sections.next()?.trim().parse().ok()?;
        if sections.next().is_some() || !zoom.is_finite() {
            return None;
        }
        Some(SoundingLayout {
            strips,
            main,
            insets,
            bottom,
            hodo_zoom_kts: zoom.clamp(80.0, 500.0),
        })
    }
}

/// Read the [`SoundingLayout`] stored in egui temp memory under `id` — the
/// key a [`SoundingView`] built with [`SoundingView::layout_memory_id`]
/// reads and writes. `None` until something stored one.
pub fn stored_layout(ctx: &egui::Context, id: egui::Id) -> Option<SoundingLayout> {
    ctx.data_mut(|d| d.get_temp(id))
}

/// Store a [`SoundingLayout`] in egui temp memory under `id`, where a
/// [`SoundingView`] built with [`SoundingView::layout_memory_id`] of the
/// same `id` picks it up on its next frame. Together with
/// [`SoundingLayout::to_tokens`] / [`from_tokens`](SoundingLayout::from_tokens)
/// this lets a host persist the layout across sessions.
pub fn store_layout(ctx: &egui::Context, id: egui::Id, layout: &SoundingLayout) {
    ctx.data_mut(|d| d.insert_temp(id, layout.clone()));
}
use crate::profile::{ParcelType, Profile};
use crate::skewt::{SkewT, SkewTStyle};

/// The complete sounding window. Compute [`DerivedParams`] once (it is not
/// cheap) and keep it alongside the profile.
pub struct SoundingView<'a> {
    prof: &'a Profile,
    derived: &'a DerivedParams,
    title: String,
    brand: Option<String>,
    parcel: ParcelType,
    style: SkewTStyle,
    size: Option<Vec2>,
    corner: CornerPanel,
    interactive: bool,
    layout_id: Option<egui::Id>,
}

impl<'a> SoundingView<'a> {
    pub fn new(prof: &'a Profile, derived: &'a DerivedParams) -> Self {
        SoundingView {
            prof,
            derived,
            title: String::new(),
            brand: None,
            parcel: ParcelType::MostUnstable,
            style: SkewTStyle::default(),
            size: None,
            corner: CornerPanel::default(),
            interactive: true,
            layout_id: None,
        }
    }

    /// Pin the egui-memory key the panel layout is kept under (default: an
    /// id derived from the widget's `ui.id()`, which shifts with the
    /// surrounding layout). With a stable id the host can read/write the
    /// layout via [`stored_layout`] / [`store_layout`] — e.g. to persist it
    /// with [`SoundingLayout::to_tokens`].
    pub fn layout_memory_id(mut self, id: egui::Id) -> Self {
        self.layout_id = Some(id);
        self
    }

    /// Enable/disable the hover readout cursor and the linked hodograph
    /// marker (default: on).
    pub fn interactive(mut self, on: bool) -> Self {
        self.interactive = on;
        self
    }

    /// Choose the fourth inset cell (default: the location map; pass
    /// [`CornerPanel::HazardType`] for the original watch box).
    pub fn corner_panel(mut self, corner: CornerPanel) -> Self {
        self.corner = corner;
        self
    }

    /// Skew-T title (top-left).
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Brand text drawn top-right above the hodograph column.
    pub fn brand(mut self, brand: impl Into<String>) -> Self {
        self.brand = Some(brand.into());
        self
    }

    pub fn parcel(mut self, parcel: ParcelType) -> Self {
        self.parcel = parcel;
        self
    }

    pub fn style(mut self, style: SkewTStyle) -> Self {
        self.style = style;
        self
    }

    pub fn size(mut self, size: Vec2) -> Self {
        self.size = Some(size);
        self
    }
}

fn weighted_bottom_rects(band: Rect, panels: &[PanelKind; 3]) -> [Rect; 3] {
    let base = [0.61_f32, 0.14, 0.25];
    let weights: [f32; 3] = std::array::from_fn(|index| {
        if panels[index] == PanelKind::Hidden {
            0.0
        } else {
            base[index]
        }
    });
    let total: f32 = weights.iter().sum();
    let weights = if total > 0.0 { weights } else { base };
    let total: f32 = weights.iter().sum();
    let mut x = band.min.x;
    std::array::from_fn(|index| {
        let min = egui::pos2(x, band.min.y);
        x = if index == 2 {
            band.max.x
        } else {
            x + band.width() * weights[index] / total
        };
        Rect::from_min_max(min, egui::pos2(x, band.max.y))
    })
}

impl Widget for SoundingView<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let size = self.size.unwrap_or_else(|| ui.available_size());
        let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
        if !ui.is_rect_visible(rect) || rect.width() < 200.0 || rect.height() < 150.0 {
            return response;
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, self.style.bg_color);

        let w = rect.width();
        let h = rect.height();

        // Vertical split: top (skew-T + upper-right) / bottom index band,
        // matching the reference proportions (~67% / 33%).
        let band_top = rect.min.y + h * 0.67;
        // Horizontal split of the top: skew-T column ~46%.
        let skew_right = rect.min.x + w * 0.46;

        // --- Skew-T (its own Widget; place it in its cell). ---
        let skew_rect = Rect::from_min_max(rect.min, egui::pos2(skew_right, band_top));
        let mut skew_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(skew_rect)
                .layout(egui::Layout::default()),
        );
        skew_ui.add(
            SkewT::new(self.prof)
                .parcel(self.parcel)
                .title(self.title.clone())
                .style(self.style.clone())
                .cursor_readout(self.interactive)
                .size(skew_rect.size()),
        );

        // --- Upper right: brand band + grid2. ---
        let ur = Rect::from_min_max(
            egui::pos2(skew_right, rect.min.y),
            egui::pos2(rect.max.x, band_top),
        );
        let brand_h = 16.0f32;
        if let Some(brand) = &self.brand {
            painter.text(
                egui::pos2(ur.max.x - 4.0, ur.min.y + 2.0),
                Align2::RIGHT_TOP,
                brand,
                egui::FontId::new(11.0, self.style.font_regular.clone()),
                self.style.fg_color,
            );
        }
        let g2 = Rect::from_min_max(egui::pos2(ur.min.x, ur.min.y + brand_h), ur.max);
        // grid2: 29 columns x 11 rows.
        let colw = g2.width() / 29.0;
        let rowh = g2.height() / 11.0;
        let cell = |c0: f32, r0: f32, cs: f32, rs: f32| {
            Rect::from_min_size(
                egui::pos2(g2.min.x + c0 * colw, g2.min.y + r0 * rowh),
                Vec2::new(cs * colw, rs * rowh),
            )
        };
        let strip_rects = [cell(0.0, 0.0, 3.0, 11.0), cell(3.0, 0.0, 2.0, 11.0)];
        let main_rect = cell(5.0, 0.0, 24.0, 8.0);
        let inset_rects = [
            cell(5.0, 8.0, 6.0, 3.0),
            cell(11.0, 8.0, 6.0, 3.0),
            cell(17.0, 8.0, 6.0, 3.0),
            cell(23.0, 8.0, 6.0, 3.0),
        ];
        // --- Bottom band cells (61/14/25% base weights). ---
        // Hidden cells surrender their allocation to the visible cells, so
        // removing the STP graphic makes the text-heavy index board wider
        // instead of leaving an empty quarter of the row.
        let band = Rect::from_min_max(egui::pos2(rect.min.x, band_top), rect.max);

        // --- Layout state (per-widget unless the host pinned an id via
        // `layout_memory_id`; edited in-app via the gear). ---
        let id = self
            .layout_id
            .unwrap_or_else(|| ui.id().with("sounding_layout"));
        let mut layout: SoundingLayout =
            ui.ctx().data_mut(|d| d.get_temp(id)).unwrap_or_else(|| {
                let mut l = SoundingLayout::default();
                if self.corner == CornerPanel::HazardType {
                    l.insets[3] = PanelKind::HazardType;
                }
                l
            });
        let bottom_rects = weighted_bottom_rects(band, &layout.bottom);

        // Scroll-to-zoom over the hodograph cell.
        if self.interactive
            && let Some(pos) = response.hover_pos()
            && main_rect.contains(pos)
            && layout.main == PanelKind::Hodograph
        {
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.0 {
                let factor = (-scroll as f64 / 400.0).exp();
                layout.hodo_zoom_kts = (layout.hodo_zoom_kts * factor).clamp(80.0, 500.0);
            }
        }

        let dv = self.derived;
        let st = &self.style;
        let zoom = layout.hodo_zoom_kts;
        for (kind, r) in layout
            .strips
            .iter()
            .zip(strip_rects.iter())
            .chain(layout.insets.iter().zip(inset_rects.iter()))
            .chain(layout.bottom.iter().zip(bottom_rects.iter()))
            .chain(std::iter::once((&layout.main, &main_rect)))
        {
            kind.draw(&painter, *r, self.prof, dv, st, zoom);
        }

        // Linked cursor: hovering the skew-T highlights the wind at that
        // height on the hodograph (wherever it currently lives).
        let hodo_cell = std::iter::once((&layout.main, &main_rect))
            .chain(layout.insets.iter().zip(inset_rects.iter()))
            .chain(layout.strips.iter().zip(strip_rects.iter()))
            .chain(layout.bottom.iter().zip(bottom_rects.iter()))
            .find(|(k, _)| **k == PanelKind::Hodograph)
            .map(|(_, r)| *r);
        if self.interactive
            && let Some(pos) = response.hover_pos()
            && skew_rect.contains(pos)
            && let Some(pres) = crate::skewt::hover_pressure(skew_rect, pos)
            && let Some(hodo_rect) = hodo_cell
        {
            let h_agl = self.prof.inner.to_agl(self.prof.inner.interp_hght(pres));
            if h_agl.is_finite() {
                panels::hodo::cursor_marker(&painter, hodo_rect, self.prof, st, h_agl, zoom);
            }
        }

        // --- Layout editor: gear button toggles per-cell pickers. ---
        if self.interactive {
            let edit_id = ui.id().with("sounding_layout_edit");
            let mut editing: bool = ui.ctx().data_mut(|d| d.get_temp(edit_id)).unwrap_or(false);
            let gear_rect = Rect::from_min_size(
                egui::pos2(rect.max.x - 24.0, band_top - 22.0),
                Vec2::new(22.0, 20.0),
            );
            if ui
                .put(gear_rect, egui::Button::new("\u{2699}").small())
                .on_hover_text("Edit panel layout")
                .clicked()
            {
                editing = !editing;
            }
            if editing {
                let mut slots: Vec<(&mut PanelKind, Rect)> = Vec::new();
                let SoundingLayout {
                    strips,
                    main,
                    insets,
                    bottom,
                    ..
                } = &mut layout;
                for (k, r) in strips.iter_mut().zip(strip_rects.iter()) {
                    slots.push((k, *r));
                }
                slots.push((main, main_rect));
                for (k, r) in insets.iter_mut().zip(inset_rects.iter()) {
                    slots.push((k, *r));
                }
                for (k, r) in bottom.iter_mut().zip(bottom_rects.iter()) {
                    slots.push((k, *r));
                }
                for (i, (kind, r)) in slots.into_iter().enumerate() {
                    painter.rect_stroke(
                        r.shrink(1.0),
                        0.0,
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(0x04, 0xDB, 0xD8)),
                        egui::StrokeKind::Inside,
                    );
                    let combo_rect = Rect::from_min_size(
                        r.min + Vec2::new(4.0, 4.0),
                        Vec2::new((r.width() - 8.0).min(150.0), 18.0),
                    );
                    let mut combo_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(combo_rect)
                            .layout(egui::Layout::default()),
                    );
                    egui::ComboBox::from_id_salt(id.with(i))
                        .selected_text(kind.label())
                        .width(combo_rect.width())
                        .show_ui(&mut combo_ui, |ui| {
                            for k in PanelKind::ALL {
                                ui.selectable_value(kind, k, k.label());
                            }
                        });
                }
                let reset_rect = Rect::from_min_size(
                    egui::pos2(rect.max.x - 84.0, band_top - 22.0),
                    Vec2::new(56.0, 20.0),
                );
                if ui
                    .put(reset_rect, egui::Button::new("reset").small())
                    .clicked()
                {
                    layout = SoundingLayout::default();
                }
            }
            ui.ctx().data_mut(|d| d.insert_temp(edit_id, editing));
        }
        ui.ctx().data_mut(|d| d.insert_temp(id, layout));

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_panel_token_round_trips() {
        for k in PanelKind::ALL {
            assert_eq!(PanelKind::from_token(k.token()), Some(k), "{k:?}");
        }
        assert_eq!(PanelKind::from_token("nonsense"), None);
    }

    #[test]
    fn default_layout_round_trips_through_tokens() {
        let layout = SoundingLayout::default();
        let tokens = layout.to_tokens();
        assert_eq!(
            tokens,
            "speed,advection|hodograph|slinky,thetae,srwinds,locationmap|\
             indexboard,streamwiseness,stp|250"
        );
        assert_eq!(SoundingLayout::from_tokens(&tokens), Some(layout));
    }

    #[test]
    fn customized_layout_round_trips_through_tokens() {
        let mut layout = SoundingLayout::default();
        layout.strips[1] = PanelKind::Hidden;
        layout.main = PanelKind::Slinky;
        layout.insets[3] = PanelKind::HazardType;
        layout.bottom[2] = PanelKind::Ship;
        layout.hodo_zoom_kts = 137.5;
        assert_eq!(
            SoundingLayout::from_tokens(&layout.to_tokens()),
            Some(layout)
        );
    }

    #[test]
    fn from_tokens_tolerates_whitespace_and_clamps_zoom() {
        let layout = SoundingLayout::from_tokens(
            " speed , advection | hodograph | slinky,thetae,srwinds,hazardtype \
             | indexboard,streamwiseness,stp | 9000 ",
        )
        .expect("padded tokens parse");
        assert_eq!(layout.insets[3], PanelKind::HazardType);
        assert_eq!(layout.hodo_zoom_kts, 500.0, "zoom clamps to 80..=500");
    }

    #[test]
    fn from_tokens_rejects_malformed_input() {
        for bad in [
            "",
            "speed,advection|hodograph|slinky,thetae,srwinds,locationmap",
            "speed,advection|hodograph|slinky,thetae,srwinds,locationmap|indexboard,streamwiseness,stp|NaN",
            "speed,advection|hodograph|slinky,thetae,srwinds,locationmap|indexboard,streamwiseness,stp|250|extra",
            "speed,advection,speed|hodograph|slinky,thetae,srwinds,locationmap|indexboard,streamwiseness,stp|250",
            "speed,advection|hodograph|slinky,thetae,srwinds,teapot|indexboard,streamwiseness,stp|250",
        ] {
            assert_eq!(SoundingLayout::from_tokens(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn store_and_read_layout_via_pinned_id() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("layout_memory_test");
        assert_eq!(stored_layout(&ctx, id), None);
        let mut layout = SoundingLayout::default();
        layout.hodo_zoom_kts = 210.0;
        store_layout(&ctx, id, &layout);
        assert_eq!(stored_layout(&ctx, id), Some(layout));
    }

    #[test]
    fn default_bottom_hides_stp_and_reclaims_its_width() {
        let layout = SoundingLayout::default();
        assert_eq!(
            layout.bottom,
            [
                PanelKind::IndexBoard,
                PanelKind::Streamwiseness,
                PanelKind::Hidden,
            ]
        );
        let band = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1000.0, 100.0));
        let rects = weighted_bottom_rects(band, &layout.bottom);
        assert!((rects[0].width() - 813.3333).abs() < 0.1);
        assert!((rects[1].width() - 186.6667).abs() < 0.1);
        assert_eq!(rects[2].width(), 0.0);
    }
}
