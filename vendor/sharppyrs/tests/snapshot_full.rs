//! Headless render of the FULL sounding window to a PNG for comparison with
//! the Python reference (`reference_render.png`).

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/testdata/hrrr_example.rs"
));

#[test]
fn render_full_window_png() {
    let profile = sharppyrs::Profile::new(hrrr_example()).expect("valid sounding");
    let derived = sharppyrs::DerivedParams::compute(&profile);
    let mut fonts_installed = false;
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(1630.0, 1100.0))
        .build_ui(move |ui| {
            if !fonts_installed {
                sharppyrs::install_fonts(ui.ctx());
                fonts_installed = true;
                ui.ctx().request_repaint();
                return;
            }
            egui::Frame::new()
                .fill(egui::Color32::BLACK)
                .show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    ui.add(
                        sharppyrs::SoundingView::new(&profile, &derived)
                            .title(TITLE)
                            .brand("sharppyrs")
                            .style(sharppyrs::SkewTStyle::space_grotesk()),
                    );
                });
        });
    harness.run();
    // Hover mid-skew-T to exercise the readout cursor + linked hodo marker.
    harness.hover_at(egui::pos2(400.0, 500.0));
    harness.run();
    let image = harness.render().expect("wgpu render");
    let out = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/target/full_window_snapshot.png"
    );
    image.save(out).expect("save png");
    println!("wrote {out}");
}
