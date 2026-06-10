//! Determinism comparator for `.rws` hour files: proves two hour files are
//! byte-identical EXCEPT for the writer-provenance meta (`writer.build`)
//! and the offset shift that a different-length meta JSON induces.
//!
//! Two independently produced hour files (e.g. a baseline build and a
//! refactored build ingesting the same GRIB inputs) cannot be compared
//! with a flat byte diff: the meta JSON embeds the writer's build sha, and
//! a build-sha length difference shifts every absolute payload offset in
//! the index. This tool compares the regions structurally:
//!
//! * header: version, index_count (meta_len/offsets are derived);
//! * meta JSON: every field, with `writer.build` excluded;
//! * index: every record field, with `offset` compared relative to the
//!   file's payload base (`record.offset - payload_offset`);
//! * payload: the byte regions `[payload_offset..]` of both files.
//!
//! Exit code 0 = equivalent, 1 = different (first difference printed),
//! 2 = usage/IO error.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use rw_store::header::RwsHeader;
use rw_store::index::ChunkRecord;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path_a, path_b] = args.as_slice() else {
        eprintln!("usage: rw_store_diff <hour_a.rws> <hour_b.rws>");
        return ExitCode::from(2);
    };
    match compare(Path::new(path_a), Path::new(path_b)) {
        Ok(()) => {
            println!("equivalent: payload + index + meta (writer.build excluded) match");
            ExitCode::SUCCESS
        }
        Err(Difference::Io(message)) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
        Err(Difference::Found(message)) => {
            eprintln!("DIFFERENT: {message}");
            ExitCode::FAILURE
        }
    }
}

enum Difference {
    Io(String),
    Found(String),
}

fn compare(path_a: &Path, path_b: &Path) -> Result<(), Difference> {
    let bytes_a = fs::read(path_a)
        .map_err(|err| Difference::Io(format!("read {}: {err}", path_a.display())))?;
    let bytes_b = fs::read(path_b)
        .map_err(|err| Difference::Io(format!("read {}: {err}", path_b.display())))?;
    let header_a = RwsHeader::parse(&bytes_a)
        .map_err(|err| Difference::Io(format!("header {}: {err}", path_a.display())))?;
    let header_b = RwsHeader::parse(&bytes_b)
        .map_err(|err| Difference::Io(format!("header {}: {err}", path_b.display())))?;

    if header_a.version != header_b.version {
        return Err(Difference::Found(format!(
            "header version {} vs {}",
            header_a.version, header_b.version
        )));
    }
    if header_a.index_count != header_b.index_count {
        return Err(Difference::Found(format!(
            "index_count {} vs {}",
            header_a.index_count, header_b.index_count
        )));
    }

    // Meta JSON with writer.build masked out.
    let meta_a = meta_without_build(&bytes_a, &header_a, path_a)?;
    let meta_b = meta_without_build(&bytes_b, &header_b, path_b)?;
    if meta_a != meta_b {
        return Err(Difference::Found(
            "meta JSON differs beyond writer.build (variables/levels/selectors/grid_hash)"
                .to_string(),
        ));
    }

    // Index records, offsets normalized to the payload base.
    for index in 0..header_a.index_count as usize {
        let record_a = record_at(&bytes_a, &header_a, index, path_a)?;
        let record_b = record_at(&bytes_b, &header_b, index, path_b)?;
        let rel_a = record_a.offset.wrapping_sub(header_a.payload_offset);
        let rel_b = record_b.offset.wrapping_sub(header_b.payload_offset);
        let fields_equal = record_a.var_id == record_b.var_id
            && record_a.kind == record_b.kind
            && record_a.flags == record_b.flags
            && record_a.tile_y == record_b.tile_y
            && record_a.tile_x == record_b.tile_x
            && rel_a == rel_b
            && record_a.len == record_b.len
            && record_a.raw_len == record_b.raw_len
            && record_a.center.to_bits() == record_b.center.to_bits()
            && record_a.scale.to_bits() == record_b.scale.to_bits()
            && record_a.min.to_bits() == record_b.min.to_bits()
            && record_a.max.to_bits() == record_b.max.to_bits()
            && record_a.valid_count == record_b.valid_count;
        if !fields_equal {
            return Err(Difference::Found(format!(
                "index record {index}: {record_a:?} (rel offset {rel_a}) vs {record_b:?} \
                 (rel offset {rel_b})"
            )));
        }
    }

    // Payload regions, byte for byte.
    let payload_a = &bytes_a[header_a.payload_offset as usize..];
    let payload_b = &bytes_b[header_b.payload_offset as usize..];
    if payload_a.len() != payload_b.len() {
        return Err(Difference::Found(format!(
            "payload length {} vs {}",
            payload_a.len(),
            payload_b.len()
        )));
    }
    if let Some(position) = payload_a
        .iter()
        .zip(payload_b.iter())
        .position(|(a, b)| a != b)
    {
        return Err(Difference::Found(format!(
            "payload bytes differ at payload offset {position} (of {})",
            payload_a.len()
        )));
    }
    println!(
        "compared: {} index records, {} payload bytes, meta keys minus writer.build",
        header_a.index_count,
        payload_a.len()
    );
    Ok(())
}

fn meta_without_build(
    bytes: &[u8],
    header: &RwsHeader,
    path: &Path,
) -> Result<serde_json::Value, Difference> {
    let start = 64usize;
    let end = start + header.meta_len as usize;
    let mut meta: serde_json::Value =
        serde_json::from_slice(bytes.get(start..end).ok_or_else(|| {
            Difference::Io(format!("{}: meta region out of range", path.display()))
        })?)
        .map_err(|err| Difference::Io(format!("{}: meta JSON: {err}", path.display())))?;
    if let Some(writer) = meta.get_mut("writer") {
        if let Some(build) = writer.get_mut("build") {
            *build = serde_json::Value::Null;
        }
    }
    Ok(meta)
}

fn record_at(
    bytes: &[u8],
    header: &RwsHeader,
    index: usize,
    path: &Path,
) -> Result<ChunkRecord, Difference> {
    let start = header.index_offset as usize + index * 64;
    let slice = bytes.get(start..start + 64).ok_or_else(|| {
        Difference::Io(format!(
            "{}: index record {index} out of range",
            path.display()
        ))
    })?;
    ChunkRecord::unpack(slice)
        .map_err(|err| Difference::Io(format!("{}: index record {index}: {err}", path.display())))
}
