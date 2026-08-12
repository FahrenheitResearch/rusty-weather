//! Interactive demo: renders the bundled example HRRR sounding with the
//! Skew-T widget. Run with `cargo run --example demo`.

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/testdata/hrrr_example.rs"
));

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 900.0]),
        ..Default::default()
    };
    let profile = sharppyrs::Profile::new(hrrr_example()).expect("valid sounding");
    eframe::run_native(
        "sharppyrs Skew-T demo",
        options,
        Box::new(move |cc| {
            sharppyrs::install_fonts(&cc.egui_ctx);
            Ok(Box::new(Demo { profile }))
        }),
    )
}

struct Demo {
    profile: sharppyrs::Profile,
}

impl eframe::App for Demo {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::BLACK))
            .show(ui, |ui| {
                ui.add(
                    sharppyrs::SkewT::new(&self.profile)
                        .title(TITLE)
                        .brand("sharppyrs demo")
                        .style(sharppyrs::SkewTStyle::space_grotesk()),
                );
            });
    }
}
