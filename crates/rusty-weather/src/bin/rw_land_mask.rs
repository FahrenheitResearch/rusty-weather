//! `rw_land_mask` — one-time importer of HRRR's `LAND:surface` field as the
//! climatology store's ocean-mask sidecar.
//!
//! Fetches just the LAND GRIB message (idx-subset ranged GET, ~100 KB) for
//! the given HRRR cycle, verifies the grid against the climatology store's
//! `climo_grid_meta.json` alignment contract, crops to the climo subgrid,
//! and writes `land_mask.bin` (one u8 per cell, row-major, 1 = land,
//! 0 = water) plus `land_mask.json` next to the meta sidecar. The anomaly
//! render lane (`climo_products::load_land_mask`) NaNs water cells on every
//! climatology/anomaly product; an absent sidecar renders unmasked.
//!
//! Ocean cells are masked because fire weather is a land/fuels problem and
//! RTMA is weakly observation-constrained over water — the offshore
//! percentile signal is model-background statistics that visually swamps
//! the land story. The climatology data itself stays untouched in the
//! store; this is render-time-only policy.

use std::path::{Path, PathBuf};

use clap::Parser;
use rustwx_core::{CanonicalField, CycleSpec, FieldSelector, ModelId, ModelRunRequest};
use rustwx_io::{
    FetchRequest, extract_field_values_partial_from_model_bytes_at_forecast_hour,
    fetch_bytes_with_cache,
};
use rw_store::grid::GridFile;
use serde::Deserialize;

#[derive(Debug, Parser)]
#[command(
    name = "rw-land-mask",
    about = "Fetch HRRR LAND and write the climo store's ocean-mask sidecar"
)]
struct Args {
    #[arg(long)]
    store_root: PathBuf,
    #[arg(
        long,
        help = "HRRR run supplying the LAND fetch cycle (grid must match the climo import), e.g. 20260701_00z"
    )]
    hrrr_run: String,
    #[arg(long, default_value = "hrrr")]
    hrrr_model: String,
    #[arg(long, default_value = "rtma_climo")]
    climo_model: String,
    #[arg(long, default_value = "seasonal_v2026_05_24")]
    climo_run: String,
    #[arg(long, help = "Fetch cache dir (default: system temp)")]
    cache_root: Option<PathBuf>,
}

/// The importer sidecar fields this tool needs (unknown fields ignored).
#[derive(Debug, Deserialize)]
struct ClimoGridMeta {
    schema: String,
    hrrr_grid_hash: String,
    hrrr_row0: usize,
    hrrr_row1: usize,
    hrrr_col0: usize,
    hrrr_col1: usize,
    ny: usize,
    nx: usize,
}

/// `20260701_00z` -> `("20260701", 0)`.
fn parse_run_slug(slug: &str) -> Result<(String, u8), String> {
    let (date, cycle) = slug
        .split_once('_')
        .ok_or_else(|| format!("run slug '{slug}' is not DATE_CCz"))?;
    if date.len() != 8 || !date.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("run slug '{slug}' date part must be YYYYMMDD"));
    }
    let hour = cycle
        .strip_suffix('z')
        .or_else(|| cycle.strip_suffix('Z'))
        .ok_or_else(|| format!("run slug '{slug}' cycle part must end in z"))?;
    let hour: u8 = hour
        .parse()
        .map_err(|_| format!("run slug '{slug}' cycle part must be numeric"))?;
    if hour > 23 {
        return Err(format!("run slug '{slug}' cycle {hour} out of range"));
    }
    Ok((date.to_string(), hour))
}

/// Crop a full-grid plane to `rows row0..row1, cols col0..col1`, row-major.
fn crop_subgrid(
    values: &[f32],
    full_nx: usize,
    row0: usize,
    row1: usize,
    col0: usize,
    col1: usize,
) -> Vec<f32> {
    let mut out = Vec::with_capacity((row1 - row0) * (col1 - col0));
    for row in row0..row1 {
        let base = row * full_nx;
        out.extend_from_slice(&values[base + col0..base + col1]);
    }
    out
}

/// LAND is a 0/1 land fraction; >= 0.5 is land. Non-finite values keep the
/// data visible (1) — a decode oddity must never hide real land signal.
fn encode_mask(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .map(|&v| if !v.is_finite() || v >= 0.5 { 1u8 } else { 0u8 })
        .collect()
}

fn run(args: &Args) -> Result<(), String> {
    let climo_dir = args
        .store_root
        .join(&args.climo_model)
        .join(&args.climo_run);
    let meta_path = climo_dir.join("climo_grid_meta.json");
    let meta: ClimoGridMeta = serde_json::from_str(
        &std::fs::read_to_string(&meta_path)
            .map_err(|err| format!("read {}: {err}", meta_path.display()))?,
    )
    .map_err(|err| format!("{}: {err}", meta_path.display()))?;
    if meta.schema != "cafire.rtma_climo_grid_meta.v1" {
        return Err(format!("unsupported climo grid meta schema {}", meta.schema));
    }
    if (meta.hrrr_row1 - meta.hrrr_row0, meta.hrrr_col1 - meta.hrrr_col0) != (meta.ny, meta.nx) {
        return Err("climo grid meta crop offsets disagree with its ny/nx".to_string());
    }

    let grid_path = args
        .store_root
        .join(&args.hrrr_model)
        .join(&args.hrrr_run)
        .join("grid.rwg");
    let hrrr = GridFile::open(&grid_path)
        .map_err(|err| format!("open {}: {err}", grid_path.display()))?;
    if hrrr.hash != meta.hrrr_grid_hash {
        return Err(format!(
            "HRRR run {} grid hash does not match the climatology import — \
             fetch from the run the climo was imported against",
            args.hrrr_run
        ));
    }

    let (date, cycle) = parse_run_slug(&args.hrrr_run)?;
    let fetch = FetchRequest {
        request: ModelRunRequest::new(
            ModelId::Hrrr,
            CycleSpec::new(date, cycle).map_err(|err| err.to_string())?,
            0,
            "sfc",
        )
        .map_err(|err| err.to_string())?,
        source_override: None,
        variable_patterns: vec!["LAND:surface".to_string()],
    };
    let cache_root = args
        .cache_root
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("rw_land_mask_cache"));
    let fetched = fetch_bytes_with_cache(&fetch, &cache_root, true)
        .map_err(|err| format!("fetch LAND: {err}"))?;
    println!(
        "fetched {} bytes from {} ({})",
        fetched.result.bytes.len(),
        fetched.result.url,
        if fetched.cache_hit { "cache hit" } else { "network" },
    );

    let extraction = extract_field_values_partial_from_model_bytes_at_forecast_hour(
        ModelId::Hrrr,
        &fetched.result.bytes,
        None,
        &[FieldSelector::surface(CanonicalField::LandSeaMask)],
        Some(0),
    )
    .map_err(|err| format!("decode LAND: {err}"))?;
    let field = extraction
        .extracted
        .first()
        .ok_or("fetched bytes carry no LAND:surface message")?;
    let grid = &extraction.grids[field.grid_index].grid;
    if (grid.shape.nx, grid.shape.ny) != (hrrr.nx, hrrr.ny) {
        return Err(format!(
            "LAND grid is {}x{} but the store's HRRR grid is {}x{}",
            grid.shape.ny, grid.shape.nx, hrrr.ny, hrrr.nx
        ));
    }

    let cropped = crop_subgrid(
        &field.values,
        hrrr.nx,
        meta.hrrr_row0,
        meta.hrrr_row1,
        meta.hrrr_col0,
        meta.hrrr_col1,
    );
    let mask = encode_mask(&cropped);
    let land = mask.iter().filter(|&&cell| cell == 1).count();
    let water = mask.len() - land;

    std::fs::write(climo_dir.join("land_mask.bin"), &mask)
        .map_err(|err| format!("write land_mask.bin: {err}"))?;
    write_header(&climo_dir, &meta, land, water, &fetched.result.url)?;
    println!(
        "land_mask written: {}x{} = {} cells, {land} land ({:.1}%), {water} water",
        meta.ny,
        meta.nx,
        mask.len(),
        100.0 * land as f64 / mask.len() as f64,
    );
    Ok(())
}

fn write_header(
    climo_dir: &Path,
    meta: &ClimoGridMeta,
    land: usize,
    water: usize,
    source_url: &str,
) -> Result<(), String> {
    std::fs::write(
        climo_dir.join("land_mask.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "cafire.rtma_climo_land_mask.v1",
            "ny": meta.ny,
            "nx": meta.nx,
            "land_cells": land,
            "water_cells": water,
            "hrrr_grid_hash": meta.hrrr_grid_hash,
            "source": source_url,
            "rule": "HRRR LAND:surface >= 0.5, non-finite kept as land",
        }))
        .map_err(|err| err.to_string())?,
    )
    .map_err(|err| format!("write land_mask.json: {err}"))
}

fn main() {
    let args = Args::parse();
    if let Err(err) = run(&args) {
        eprintln!("rw_land_mask: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_slug_parses_date_and_cycle() {
        assert_eq!(
            parse_run_slug("20260701_00z").unwrap(),
            ("20260701".to_string(), 0)
        );
        assert_eq!(
            parse_run_slug("20251231_18z").unwrap(),
            ("20251231".to_string(), 18)
        );
        assert!(parse_run_slug("20260701").is_err(), "missing cycle");
        assert!(parse_run_slug("2026071_00z").is_err(), "short date");
        assert!(parse_run_slug("20260701_00").is_err(), "missing z");
        assert!(parse_run_slug("20260701_25z").is_err(), "cycle out of range");
    }

    #[test]
    fn crop_extracts_the_requested_window() {
        // 3x4 grid, values = row*10 + col.
        let full: Vec<f32> = (0..3)
            .flat_map(|row| (0..4).map(move |col| (row * 10 + col) as f32))
            .collect();
        let cropped = crop_subgrid(&full, 4, 1, 3, 1, 3);
        assert_eq!(cropped, vec![11.0, 12.0, 21.0, 22.0]);
    }

    #[test]
    fn mask_thresholds_at_half_and_keeps_non_finite() {
        let mask = encode_mask(&[0.0, 1.0, 0.49, 0.5, f32::NAN]);
        assert_eq!(mask, vec![0, 1, 0, 1, 1]);
    }
}
