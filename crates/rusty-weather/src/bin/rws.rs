//! `rws` — Swiss-army CLI for rw-store hour files, run directories, and
//! store roots.
//!
//! Subcommands:
//!
//! ```text
//! rws ls   <path> [--json]
//! rws dump <hour.rws> [--var NAME] [--json]
//! rws validate <path> [--deep] [--json]
//! rws diff <a.rws> <b.rws>
//! rws export <hour.rws> [--grid <grid.rwg>] -o <out.nc> [--vars a,b,c]
//! ```
//!
//! Path-type detection for `ls` and `validate`:
//! - `.rws` extension → hour file
//! - directory containing `run.json` → run directory
//! - any other directory → store root / model directory; recursively searched
//!   for `run.json` files up to depth 2

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde_json::json;

use rw_store::diff::{Difference, compare};
use rw_store::export_hour_to_netcdf3;
use rw_store::format::{
    FLAG_CONSTANT, FLAG_EMPTY, FLAG_HAS_MISSING, HEADER_LEN, INDEX_RECORD_LEN, KIND_TILE2D,
};
use rw_store::grid::GridFile;
use rw_store::header::RwsHeader;
use rw_store::index::ChunkRecord;
use rw_store::reader::HourReader;
use rw_store::run::RwsRunManifest;
use rw_store::validate::{ValidateDepth, validate_hour_file, validate_run_dir};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "rws",
    about = "rw-store CLI: ls / dump / validate / diff / export hour files"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// List contents of a store root, model dir, or run dir
    Ls(LsArgs),
    /// Show header and meta for a .rws hour file; optionally show per-var index records
    Dump(DumpArgs),
    /// Validate a .rws file or run directory
    Validate(ValidateArgs),
    /// Structurally compare two .rws hour files (exit 0 = same, 1 = different)
    Diff(DiffArgs),
    /// Export a .rws hour file + grid to NetCDF3
    Export(ExportArgs),
}

#[derive(Debug, Args)]
struct LsArgs {
    /// Store root, model dir, or run dir (directory with run.json)
    path: PathBuf,
    /// Output machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DumpArgs {
    /// Hour file (.rws)
    path: PathBuf,
    /// Show index records for this variable only
    #[arg(long = "var")]
    var_name: Option<String>,
    /// Output machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ValidateArgs {
    /// .rws hour file or run directory
    path: PathBuf,
    /// Deep validation: decompress every chunk and cross-check statistics
    #[arg(long)]
    deep: bool,
    /// Output machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DiffArgs {
    /// First hour file
    a: PathBuf,
    /// Second hour file
    b: PathBuf,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Hour file (.rws)
    hour: PathBuf,
    /// Grid file (.rwg); defaults to grid.rwg next to the hour file
    #[arg(long)]
    grid: Option<PathBuf>,
    /// Output NetCDF3 file
    #[arg(short = 'o', long)]
    out: PathBuf,
    /// Comma-separated variable names to export (default: all)
    #[arg(long)]
    vars: Option<String>,
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Ls(args) => cmd_ls(args),
        Cmd::Dump(args) => cmd_dump(args),
        Cmd::Validate(args) => cmd_validate(args),
        Cmd::Diff(args) => cmd_diff(args),
        Cmd::Export(args) => cmd_export(args),
    }
}

// ── ls ────────────────────────────────────────────────────────────────────────

/// One hour entry collected for human-readable `ls` output.
struct HourListing {
    hour: u16,
    file: String,
    variables: Vec<String>,
    file_bytes: Option<u64>,
}

/// One run directory's manifest summary collected for human-readable `ls` output.
struct RunListing {
    model: String,
    run: String,
    grid_hash: String,
    nx: usize,
    ny: usize,
    writer_build: String,
    hours: Vec<HourListing>,
}

fn cmd_ls(args: LsArgs) -> ExitCode {
    let path = &args.path;

    // Collect run directories to list.
    let run_dirs = collect_run_dirs(path);
    if run_dirs.is_empty() {
        eprintln!("error: no run directories found under {}", path.display());
        return ExitCode::from(2);
    }

    let mut runs: Vec<serde_json::Value> = Vec::new();
    let mut run_displays: Vec<RunListing> = Vec::new();
    let mut any_error = false;

    for run_dir in &run_dirs {
        let manifest_path = run_dir.join("run.json");
        match std::fs::read(&manifest_path) {
            Err(e) => {
                eprintln!("error: read {}: {e}", manifest_path.display());
                any_error = true;
                continue;
            }
            Ok(bytes) => match serde_json::from_slice::<RwsRunManifest>(&bytes) {
                Err(e) => {
                    eprintln!("error: parse {}: {e}", manifest_path.display());
                    any_error = true;
                    continue;
                }
                Ok(manifest) => {
                    let hours: Vec<HourListing> = manifest
                        .hours
                        .iter()
                        .map(|(&hour, entry)| {
                            let fpath = run_dir.join(&entry.file);
                            let file_bytes = std::fs::metadata(&fpath).ok().map(|m| m.len());
                            HourListing {
                                hour,
                                file: entry.file.clone(),
                                variables: entry.variables.clone(),
                                file_bytes,
                            }
                        })
                        .collect();
                    let hour_json: Vec<serde_json::Value> = hours
                        .iter()
                        .map(|h| {
                            json!({
                                "hour": h.hour, "file": h.file, "variables": h.variables,
                                "file_bytes": h.file_bytes
                            })
                        })
                        .collect();
                    runs.push(json!({
                        "model": manifest.model,
                        "run": manifest.run,
                        "grid_hash": manifest.grid_hash,
                        "nx": manifest.nx,
                        "ny": manifest.ny,
                        "writer_build": manifest.writer.build,
                        "hours": hour_json,
                    }));
                    run_displays.push(RunListing {
                        model: manifest.model,
                        run: manifest.run,
                        grid_hash: manifest.grid_hash,
                        nx: manifest.nx,
                        ny: manifest.ny,
                        writer_build: manifest.writer.build,
                        hours,
                    });
                }
            },
        }
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&runs).unwrap());
    } else {
        for listing in &run_displays {
            println!(
                "model={} run={} grid={}x{} build={}",
                listing.model, listing.run, listing.ny, listing.nx, listing.writer_build
            );
            println!("  grid_hash: {}", listing.grid_hash);
            for hour in &listing.hours {
                let size_str = match hour.file_bytes {
                    Some(b) => format!("{b} B"),
                    None => "?".to_string(),
                };
                println!(
                    "  f{:03}  {}  {}  vars: {}",
                    hour.hour,
                    hour.file,
                    size_str,
                    hour.variables.join(", ")
                );
            }
        }
    }

    if any_error {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

/// Collect run directories (those containing run.json) at or under `path`.
/// Searches depth ≤ 2 for non-run-dir inputs (store root or model dir).
fn collect_run_dirs(path: &Path) -> Vec<PathBuf> {
    if !path.is_dir() {
        return Vec::new();
    }
    // If this directory itself has a run.json, it is a run dir.
    if path.join("run.json").exists() {
        return vec![path.to_path_buf()];
    }
    // Walk up to depth 2 looking for run.json files.
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if !child.is_dir() {
                continue;
            }
            if child.join("run.json").exists() {
                result.push(child.clone());
            } else if let Ok(grandchildren) = std::fs::read_dir(&child) {
                for gc in grandchildren.flatten() {
                    let gc_path = gc.path();
                    if gc_path.is_dir() && gc_path.join("run.json").exists() {
                        result.push(gc_path);
                    }
                }
            }
        }
    }
    result.sort();
    result
}

// ── dump ──────────────────────────────────────────────────────────────────────

fn cmd_dump(args: DumpArgs) -> ExitCode {
    // Parse the file manually to access raw index records for the --var display.
    let bytes = match std::fs::read(&args.path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: read {}: {e}", args.path.display());
            return ExitCode::from(2);
        }
    };
    let header = match RwsHeader::parse(&bytes) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("error: {}: {e}", args.path.display());
            return ExitCode::from(2);
        }
    };

    // Parse meta JSON.
    let meta_start = HEADER_LEN;
    let meta_end = meta_start + header.meta_len as usize;
    if bytes.len() < meta_end {
        eprintln!("error: {}: meta region truncated", args.path.display());
        return ExitCode::from(2);
    }
    let meta: rw_store::format::RwsHourMeta =
        match serde_json::from_slice(&bytes[meta_start..meta_end]) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {}: meta JSON: {e}", args.path.display());
                return ExitCode::from(2);
            }
        };

    // Parse all index records.
    let n = header.index_count as usize;
    let mut records: Vec<ChunkRecord> = Vec::with_capacity(n);
    for i in 0..n {
        let start = header.index_offset as usize + i * INDEX_RECORD_LEN;
        match ChunkRecord::unpack(&bytes[start..start + INDEX_RECORD_LEN]) {
            Ok(r) => records.push(r),
            Err(e) => {
                eprintln!("error: {}: index record {i}: {e}", args.path.display());
                return ExitCode::from(2);
            }
        }
    }

    // If --var requested, filter records to that variable.
    if let Some(ref name) = args.var_name {
        let var_meta = match meta.variables.iter().find(|v| &v.name == name) {
            Some(v) => v,
            None => {
                let available: Vec<&str> = meta.variables.iter().map(|v| v.name.as_str()).collect();
                eprintln!(
                    "error: variable '{name}' not found; available: {}",
                    available.join(", ")
                );
                return ExitCode::from(2);
            }
        };
        let var_records: Vec<&ChunkRecord> =
            records.iter().filter(|r| r.var_id == var_meta.id).collect();

        if args.json {
            let json_records: Vec<serde_json::Value> = var_records
                .iter()
                .map(|r| {
                    json!({
                        "tile_y": r.tile_y, "tile_x": r.tile_x,
                        "kind": if r.kind == KIND_TILE2D { "TILE2D" } else { "COLUMN3D" },
                        "flags": flags_str(r.flags),
                        "len": r.len, "raw_len": r.raw_len,
                        "min": nan_to_null(r.min), "max": nan_to_null(r.max),
                        "valid_count": r.valid_count,
                    })
                })
                .collect();
            let out = json!({
                "file": args.path.display().to_string(),
                "variable": var_meta.name,
                "records": json_records
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        } else {
            println!(
                "var={} id={} kind={} codec={} levels={}",
                var_meta.name,
                var_meta.id,
                var_meta.kind,
                var_meta.codec,
                var_meta.levels_hpa.len()
            );
            println!(
                "  {:>6}  {:>6}  {:>8}  {:>8}  {:>8}  {:>12}  {:>12}  {:>12}",
                "tile_y", "tile_x", "flags", "len", "raw_len", "min", "max", "valid_count"
            );
            for r in &var_records {
                let kind_ch = if r.kind == KIND_TILE2D { 'T' } else { 'C' };
                println!(
                    "  {:>6}  {:>6}  {:>8}  {:>8}  {:>8}  {:>12}  {:>12}  {:>12}",
                    r.tile_y,
                    r.tile_x,
                    format!("{kind_ch}{}", flags_str(r.flags)),
                    r.len,
                    r.raw_len,
                    fmt_f32(r.min),
                    fmt_f32(r.max),
                    r.valid_count,
                );
            }
        }
        return ExitCode::SUCCESS;
    }

    // Summary dump: header + meta overview + per-variable table.
    if args.json {
        let vars: Vec<serde_json::Value> = meta
            .variables
            .iter()
            .map(|v| {
                let chunk_count = records.iter().filter(|r| r.var_id == v.id).count();
                json!({
                    "id": v.id, "name": v.name, "units": v.units,
                    "kind": v.kind, "codec": v.codec,
                    "levels": v.levels_hpa,
                    "chunk_count": chunk_count,
                })
            })
            .collect();
        let out = json!({
            "file": args.path.display().to_string(),
            "header": {
                "version": header.version,
                "meta_len": header.meta_len,
                "index_count": header.index_count,
                "index_offset": header.index_offset,
                "payload_offset": header.payload_offset,
            },
            "meta": {
                "schema": meta.schema,
                "model": meta.model,
                "run": meta.run,
                "forecast_hour": meta.forecast_hour,
                "nx": meta.nx,
                "ny": meta.ny,
                "grid_hash": meta.grid_hash,
                "chunking": meta.chunking,
                "writer": meta.writer,
            },
            "variables": vars,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        println!("file:      {}", args.path.display());
        println!("schema:    {}", meta.schema);
        println!("model:     {}", meta.model);
        println!("run:       {}", meta.run);
        println!("fh:        {}", meta.forecast_hour);
        println!("grid:      {}x{}", meta.ny, meta.nx);
        println!("grid_hash: {}", meta.grid_hash);
        println!(
            "writer:    {} {} (build: {})",
            meta.writer.name, meta.writer.version, meta.writer.build
        );
        println!("index:     {} records", header.index_count);
        println!(
            "payload:   {} B",
            bytes.len() - header.payload_offset as usize
        );
        println!();
        println!(
            "  {:>3}  {:>12}  {:>10}  {:>16}  {:>6}  {:>6}",
            "id", "name", "kind", "codec", "levels", "chunks"
        );
        for var in &meta.variables {
            let chunk_count = records.iter().filter(|r| r.var_id == var.id).count();
            println!(
                "  {:>3}  {:>12}  {:>10}  {:>16}  {:>6}  {:>6}",
                var.id,
                var.name,
                var.kind,
                var.codec,
                var.levels_hpa.len(),
                chunk_count
            );
        }
    }
    ExitCode::SUCCESS
}

fn flags_str(flags: u8) -> String {
    let mut s = String::new();
    if flags & FLAG_EMPTY != 0 {
        s.push('E');
    }
    if flags & FLAG_CONSTANT != 0 {
        s.push('C');
    }
    if flags & FLAG_HAS_MISSING != 0 {
        s.push('M');
    }
    if s.is_empty() {
        s.push('-');
    }
    s
}

fn fmt_f32(v: f32) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else {
        format!("{v:.4}")
    }
}

fn nan_to_null(v: f32) -> serde_json::Value {
    if v.is_nan() {
        serde_json::Value::Null
    } else {
        json!(v)
    }
}

// ── validate ──────────────────────────────────────────────────────────────────

fn cmd_validate(args: ValidateArgs) -> ExitCode {
    let depth = if args.deep {
        ValidateDepth::Deep
    } else {
        ValidateDepth::Structural
    };

    let report = if args.path.is_file() || args.path.extension().is_some_and(|e| e == "rws") {
        // Single file.
        match validate_hour_file(&args.path, depth) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {}: {e}", args.path.display());
                return ExitCode::from(2);
            }
        }
    } else if args.path.is_dir() {
        match validate_run_dir(&args.path, depth) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {}: {e}", args.path.display());
                return ExitCode::from(2);
            }
        }
    } else {
        eprintln!("error: {}: not a file or directory", args.path.display());
        return ExitCode::from(2);
    };

    if args.json {
        let out = json!({
            "path": args.path.display().to_string(),
            "ok": report.is_ok(),
            "errors": report.errors,
            "warnings": report.warnings,
            "stats": {
                "variables": report.stats.variables,
                "chunks": report.stats.chunks,
                "payload_bytes": report.stats.payload_bytes,
            }
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        for e in &report.errors {
            println!("error: {e}");
        }
        for w in &report.warnings {
            println!("warning: {w}");
        }
        println!(
            "stats: variables={} chunks={} payload_bytes={}",
            report.stats.variables, report.stats.chunks, report.stats.payload_bytes
        );
        if report.is_ok() {
            println!("ok");
        } else {
            println!("FAILED ({} error(s))", report.errors.len());
        }
    }

    if report.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ── diff ──────────────────────────────────────────────────────────────────────

fn cmd_diff(args: DiffArgs) -> ExitCode {
    match compare(&args.a, &args.b) {
        Ok(()) => {
            println!("equivalent: payload + index + meta (writer.build excluded) match");
            ExitCode::SUCCESS
        }
        Err(Difference::Io(msg)) => {
            eprintln!("error: {msg}");
            ExitCode::from(2)
        }
        Err(Difference::Found(msg)) => {
            eprintln!("DIFFERENT: {msg}");
            ExitCode::FAILURE
        }
    }
}

// ── export ────────────────────────────────────────────────────────────────────

fn cmd_export(args: ExportArgs) -> ExitCode {
    // Open the hour file.
    let hour = match HourReader::open(&args.hour) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("error: open {}: {e}", args.hour.display());
            return ExitCode::from(2);
        }
    };

    // Resolve the grid path.
    let grid_path = match &args.grid {
        Some(p) => p.clone(),
        None => {
            // Default: grid.rwg next to the hour file.
            match args.hour.parent() {
                Some(parent) => parent.join("grid.rwg"),
                None => PathBuf::from("grid.rwg"),
            }
        }
    };
    let grid = match GridFile::open(&grid_path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: open grid {}: {e}", grid_path.display());
            return ExitCode::from(2);
        }
    };

    // Parse --vars.
    let vars: Option<Vec<String>> = args.vars.map(|v| {
        v.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    });

    match export_hour_to_netcdf3(&hour, &grid, vars.as_deref(), &args.out) {
        Ok(summary) => {
            println!(
                "exported {} variable(s), {} bytes → {}",
                summary.variables,
                summary.bytes_written,
                args.out.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: export: {e}");
            ExitCode::FAILURE
        }
    }
}
