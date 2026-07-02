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
    #[arg(long, default_value_t = 2)]
    keep_recent: usize,
    #[arg(long, default_value_t = 30)]
    long_hours: u16,
    #[arg(long, help = "Directory holding rw_batch/rw_prune/rw_fuel_fetch (default: this exe's dir)")]
    bin_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 8, help = "How many cycles back to search for the newest run")]
    max_cycle_lag_hours: i64,
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

/// Fuel valid date: run date minus one day (gridMET lags realtime).
fn fuel_date_for(date_yyyymmdd: &str) -> String {
    let y: i64 = date_yyyymmdd[0..4].parse().unwrap_or(2000);
    let m: u32 = date_yyyymmdd[4..6].parse().unwrap_or(1);
    let d: u32 = date_yyyymmdd[6..8].parse().unwrap_or(1);
    let (y, m, d) = civil_from_days(days_from_civil(y, m, d) - 1);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Synoptic cycles run long; off-cycles run short.
fn target_max_hour(cycle: u8) -> u16 {
    if cycle % 6 == 0 { 48 } else { 18 }
}

fn hour_span(hours: &[u16]) -> String {
    match (hours.first(), hours.last()) {
        (Some(first), Some(last)) if first != last => format!("{first}-{last}"),
        (Some(only), _) => format!("{only}"),
        _ => String::new(),
    }
}

fn idx_url(date: &str, cycle: u8, hour: u16) -> String {
    format!(
        "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.{date}/conus/hrrr.t{cycle:02}z.wrfsfcf{hour:02}.grib2.idx"
    )
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

fn write_latest(
    model_dir: &Path,
    model: &str,
    run_slug: &str,
    stored: &[u16],
    target_max: u16,
) -> Result<(), String> {
    let latest = serde_json::json!({
        "schema": "cafire.latest_run.v1",
        "model": model,
        "run": run_slug,
        "stored_hours": stored,
        "target_max_hour": target_max,
        "complete": stored.iter().copied().max().unwrap_or(0) >= target_max,
        "updated_unix": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
    });
    let tmp = model_dir.join("latest.json.tmp");
    let path = model_dir.join("latest.json");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&latest).map_err(|e| e.to_string())?)
        .map_err(|err| format!("write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|err| format!("publish {}: {err}", path.display()))
}

fn prewarm(agent: &ureq::Agent, api: &str, run_slug: &str) {
    let domains = [
        ("cafire_california", [-126.0, -113.8, 31.9, 42.5]),
        ("cafire_wide_west", [-125.7, -103.8, 31.9, 46.5]),
    ];
    let presets = ["cafire-anomaly", "cafire-record", "cafire-core", "fuels"];
    for (slug, bounds) in domains {
        for preset in presets {
            let body = serde_json::json!({
                "model": "hrrr",
                "run": run_slug,
                "hour": 12,
                "products": preset,
                "output_format": "webp",
                "plot_style": "operational",
                "place_label_density": 3,
                "place_label_size": 2,
                "output_width": 1800,
                "domain_slug": slug,
                "bounds": bounds,
            });
            let started = agent
                .post(&format!("{api}/api/render"))
                .header("content-type", "application/json")
                .send(body.to_string());
            let mut started = match started {
                Ok(resp) => resp,
                Err(err) => {
                    eprintln!("[pipeline] prewarm {slug}/{preset} submit failed: {err}");
                    continue;
                }
            };
            let Some(job) = started
                .body_mut()
                .read_to_string()
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            else {
                continue;
            };
            let Some(status_url) = job.get("status_url").and_then(|v| v.as_str()) else {
                continue;
            };
            // Poll to completion so the gate isn't flooded; ~4 min cap.
            for _ in 0..240 {
                std::thread::sleep(Duration::from_secs(1));
                let Some(state) = get_json(agent, &format!("{api}{status_url}")) else {
                    break;
                };
                match state.get("state").and_then(|v| v.as_str()) {
                    Some("succeeded") => {
                        println!("[pipeline] prewarm {slug}/{preset} done");
                        break;
                    }
                    Some("failed") => {
                        eprintln!(
                            "[pipeline] prewarm {slug}/{preset} failed: {}",
                            state.get("message").and_then(|v| v.as_str()).unwrap_or("?")
                        );
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn tick(args: &Args, agent: &ureq::Agent, bin_dir: &Path) {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Newest cycle whose first file exists (HRRR appears ~50-90 min after
    // cycle time, so start one hour back).
    let mut found: Option<(String, u8)> = None;
    for lag in 1..=args.max_cycle_lag_hours {
        let (date, cycle) = cycle_at(now_unix, lag);
        if probe(agent, &idx_url(&date, cycle, 1)) {
            found = Some((date, cycle));
            break;
        }
    }
    let Some((date, cycle)) = found else {
        eprintln!("[pipeline] no available HRRR cycle found in the last {} h", args.max_cycle_lag_hours);
        return;
    };
    let run_slug = format!("{date}_{cycle:02}z");
    let target_max = target_max_hour(cycle);
    let model_dir = args.store_root.join(&args.model);
    let run_dir = model_dir.join(&run_slug);
    let stored = stored_hours(&run_dir);

    // Highest hour published upstream so far (probe down from target).
    let mut available_max = None;
    for hour in (1..=target_max).rev() {
        if probe(agent, &idx_url(&date, cycle, hour)) {
            available_max = Some(hour);
            break;
        }
    }
    let Some(available_max) = available_max else { return };
    let missing: Vec<u16> = (0..=available_max)
        .filter(|hour| !stored.contains(hour))
        .collect();

    println!(
        "[pipeline] run {run_slug}: stored {} / available F000-F{available_max:03} / target F{target_max:03}",
        stored.len()
    );

    if !missing.is_empty() {
        let mut command = Command::new(bin_dir.join(exe("rw_batch")));
        command
            .arg("--date").arg(&date)
            .arg("--cycle").arg(cycle.to_string())
            .arg("--hours").arg(hour_span(&missing))
            .arg("--products").arg("none")
            .arg("--profile").arg("view")
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
    if let Err(err) = write_latest(&model_dir, &args.model, &run_slug, &stored, target_max) {
        eprintln!("[pipeline] latest.json: {err}");
    }

    // Fuels: once per run, only after the day window is present.
    let fuel_flag = run_dir.join(".fuels-imported");
    if !fuel_flag.exists() && stored.iter().any(|&hour| hour >= 23) {
        let mut command = Command::new(bin_dir.join(exe("rw_fuel_fetch")));
        command
            .arg("--store-root").arg(&args.store_root)
            .arg("--model").arg(&args.model)
            .arg("--run").arg(&run_slug)
            .arg("--hours").arg(hour_span(&stored))
            .arg("--date").arg(fuel_date_for(&date));
        if let Some(cache) = &args.cache_dir {
            command.arg("--cache-dir").arg(cache.join("fuel"));
        }
        if run_command("fuels", &mut command) {
            let _ = std::fs::write(&fuel_flag, b"ok");
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

    // Prewarm once per run, when the target is fully stored + fuels done.
    let warm_flag = run_dir.join(".prewarmed");
    let complete = stored.iter().copied().max().unwrap_or(0) >= target_max;
    if complete && !warm_flag.exists() && fuel_flag.exists() {
        if let Some(api) = &args.api_url {
            prewarm(agent, api.trim_end_matches('/'), &run_slug);
            let _ = std::fs::write(&warm_flag, b"ok");
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
    let lock_path = args.store_root.join(".rw-pipeline-lock");
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
    fn fuel_date_is_previous_day_dashed() {
        assert_eq!(fuel_date_for("20260701"), "2026-06-30");
        assert_eq!(fuel_date_for("20260101"), "2025-12-31");
    }

    #[test]
    fn hour_spans_render_compactly() {
        assert_eq!(hour_span(&[0, 1, 2, 3]), "0-3");
        assert_eq!(hour_span(&[7]), "7");
        assert_eq!(hour_span(&[]), "");
    }
}
