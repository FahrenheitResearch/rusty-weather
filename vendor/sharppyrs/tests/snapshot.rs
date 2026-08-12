//! Headless render of the example sounding to a PNG for visual comparison
//! with the original SHARPpy-Reimagined render.

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/testdata/hrrr_example.rs"
));

#[test]
fn render_skewt_png() {
    let profile = sharppyrs::Profile::new(hrrr_example()).expect("valid sounding");
    let mut fonts_installed = false;
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(1000.0, 980.0))
        .build_ui(move |ui| {
            if !fonts_installed {
                // set_fonts applies at the start of the NEXT pass; skip drawing
                // the widget this frame.
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
                        sharppyrs::SkewT::new(&profile)
                            .title(TITLE)
                            .style(sharppyrs::SkewTStyle::space_grotesk()),
                    );
                });
        });
    harness.run();
    let image = harness.render().expect("wgpu render");
    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/target/skewt_snapshot.png");
    image.save(out).expect("save png");
    println!("wrote {out}");
}
