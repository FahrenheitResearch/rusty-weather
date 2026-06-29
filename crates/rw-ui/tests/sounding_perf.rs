use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rw_ui::skewt::{build_native_sounding, render_sounding_image};
use rw_ui::{HourKey, StoreRequest, StoreResponse, StoreView, StoreWorker};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn newest_hrrr_hour(store: &StoreView) -> Option<HourKey> {
    let tree = store.enumerate();
    let model = tree.models.iter().find(|model| model.model == "hrrr")?;
    let run = model.runs.first()?;
    let hour = run.hours.first()?;
    Some(HourKey {
        model: model.model.clone(),
        run: run.run.clone(),
        hour: hour.hour,
    })
}

fn load_sounding(store_root: PathBuf, hour: HourKey) -> rw_ui::SoundingData {
    let worker = StoreWorker::spawn(StoreView::new(store_root), || {});
    worker.send(StoreRequest::LoadSounding {
        hour: hour.clone(),
        fx: 338.4,
        fy: 370.9,
    });
    match worker
        .recv_timeout(Duration::from_secs(60))
        .expect("store worker response")
    {
        StoreResponse::Sounding(got, Ok(data)) => {
            assert_eq!(got, hour);
            data
        }
        other => panic!("expected sounding data, got {other:?}"),
    }
}

fn median(values: &mut [f32]) -> f32 {
    values.sort_by(|a, b| a.total_cmp(b));
    values[values.len() / 2]
}

#[test]
#[ignore = "local perf smoke: requires a downloaded hrrr rw-store under ./store"]
fn local_sounding_native_vs_png_perf() {
    let store_root = workspace_root().join("store");
    let store = StoreView::new(&store_root);
    let hour = newest_hrrr_hour(&store).expect("downloaded HRRR run under ./store");
    let data = load_sounding(store_root, hour.clone());

    let mut native_build_ms = Vec::new();
    let mut sharppy_png_ms = Vec::new();
    let mut sharppy_png_decode_ms = Vec::new();
    let mut full_png_ms = Vec::new();
    let mut sharppy_png_bytes = 0usize;
    let mut full_png_bytes = 0usize;

    for _ in 0..7 {
        let started = Instant::now();
        let native = build_native_sounding(&data).expect("native sounding");
        native_build_ms.push(started.elapsed().as_secs_f32() * 1000.0);

        let started = Instant::now();
        let png = native.render_sharppy_png();
        sharppy_png_ms.push(started.elapsed().as_secs_f32() * 1000.0);
        sharppy_png_bytes = png.len();
        black_box(&png);

        let started = Instant::now();
        let image = render_sounding_image(&native).expect("png decode");
        sharppy_png_decode_ms.push(started.elapsed().as_secs_f32() * 1000.0);
        black_box(image.size);

        let started = Instant::now();
        let png = native.render_full_png();
        full_png_ms.push(started.elapsed().as_secs_f32() * 1000.0);
        full_png_bytes = png.len();
        black_box(&png);
    }

    println!(
        "sounding perf {}/{} f{:03} @ {:.1},{:.1}",
        hour.model, hour.run, hour.hour, data.fx, data.fy
    );
    println!("store profile read: {:.2} ms", data.read_ms);
    println!(
        "native build, no PNG (column + params + bottom tables): {:.2} ms median",
        median(&mut native_build_ms)
    );
    println!(
        "SHARPpy PNG render+encode only: {:.2} ms median, {} bytes",
        median(&mut sharppy_png_ms),
        sharppy_png_bytes
    );
    println!(
        "SHARPpy PNG render+encode+decode to egui image: {:.2} ms median",
        median(&mut sharppy_png_decode_ms)
    );
    println!(
        "full rustwx PNG render+table replace+encode: {:.2} ms median, {} bytes",
        median(&mut full_png_ms),
        full_png_bytes
    );
}
