//! Publish CAFire's GLM lightning artifacts from already-downloaded granules.
//!
//! Background: NOAA began writing GOES GLM L2 LCFA granules with
//! shuffle + deflate filters on 2026-07-09. The legacy `rustwx` 0.4.4 reader
//! cannot decode filtered datasets and reports the variables as missing, which
//! aborted the legacy lightning worker before it published anything. This bin
//! replaces only the decode+publish half using rw-glm's pure-Rust HDF5 reader
//! (which handles shuffle/deflate), and writes the exact artifacts the legacy
//! API already serves:
//!
//!   <artifact-root>/lightning/<domain>/<ts>/raw/glm_flashes.json
//!   <artifact-root>/lightning/latest.json
//!
//! The legacy API resolves `latest.json` -> `hours[].uploaded[]` -> the entry
//! whose key ends in `/glm_flashes.json`, then reads it from
//! `<artifact-root>/<key>` on local disk, so no R2 upload is required.
//!
//! The legacy lightning worker keeps downloading granules into the GLM dir
//! (its fetch step works; only its render step fails), so this bin just reads
//! that directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde_json::{Value, json};

#[derive(Parser, Debug)]
#[command(
    name = "rw_glm_cafire",
    about = "Decode GLM granules with rw-glm and publish CAFire lightning artifacts"
)]
struct Args {
    /// Directory holding OR_GLM-L2-LCFA_*.nc granules (kept fresh by the
    /// legacy lightning worker's fetch step).
    #[arg(long, default_value = "/data/glm")]
    glm_dir: PathBuf,
    /// Artifact root the legacy API reads from.
    #[arg(long, default_value = "/data/artifacts")]
    artifact_root: PathBuf,
    #[arg(long, default_value = "california")]
    domain: String,
    #[arg(long, default_value = "California GLM Lightning")]
    domain_label: String,
    /// Domain bounds as west,east,south,north (degrees).
    #[arg(long, default_value = "-124.9,-113.8,31.9,42.5")]
    bounds: String,
    /// Only include flashes newer than this many minutes.
    #[arg(long, default_value_t = 30.0)]
    max_age_min: f64,
    #[arg(long, default_value = "goes18")]
    satellite: String,
    #[arg(long, default_value = "noaa-goes18")]
    source: String,
    /// Seconds between cycles when looping.
    #[arg(long, default_value_t = 30)]
    interval_sec: u64,
    /// Run one cycle and exit.
    #[arg(long, default_value_t = false)]
    once: bool,
}

fn main() {
    let args = Args::parse();
    let bounds = match parse_bounds(&args.bounds) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}", json!({"ok": false, "error": e}));
            std::process::exit(2);
        }
    };
    loop {
        match run_once(&args, bounds) {
            Ok(report) => println!("{report}"),
            Err(e) => println!("{}", json!({"ok": false, "worker": "rw_glm_cafire", "error": e})),
        }
        if args.once {
            break;
        }
        std::thread::sleep(Duration::from_secs(args.interval_sec.max(1)));
    }
}

/// west, east, south, north
fn parse_bounds(raw: &str) -> Result<[f64; 4], String> {
    let parts: Vec<f64> = raw
        .split(',')
        .map(|p| p.trim().parse::<f64>().map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    if parts.len() != 4 {
        return Err(format!("bounds needs 4 values (west,east,south,north), got {}", parts.len()));
    }
    Ok([parts[0], parts[1], parts[2], parts[3]])
}

fn run_once(args: &Args, bounds: [f64; 4]) -> Result<String, String> {
    let mut granules: Vec<PathBuf> = fs::read_dir(&args.glm_dir)
        .map_err(|e| format!("read {}: {e}", args.glm_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("nc")
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.contains("GLM-L2-LCFA"))
        })
        .collect();
    granules.sort();
    if granules.is_empty() {
        return Err(format!("no GLM granules in {}", args.glm_dir.display()));
    }

    let now_ms = unix_ms_now();
    let cutoff_ms = now_ms - (args.max_age_min * 60_000.0) as i64;

    let mut all_flashes: Vec<(crate_flash::Flash, String)> = Vec::new();
    let mut decoded_ok = 0usize;
    let mut decode_errors: Vec<String> = Vec::new();
    let mut latest_key: Option<String> = None;
    let mut latest_mtime_ms: i64 = 0;

    for path in &granules {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        match rw_glm::granule::decode_granule(path) {
            Ok(g) => {
                decoded_ok += 1;
                for f in g.flashes {
                    all_flashes.push((f, name.clone()));
                }
                if let Ok(meta) = fs::metadata(path) {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    if mtime >= latest_mtime_ms {
                        latest_mtime_ms = mtime;
                        latest_key = Some(name.clone());
                    }
                }
            }
            Err(e) => {
                if decode_errors.len() < 3 {
                    decode_errors.push(format!("{name}: {e}"));
                }
            }
        }
    }

    let flash_count_total = all_flashes.len();

    // Time window across every decoded flash (before domain filtering), so the
    // manifest reports the same span the legacy worker did.
    let mut first_ms = i64::MAX;
    let mut last_ms = i64::MIN;
    for (f, _) in &all_flashes {
        first_ms = first_ms.min(f.time_unix_ms);
        last_ms = last_ms.max(f.time_unix_ms);
    }
    if flash_count_total == 0 {
        first_ms = now_ms;
        last_ms = now_ms;
    }

    let [west, east, south, north] = bounds;
    let in_domain: Vec<&(crate_flash::Flash, String)> = all_flashes
        .iter()
        .filter(|(f, _)| {
            f.time_unix_ms >= cutoff_ms
                && (f.lat as f64) >= south
                && (f.lat as f64) <= north
                && (f.lon as f64) >= west
                && (f.lon as f64) <= east
        })
        .collect();

    let flashes_json: Vec<Value> = in_domain
        .iter()
        .map(|(f, src)| {
            json!({
                "lat": f.lat,
                "lon": f.lon,
                "time_utc": rfc3339_ms(f.time_unix_ms),
                "energy_j": f.energy,
                // rw-glm exposes area in km^2; the legacy contract is m^2.
                "area_m2": (f.area as f64) * 1.0e6,
                "source_file": src,
                "flash_id": f.flash_id,
                "degraded": f.is_degraded(),
            })
        })
        .collect();

    let generated_at = rfc3339_ms(now_ms);
    let stamp = compact_stamp(now_ms);
    let prefix = format!("lightning/{}/{}/raw", args.domain, stamp);
    let flashes_key = format!("{prefix}/glm_flashes.json");
    let flashes_path = args.artifact_root.join(&flashes_key);

    let window = json!({ "first": rfc3339_ms(first_ms), "last": rfc3339_ms(last_ms) });

    let flashes_doc = json!({
        "domain": args.domain,
        "domain_label": args.domain_label,
        "bounds": [west, east, south, north],
        "time_window": window,
        "flash_count_total": flash_count_total,
        "flash_count_in_domain": flashes_json.len(),
        "flashes": flashes_json,
    });
    write_json(&flashes_path, &flashes_doc)?;
    let flashes_size = fs::metadata(&flashes_path).map(|m| m.len()).unwrap_or(0);

    let manifest = json!({
        "schema_version": 1,
        "generated_at_utc": generated_at,
        "kind": "glm_lightning",
        "model": "goes_glm",
        "source": args.source,
        "satellite": args.satellite,
        "domain": args.domain,
        "domain_label": args.domain_label,
        "products": ["glm_lightning_flashes"],
        "forecast_hours": [0],
        "artifact_prefix": prefix,
        "time_window": window,
        "flash_count_total": flash_count_total,
        "flash_count_in_domain": flashes_doc["flash_count_in_domain"],
        "flash_count_drawn": flashes_doc["flash_count_in_domain"],
        "n_files": granules.len(),
        "latest_glm_key": latest_key,
        "latest_glm_last_modified": rfc3339_ms(latest_mtime_ms),
        "producer": "rw_glm_cafire",
        "hours": [{
            "forecast_hour": 0,
            "valid_time_utc": rfc3339_ms(last_ms),
            "uploaded": [{
                "path": flashes_path.display().to_string(),
                "key": flashes_key,
                "format": "json",
                "size_bytes": flashes_size,
            }],
        }],
    });
    let latest_path = args.artifact_root.join("lightning").join("latest.json");
    write_json(&latest_path, &manifest)?;

    let mut report = BTreeMap::new();
    report.insert("ok", json!(true));
    report.insert("worker", json!("rw_glm_cafire"));
    report.insert("granules", json!(granules.len()));
    report.insert("decoded_ok", json!(decoded_ok));
    report.insert("decode_errors", json!(decode_errors));
    report.insert("flash_count_total", json!(flash_count_total));
    report.insert("flash_count_in_domain", json!(flashes_doc["flash_count_in_domain"]));
    report.insert("generated_at_utc", json!(generated_at));
    Ok(serde_json::to_string(&report).unwrap_or_default())
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    // Write-then-rename so a reader never sees a half-written file.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(value).map_err(|e| e.to_string())?)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename -> {}: {e}", path.display()))?;
    Ok(())
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `YYYY-MM-DDTHH:MM:SS.sssZ`
fn rfc3339_ms(unix_ms: i64) -> String {
    let (y, mo, d, h, mi, s, ms) = civil_from_unix_ms(unix_ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// `YYYYMMDDTHHMMZ` — matches the legacy artifact prefix stamp.
fn compact_stamp(unix_ms: i64) -> String {
    let (y, mo, d, h, mi, _, _) = civil_from_unix_ms(unix_ms);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}Z")
}

/// Howard Hinnant's civil-from-days, adapted for Unix milliseconds.
fn civil_from_unix_ms(unix_ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let ms = unix_ms.rem_euclid(1000) as u32;
    let secs = unix_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (h, mi, s) = (
        (sod / 3600) as u32,
        ((sod % 3600) / 60) as u32,
        (sod % 60) as u32,
    );

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s, ms)
}

/// Local alias so the signature above reads clearly.
mod crate_flash {
    pub use rw_glm::reader::Flash;
}
