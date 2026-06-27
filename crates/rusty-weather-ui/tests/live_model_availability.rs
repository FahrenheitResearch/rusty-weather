use chrono::Utc;
use rustwx_core::ModelId;

fn products_for(model: ModelId) -> Vec<&'static str> {
    let mut products = Vec::new();
    for entry in rw_ingest::fetch_plan(model).expect("enabled model has a fetch plan") {
        if !products.iter().any(|seen| seen == &entry.product) {
            products.push(entry.product);
        }
    }
    products
}

#[test]
#[ignore = "network smoke test against live model feeds"]
fn live_ingest_models_have_latest_f006_available() {
    let today = Utc::now().format("%Y%m%d").to_string();
    let models = [
        ModelId::Hrrr,
        ModelId::HrrrAk,
        ModelId::Rap,
        ModelId::Gfs,
        ModelId::Gdas,
        ModelId::Gefs,
        ModelId::Aigfs,
        ModelId::Aigefs,
        ModelId::Hgefs,
        ModelId::EcmwfOpenData,
        ModelId::Nam,
        ModelId::RrfsA,
    ];

    let mut failures = Vec::new();
    for model in models {
        let products = products_for(model);
        match rustwx_models::latest_available_run_for_products_at_forecast_hour(
            model, None, &today, &products, 6,
        ) {
            Ok(run) => eprintln!(
                "ok {model:<16} {} {:02}z f006 via {} ({})",
                run.cycle.date_yyyymmdd,
                run.cycle.hour_utc,
                run.source,
                products.join(", ")
            ),
            Err(err) => failures.push(format!("{model}: {err} ({})", products.join(", "))),
        }
    }

    assert!(
        failures.is_empty(),
        "live f006 availability failures:\n{}",
        failures.join("\n")
    );
}
