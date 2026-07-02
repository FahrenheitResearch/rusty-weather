//! `rw_pipeline` — the unattended refresh daemon.
//!
//! Each tick (default every 5 minutes): discover the newest available HRRR
//! cycle by probing AWS `.idx` existence, ingest any not-yet-stored hours
//! by shelling to the proven `rw_batch` (fetch -> ingest, no render),
//! attach the daily fuel grids via `rw_fuel_fetch` (once per run, flagged),
//! publish an atomic `latest.json` run manifest the API can serve, prune
//! old runs/caches via `rw_prune`, and prewarm the render API's request
//! cache for the standard domains once a run first reaches its target.
//!
//! v1 is deliberately an orchestrator over battle-tested binaries (the
//! same pattern the API uses for rw_render) — one process to supervise,
//! `--once` for cron/systemd-timer setups, a singleton lock file so two
//! daemons never fight over the store.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "rw-pipeline", about = "Unattended HRRR refresh daemon")]
struct Args {
    #[arg(long)]
    store_root: PathBuf,
    #[arg(long, default_value = "hrrr")]
    model: String,
    #[arg(long, help = "Fetch cache dir passed to rw_batch/rw_fuel_fetch")]
    cache_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 5)]
    interval_mins: u64,
    #[arg(long, help = "Run one tick and exit (cron/systemd-timer mode)")]
    once: bool,
    #[arg(long, help = "Local render API base for cache prewarm, e.g. http://127.0.0.1:8788")]
    api_url: Option<String>,
    #[arg(long, default_value_t = 3)]
    keep_recent: usize,
    #[arg(long, default_value_t = 30)]
    long_hours: u16,
    #[arg(long, help = "Directory holding rw_batch/rw_prune/rw_fuel_fetch (default: this exe's dir)")]
    bin_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 8, help = "How many cycles back to search for the newest run")]
    max_cycle_lag_hours: i64,
    #[arg(
        long,
        default_value = "view",
        help = "Ingest profile passed to rw_batch: view (2D, serving nodes) or full (volumes + heavy/ECAPE, compute nodes)"
    )]
    profile: String,
}

// ---- civil-date math (Hinnant), UTC clock helpers ----

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = ((m + 9) % 12) as u64;
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}

/// (yyyymmdd string, cycle hour) for `hours_back` hours before now (UTC).
fn cycle_at(now_unix: i64, hours_back: i64) -> (String, u8) {
    let total_hours = now_unix.div_euclid(3600) - hours_back;
    let days = total_hours.div_euclid(24);
    let hour = total_hours.rem_euclid(24) as u8;
    let (y, m, d) = civil_from_days(days);
    (format!("{y:04}{m:02}{d:02}"), hour)
}

/// Fuel valid date candidate: run date minus `days_back` (gridMET lags
/// realtime by 1-2 days; callers try 1 then 2 then 3).
fn fuel_date_for(date_yyyymmdd: &str, days_back: i64) -> String {
    let y: i64 = date_yyyymmdd[0..4].parse().unwrap_or(2000);
    let m: u32 = date_yyyymmdd[4..6].parse().unwrap_or(1);
    let d: u32 = date_yyyymmdd[6..8].parse().unwrap_or(1);
    let (y, m, d) = civil_from_days(days_from_civil(y, m, d) - days_back);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Synoptic cycles run long; off-cycles run short.
fn target_max_hour(cycle: u8) -> u16 {
    if cycle % 6 == 0 { 48 } else { 18 }
}

/// The forecast hours a model's cycle should ingest. HRRR: hourly to
/// 18/48. GFS: 6-hourly to F192 (the daily-outlook span) on synoptic
/// cycles only.
fn target_hours(model: &str, cycle: u8) -> Vec<u16> {
    match model {
        "gfs" => (0..=192u16).step_by(6).collect(),
        _ => (0..=target_max_hour(cycle)).collect(),
    }
}

/// Whether this model publishes the given cycle at all.
fn model_has_cycle(model: &str, cycle: u8) -> bool {
    match model {
        "gfs" => cycle % 6 == 0,
        _ => true,
    }
}

fn hour_span(hours: &[u16]) -> String {
    match (hours.first(), hours.last()) {
        (Some(first), Some(last)) if first != last => format!("{first}-{last}"),
        (Some(only), _) => format!("{only}"),
        _ => String::new(),
    }
}

fn idx_url(model: &str, date: &str, cycle: u8, hour: u16) -> String {
    match model {
        "gfs" => format!(
            "https://noaa-gfs-bdp-pds.s3.amazonaws.com/gfs.{date}/{cycle:02}/atmos/gfs.t{cycle:02}z.pgrb2.0p25.f{hour:03}.idx"
        ),
        _ => format!(
            "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.{date}/conus/hrrr.t{cycle:02}z.wrfsfcf{hour:02}.grib2.idx"
        ),
    }
}

fn probe(agent: &ureq::Agent, url: &str) -> bool {
    agent
        .head(url)
        .call()
        .map(|resp| resp.status().as_u16() == 200)
        .unwrap_or(false)
}

fn get_json(agent: &ureq::Agent, url: &str) -> Option<serde_json::Value> {
    let mut response = agent.get(url).call().ok()?;
    let text = response.body_mut().read_to_string().ok()?;
    serde_json::from_str(&text).ok()
}

fn stored_hours(run_dir: &Path) -> Vec<u16> {
    let mut hours: Vec<u16> = std::fs::read_dir(run_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix('f')?.strip_suffix(".rws")?.parse::<u16>().ok()
        })
        .collect();
    hours.sort_unstable();
    hours.dedup();
    hours
}

fn run_command(label: &str, command: &mut Command) -> bool {
    println!("[pipeline] {label}: {command:?}");
    match command.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("[pipeline] {label} exited with {status}");
            false
        }
        Err(err) => {
            eprintln!("[pipeline] {label} failed to start: {err}");
            false
        }
    }
}

/// Newest run in the store whose stored hours reach its cycle's target —
/// the run day-window and anomaly products can actually fold.
fn newest_complete_run(model: &str, model_dir: &Path) -> Option<String> {
    let mut runs: Vec<String> = std::fs::read_dir(model_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    runs.sort();
    runs.reverse();
    runs.into_iter().find(|slug| {
        let Some((_, cycle)) = slug
            .split_once('_')
            .and_then(|(date, cycle)| {
                let hour: u8 = cycle.strip_suffix('z')?.parse().ok()?;
                (date.len() == 8).then_some((date, hour))
            })
        else {
            return false;
        };
        let stored = stored_hours(&model_dir.join(slug));
        let target = target_hours(model, cycle).last().copied().unwrap_or(0);
        stored.iter().copied().max().unwrap_or(0) >= target
    })
}

/// Hours of any single UTC day covered by `stored` forecast hours of a
/// `cycle`Z run — day-window (anomaly) products need >= 20.
fn best_day_coverage(cycle: u8, stored: &[u16]) -> usize {
    let mut per_day = std::collections::BTreeMap::new();
    for &hour in stored {
        let day = (u32::from(cycle) + u32::from(hour)) / 24;
        *per_day.entry(day).or_insert(0usize) += 1;
    }
    per_day.values().copied().max().unwrap_or(0)
}

/// Newest run whose stored hours cover a full-enough UTC day for the
/// anomaly/day-window lanes.
fn newest_day_covering_run(model_dir: &Path) -> Option<String> {
    let mut runs: Vec<String> = std::fs::read_dir(model_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    runs.sort();
    runs.reverse();
    runs.into_iter().find(|slug| {
        let Some(cycle) = slug
            .split_once('_')
            .and_then(|(_, cycle)| cycle.strip_suffix('z')?.parse::<u8>().ok())
        else {
            return false;
        };
        best_day_coverage(cycle, &stored_hours(&model_dir.join(slug))) >= 20
    })
}

#[allow(clippy::too_many_arguments)]
fn write_latest(
    model_dir: &Path,
    model: &str,
    run_slug: &str,
    stored: &[u16],
    target_max: u16,
    complete_run: Option<&str>,
    day_run: Option<&str>,
) -> Result<(), String> {
    let latest = serde_json::json!({
        "schema": "cafire.latest_run.v1",
        "model": model,
        "run": run_slug,
        "stored_hours": stored,
        "target_max_hour": target_max,
        "complete": stored.iter().copied().max().unwrap_or(0) >= target_max,
        // Newest fully-stored run: what `latest` resolves to.
        "complete_run": complete_run,
        // Newest run covering >=20 h of a UTC day: what `latest-day`
        // resolves to (anomaly/day-window lanes; off-cycle 18 h runs
        // never cover a full day).
        "day_run": day_run,
        "updated_unix": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
    });
    let tmp = model_dir.join("latest.json.tmp");
    let path = model_dir.join("latest.json");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&latest).map_err(|e| e.to_string())?)
        .map_err(|err| format!("write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|err| format!("publish {}: {err}", path.display()))
}

/// Submit one render request and poll it to completion (~4 min cap) so
/// prewarm never floods the render gate.
fn warm_one(agent: &ureq::Agent, api: &str, label: &str, body: serde_json::Value) {
    let started = agent
        .post(&format!("{api}/api/render"))
        .header("content-type", "application/json")
        .send(body.to_string());
    let mut started = match started {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("[pipeline] prewarm {label} submit failed: {err}");
            return;
        }
    };
    let Some(job) = started
        .body_mut()
        .read_to_string()
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return;
    };
    let Some(status_url) = job.get("status_url").and_then(|v| v.as_str()) else {
        return;
    };
    for _ in 0..240 {
        std::thread::sleep(Duration::from_secs(1));
        let Some(state) = get_json(agent, &format!("{api}{status_url}")) else { break };
        match state.get("state").and_then(|v| v.as_str()) {
            Some("succeeded") => {
                println!("[pipeline] prewarm {label} done");
                break;
            }
            Some("failed") => {
                eprintln!(
                    "[pipeline] prewarm {label} failed: {}",
                    state.get("message").and_then(|v| v.as_str()).unwrap_or("?")
                );
                break;
            }
            _ => {}
        }
    }
}

fn base_render_body(run_slug: &str, preset: &str) -> serde_json::Value {
    serde_json::json!({
        "model": "hrrr",
        "run": run_slug,
        "hour": 12,
        "products": preset,
        "output_format": "webp",
        "plot_style": "operational",
        "place_label_density": 3,
        "place_label_size": 2,
        "output_width": 1800,
    })
}

fn prewarm(agent: &ureq::Agent, api: &str, complete_run: &str, day_run: Option<&str>) {
    let domains = [
        ("cafire_california", [-126.0, -113.8, 31.9, 42.5]),
        ("cafire_wide_west", [-125.7, -103.8, 31.9, 46.5]),
    ];
    let presets = ["cafire-anomaly", "cafire-record", "cafire-core", "fuels"];
    for (slug, bounds) in domains {
        for preset in presets {
            // Day-window anomaly folds need a day-covering run; hourly
            // families warm on the newest complete run.
            let run_slug = if preset.contains("anomaly") || preset.contains("record") {
                match day_run {
                    Some(day_run) => day_run,
                    None => continue,
                }
            } else {
                complete_run
            };
            let mut body = base_render_body(run_slug, preset);
            body["domain_slug"] = serde_json::json!(slug);
            body["bounds"] = serde_json::json!(bounds);
            warm_one(agent, api, &format!("{slug}/{preset}"), body);
        }
    }

    // Per-incident pregen: auto-framed domains for the largest active
    // perimeters (the API's cached WFIGS feed) so the site's fire picker
    // is a cache hit for everyone.
    let Some(fires) = get_json(agent, &format!("{api}/api/fires")) else { return };
    let fires = fires
        .get("fires")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    for fire in fires.iter().take(4) {
        let Some(ring) = fire.get("ring") else { continue };
        let name = fire.get("name").and_then(|value| value.as_str()).unwrap_or("incident");
        let slug: String = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let slug = format!("{}_fire", slug.trim_matches('_').chars().take(24).collect::<String>());
        for (preset, run_slug) in
            [("cafire-core", Some(complete_run)), ("cafire-anomaly", day_run)]
        {
            let Some(run_slug) = run_slug else { continue };
            let mut body = base_render_body(run_slug, preset);
            body["domain_slug"] = serde_json::json!(slug);
            body["perimeter"] = ring.clone();
            body["padding_km"] = serde_json::json!(50);
            body["overlay_perimeter"] = serde_json::json!(true);
            body["title_note"] = serde_json::json!(format!("{name} Fire"));
            warm_one(agent, api, &format!("{name}/{preset}"), body);
        }
    }
}

fn tick(args: &Args, agent: &ureq::Agent, bin_dir: &Path) {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Newest cycle whose first file exists (HRRR appears ~50-90 min after
    // cycle time; GFS synoptic cycles lag ~3.5-5 h, so search further back).
    let mut found: Option<(String, u8)> = None;
    let max_lag = if args.model == "gfs" { args.max_cycle_lag_hours.max(12) } else { args.max_cycle_lag_hours };
    for lag in 1..=max_lag {
        let (date, cycle) = cycle_at(now_unix, lag);
        if !model_has_cycle(&args.model, cycle) {
            continue;
        }
        if probe(agent, &idx_url(&args.model, &date, cycle, 1)) {
            found = Some((date, cycle));
            break;
        }
    }
    let Some((date, cycle)) = found else {
        eprintln!(
            "[pipeline] no available {} cycle found in the last {max_lag} h",
            args.model
        );
        return;
    };
    let run_slug = format!("{date}_{cycle:02}z");
    let targets = target_hours(&args.model, cycle);
    let target_max = targets.last().copied().unwrap_or(0);
    let model_dir = args.store_root.join(&args.model);
    let run_dir = model_dir.join(&run_slug);
    let stored = stored_hours(&run_dir);

    // Highest target hour published upstream so far (probe down).
    let mut available_max = None;
    for &hour in targets.iter().rev() {
        if probe(agent, &idx_url(&args.model, &date, cycle, hour)) {
            available_max = Some(hour);
            break;
        }
    }
    let Some(available_max) = available_max else { return };
    let missing: Vec<u16> = targets
        .iter()
        .copied()
        .filter(|hour| *hour <= available_max && !stored.contains(hour))
        .collect();

    println!(
        "[pipeline] run {run_slug}: stored {} / available F000-F{available_max:03} / target F{target_max:03}",
        stored.len()
    );

    if !missing.is_empty() {
        let hours_arg = missing
            .iter()
            .map(|hour| hour.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut command = Command::new(bin_dir.join(exe("rw_batch")));
        command
            .arg("--model").arg(&args.model)
            .arg("--date").arg(&date)
            .arg("--cycle").arg(cycle.to_string())
            .arg("--hours").arg(hours_arg)
            .arg("--products").arg("none")
            .arg("--profile").arg(&args.profile)
            .arg("--store-root").arg(&args.store_root);
        if let Some(cache) = &args.cache_dir {
            command.arg("--cache-dir").arg(cache);
        }
        run_command("ingest", &mut command);
    }

    let stored = stored_hours(&run_dir);
    if stored.is_empty() {
        return;
    }

    // Fuels + prewarm target the newest COMPLETE run — a brand-new cycle
    // with two hours stored must not steal them from the run users see.
    let complete_run = newest_complete_run(&args.model, &model_dir);
    let day_run = newest_day_covering_run(&model_dir);
    if let Err(err) = write_latest(
        &model_dir,
        &args.model,
        &run_slug,
        &stored,
        target_max,
        complete_run.as_deref(),
        day_run.as_deref(),
    ) {
        eprintln!("[pipeline] latest.json: {err}");
    }

    // Fuels + prewarm are HRRR-lane concerns (fuel grids + anomaly presets
    // live on the HRRR grid); other models just ingest + publish.
    if let Some(target_run) = complete_run.as_ref().filter(|_| args.model == "hrrr") {
        let target_dir = model_dir.join(target_run);
        let target_hours = stored_hours(&target_dir);
        let target_date = target_run.split('_').next().unwrap_or(target_run).to_string();
        // Once per run; the fuel step rewrites hour files, so it only runs
        // after every target hour is stored.
        let fuel_flag = target_dir.join(".fuels-imported");
        if !fuel_flag.exists() {
            // gridMET publishes with a 1-2 day lag; walk back until a day lands.
            for days_back in 1..=3 {
                let mut command = Command::new(bin_dir.join(exe("rw_fuel_fetch")));
                command
                    .arg("--store-root").arg(&args.store_root)
                    .arg("--model").arg(&args.model)
                    .arg("--run").arg(target_run)
                    .arg("--hours").arg(hour_span(&target_hours))
                    .arg("--date").arg(fuel_date_for(&target_date, days_back));
                if let Some(cache) = &args.cache_dir {
                    command.arg("--cache-dir").arg(cache.join("fuel"));
                }
                if run_command(&format!("fuels (day -{days_back})"), &mut command) {
                    let _ = std::fs::write(&fuel_flag, b"ok");
                    break;
                }
            }
        }
    }

    // Retention.
    let mut command = Command::new(bin_dir.join(exe("rw_prune")));
    command
        .arg("--store-root").arg(&args.store_root)
        .arg("--model").arg(&args.model)
        .arg("--keep-recent").arg(args.keep_recent.to_string())
        .arg("--long-hours").arg(args.long_hours.to_string());
    if let Some(cache) = &args.cache_dir {
        command.arg("--fetch-cache").arg(cache);
    }
    run_command("prune", &mut command);

    // Prewarm once per complete run, after its fuels landed.
    if let Some(target_run) = complete_run.as_ref().filter(|_| args.model == "hrrr") {
        let target_dir = model_dir.join(target_run);
        let warm_flag = target_dir.join(".prewarmed");
        if !warm_flag.exists() && target_dir.join(".fuels-imported").exists() {
            if let Some(api) = &args.api_url {
                prewarm(agent, api.trim_end_matches('/'), target_run, day_run.as_deref());
                let _ = std::fs::write(&warm_flag, b"ok");
            }
        }
    }
}

/// ureq over the workspace's pure-Rust TLS stack (same as rw-sat/rw-glm).
fn build_agent() -> ureq::Agent {
    static CRYPTO_PROVIDER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    CRYPTO_PROVIDER.get_or_init(|| {
        rustls::crypto::CryptoProvider::install_default(rustls_rustcrypto::provider()).ok();
    });
    let crypto = std::sync::Arc::new(rustls_rustcrypto::provider());
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::WebPki)
                .unversioned_rustls_crypto_provider(crypto)
                .build(),
        )
        .build()
        .new_agent()
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

fn main() {
    let args = Args::parse();
    let bin_dir = args.bin_dir.clone().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    });

    // Singleton guard: refuse to start if another daemon holds the lock.
    let lock_path = args.store_root.join(format!(".rw-pipeline-lock-{}", args.model));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock_path) {
        Ok(_) => {}
        Err(_) => {
            eprintln!(
                "[pipeline] lock {} exists — another daemon running? Delete it if stale.",
                lock_path.display()
            );
            std::process::exit(1);
        }
    }
    let agent = build_agent();

    loop {
        tick(&args, &agent, &bin_dir);
        if args.once {
            break;
        }
        std::thread::sleep(Duration::from_secs(args.interval_mins.max(1) * 60));
    }
    let _ = std::fs::remove_file(&lock_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_math_walks_back_across_midnight() {
        // 2026-07-01 01:30 UTC, 2 hours back -> 2026-06-30 23z.
        let now = (days_from_civil(2026, 7, 1) * 24 + 1) * 3600 + 1800;
        assert_eq!(cycle_at(now, 2), ("20260630".to_string(), 23));
        assert_eq!(cycle_at(now, 1), ("20260701".to_string(), 0));
    }

    #[test]
    fn synoptic_cycles_run_long() {
        assert_eq!(target_max_hour(0), 48);
        assert_eq!(target_max_hour(6), 48);
        assert_eq!(target_max_hour(3), 18);
        assert_eq!(target_max_hour(23), 18);
    }

    #[test]
    fn gfs_targets_six_hourly_to_f192_on_synoptic_cycles_only() {
        let hours = target_hours("gfs", 12);
        assert_eq!(hours.first(), Some(&0));
        assert_eq!(hours.last(), Some(&192));
        assert!(hours.windows(2).all(|pair| pair[1] - pair[0] == 6));
        assert!(model_has_cycle("gfs", 6));
        assert!(!model_has_cycle("gfs", 7));
        assert!(model_has_cycle("hrrr", 7));
        assert!(idx_url("gfs", "20260702", 6, 96).contains("atmos/gfs.t06z.pgrb2.0p25.f096.idx"));
    }

    #[test]
    fn fuel_date_walks_back_dashed() {
        assert_eq!(fuel_date_for("20260701", 1), "2026-06-30");
        assert_eq!(fuel_date_for("20260101", 1), "2025-12-31");
        assert_eq!(fuel_date_for("20260702", 2), "2026-06-30");
    }

    #[test]
    fn day_coverage_counts_hours_within_one_utc_day() {
        // 00z F0-F23 covers the full first day.
        let hours: Vec<u16> = (0..=23).collect();
        assert_eq!(best_day_coverage(0, &hours), 24);
        // 04z F0-F18 covers at most 20 hours of day 1 (04-23Z)... only F0-F18
        // reaches 22Z: 19 hours — below the fold threshold.
        let hours: Vec<u16> = (0..=18).collect();
        assert_eq!(best_day_coverage(4, &hours), 19);
        // 06z 48 h run: day 2 (F18-F41) is fully covered.
        let hours: Vec<u16> = (0..=48).collect();
        assert_eq!(best_day_coverage(6, &hours), 24);
    }

    #[test]
    fn hour_spans_render_compactly() {
        assert_eq!(hour_span(&[0, 1, 2, 3]), "0-3");
        assert_eq!(hour_span(&[7]), "7");
        assert_eq!(hour_span(&[]), "");
    }
}
