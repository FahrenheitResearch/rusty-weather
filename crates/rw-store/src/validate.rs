//! Validation library for rw-store hour files and run directories.
//!
//! Two depth levels: [`ValidateDepth::Structural`] checks the binary layout,
//! metadata, index geometry, and payload bounds without reading any compressed
//! bytes; [`ValidateDepth::Deep`] additionally decompresses every non-empty
//! chunk and verifies the decoded content against the index statistics.
//!
//! [`validate_hour_file`] returns `Ok(report)` for any file that opens —
//! format problems land in `report.errors` so a CLI can print them all rather
//! than stopping at the first. `Err(_)` is reserved for I/O failures.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::codec::MISSING_Q;
use crate::error::RwResult;
use crate::format::{
    CODEC_2D, CODEC_3D, COL_X, COL_Y, FLAG_CONSTANT, FLAG_EMPTY, FLAG_HAS_MISSING, HEADER_LEN,
    INDEX_RECORD_LEN, KIND_COLUMN3D, KIND_TILE2D, RwsHourMeta, SCHEMA_HOUR, TILE_X, TILE_Y,
};
use crate::grid::GridFile;
use crate::header::RwsHeader;
use crate::index::ChunkRecord;
use crate::run::{SCHEMA_RUN, RwsRunManifest};

/// How deeply to validate a store file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidateDepth {
    /// Header, meta, index geometry, payload bounds — no decompression.
    Structural,
    /// Structural + decompress every chunk, verify raw_len, stats, flags.
    Deep,
}

/// Aggregate result of validating one or more store files.
#[derive(Debug, Default)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub stats: ValidationStats,
}

/// Counts collected during validation.
#[derive(Debug, Default)]
pub struct ValidationStats {
    pub variables: usize,
    pub chunks: u64,
    pub payload_bytes: u64,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    fn merge_prefixed(&mut self, prefix: &str, other: ValidationReport) {
        for e in other.errors {
            self.errors.push(format!("{prefix}: {e}"));
        }
        for w in other.warnings {
            self.warnings.push(format!("{prefix}: {w}"));
        }
        self.stats.variables += other.stats.variables;
        self.stats.chunks += other.stats.chunks;
        self.stats.payload_bytes += other.stats.payload_bytes;
    }
}

/// Validate one `.rws` hour file at the requested depth.
///
/// Returns `Err` only for I/O failure opening the file. All format problems
/// are reported in `report.errors`.
pub fn validate_hour_file(path: &Path, depth: ValidateDepth) -> RwResult<ValidationReport> {
    let data = std::fs::read(path)?;
    let mut report = ValidationReport::default();
    check_hour_file(&data, depth, &mut report);
    Ok(report)
}

/// Validate all hour files referenced by the run manifest in `run_dir`, plus
/// the directory structure itself (run.json, grid.rwg, stray files).
pub fn validate_run_dir(run_dir: &Path, depth: ValidateDepth) -> RwResult<ValidationReport> {
    let mut report = ValidationReport::default();

    // 1. run.json
    let manifest_path = run_dir.join("run.json");
    let manifest_bytes = match std::fs::read(&manifest_path) {
        Ok(b) => b,
        Err(err) => {
            report.error(format!("run.json: I/O error: {err}"));
            return Ok(report);
        }
    };
    let manifest: RwsRunManifest = match serde_json::from_slice(&manifest_bytes) {
        Ok(m) => m,
        Err(err) => {
            report.error(format!("run.json: JSON parse error: {err}"));
            return Ok(report);
        }
    };
    if manifest.schema != SCHEMA_RUN {
        report.error(format!(
            "run.json: unexpected schema '{}' (expected '{SCHEMA_RUN}')",
            manifest.schema
        ));
    }

    // 2. grid.rwg
    let grid_path = run_dir.join("grid.rwg");
    let grid_file = match GridFile::open(&grid_path) {
        Ok(g) => Some(g),
        Err(err) => {
            report.error(format!("grid.rwg: {err}"));
            None
        }
    };
    if let Some(ref grid) = grid_file {
        if grid.hash != manifest.grid_hash {
            report.error(format!(
                "grid.rwg: sha256 {} does not match manifest grid_hash {}",
                grid.hash, manifest.grid_hash
            ));
        }
        if grid.nx != manifest.nx || grid.ny != manifest.ny {
            report.error(format!(
                "grid.rwg: dimensions {}x{} do not match manifest nx={} ny={}",
                grid.nx, grid.ny, manifest.nx, manifest.ny
            ));
        }
    }

    // 3. Validate each registered hour file.
    let mut referenced_files: HashSet<String> = HashSet::new();
    for (hour, entry) in &manifest.hours {
        referenced_files.insert(entry.file.clone());
        let hour_path = run_dir.join(&entry.file);
        if !hour_path.exists() {
            report.error(format!("hour {hour}: file '{}' not found", entry.file));
            continue;
        }
        let hour_data = match std::fs::read(&hour_path) {
            Ok(b) => b,
            Err(err) => {
                report.error(format!("hour {hour}: I/O error reading '{}': {err}", entry.file));
                continue;
            }
        };
        let mut hour_report = ValidationReport::default();
        check_hour_file(&hour_data, depth, &mut hour_report);
        report.merge_prefixed(&entry.file, hour_report);

        // Cross-check hour meta against manifest.
        if let Ok(header) = RwsHeader::parse(&hour_data) {
            let meta_end = HEADER_LEN + header.meta_len as usize;
            if meta_end <= hour_data.len() {
                if let Ok(meta) =
                    serde_json::from_slice::<RwsHourMeta>(&hour_data[HEADER_LEN..meta_end])
                {
                    if meta.model != manifest.model {
                        report.error(format!(
                            "{}: hour meta model '{}' != manifest model '{}'",
                            entry.file, meta.model, manifest.model
                        ));
                    }
                    if meta.run != manifest.run {
                        report.error(format!(
                            "{}: hour meta run '{}' != manifest run '{}'",
                            entry.file, meta.run, manifest.run
                        ));
                    }
                    if meta.grid_hash != manifest.grid_hash {
                        report.error(format!(
                            "{}: hour meta grid_hash '{}' != manifest grid_hash '{}'",
                            entry.file, meta.grid_hash, manifest.grid_hash
                        ));
                    }
                    if meta.nx != manifest.nx || meta.ny != manifest.ny {
                        report.error(format!(
                            "{}: hour meta {}x{} != manifest {}x{}",
                            entry.file, meta.nx, meta.ny, manifest.nx, manifest.ny
                        ));
                    }
                    // Check that all manifest-listed vars are present in the hour file.
                    let hour_var_names: HashSet<&str> =
                        meta.variables.iter().map(|v| v.name.as_str()).collect();
                    for var_name in &entry.variables {
                        if !hour_var_names.contains(var_name.as_str()) {
                            report.error(format!(
                                "{}: manifest lists variable '{}' but it is not in the hour file",
                                entry.file, var_name
                            ));
                        }
                    }
                    // Hour file has more variables than the manifest entry lists -> stale manifest.
                    let manifest_var_set: HashSet<&str> =
                        entry.variables.iter().map(|v| v.as_str()).collect();
                    for var in &meta.variables {
                        if !manifest_var_set.contains(var.name.as_str()) {
                            report.warn(format!(
                                "{}: hour file variable '{}' is not listed in the manifest entry (stale manifest?)",
                                entry.file, var.name
                            ));
                        }
                    }
                }
            }
        }
    }

    // 4. Stray .rws files not referenced by the manifest.
    let entries = match std::fs::read_dir(run_dir) {
        Ok(e) => e,
        Err(err) => {
            report.error(format!("run dir read: {err}"));
            return Ok(report);
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Ignore lock and temp files.
        if name == ".rw-lock" || name.starts_with('.') && name.contains(".tmp-") {
            continue;
        }
        if name.ends_with(".rws") && !referenced_files.contains(&name) {
            report.warn(format!("stray file '{}' not referenced by run.json", name));
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Core hour-file checker (operates on in-memory bytes)
// ---------------------------------------------------------------------------

fn check_hour_file(data: &[u8], depth: ValidateDepth, report: &mut ValidationReport) {
    let file_len = data.len() as u64;

    // Check 1: header parses.
    let header = match RwsHeader::parse(data) {
        Ok(h) => h,
        Err(err) => {
            report.error(format!("header: {err}"));
            return;
        }
    };

    // Check 2: meta region.
    let meta_start = HEADER_LEN;
    let meta_end = meta_start + header.meta_len as usize;
    if data.len() < meta_end {
        report.error(format!(
            "meta region [{meta_start},{meta_end}) out of bounds (file_len {file_len})"
        ));
        return;
    }
    let meta_bytes = &data[meta_start..meta_end];
    let meta_str = match std::str::from_utf8(meta_bytes) {
        Ok(s) => s,
        Err(err) => {
            report.error(format!("meta region: invalid UTF-8: {err}"));
            return;
        }
    };
    let meta: RwsHourMeta = match serde_json::from_str(meta_str) {
        Ok(m) => m,
        Err(err) => {
            report.error(format!("meta JSON parse: {err}"));
            return;
        }
    };
    if meta.schema != SCHEMA_HOUR {
        report.error(format!(
            "meta schema '{}' != expected '{SCHEMA_HOUR}'",
            meta.schema
        ));
    }
    if meta.nx == 0 || meta.ny == 0 {
        report.error(format!(
            "meta: degenerate grid {}x{} (nx and ny must be nonzero)",
            meta.nx, meta.ny
        ));
    }

    // Check 3: variable metadata consistency.
    let mut var_ids_seen: HashSet<u16> = HashSet::new();
    let mut var_names_seen: HashSet<String> = HashSet::new();
    for var in &meta.variables {
        if !var_ids_seen.insert(var.id) {
            report.error(format!("meta: duplicate variable id {}", var.id));
        }
        if !var_names_seen.insert(var.name.clone()) {
            report.error(format!("meta: duplicate variable name '{}'", var.name));
        }
        match var.kind.as_str() {
            "surface2d" => {
                if var.codec != CODEC_2D {
                    report.error(format!(
                        "variable '{}': surface2d codec must be '{}', got '{}'",
                        var.name, CODEC_2D, var.codec
                    ));
                }
                if !var.levels_hpa.is_empty() {
                    report.error(format!(
                        "variable '{}': surface2d must have empty levels_hpa, got {} levels",
                        var.name,
                        var.levels_hpa.len()
                    ));
                }
            }
            "pressure3d" => {
                if var.codec != CODEC_3D {
                    report.error(format!(
                        "variable '{}': pressure3d codec must be '{}', got '{}'",
                        var.name, CODEC_3D, var.codec
                    ));
                }
                if var.levels_hpa.is_empty() {
                    report.error(format!(
                        "variable '{}': pressure3d must have non-empty levels_hpa",
                        var.name
                    ));
                }
            }
            other => {
                report.error(format!(
                    "variable '{}': unknown kind '{}' (expected 'surface2d' or 'pressure3d')",
                    var.name, other
                ));
            }
        }
    }

    // Warn if the chunking values in meta differ from the format constants.
    let chunking = &meta.chunking;
    if chunking.tile_y != TILE_Y || chunking.tile_x != TILE_X {
        report.warn(format!(
            "meta chunking tile_y={} tile_x={} differs from format constants {TILE_Y}/{TILE_X}",
            chunking.tile_y, chunking.tile_x
        ));
    }
    if chunking.col_y != COL_Y || chunking.col_x != COL_X {
        report.warn(format!(
            "meta chunking col_y={} col_x={} differs from format constants {COL_Y}/{COL_X}",
            chunking.col_y, chunking.col_x
        ));
    }
    // Use meta chunking values for tile-geometry checks as the spec says.
    let tile_y = chunking.tile_y;
    let tile_x = chunking.tile_x;
    let col_y = chunking.col_y;
    let col_x = chunking.col_x;

    // Check 4: index region in bounds and every record parses.
    let index_region_end = header.payload_offset;
    if file_len < index_region_end {
        report.error(format!(
            "index region ends at {index_region_end} but file_len is {file_len}"
        ));
        return;
    }

    let index_count = header.index_count as usize;
    let var_id_map: HashMap<u16, &crate::format::RwsVariableMeta> =
        meta.variables.iter().map(|v| (v.id, v)).collect();

    let mut records: Vec<ChunkRecord> = Vec::with_capacity(index_count);
    for i in 0..index_count {
        let start = header.index_offset as usize + i * INDEX_RECORD_LEN;
        // Already checked file_len >= index_region_end which covers the index.
        let rec = match ChunkRecord::unpack(&data[start..start + INDEX_RECORD_LEN]) {
            Ok(r) => r,
            Err(err) => {
                report.error(format!("index record {i}: {err}"));
                continue;
            }
        };

        // var_id must exist in meta.
        let var_meta = match var_id_map.get(&rec.var_id) {
            Some(v) => v,
            None => {
                report.error(format!(
                    "index record {i}: var_id {} not found in meta",
                    rec.var_id
                ));
                records.push(rec);
                continue;
            }
        };

        // kind must match variable's kind.
        let expected_kind = match var_meta.kind.as_str() {
            "surface2d" => KIND_TILE2D,
            "pressure3d" => KIND_COLUMN3D,
            _ => {
                records.push(rec);
                continue; // already reported in variable check
            }
        };
        if rec.kind != expected_kind {
            report.error(format!(
                "index record {i} (var '{}'): kind {} != expected {} for kind '{}'",
                var_meta.name, rec.kind, expected_kind, var_meta.kind
            ));
        }

        // flags must be a subset of known flags.
        let valid_flags = FLAG_EMPTY | FLAG_CONSTANT | FLAG_HAS_MISSING;
        if rec.flags & !valid_flags != 0 {
            report.error(format!(
                "index record {i} (var '{}'): flags 0x{:02x} has unknown bits",
                var_meta.name, rec.flags
            ));
        }

        // Reserved bytes [48..64] should be zero.
        let rec_start = header.index_offset as usize + i * INDEX_RECORD_LEN;
        let reserved = &data[rec_start + 48..rec_start + 64];
        if reserved.iter().any(|&b| b != 0) {
            report.warn(format!(
                "index record {i} (var '{}'): reserved bytes [48..64] are non-zero",
                var_meta.name
            ));
        }

        records.push(rec);
    }

    // Check 5: index strictly ascending by sort_key.
    for i in 1..records.len() {
        if records[i - 1].sort_key() >= records[i].sort_key() {
            report.error(format!(
                "index sort order violated at records {}..{}: {:?} !< {:?}",
                i - 1,
                i,
                records[i - 1].sort_key(),
                records[i].sort_key()
            ));
        }
    }

    // Build tile-count expectations per variable.
    let (nx, ny) = (meta.nx, meta.ny);

    // Check 6: tile coords in range.
    for (i, rec) in records.iter().enumerate() {
        if let Some(var_meta) = var_id_map.get(&rec.var_id) {
            match var_meta.kind.as_str() {
                "surface2d" => {
                    let max_ty = ny.div_ceil(tile_y) as u32;
                    let max_tx = nx.div_ceil(tile_x) as u32;
                    if rec.tile_y >= max_ty {
                        report.error(format!(
                            "index record {i} (var '{}'): tile_y {} >= max {} for ny={ny} tile_y={tile_y}",
                            var_meta.name, rec.tile_y, max_ty
                        ));
                    }
                    if rec.tile_x >= max_tx {
                        report.error(format!(
                            "index record {i} (var '{}'): tile_x {} >= max {} for nx={nx} tile_x={tile_x}",
                            var_meta.name, rec.tile_x, max_tx
                        ));
                    }
                }
                "pressure3d" => {
                    let max_ty = ny.div_ceil(col_y) as u32;
                    let max_tx = nx.div_ceil(col_x) as u32;
                    if rec.tile_y >= max_ty {
                        report.error(format!(
                            "index record {i} (var '{}'): tile_y {} >= max {} for ny={ny} col_y={col_y}",
                            var_meta.name, rec.tile_y, max_ty
                        ));
                    }
                    if rec.tile_x >= max_tx {
                        report.error(format!(
                            "index record {i} (var '{}'): tile_x {} >= max {} for nx={nx} col_x={col_x}",
                            var_meta.name, rec.tile_x, max_tx
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    // Check 7: payload spans within [payload_offset, file_len], no overlaps,
    // and len == 0 iff EMPTY or (CONSTANT without HAS_MISSING).
    for (i, rec) in records.iter().enumerate() {
        let is_empty = rec.flags & FLAG_EMPTY != 0;
        let is_constant_no_missing =
            rec.flags & FLAG_CONSTANT != 0 && rec.flags & FLAG_HAS_MISSING == 0;
        let expect_zero_len = is_empty || is_constant_no_missing;

        if expect_zero_len && rec.len != 0 {
            report.error(format!(
                "index record {i} (var_id {}): EMPTY/CONSTANT-without-missing chunk has len {} != 0",
                rec.var_id, rec.len
            ));
        } else if !expect_zero_len && rec.len == 0 {
            report.error(format!(
                "index record {i} (var_id {}): non-empty chunk has len == 0",
                rec.var_id
            ));
        }

        if rec.len > 0 {
            // offset must be >= payload_offset and payload must not exceed file_len.
            if rec.offset < header.payload_offset {
                report.error(format!(
                    "index record {i} (var_id {}): offset {} < payload_offset {}",
                    rec.var_id, rec.offset, header.payload_offset
                ));
            }
            let end = match rec.offset.checked_add(rec.len as u64) {
                Some(e) => e,
                None => {
                    report.error(format!(
                        "index record {i} (var_id {}): offset+len overflows u64",
                        rec.var_id
                    ));
                    continue;
                }
            };
            if end > file_len {
                report.error(format!(
                    "index record {i} (var_id {}): payload [{},{}] exceeds file_len {file_len}",
                    rec.var_id, rec.offset, end
                ));
            }
            report.stats.payload_bytes += rec.len as u64;
        }
    }

    // Check overlaps: collect non-empty records sorted by offset, check gaps.
    let mut payload_records: Vec<&ChunkRecord> =
        records.iter().filter(|r| r.len > 0).collect();
    payload_records.sort_by_key(|r| r.offset);
    for pair in payload_records.windows(2) {
        let end_a = pair[0].offset + pair[0].len as u64;
        if end_a > pair[1].offset {
            report.error(format!(
                "payload overlap: record ending at {end_a} overlaps record starting at {}",
                pair[1].offset
            ));
        }
    }

    // Check 8: expected raw_len per record.
    for (i, rec) in records.iter().enumerate() {
        let Some(var_meta) = var_id_map.get(&rec.var_id) else {
            continue;
        };
        let is_empty = rec.flags & FLAG_EMPTY != 0;
        let is_constant_no_missing =
            rec.flags & FLAG_CONSTANT != 0 && rec.flags & FLAG_HAS_MISSING == 0;

        if is_empty || is_constant_no_missing {
            if rec.raw_len != 0 {
                report.error(format!(
                    "index record {i} (var '{}'): EMPTY/CONSTANT-without-missing chunk has raw_len {} != 0",
                    var_meta.name, rec.raw_len
                ));
            }
            continue;
        }

        let expected_raw_len: u32 = match var_meta.kind.as_str() {
            "surface2d" => {
                let y0 = rec.tile_y as usize * tile_y;
                let x0 = rec.tile_x as usize * tile_x;
                let th = (ny - y0).min(tile_y);
                let tw = (nx - x0).min(tile_x);
                (4 * th * tw) as u32
            }
            "pressure3d" => {
                let y0 = rec.tile_y as usize * col_y;
                let x0 = rec.tile_x as usize * col_x;
                let ch = (ny - y0).min(col_y);
                let cw = (nx - x0).min(col_x);
                let levels = var_meta.levels_hpa.len();
                (2 * ch * cw * levels) as u32
            }
            _ => continue,
        };

        if rec.raw_len != expected_raw_len {
            report.error(format!(
                "index record {i} (var '{}'): raw_len {} != expected {expected_raw_len}",
                var_meta.name, rec.raw_len
            ));
        }
    }

    // Check 9: file length == max over records of (offset+len), or payload_offset when none.
    let expected_file_len: u64 = payload_records
        .iter()
        .map(|r| r.offset + r.len as u64)
        .max()
        .unwrap_or(header.payload_offset);
    if file_len != expected_file_len {
        if file_len > expected_file_len {
            report.error(format!(
                "trailing bytes: file_len {file_len} > expected {expected_file_len} (trailing {} bytes)",
                file_len - expected_file_len
            ));
        } else {
            report.error(format!(
                "file truncated: file_len {file_len} < expected {expected_file_len}"
            ));
        }
    }

    // Check 10: per-variable chunk-set completeness.
    for var_meta in &meta.variables {
        let (tiles_y, tiles_x) = match var_meta.kind.as_str() {
            "surface2d" => (ny.div_ceil(tile_y), nx.div_ceil(tile_x)),
            "pressure3d" => (ny.div_ceil(col_y), nx.div_ceil(col_x)),
            _ => continue,
        };
        let expected_count = tiles_y * tiles_x;
        let present: HashSet<(u32, u32)> = records
            .iter()
            .filter(|r| r.var_id == var_meta.id)
            .map(|r| (r.tile_y, r.tile_x))
            .collect();
        if present.len() != expected_count {
            let missing: Vec<(u32, u32)> = (0..tiles_y as u32)
                .flat_map(|ty| (0..tiles_x as u32).map(move |tx| (ty, tx)))
                .filter(|coord| !present.contains(coord))
                .take(5) // report up to 5 examples
                .collect();
            let extra_msg = if present.len() + missing.len() < expected_count {
                " (too many missing to list)".to_string()
            } else {
                String::new()
            };
            report.error(format!(
                "variable '{}': expected {expected_count} chunks, found {}; missing tiles: {:?}{extra_msg}",
                var_meta.name,
                present.len(),
                missing
            ));
        }
        // Check for duplicate (tile_y, tile_x) per variable.
        let total_for_var = records.iter().filter(|r| r.var_id == var_meta.id).count();
        if total_for_var != present.len() {
            report.error(format!(
                "variable '{}': {} index records but only {} distinct (tile_y, tile_x) positions (duplicates present)",
                var_meta.name, total_for_var, present.len()
            ));
        }
    }

    // Update stats.
    report.stats.variables = meta.variables.len();
    report.stats.chunks = records.len() as u64;

    // Structural done. Early-exit if not Deep.
    if depth == ValidateDepth::Structural {
        return;
    }

    // =======================================================================
    // Deep checks: decompress each non-empty chunk and verify content.
    // =======================================================================
    for (i, rec) in records.iter().enumerate() {
        let Some(var_meta) = var_id_map.get(&rec.var_id) else {
            continue;
        };
        let is_empty = rec.flags & FLAG_EMPTY != 0;
        let is_constant_no_missing =
            rec.flags & FLAG_CONSTANT != 0 && rec.flags & FLAG_HAS_MISSING == 0;

        // Check 14: EMPTY records.
        if is_empty {
            if rec.valid_count != 0 {
                report.error(format!(
                    "index record {i} (var '{}'): EMPTY chunk has valid_count {} != 0",
                    var_meta.name, rec.valid_count
                ));
            }
            if rec.min.is_finite() {
                report.error(format!(
                    "index record {i} (var '{}'): EMPTY chunk min should be NaN, got {}",
                    var_meta.name, rec.min
                ));
            }
            if rec.max.is_finite() {
                report.error(format!(
                    "index record {i} (var '{}'): EMPTY chunk max should be NaN, got {}",
                    var_meta.name, rec.max
                ));
            }
            continue;
        }

        // CONSTANT without missing: no payload.
        if is_constant_no_missing {
            continue;
        }

        // Check 11: decompress.
        let end = rec.offset + rec.len as u64;
        if end > file_len {
            continue; // already reported in structural
        }
        let compressed = &data[rec.offset as usize..end as usize];
        let raw = match zstd::bulk::decompress(compressed, rec.raw_len as usize) {
            Ok(r) => r,
            Err(err) => {
                report.error(format!(
                    "index record {i} (var '{}' chunk ({},{})): zstd decompress failed: {err}",
                    var_meta.name, rec.tile_y, rec.tile_x
                ));
                continue;
            }
        };
        if raw.len() != rec.raw_len as usize {
            report.error(format!(
                "index record {i} (var '{}' chunk ({},{})): decompressed {} bytes, raw_len says {}",
                var_meta.name, rec.tile_y, rec.tile_x, raw.len(), rec.raw_len
            ));
            continue;
        }

        match var_meta.kind.as_str() {
            "surface2d" => check_tile2d_deep(i, rec, var_meta, &raw, report),
            "pressure3d" => check_column3d_deep(i, rec, var_meta, &raw, report),
            _ => {}
        }
    }
}

fn check_tile2d_deep(
    i: usize,
    rec: &ChunkRecord,
    var_meta: &crate::format::RwsVariableMeta,
    raw: &[u8],
    report: &mut ValidationReport,
) {
    // Check 12: TILE2D: f32 count == th*tw; finite-count == valid_count;
    // finite min/max == record min/max; HAS_MISSING iff any non-finite.
    if raw.len() % 4 != 0 {
        report.error(format!(
            "index record {i} (var '{}' tile ({},{})): raw payload length {} not divisible by 4",
            var_meta.name, rec.tile_y, rec.tile_x, raw.len()
        ));
        return;
    }
    let values: Vec<f32> = raw
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();

    let mut finite_min = f32::INFINITY;
    let mut finite_max = f32::NEG_INFINITY;
    let mut finite_count: u32 = 0;
    let mut has_non_finite = false;
    for &v in &values {
        if v.is_finite() {
            finite_min = finite_min.min(v);
            finite_max = finite_max.max(v);
            finite_count += 1;
        } else {
            has_non_finite = true;
        }
    }

    if finite_count != rec.valid_count {
        report.error(format!(
            "index record {i} (var '{}' tile ({},{})): decoded finite_count {finite_count} != valid_count {}",
            var_meta.name, rec.tile_y, rec.tile_x, rec.valid_count
        ));
    }

    if finite_count > 0 {
        if rec.min.to_bits() != finite_min.to_bits() {
            report.error(format!(
                "index record {i} (var '{}' tile ({},{})): decoded min {} != record min {}",
                var_meta.name, rec.tile_y, rec.tile_x, finite_min, rec.min
            ));
        }
        if rec.max.to_bits() != finite_max.to_bits() {
            report.error(format!(
                "index record {i} (var '{}' tile ({},{})): decoded max {} != record max {}",
                var_meta.name, rec.tile_y, rec.tile_x, finite_max, rec.max
            ));
        }
    } else {
        // valid_count == 0: min/max should be NaN
        if rec.min.is_finite() {
            report.error(format!(
                "index record {i} (var '{}' tile ({},{})): valid_count==0 but min is {}",
                var_meta.name, rec.tile_y, rec.tile_x, rec.min
            ));
        }
        if rec.max.is_finite() {
            report.error(format!(
                "index record {i} (var '{}' tile ({},{})): valid_count==0 but max is {}",
                var_meta.name, rec.tile_y, rec.tile_x, rec.max
            ));
        }
    }

    let has_missing_flag = rec.flags & FLAG_HAS_MISSING != 0;
    if has_non_finite != has_missing_flag {
        report.error(format!(
            "index record {i} (var '{}' tile ({},{})): HAS_MISSING flag {} but has_non_finite {}",
            var_meta.name, rec.tile_y, rec.tile_x, has_missing_flag, has_non_finite
        ));
    }
}

fn check_column3d_deep(
    i: usize,
    rec: &ChunkRecord,
    var_meta: &crate::format::RwsVariableMeta,
    raw: &[u8],
    report: &mut ValidationReport,
) {
    // Check 13: COLUMN3D: i16 count; count of non-MISSING_Q == valid_count;
    // HAS_MISSING iff any MISSING_Q; CONSTANT => scale==0 and all non-missing q==0.
    if raw.len() % 2 != 0 {
        report.error(format!(
            "index record {i} (var '{}' chunk ({},{})): raw payload length {} not divisible by 2",
            var_meta.name, rec.tile_y, rec.tile_x, raw.len()
        ));
        return;
    }
    let quants: Vec<i16> = raw
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes(b.try_into().unwrap()))
        .collect();

    let valid_count: u32 = quants.iter().filter(|&&q| q != MISSING_Q).count() as u32;
    let has_missing = quants.iter().any(|&q| q == MISSING_Q);

    if valid_count != rec.valid_count {
        report.error(format!(
            "index record {i} (var '{}' chunk ({},{})): decoded valid_count {valid_count} != record valid_count {}",
            var_meta.name, rec.tile_y, rec.tile_x, rec.valid_count
        ));
    }

    let has_missing_flag = rec.flags & FLAG_HAS_MISSING != 0;
    if has_missing != has_missing_flag {
        report.error(format!(
            "index record {i} (var '{}' chunk ({},{})): HAS_MISSING flag {} but has_missing_q {}",
            var_meta.name, rec.tile_y, rec.tile_x, has_missing_flag, has_missing
        ));
    }

    let is_constant = rec.flags & FLAG_CONSTANT != 0;
    if is_constant {
        if rec.scale != 0.0 {
            report.error(format!(
                "index record {i} (var '{}' chunk ({},{})): CONSTANT chunk has scale {} != 0",
                var_meta.name, rec.tile_y, rec.tile_x, rec.scale
            ));
        }
        for &q in quants.iter().filter(|&&q| q != MISSING_Q) {
            if q != 0 {
                report.error(format!(
                    "index record {i} (var '{}' chunk ({},{})): CONSTANT chunk has non-missing q={q} != 0",
                    var_meta.name, rec.tile_y, rec.tile_x
                ));
                break; // report once per chunk
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::HourWriter;
    use rustwx_core::{GridShape, LatLonGrid};
    use std::fs;
    use std::path::PathBuf;

    const NX: usize = 40;
    const NY: usize = 30;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("rw-store-validate-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a small valid hour file with one 2D var (including a NaN hole)
    /// and one 3D var with 3 levels.
    fn write_valid_hour(path: &std::path::Path) {
        let mut values_2d: Vec<f32> = (0..NY)
            .flat_map(|y| (0..NX).map(move |x| 0.1 * x as f32 + 0.2 * y as f32))
            .collect();
        // A NaN hole in the middle.
        for y in 5..10 {
            for x in 5..10 {
                values_2d[y * NX + x] = f32::NAN;
            }
        }

        let levels: [u16; 3] = [1000, 500, 200];
        let planes: Vec<Vec<f32>> = levels
            .iter()
            .map(|&level| {
                (0..NY)
                    .flat_map(|_y| {
                        (0..NX).map(move |x| 100.0 + 0.5 * x as f32 + 0.01 * level as f32)
                    })
                    .collect()
            })
            .collect();
        let plane_refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();

        let mut writer = HourWriter::new(
            "hrrr",
            "2026-06-10T00:00:00Z",
            3,
            NX,
            NY,
            "test-grid-hash",
            "validate-test",
        );
        writer
            .add_surface2d(
                "temp_2m",
                "K",
                serde_json::json!({"test": true}),
                &values_2d,
            )
            .unwrap();
        writer
            .add_pressure3d(
                "wind_iso",
                "m/s",
                serde_json::json!({"test3d": true}),
                &levels,
                &plane_refs,
            )
            .unwrap();
        writer.finish(path).unwrap();
    }

    /// Build a regular lat/lon grid for use in run-dir tests.
    fn small_grid() -> LatLonGrid {
        let mut lat = Vec::with_capacity(NX * NY);
        let mut lon = Vec::with_capacity(NX * NY);
        for y in 0..NY {
            for x in 0..NX {
                lat.push((35.0 + 0.1 * y as f64) as f32);
                lon.push((-100.0 + 0.1 * x as f64) as f32);
            }
        }
        LatLonGrid::new(GridShape::new(NX, NY).unwrap(), lat, lon).unwrap()
    }

    // -----------------------------------------------------------------------
    // Happy-path tests
    // -----------------------------------------------------------------------

    #[test]
    fn valid_hour_structural_ok() {
        let dir = test_dir("valid-structural");
        let path = dir.join("f003.rws");
        write_valid_hour(&path);
        let report = validate_hour_file(&path, ValidateDepth::Structural).unwrap();
        assert!(
            report.is_ok(),
            "valid file must pass structural: {:?}",
            report.errors
        );
        assert_eq!(report.stats.variables, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn valid_hour_deep_ok_with_two_variables() {
        let dir = test_dir("valid-deep");
        let path = dir.join("f003.rws");
        write_valid_hour(&path);
        let report = validate_hour_file(&path, ValidateDepth::Deep).unwrap();
        assert!(
            report.is_ok(),
            "valid file must pass deep: {:?}",
            report.errors
        );
        assert_eq!(report.stats.variables, 2, "expected 2 variables");
        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Corruption tests (each from a fresh copy of valid bytes)
    // -----------------------------------------------------------------------

    fn load_valid_bytes(dir: &std::path::Path) -> Vec<u8> {
        let path = dir.join("base.rws");
        write_valid_hour(&path);
        fs::read(&path).unwrap()
    }

    fn write_corrupt(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn truncated_file_reports_error() {
        let dir = test_dir("truncate");
        let mut bytes = load_valid_bytes(&dir);
        let orig_len = bytes.len();
        bytes.truncate(orig_len - 10);
        let path = write_corrupt(&dir, "truncated.rws", &bytes);
        let report = validate_hour_file(&path, ValidateDepth::Structural).unwrap();
        assert!(
            !report.is_ok(),
            "truncated file must fail; errors: {:?}",
            report.errors
        );
        let joined = report.errors.join(" ");
        assert!(
            joined.contains("trun") || joined.contains("bound") || joined.contains("length"),
            "error should mention truncation/bounds/length: {joined}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn swapped_index_records_reports_sort_error() {
        let dir = test_dir("swap-index");
        let mut bytes = load_valid_bytes(&dir);
        let header = RwsHeader::parse(&bytes).unwrap();
        let start = header.index_offset as usize;
        // Swap the first two 64-byte index records.
        let (a, b) = (
            bytes[start..start + 64].to_vec(),
            bytes[start + 64..start + 128].to_vec(),
        );
        bytes[start..start + 64].copy_from_slice(&b);
        bytes[start + 64..start + 128].copy_from_slice(&a);
        let path = write_corrupt(&dir, "swapped.rws", &bytes);
        let report = validate_hour_file(&path, ValidateDepth::Structural).unwrap();
        assert!(
            !report.is_ok(),
            "swapped records must fail; errors: {:?}",
            report.errors
        );
        let joined = report.errors.join(" ");
        assert!(
            joined.contains("sort") || joined.contains("order"),
            "error should mention sort/order: {joined}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_payload_deep_only() {
        let dir = test_dir("corrupt-payload");
        let bytes = load_valid_bytes(&dir);
        let header = RwsHeader::parse(&bytes).unwrap();

        // Find first dense chunk record.
        let record_count = header.index_count as usize;
        let mut dense_rec: Option<ChunkRecord> = None;
        for i in 0..record_count {
            let start = header.index_offset as usize + i * 64;
            let rec = ChunkRecord::unpack(&bytes[start..start + 64]).unwrap();
            if rec.len > 0 {
                dense_rec = Some(rec);
                break;
            }
        }
        let dense_rec = dense_rec.expect("must have at least one dense chunk");

        let mut corrupt_bytes = bytes.clone();
        let off = dense_rec.offset as usize;
        // Overwrite 4 mid-payload bytes with 0xFF.
        for b in &mut corrupt_bytes[off..off + 4] {
            *b = 0xFF;
        }
        let path = write_corrupt(&dir, "corrupt.rws", &corrupt_bytes);

        // Structural must pass (no decompression).
        let structural = validate_hour_file(&path, ValidateDepth::Structural).unwrap();
        assert!(
            structural.is_ok(),
            "structural must pass corrupt payload: {:?}",
            structural.errors
        );

        // Deep must report decompress error.
        let deep = validate_hour_file(&path, ValidateDepth::Deep).unwrap();
        assert!(
            !deep.is_ok(),
            "deep must fail corrupt payload; errors: {:?}",
            deep.errors
        );
        let joined = deep.errors.join(" ");
        assert!(
            joined.contains("decompress") || joined.contains("zstd"),
            "error should mention decompress/zstd: {joined}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_raw_len_reports_error() {
        let dir = test_dir("raw-len");
        let mut bytes = load_valid_bytes(&dir);
        let header = RwsHeader::parse(&bytes).unwrap();

        // Find first dense chunk and bump its raw_len by 2.
        for i in 0..header.index_count as usize {
            let start = header.index_offset as usize + i * 64;
            let rec = ChunkRecord::unpack(&bytes[start..start + 64]).unwrap();
            if rec.len > 0 {
                let current = u32::from_le_bytes(bytes[start + 24..start + 28].try_into().unwrap());
                bytes[start + 24..start + 28].copy_from_slice(&(current + 2).to_le_bytes());
                break;
            }
        }
        let path = write_corrupt(&dir, "rawlen.rws", &bytes);
        let report = validate_hour_file(&path, ValidateDepth::Structural).unwrap();
        assert!(
            !report.is_ok(),
            "wrong raw_len must fail; errors: {:?}",
            report.errors
        );
        assert!(
            report.errors.iter().any(|e| e.contains("raw_len")),
            "error should mention raw_len: {:?}",
            report.errors
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn appended_junk_reports_trailing_error() {
        let dir = test_dir("trailing");
        let mut bytes = load_valid_bytes(&dir);
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let path = write_corrupt(&dir, "trailing.rws", &bytes);
        let report = validate_hour_file(&path, ValidateDepth::Structural).unwrap();
        assert!(
            !report.is_ok(),
            "trailing bytes must fail; errors: {:?}",
            report.errors
        );
        let joined = report.errors.join(" ");
        assert!(
            joined.contains("trailing"),
            "error should mention trailing: {joined}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_version_reports_error_not_panic() {
        let dir = test_dir("bad-version");
        let mut bytes = load_valid_bytes(&dir);
        // Set version field (bytes 8..12) to 9.
        bytes[8..12].copy_from_slice(&9u32.to_le_bytes());
        let path = write_corrupt(&dir, "badversion.rws", &bytes);
        // Must return Ok(report_with_error), not Err or panic.
        let report = validate_hour_file(&path, ValidateDepth::Structural).unwrap();
        assert!(
            !report.is_ok(),
            "bad version must produce errors: {:?}",
            report.errors
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // validate_run_dir tests
    // -----------------------------------------------------------------------

    #[test]
    fn valid_run_dir_deep_ok() {
        use rustwx_core::{FieldSelector, GridProjection, SelectedField2D};

        let dir = test_dir("rundir-ok");
        let store_root = dir.join("store");
        let grid = small_grid();

        let values: Vec<f32> = (0..NY)
            .flat_map(|y| (0..NX).map(move |x| 280.0 + 0.05 * x as f32 - 0.02 * y as f32))
            .collect();
        let selector =
            FieldSelector::height_agl(rustwx_core::CanonicalField::Temperature, 2);
        let field = SelectedField2D::new(selector, "K", grid, values)
            .unwrap()
            .with_projection(GridProjection::Geographic);

        crate::ingest::write_hour_from_fields(
            &store_root,
            "hrrr",
            "20260610_00z",
            3,
            &[("temp_2m", &field)],
            &[],
            "validate-test",
            1_000_000,
        )
        .unwrap();

        let run_dir_path = store_root.join("hrrr").join("20260610_00z");
        let report = validate_run_dir(&run_dir_path, ValidateDepth::Deep).unwrap();
        assert!(
            report.is_ok(),
            "valid run dir must pass deep: {:?}",
            report.errors
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_hour_file_reports_error() {
        use rustwx_core::{FieldSelector, GridProjection, SelectedField2D};

        let dir = test_dir("rundir-missing-hour");
        let store_root = dir.join("store");
        let grid = small_grid();

        let values: Vec<f32> = vec![280.0f32; NX * NY];
        let selector =
            FieldSelector::height_agl(rustwx_core::CanonicalField::Temperature, 2);
        let field = SelectedField2D::new(selector, "K", grid, values)
            .unwrap()
            .with_projection(GridProjection::Geographic);

        crate::ingest::write_hour_from_fields(
            &store_root,
            "hrrr",
            "20260610_00z",
            3,
            &[("temp_2m", &field)],
            &[],
            "validate-test",
            1_000_000,
        )
        .unwrap();

        let run_dir_path = store_root.join("hrrr").join("20260610_00z");
        // Delete the hour file.
        fs::remove_file(run_dir_path.join("f003.rws")).unwrap();
        let report = validate_run_dir(&run_dir_path, ValidateDepth::Structural).unwrap();
        assert!(
            !report.is_ok(),
            "missing hour file must produce an error: {:?}",
            report.errors
        );
        assert!(
            report.errors.iter().any(|e| e.contains("f003.rws")),
            "error must mention the missing file: {:?}",
            report.errors
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stray_rws_file_reports_warning() {
        use rustwx_core::{FieldSelector, GridProjection, SelectedField2D};

        let dir = test_dir("rundir-stray");
        let store_root = dir.join("store");
        let grid = small_grid();

        let values: Vec<f32> = vec![280.0f32; NX * NY];
        let selector =
            FieldSelector::height_agl(rustwx_core::CanonicalField::Temperature, 2);
        let field = SelectedField2D::new(selector, "K", grid, values)
            .unwrap()
            .with_projection(GridProjection::Geographic);

        crate::ingest::write_hour_from_fields(
            &store_root,
            "hrrr",
            "20260610_00z",
            3,
            &[("temp_2m", &field)],
            &[],
            "validate-test",
            1_000_000,
        )
        .unwrap();

        let run_dir_path = store_root.join("hrrr").join("20260610_00z");
        // Add a stray .rws file not in the manifest.
        fs::write(run_dir_path.join("f099.rws"), b"garbage").unwrap();
        let report = validate_run_dir(&run_dir_path, ValidateDepth::Structural).unwrap();
        assert!(
            report.warnings.iter().any(|w| w.contains("f099.rws")),
            "stray file must produce a warning: {:?}",
            report.warnings
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
