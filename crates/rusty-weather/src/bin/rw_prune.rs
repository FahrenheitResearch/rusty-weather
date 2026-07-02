//! `rw_prune` — store retention for unattended operation.
//!
//! Keep policy (per model): the newest `--keep-recent` runs (any length)
//! plus the newest "long" run (max stored hour >= `--long-hours`), so the
//! serving node always holds one extended run and the freshest short ones.
//! Everything else under `<store>/<model>/` that looks like a run dir
//! (`YYYYMMDD_CCz`) and is older than `--min-age-hours` is deleted.
//!
//! The climatology model dirs (`rtma_climo`, or anything not named by
//! `--model`) are never touched. `--fetch-cache` optionally prunes files
//! older than `--cache-max-age-days` from a fetch-cache dir — the past
//! Hetzner outage was a disk filled by stale caches, so cache pruning is
//! part of retention, not an afterthought.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "rw-prune", about = "Prune old model runs and fetch caches")]
struct Args {
    #[arg(long)]
    store_root: PathBuf,
    #[arg(long, default_value = "hrrr")]
    model: String,
    #[arg(long, default_value_t = 2, help = "Newest runs kept regardless of length")]
    keep_recent: usize,
    #[arg(long, default_value_t = 30, help = "Max stored hour >= this marks a 'long' run")]
    long_hours: u16,
    #[arg(long, default_value_t = 6, help = "Never delete runs newer than this many hours")]
    min_age_hours: u64,
    #[arg(long, help = "Also prune files in this fetch-cache dir")]
    fetch_cache: Option<PathBuf>,
    #[arg(long, default_value_t = 3)]
    cache_max_age_days: u64,
    #[arg(long)]
    dry_run: bool,
}

/// A run dir eligible for retention decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunInfo {
    slug: String,
    /// Sortable key: (yyyymmdd, cycle).
    key: (u32, u8),
    max_hour: u16,
}

fn parse_run_slug(slug: &str) -> Option<(u32, u8)> {
    let (date, cycle) = slug.split_once('_')?;
    if date.len() != 8 {
        return None;
    }
    let date: u32 = date.parse().ok()?;
    let cycle: u8 = cycle.strip_suffix('z').or_else(|| cycle.strip_suffix('Z'))?.parse().ok()?;
    (cycle <= 23).then_some((date, cycle))
}

/// Which runs to keep: the newest `keep_recent` plus the newest long run.
fn keep_set(runs: &[RunInfo], keep_recent: usize, long_hours: u16) -> Vec<String> {
    let mut sorted: Vec<&RunInfo> = runs.iter().collect();
    sorted.sort_by(|a, b| b.key.cmp(&a.key));
    let mut keep: Vec<String> = sorted
        .iter()
        .take(keep_recent)
        .map(|run| run.slug.clone())
        .collect();
    if let Some(long) = sorted.iter().find(|run| run.max_hour >= long_hours) {
        if !keep.contains(&long.slug) {
            keep.push(long.slug.clone());
        }
    }
    keep
}

fn stored_max_hour(run_dir: &Path) -> u16 {
    std::fs::read_dir(run_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix('f')?.strip_suffix(".rws")?.parse::<u16>().ok()
        })
        .max()
        .unwrap_or(0)
}

fn dir_age(path: &Path) -> Duration {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .unwrap_or(Duration::ZERO)
}

fn main() -> Result<(), String> {
    let args = Args::parse();
    let model_dir = args.store_root.join(&args.model);
    let mut runs: Vec<RunInfo> = Vec::new();
    for entry in std::fs::read_dir(&model_dir)
        .map_err(|err| format!("read {}: {err}", model_dir.display()))?
        .filter_map(|entry| entry.ok())
    {
        if !entry.path().is_dir() {
            continue;
        }
        let slug = entry.file_name().to_string_lossy().into_owned();
        let Some(key) = parse_run_slug(&slug) else { continue };
        runs.push(RunInfo {
            max_hour: stored_max_hour(&entry.path()),
            slug,
            key,
        });
    }
    if runs.is_empty() {
        println!("no run dirs under {}", model_dir.display());
        return Ok(());
    }

    let keep = keep_set(&runs, args.keep_recent, args.long_hours);
    let mut deleted = 0usize;
    let mut freed = 0u64;
    for run in &runs {
        if keep.contains(&run.slug) {
            println!("keep   {} (F{:03} max)", run.slug, run.max_hour);
            continue;
        }
        let path = model_dir.join(&run.slug);
        let age_hours = dir_age(&path).as_secs() / 3600;
        if age_hours < args.min_age_hours {
            println!("skip   {} (only {age_hours}h old)", run.slug);
            continue;
        }
        let bytes: u64 = walk_size(&path);
        if args.dry_run {
            println!("would delete {} ({:.2} GB)", run.slug, bytes as f64 / 1e9);
        } else {
            std::fs::remove_dir_all(&path)
                .map_err(|err| format!("delete {}: {err}", path.display()))?;
            println!("deleted {} ({:.2} GB)", run.slug, bytes as f64 / 1e9);
        }
        deleted += 1;
        freed += bytes;
    }

    if let Some(cache) = &args.fetch_cache {
        let cutoff = Duration::from_secs(args.cache_max_age_days * 86_400);
        let (cache_files, cache_bytes) = prune_cache(cache, cutoff, args.dry_run)?;
        println!(
            "cache: {} {} file(s), {:.2} GB",
            if args.dry_run { "would delete" } else { "deleted" },
            cache_files,
            cache_bytes as f64 / 1e9
        );
        freed += cache_bytes;
    }

    println!(
        "{}: {} run(s) removed, {:.2} GB reclaimed",
        if args.dry_run { "dry-run" } else { "done" },
        deleted,
        freed as f64 / 1e9
    );
    Ok(())
}

fn walk_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

fn prune_cache(cache: &Path, max_age: Duration, dry_run: bool) -> Result<(usize, u64), String> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![cache.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let age = meta
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .unwrap_or(Duration::ZERO);
            if age > max_age {
                if !dry_run {
                    std::fs::remove_file(&path)
                        .map_err(|err| format!("delete {}: {err}", path.display()))?;
                }
                files += 1;
                bytes += meta.len();
            }
        }
    }
    Ok((files, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(slug: &str, max_hour: u16) -> RunInfo {
        RunInfo {
            slug: slug.to_string(),
            key: parse_run_slug(slug).unwrap(),
            max_hour,
        }
    }

    #[test]
    fn keep_newest_recent_plus_newest_long() {
        let runs = vec![
            run("20260629_03z", 3),
            run("20260630_12z", 48),
            run("20260701_00z", 30),
            run("20260701_03z", 18),
            run("20260701_04z", 18),
        ];
        let keep = keep_set(&runs, 2, 30);
        // Newest two: 04z + 03z; newest long (>=F030): 20260701_00z.
        assert!(keep.contains(&"20260701_04z".to_string()));
        assert!(keep.contains(&"20260701_03z".to_string()));
        assert!(keep.contains(&"20260701_00z".to_string()));
        assert_eq!(keep.len(), 3);
        // The 48h run from yesterday and the tiny dev run go.
        assert!(!keep.contains(&"20260630_12z".to_string()));
        assert!(!keep.contains(&"20260629_03z".to_string()));
    }

    #[test]
    fn long_run_in_recent_set_is_not_double_kept() {
        let runs = vec![run("20260701_00z", 48), run("20260630_18z", 18)];
        let keep = keep_set(&runs, 2, 30);
        assert_eq!(keep.len(), 2, "long run already in recent set");
    }

    #[test]
    fn slug_parse_rejects_junk() {
        assert!(parse_run_slug("20260701_00z").is_some());
        assert!(parse_run_slug(".rw-lock").is_none());
        assert!(parse_run_slug("latest.json").is_none());
        assert!(parse_run_slug("20260701_25z").is_none());
    }
}
