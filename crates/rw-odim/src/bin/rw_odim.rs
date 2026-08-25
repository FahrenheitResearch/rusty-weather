//! `rw_odim` -- the ops front end over the ODIM_H5 polar-volume decoder.
//!
//! ```text
//! rw_odim inspect --file F           structure, geometry and Nyquist, no payload decode
//! rw_odim decode  --file F           full decode; per-sweep, per-moment census and value range
//! rw_odim nyquist --file F           the dealias handoff: per-sweep interval and its provenance
//! rw_odim export  --file F --sweep N --quantity Q --out PREFIX
//!                                    one sweep's values and censor plane as flat binary
//! rw_odim --abi                      the record contract this build writes
//! ```
//!
//! This is a deliberately thin front end: all behaviour lives in the library.
//! Every subcommand writes one JSON record to stdout, each carrying a `schema`
//! string, so a consumer pins the contract rather than the binary's version.
//!
//! `export` exists for cross-checking against another decoder. It writes the
//! decoded sweep as three files -- a JSON sidecar, a little-endian `f64` value
//! plane and a `u8` censor plane, both row-major `[ray][bin]` -- which is the
//! smallest thing a referee implementation can read without this crate.

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use rw_odim::{
    DecodeOptions, Moment, PolarVolume, Sweep, censor, read_volume_with, volume::NyquistSource,
};
use serde::Serialize;

/// The record contract this build writes.
///
/// Not a version number: it names the fields a consumer was written against,
/// so a rebuilt-but-unchanged binary still matches and one whose records
/// changed shape does not. Same idea as `rw_opera --abi` on the gpuwm side.
const ABI_MARKER: &str = "rw-odim.inspect.v1\trw-odim.decode.v1\trw-odim.nyquist.v1\t\
rw-odim.export.v1\tsite\telangle\tnbins\tnrays\trscale\trstart\ta1gate\tazimuth\tnyquist_ms\t\
nyquist_source\tcensor\tmeasured\tundetect\tnodata\tsentinel_ambiguous";

const INSPECT_SCHEMA: &str = "rw-odim.inspect.v1";
const DECODE_SCHEMA: &str = "rw-odim.decode.v1";
const NYQUIST_SCHEMA: &str = "rw-odim.nyquist.v1";
const EXPORT_SCHEMA: &str = "rw-odim.export.v1";

#[derive(Parser)]
#[command(
    name = "rw_odim",
    version,
    about = "EUMETNET OPERA ODIM_H5 polar-volume decoder (PVOL/SCAN)"
)]
struct Cli {
    /// Print the record contract this build writes, then exit.
    #[arg(long)]
    abi: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Report structure, geometry and Nyquist without decoding any payload.
    Inspect(FileArgs),
    /// Decode every sweep and moment, reporting the per-gate census.
    Decode(DecodeArgs),
    /// Report the per-sweep Nyquist interval and where it came from.
    Nyquist(FileArgs),
    /// Write one sweep's decoded values and censor plane as flat binary.
    Export(ExportArgs),
}

#[derive(Args)]
struct FileArgs {
    /// The ODIM_H5 file to read.
    #[arg(long)]
    file: PathBuf,
}

#[derive(Args)]
struct DecodeArgs {
    /// The ODIM_H5 file to read.
    #[arg(long)]
    file: PathBuf,
    /// Decode only this ODIM quantity (repeatable), e.g. VRADH.
    #[arg(long)]
    quantity: Vec<String>,
    /// Decode only this /datasetN index (repeatable).
    #[arg(long)]
    sweep: Vec<usize>,
}

#[derive(Args)]
struct ExportArgs {
    /// The ODIM_H5 file to read.
    #[arg(long)]
    file: PathBuf,
    /// The /datasetN index to export.
    #[arg(long)]
    sweep: usize,
    /// The ODIM quantity to export, e.g. VRADH.
    #[arg(long)]
    quantity: String,
    /// Output path prefix. Writes PREFIX.json, PREFIX.values.f64, PREFIX.censor.u8.
    #[arg(long)]
    out: PathBuf,
}

fn main() -> ExitCode {
    // `Result` from `main` prints the error with `Debug`, which would show a
    // refusal as `Format { context: ..., detail: ... }` and bury the sentence
    // explaining what is wrong with the file. These messages are the product,
    // so they are printed with `Display`, along with the source chain.
    if let Err(err) = run() {
        eprintln!("rw_odim: {err}");
        let mut source = err.source();
        while let Some(cause) = source {
            eprintln!("  caused by: {cause}");
            source = cause.source();
        }
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    if cli.abi {
        println!("{ABI_MARKER}");
        return Ok(());
    }
    let Some(command) = cli.command else {
        // clap prints the long help for `--help`; a bare invocation should
        // say what to do rather than succeed silently.
        return Err("no subcommand: try `rw_odim --help`".into());
    };
    match command {
        Command::Inspect(args) => cmd_inspect(&args),
        Command::Decode(args) => cmd_decode(&args),
        Command::Nyquist(args) => cmd_nyquist(&args),
        Command::Export(args) => cmd_export(&args),
    }
}

// ---------------------------------------------------------------- records

#[derive(Serialize)]
struct VolumeHeader<'a> {
    file: String,
    object: &'a str,
    conventions: Option<&'a str>,
    version: Option<&'a str>,
    source: &'a rw_odim::Source,
    nominal_time: Option<String>,
    site: &'a rw_odim::Site,
    system: &'a rw_odim::SystemNotes,
    sweep_count: usize,
    quantities: Vec<String>,
}

impl<'a> VolumeHeader<'a> {
    fn new(file: &Path, volume: &'a PolarVolume) -> Self {
        VolumeHeader {
            file: file.display().to_string(),
            object: &volume.object,
            conventions: volume.conventions.as_deref(),
            version: volume.version.as_deref(),
            source: &volume.source,
            nominal_time: volume.nominal_time.map(|t| t.to_rfc3339()),
            site: &volume.site,
            system: &volume.system,
            sweep_count: volume.sweeps.len(),
            quantities: volume.quantities(),
        }
    }
}

#[derive(Serialize)]
struct SweepGeometry {
    index: usize,
    elevation_deg: f64,
    nrays: usize,
    nbins: usize,
    range_scale_m: f64,
    range_start_m: f64,
    first_gate_centre_m: f64,
    max_range_m: f64,
    a1gate: Option<usize>,
    start_time: Option<String>,
    end_time: Option<String>,
    azimuth_source: String,
    azimuth_first_deg: Option<f64>,
    azimuth_last_deg: Option<f64>,
    has_per_ray_elevation: bool,
    nyquist_ms: Option<f64>,
    nyquist_source: NyquistSource,
    dual_prf: bool,
    high_prf_hz: Option<f64>,
    low_prf_hz: Option<f64>,
    quantities: Vec<String>,
}

impl SweepGeometry {
    fn new(sweep: &Sweep) -> Self {
        SweepGeometry {
            index: sweep.index,
            elevation_deg: sweep.elevation_deg,
            nrays: sweep.nrays,
            nbins: sweep.nbins,
            range_scale_m: sweep.range_scale_m,
            range_start_m: sweep.range_start_m,
            first_gate_centre_m: sweep.gate_centre_range_m(0),
            max_range_m: sweep.max_range_m(),
            a1gate: sweep.a1gate,
            start_time: sweep.start_time.map(|t| t.to_rfc3339()),
            end_time: sweep.end_time.map(|t| t.to_rfc3339()),
            azimuth_source: sweep.azimuth_source.to_string(),
            azimuth_first_deg: sweep.azimuth_deg.first().copied(),
            azimuth_last_deg: sweep.azimuth_deg.last().copied(),
            has_per_ray_elevation: sweep.ray_elevation_deg.is_some(),
            nyquist_ms: sweep.nyquist.interval_ms,
            nyquist_source: sweep.nyquist.source,
            dual_prf: sweep.nyquist.dual_prf,
            high_prf_hz: sweep.nyquist.high_prf_hz,
            low_prf_hz: sweep.nyquist.low_prf_hz,
            quantities: sweep.quantities().into_iter().map(str::to_string).collect(),
        }
    }
}

#[derive(Serialize)]
struct MomentReport {
    quantity: String,
    path: String,
    unit: String,
    kind: rw_odim::QuantityKind,
    gain: f64,
    offset: f64,
    nodata: Option<f64>,
    undetect: Option<f64>,
    sentinels_collide: bool,
    census: rw_odim::Census,
    observed_fraction: f64,
    measured_min: Option<f64>,
    measured_max: Option<f64>,
}

impl MomentReport {
    fn new(moment: &Moment) -> Self {
        let (measured_min, measured_max) = match moment.measured_range() {
            Some((lo, hi)) => (Some(lo), Some(hi)),
            None => (None, None),
        };
        MomentReport {
            quantity: moment.quantity.clone(),
            path: moment.path.clone(),
            unit: moment.unit.clone(),
            kind: moment.kind,
            gain: moment.calibration.gain,
            offset: moment.calibration.offset,
            nodata: moment.calibration.nodata,
            undetect: moment.calibration.undetect,
            sentinels_collide: moment.calibration.sentinels_collide,
            census: moment.census,
            observed_fraction: moment.census.observed_fraction(),
            measured_min,
            measured_max,
        }
    }
}

// --------------------------------------------------------------- commands

fn cmd_inspect(args: &FileArgs) -> Result<(), Box<dyn Error>> {
    let volume = read_volume_with(&args.file, &DecodeOptions::geometry_only())?;
    #[derive(Serialize)]
    struct Record<'a> {
        schema: &'a str,
        #[serde(flatten)]
        header: VolumeHeader<'a>,
        sweeps: Vec<SweepGeometry>,
    }
    let record = Record {
        schema: INSPECT_SCHEMA,
        header: VolumeHeader::new(&args.file, &volume),
        sweeps: volume.sweeps.iter().map(SweepGeometry::new).collect(),
    };
    println!("{}", serde_json::to_string_pretty(&record)?);
    Ok(())
}

fn cmd_decode(args: &DecodeArgs) -> Result<(), Box<dyn Error>> {
    let options = DecodeOptions {
        quantities: (!args.quantity.is_empty()).then(|| args.quantity.clone()),
        sweep_indices: (!args.sweep.is_empty()).then(|| args.sweep.clone()),
        geometry_only: false,
    };
    let volume = read_volume_with(&args.file, &options)?;

    #[derive(Serialize)]
    struct SweepRecord {
        #[serde(flatten)]
        geometry: SweepGeometry,
        moments: Vec<MomentReport>,
    }
    #[derive(Serialize)]
    struct Record<'a> {
        schema: &'a str,
        #[serde(flatten)]
        header: VolumeHeader<'a>,
        totals: Totals,
        sweeps: Vec<SweepRecord>,
    }
    #[derive(Serialize, Default)]
    struct Totals {
        moments: usize,
        gates: usize,
        measured: usize,
        undetect: usize,
        nodata: usize,
        sentinel_ambiguous: usize,
        velocity_sweeps: usize,
        velocity_dealiasable: bool,
    }

    let mut totals = Totals {
        velocity_sweeps: volume.velocity_sweeps().len(),
        velocity_dealiasable: volume.velocity_is_dealiasable(),
        ..Totals::default()
    };
    let mut sweeps = Vec::new();
    for sweep in &volume.sweeps {
        for moment in &sweep.moments {
            totals.moments += 1;
            totals.gates += moment.census.total();
            totals.measured += moment.census.measured;
            totals.undetect += moment.census.undetect;
            totals.nodata += moment.census.nodata;
            totals.sentinel_ambiguous += moment.census.sentinel_ambiguous;
        }
        sweeps.push(SweepRecord {
            geometry: SweepGeometry::new(sweep),
            moments: sweep.moments.iter().map(MomentReport::new).collect(),
        });
    }

    let record = Record {
        schema: DECODE_SCHEMA,
        header: VolumeHeader::new(&args.file, &volume),
        totals,
        sweeps,
    };
    println!("{}", serde_json::to_string_pretty(&record)?);
    Ok(())
}

fn cmd_nyquist(args: &FileArgs) -> Result<(), Box<dyn Error>> {
    let volume = read_volume_with(&args.file, &DecodeOptions::geometry_only())?;

    #[derive(Serialize)]
    struct SweepNyquist {
        index: usize,
        elevation_deg: f64,
        nyquist_ms: Option<f64>,
        source: NyquistSource,
        dual_prf: bool,
        high_prf_hz: Option<f64>,
        low_prf_hz: Option<f64>,
        wavelength_cm: Option<f64>,
        velocity_quantities: Vec<String>,
        usable: bool,
    }
    #[derive(Serialize)]
    struct Record<'a> {
        schema: &'a str,
        file: String,
        site: String,
        wavelength_cm: Option<f64>,
        band: &'a str,
        velocity_sweeps: usize,
        dealiasable: bool,
        sweeps: Vec<SweepNyquist>,
    }

    let sweeps: Vec<SweepNyquist> = volume
        .sweeps
        .iter()
        .map(|sweep| SweepNyquist {
            index: sweep.index,
            elevation_deg: sweep.elevation_deg,
            nyquist_ms: sweep.nyquist.interval_ms,
            source: sweep.nyquist.source,
            dual_prf: sweep.nyquist.dual_prf,
            high_prf_hz: sweep.nyquist.high_prf_hz,
            low_prf_hz: sweep.nyquist.low_prf_hz,
            wavelength_cm: volume.system.wavelength_cm,
            velocity_quantities: sweep
                .moments
                .iter()
                .filter(|m| rw_odim::is_radial_velocity(&m.quantity))
                .map(|m| m.quantity.clone())
                .collect(),
            usable: sweep.nyquist.is_usable(),
        })
        .collect();

    let record = Record {
        schema: NYQUIST_SCHEMA,
        file: args.file.display().to_string(),
        site: volume.source.label(),
        wavelength_cm: volume.system.wavelength_cm,
        band: band_of(volume.system.wavelength_cm),
        velocity_sweeps: volume.velocity_sweeps().len(),
        dealiasable: volume.velocity_is_dealiasable(),
        sweeps,
    };
    println!("{}", serde_json::to_string_pretty(&record)?);
    Ok(())
}

/// The radar band a wavelength falls in. European radar is mostly C-band, but
/// not entirely: the Romanian S-band sites are why this is reported rather
/// than assumed.
fn band_of(wavelength_cm: Option<f64>) -> &'static str {
    match wavelength_cm {
        Some(cm) if (2.0..=4.0).contains(&cm) => "X",
        Some(cm) if (4.0..=8.0).contains(&cm) => "C",
        Some(cm) if (8.0..=15.0).contains(&cm) => "S",
        Some(_) => "other",
        None => "unknown",
    }
}

fn cmd_export(args: &ExportArgs) -> Result<(), Box<dyn Error>> {
    let options = DecodeOptions {
        quantities: Some(vec![args.quantity.clone()]),
        sweep_indices: Some(vec![args.sweep]),
        geometry_only: false,
    };
    let volume = read_volume_with(&args.file, &options)?;
    let sweep = volume
        .sweeps
        .iter()
        .find(|s| s.index == args.sweep)
        .ok_or_else(|| {
            format!(
                "sweep /dataset{} not found in {}",
                args.sweep,
                args.file.display()
            )
        })?;
    let moment = sweep.moment(&args.quantity).ok_or_else(|| {
        format!(
            "quantity {} not found on /dataset{}; it carries {:?}",
            args.quantity,
            args.sweep,
            sweep.quantities()
        )
    })?;

    let values_path = with_suffix(&args.out, "values.f64");
    let censor_path = with_suffix(&args.out, "censor.u8");
    let json_path = with_suffix(&args.out, "json");

    write_f64_le(&values_path, &moment.values)?;
    write_u8(&censor_path, &moment.censor)?;

    #[derive(Serialize)]
    struct Record<'a> {
        schema: &'a str,
        #[serde(flatten)]
        header: VolumeHeader<'a>,
        sweep: SweepGeometry,
        moment: MomentReport,
        /// Per-ray azimuths, degrees clockwise from north in [0, 360).
        azimuth_deg: &'a [f64],
        /// Per-ray elevations when the file recorded them.
        ray_elevation_deg: Option<&'a [f64]>,
        /// Range to the centre of each bin, metres.
        gate_centre_range_m: Vec<f64>,
        layout: Layout,
    }
    #[derive(Serialize)]
    struct Layout {
        order: &'static str,
        shape: [usize; 2],
        values_file: String,
        values_dtype: &'static str,
        censor_file: String,
        censor_dtype: &'static str,
        censor_codes: Vec<CensorCode>,
    }
    #[derive(Serialize)]
    struct CensorCode {
        code: u8,
        name: &'static str,
        observed: bool,
    }

    let record = Record {
        schema: EXPORT_SCHEMA,
        header: VolumeHeader::new(&args.file, &volume),
        sweep: SweepGeometry::new(sweep),
        moment: MomentReport::new(moment),
        azimuth_deg: &sweep.azimuth_deg,
        ray_elevation_deg: sweep.ray_elevation_deg.as_deref(),
        gate_centre_range_m: (0..sweep.nbins)
            .map(|bin| sweep.gate_centre_range_m(bin))
            .collect(),
        layout: Layout {
            order: "row-major [ray][bin]",
            shape: [moment.nrays, moment.nbins],
            values_file: values_path.display().to_string(),
            values_dtype: "<f8",
            censor_file: censor_path.display().to_string(),
            censor_dtype: "|u1",
            censor_codes: [
                censor::MEASURED,
                censor::UNDETECT,
                censor::RESERVED_RANGE_FOLDED,
                censor::NOT_COLLECTED,
                censor::NODATA,
                censor::SENTINEL_AMBIGUOUS,
            ]
            .into_iter()
            .map(|code| CensorCode {
                code,
                name: censor::name(code),
                observed: censor::is_observed(code),
            })
            .collect(),
        },
    };
    let json = serde_json::to_string_pretty(&record)?;
    std::fs::write(&json_path, json.as_bytes())?;
    println!("{json}");
    Ok(())
}

fn with_suffix(prefix: &Path, suffix: &str) -> PathBuf {
    let mut name = prefix.as_os_str().to_os_string();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

fn write_f64_le(path: &Path, values: &[f64]) -> Result<(), Box<dyn Error>> {
    let mut out = BufWriter::new(File::create(path)?);
    for value in values {
        out.write_all(&value.to_le_bytes())?;
    }
    out.flush()?;
    Ok(())
}

fn write_u8(path: &Path, values: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut out = BufWriter::new(File::create(path)?);
    out.write_all(values)?;
    out.flush()?;
    Ok(())
}
