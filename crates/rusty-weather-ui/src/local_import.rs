use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{SystemTime, UNIX_EPOCH};

use netcrust::{File as NcFile, Variable as NcVariable};
use rustwx_core::{
    CanonicalField, FieldSelector, GridProjection, GridShape, LatLonGrid, SelectedField2D,
};
use rw_store::{DerivedFieldInput, WrittenHour, write_hour_from_fields_with_derived};

const LOCAL_IMPORT_MAX_SCAN_DEPTH: usize = 8;
const LOCAL_IMPORT_MAX_DISCOVERED_FILES: usize = 10_000;

#[derive(Debug)]
pub struct LocalImportTask {
    pub label: String,
    pub rx: Receiver<Result<LocalImportSummary, String>>,
}

#[derive(Debug, Clone)]
pub struct LocalImportSummary {
    pub store_root: PathBuf,
    pub model: String,
    pub run: String,
    pub files_seen: usize,
    pub hours_written: usize,
    pub variables: Vec<String>,
}

struct ImportedWrfFields {
    canonical: Vec<(String, SelectedField2D)>,
    raw_2d: Vec<RawField2D>,
}

struct RawField2D {
    name: String,
    units: String,
    values: Vec<f32>,
}

pub fn spawn_import_paths(paths: Vec<PathBuf>, store_root: PathBuf) -> LocalImportTask {
    let label = if paths.len() == 1 {
        format!("Import {}", display_name(&paths[0]))
    } else {
        format!("Import {} local files", paths.len())
    };
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("rw-ui-local-import".to_string())
        .spawn(move || {
            let result = import_paths(&paths, &store_root).map_err(|err| err.to_string());
            let _ = tx.send(result);
        })
        .expect("spawn local import worker");
    LocalImportTask { label, rx }
}

pub fn supported_files_in_folder(folder: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut stack = vec![(folder.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_supported_model_file(&path) {
                paths.push(path);
                if paths.len() >= LOCAL_IMPORT_MAX_DISCOVERED_FILES {
                    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                    return paths;
                }
            } else if depth < LOCAL_IMPORT_MAX_SCAN_DEPTH && path.is_dir() {
                stack.push((path, depth + 1));
            }
        }
    }
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    paths
}

pub fn is_supported_model_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.starts_with("wrfout")
        || matches!(
            path.extension()
                .and_then(|value| value.to_str())
                .map(|value| value.to_ascii_lowercase())
                .as_deref(),
            Some("nc" | "nc4" | "cdf")
        )
}

fn import_paths(paths: &[PathBuf], store_root: &Path) -> Result<LocalImportSummary, ImportError> {
    if paths.is_empty() {
        return Err(ImportError::NoFiles);
    }
    let mut files: Vec<PathBuf> = paths
        .iter()
        .filter(|path| is_supported_model_file(path))
        .cloned()
        .collect();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if files.is_empty() {
        return Err(ImportError::NoSupportedFiles);
    }
    if files.len() > u16::MAX as usize {
        return Err(ImportError::TooManyFiles(files.len()));
    }

    let model = "wrf".to_string();
    let run = import_run_name(&files);
    let mut all_vars = Vec::new();
    let mut written = Vec::<WrittenHour>::new();
    for (index, path) in files.iter().enumerate() {
        let hour = u16::try_from(index).expect("bounded above");
        let fields = read_wrf_2d_fields(path)?;
        if fields.canonical.is_empty() {
            return Err(ImportError::NoFields(path.clone()));
        }
        let refs = fields
            .canonical
            .iter()
            .map(|(name, field)| (name.as_str(), field))
            .collect::<Vec<_>>();
        let raw_refs = fields
            .raw_2d
            .iter()
            .map(|field| DerivedFieldInput {
                name: field.name.as_str(),
                units: field.units.as_str(),
                values: field.values.as_slice(),
            })
            .collect::<Vec<_>>();
        let result = write_hour_from_fields_with_derived(
            store_root,
            &model,
            &run,
            hour,
            &refs,
            &raw_refs,
            &[],
            writer_build(),
            now_unix(),
        )?;
        all_vars.extend(result.vars.iter().cloned());
        written.push(result);
    }
    all_vars.sort();
    all_vars.dedup();
    Ok(LocalImportSummary {
        store_root: store_root.to_path_buf(),
        model,
        run,
        files_seen: files.len(),
        hours_written: written.len(),
        variables: all_vars,
    })
}

fn read_wrf_2d_fields(path: &Path) -> Result<ImportedWrfFields, ImportError> {
    let nc = netcrust::open(path)?;
    let lat = read_first_2d_any(&nc, &["XLAT", "XLAT_M", "lat", "latitude"])?;
    let lon = read_first_2d_any(&nc, &["XLONG", "XLONG_M", "lon", "longitude"])?;
    if lat.nx != lon.nx || lat.ny != lon.ny || lat.values.len() != lon.values.len() {
        return Err(ImportError::GridMismatch(path.to_path_buf()));
    }
    let shape = GridShape::new(lat.nx, lat.ny)?;
    let grid = LatLonGrid::new(shape, lat.values, lon.values)?;
    let projection = wrf_projection(&nc);
    let mut canonical = Vec::new();

    push_direct(
        &mut canonical,
        &nc,
        &grid,
        projection.clone(),
        "T2",
        "temperature_2m",
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
        Some("K"),
    )?;
    push_direct(
        &mut canonical,
        &nc,
        &grid,
        projection.clone(),
        "U10",
        "u_10m",
        FieldSelector::height_agl(CanonicalField::UWind, 10),
        Some("m/s"),
    )?;
    push_direct(
        &mut canonical,
        &nc,
        &grid,
        projection.clone(),
        "V10",
        "v_10m",
        FieldSelector::height_agl(CanonicalField::VWind, 10),
        Some("m/s"),
    )?;
    push_direct(
        &mut canonical,
        &nc,
        &grid,
        projection.clone(),
        "PSFC",
        "surface_pressure",
        FieldSelector::surface(CanonicalField::Pressure),
        Some("Pa"),
    )?;
    push_direct(
        &mut canonical,
        &nc,
        &grid,
        projection.clone(),
        "HGT",
        "orography",
        FieldSelector::surface(CanonicalField::GeopotentialHeight),
        Some("m"),
    )?;
    push_direct(
        &mut canonical,
        &nc,
        &grid,
        projection.clone(),
        "SLP",
        "mslp",
        FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel),
        Some("Pa"),
    )?;
    push_direct(
        &mut canonical,
        &nc,
        &grid,
        projection.clone(),
        "REFD_MAX",
        "composite_reflectivity",
        FieldSelector::entire_atmosphere(CanonicalField::CompositeReflectivity),
        Some("dBZ"),
    )?;
    push_direct(
        &mut canonical,
        &nc,
        &grid,
        projection.clone(),
        "WSPD10MAX",
        "wind_speed_10m_max",
        FieldSelector::height_agl(CanonicalField::WindGust, 10),
        Some("m/s"),
    )?;

    if let (Some(u10), Some(v10)) = (read_first_2d(&nc, "U10")?, read_first_2d(&nc, "V10")?) {
        let values = combine_same_grid(&u10, &v10, |u, v| (u.mul_add(u, v * v)).sqrt())?;
        push_computed(
            &mut canonical,
            &grid,
            projection.clone(),
            "wind_speed_10m",
            FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
            "m/s",
            values,
        )?;
    }

    if let (Some(t2), Some(q2), Some(psfc)) = (
        read_first_2d(&nc, "T2")?,
        read_first_2d(&nc, "Q2")?,
        read_first_2d(&nc, "PSFC")?,
    ) {
        let dewpoint = derive_dewpoint_k(&t2, &q2, &psfc)?;
        push_computed(
            &mut canonical,
            &grid,
            projection.clone(),
            "dewpoint_2m",
            FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
            "K",
            dewpoint,
        )?;
        let rh = derive_relative_humidity_percent(&t2, &q2, &psfc)?;
        push_computed(
            &mut canonical,
            &grid,
            projection.clone(),
            "relative_humidity_2m",
            FieldSelector::height_agl(CanonicalField::RelativeHumidity, 2),
            "%",
            rh,
        )?;
    }

    if let (Some(rainc), Some(rainnc)) =
        (read_first_2d(&nc, "RAINC")?, read_first_2d(&nc, "RAINNC")?)
    {
        let rainsh = read_first_2d(&nc, "RAINSH")?;
        let values = combine_precip(&rainc, &rainnc, rainsh.as_ref())?;
        push_computed(
            &mut canonical,
            &grid,
            projection,
            "apcp",
            FieldSelector::surface(CanonicalField::TotalPrecipitation),
            "kg/m^2",
            values,
        )?;
    }

    let raw_2d = read_raw_wrf_mass_grid_fields(&nc, lat.nx, lat.ny)?;

    Ok(ImportedWrfFields { canonical, raw_2d })
}

fn push_direct(
    out: &mut Vec<(String, SelectedField2D)>,
    nc: &NcFile,
    grid: &LatLonGrid,
    projection: Option<GridProjection>,
    wrf_name: &str,
    store_name: &str,
    selector: FieldSelector,
    units_override: Option<&str>,
) -> Result<(), ImportError> {
    let Some(plane) = read_first_2d(nc, wrf_name)? else {
        return Ok(());
    };
    let units = units_override
        .map(str::to_string)
        .or_else(|| variable_units(nc, wrf_name))
        .unwrap_or_else(|| selector.native_units().to_string());
    push_computed(
        out,
        grid,
        projection,
        store_name,
        selector,
        &units,
        plane.values,
    )
}

fn push_computed(
    out: &mut Vec<(String, SelectedField2D)>,
    grid: &LatLonGrid,
    projection: Option<GridProjection>,
    store_name: &str,
    selector: FieldSelector,
    units: &str,
    values: Vec<f32>,
) -> Result<(), ImportError> {
    let mut field = SelectedField2D::new(selector, units, grid.clone(), values)?;
    if let Some(projection) = projection {
        field = field.with_projection(projection);
    }
    out.push((store_name.to_string(), field));
    Ok(())
}

fn read_raw_wrf_mass_grid_fields(
    nc: &NcFile,
    nx: usize,
    ny: usize,
) -> Result<Vec<RawField2D>, ImportError> {
    let mut seen = HashSet::<String>::new();
    let mut raw = Vec::new();
    for var in nc.variables()? {
        let wrf_name = var.name();
        if !is_raw_wrf_mass_grid_variable(&var, nx, ny) || !raw_wrf_variable_allowed(wrf_name) {
            continue;
        }
        let Some(plane) = read_first_2d(nc, wrf_name)? else {
            continue;
        };
        if plane.nx != nx || plane.ny != ny {
            continue;
        }
        let name = format!("wrf_{}", sanitize_store_var_name(wrf_name));
        if name == "wrf_" || !seen.insert(name.clone()) {
            continue;
        }
        raw.push(RawField2D {
            name,
            units: variable_units(nc, wrf_name).unwrap_or_else(|| "1".to_string()),
            values: plane.values,
        });
    }
    Ok(raw)
}

fn is_raw_wrf_mass_grid_variable(var: &NcVariable, nx: usize, ny: usize) -> bool {
    let dims = var.dimensions();
    let shape = var.shape();
    dims.len() == 3
        && shape.len() == 3
        && dims[0].name() == "Time"
        && dims[1].name() == "south_north"
        && dims[2].name() == "west_east"
        && shape[1] == ny
        && shape[2] == nx
}

fn raw_wrf_variable_allowed(name: &str) -> bool {
    !matches!(
        name.to_ascii_uppercase().as_str(),
        "XLAT"
            | "XLONG"
            | "XLAT_M"
            | "XLONG_M"
            | "CLAT"
            | "NEST_POS"
            | "AREA2D"
            | "DX2D"
            | "MAPFAC_M"
            | "MAPFAC_MX"
            | "MAPFAC_MY"
            | "F"
            | "E"
            | "SINALPHA"
            | "COSALPHA"
    )
}

fn sanitize_store_var_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_underscore = false;
        } else if !last_was_underscore {
            out.push('_');
            last_was_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

#[derive(Debug, Clone)]
struct Plane2D {
    nx: usize,
    ny: usize,
    values: Vec<f32>,
}

fn read_first_2d_any(nc: &NcFile, names: &[&str]) -> Result<Plane2D, ImportError> {
    for name in names {
        if let Some(plane) = read_first_2d(nc, name)? {
            return Ok(plane);
        }
    }
    Err(ImportError::MissingAny(
        names.iter().map(|value| value.to_string()).collect(),
    ))
}

fn read_first_2d(nc: &NcFile, name: &str) -> Result<Option<Plane2D>, ImportError> {
    if nc.variable(name).is_none() {
        return Ok(None);
    }
    let array = nc.read_array_f64_first_record_or_all(name)?;
    let shape = array.shape();
    if shape.len() < 2 {
        return Ok(None);
    }
    let ny = shape[shape.len() - 2];
    let nx = shape[shape.len() - 1];
    let cells = nx
        .checked_mul(ny)
        .ok_or_else(|| ImportError::BadShape(name.to_string(), shape.to_vec()))?;
    let values = array.values();
    if values.len() < cells {
        return Err(ImportError::BadShape(name.to_string(), shape.to_vec()));
    }
    let offset = values.len() - cells;
    Ok(Some(Plane2D {
        nx,
        ny,
        values: values[offset..]
            .iter()
            .map(|value| {
                if value.is_finite() {
                    *value as f32
                } else {
                    f32::NAN
                }
            })
            .collect(),
    }))
}

fn variable_units(nc: &NcFile, name: &str) -> Option<String> {
    nc.variable(name)?
        .attribute("units")
        .and_then(|attr| attr.as_string())
        .map(str::to_string)
}

fn combine_same_grid(
    a: &Plane2D,
    b: &Plane2D,
    f: impl Fn(f32, f32) -> f32,
) -> Result<Vec<f32>, ImportError> {
    ensure_same_grid(a, b)?;
    Ok(a.values
        .iter()
        .zip(&b.values)
        .map(|(&a, &b)| {
            if a.is_finite() && b.is_finite() {
                f(a, b)
            } else {
                f32::NAN
            }
        })
        .collect())
}

fn combine_precip(
    rainc: &Plane2D,
    rainnc: &Plane2D,
    rainsh: Option<&Plane2D>,
) -> Result<Vec<f32>, ImportError> {
    ensure_same_grid(rainc, rainnc)?;
    if let Some(rainsh) = rainsh {
        ensure_same_grid(rainc, rainsh)?;
    }
    Ok((0..rainc.values.len())
        .map(|idx| {
            let mut value = 0.0;
            let mut valid = true;
            for plane in [Some(rainc), Some(rainnc), rainsh].into_iter().flatten() {
                let v = plane.values[idx];
                if v.is_finite() {
                    value += v;
                } else {
                    valid = false;
                }
            }
            if valid { value } else { f32::NAN }
        })
        .collect())
}

fn derive_dewpoint_k(t2: &Plane2D, q2: &Plane2D, psfc: &Plane2D) -> Result<Vec<f32>, ImportError> {
    ensure_same_grid(t2, q2)?;
    ensure_same_grid(t2, psfc)?;
    Ok((0..t2.values.len())
        .map(|idx| dewpoint_from_q_psfc(q2.values[idx], psfc.values[idx]))
        .collect())
}

fn derive_relative_humidity_percent(
    t2: &Plane2D,
    q2: &Plane2D,
    psfc: &Plane2D,
) -> Result<Vec<f32>, ImportError> {
    ensure_same_grid(t2, q2)?;
    ensure_same_grid(t2, psfc)?;
    Ok((0..t2.values.len())
        .map(|idx| {
            relative_humidity_from_t_q_psfc(t2.values[idx], q2.values[idx], psfc.values[idx])
        })
        .collect())
}

fn dewpoint_from_q_psfc(q: f32, p_pa: f32) -> f32 {
    if !q.is_finite() || !p_pa.is_finite() || q <= 0.0 || p_pa <= 0.0 {
        return f32::NAN;
    }
    let q = q as f64;
    let p = p_pa as f64;
    let e = (q * p / (0.622 + 0.378 * q)).max(1.0);
    let ln = (e / 611.2).ln();
    let td_c = 243.5 * ln / (17.67 - ln);
    (td_c + 273.15) as f32
}

fn relative_humidity_from_t_q_psfc(t_k: f32, q: f32, p_pa: f32) -> f32 {
    if !t_k.is_finite() || !q.is_finite() || !p_pa.is_finite() || t_k <= 0.0 {
        return f32::NAN;
    }
    let e = q as f64 * p_pa as f64 / (0.622 + 0.378 * q as f64);
    let t_c = t_k as f64 - 273.15;
    let es = 611.2 * (17.67 * t_c / (t_c + 243.5)).exp();
    (100.0 * e / es).clamp(0.0, 100.0) as f32
}

fn ensure_same_grid(a: &Plane2D, b: &Plane2D) -> Result<(), ImportError> {
    if a.nx == b.nx && a.ny == b.ny && a.values.len() == b.values.len() {
        Ok(())
    } else {
        Err(ImportError::PlaneMismatch)
    }
}

fn wrf_projection(nc: &NcFile) -> Option<GridProjection> {
    let map_proj = global_attr_f64(nc, "MAP_PROJ")? as i32;
    match map_proj {
        1 => Some(GridProjection::LambertConformal {
            standard_parallel_1_deg: global_attr_f64(nc, "TRUELAT1").unwrap_or(30.0),
            standard_parallel_2_deg: global_attr_f64(nc, "TRUELAT2")
                .or_else(|| global_attr_f64(nc, "TRUELAT1"))
                .unwrap_or(60.0),
            central_meridian_deg: global_attr_f64(nc, "STAND_LON")
                .or_else(|| global_attr_f64(nc, "CEN_LON"))
                .unwrap_or(0.0),
        }),
        2 => Some(GridProjection::PolarStereographic {
            true_latitude_deg: global_attr_f64(nc, "TRUELAT1").unwrap_or(60.0),
            central_meridian_deg: global_attr_f64(nc, "STAND_LON")
                .or_else(|| global_attr_f64(nc, "CEN_LON"))
                .unwrap_or(0.0),
            south_pole_on_projection_plane: global_attr_f64(nc, "CEN_LAT").unwrap_or(45.0) < 0.0,
        }),
        3 => Some(GridProjection::Mercator {
            latitude_of_true_scale_deg: global_attr_f64(nc, "TRUELAT1").unwrap_or(0.0),
            central_meridian_deg: global_attr_f64(nc, "STAND_LON")
                .or_else(|| global_attr_f64(nc, "CEN_LON"))
                .unwrap_or(0.0),
        }),
        6 => Some(GridProjection::Geographic),
        other => Some(GridProjection::Other {
            template: other.max(0) as u16,
        }),
    }
}

fn global_attr_f64(nc: &NcFile, name: &str) -> Option<f64> {
    nc.attribute(name).and_then(|attr| attr.as_f64())
}

fn import_run_name(paths: &[PathBuf]) -> String {
    let first = paths.first();
    let stamp = first
        .and_then(|path| timestamp_from_path(path))
        .unwrap_or_else(|| {
            first
                .and_then(|path| path.file_stem())
                .and_then(|value| value.to_str())
                .unwrap_or("local")
                .to_string()
        });
    sanitize_run_name(&format!("local_wrf_{stamp}"))
}

fn timestamp_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let bytes = name.as_bytes();
    for start in 0..bytes.len().saturating_sub(18) {
        let slice = name.get(start..start + 19)?;
        if is_wrf_timestamp(slice) {
            return Some(normalize_wrf_timestamp(slice));
        }
    }
    None
}

fn is_wrf_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'_'
        && matches!(b[13], b':' | b'_')
        && matches!(b[16], b':' | b'_')
        && b.iter()
            .enumerate()
            .all(|(idx, byte)| matches!(idx, 4 | 7 | 10 | 13 | 16) || byte.is_ascii_digit())
}

fn normalize_wrf_timestamp(value: &str) -> String {
    let date = value[..10]
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    let time = value[11..]
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    format!("{date}_{time}")
}

fn sanitize_run_name(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "local_wrf".to_string()
    } else {
        out
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("local file")
        .to_string()
}

fn writer_build() -> &'static str {
    concat!("rusty-weather-ui-local-import-", env!("CARGO_PKG_VERSION"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = now_unix();
        std::env::temp_dir().join(format!("rw-local-import-{name}-{unique}"))
    }

    #[test]
    fn wrf_timestamp_accepts_colon_and_underscore_time() {
        let colon = Path::new("wrfout_d02_1974-04-03_09:00:00");
        let underscore = Path::new("wrfout_d02_1974-04-03_09_00_00");
        assert_eq!(
            timestamp_from_path(colon).as_deref(),
            Some("19740403_090000")
        );
        assert_eq!(
            timestamp_from_path(underscore).as_deref(),
            Some("19740403_090000")
        );
    }

    #[test]
    fn folder_scan_finds_extensionless_nested_wrf_files() {
        let root = temp_dir("scan");
        let nested = root.join("member").join("d02");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::File::create(nested.join("wrfout_d02_1974-04-03_09_00_00")).unwrap();
        std::fs::File::create(root.join("not_a_model.txt")).unwrap();

        let files = supported_files_in_folder(&root);
        assert_eq!(files.len(), 1);
        assert!(
            files[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("wrfout")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn optional_wrf_fixture_imports_to_store() {
        let Ok(fixture) = std::env::var("RW_LOCAL_IMPORT_FIXTURE") else {
            eprintln!("skipping WRF import fixture; set RW_LOCAL_IMPORT_FIXTURE");
            return;
        };
        let store_root = temp_dir("store");
        let summary = import_paths(&[PathBuf::from(&fixture)], &store_root).unwrap();
        assert_eq!(summary.model, "wrf");
        assert_eq!(summary.hours_written, 1);
        assert!(summary.variables.iter().any(|var| var == "temperature_2m"));
        assert!(summary.variables.iter().any(|var| var == "dewpoint_2m"));
        assert!(summary.variables.iter().any(|var| var == "wind_speed_10m"));
        assert!(summary.variables.iter().any(|var| var == "apcp"));

        let _ = std::fs::remove_dir_all(store_root);
    }
}

#[derive(Debug, thiserror::Error)]
enum ImportError {
    #[error("no files selected")]
    NoFiles,
    #[error("no supported local model files found in selection")]
    NoSupportedFiles,
    #[error("folder contains too many files to map into rw-store forecast-hour slots: {0}")]
    TooManyFiles(usize),
    #[error("missing any required grid variable: {0:?}")]
    MissingAny(Vec<String>),
    #[error("bad shape for variable {0}: {1:?}")]
    BadShape(String, Vec<usize>),
    #[error("XLAT/XLONG grid dimensions do not match in {0}")]
    GridMismatch(PathBuf),
    #[error("WRF planes do not share the same grid shape")]
    PlaneMismatch,
    #[error("no importable 2D WRF fields found in {0}")]
    NoFields(PathBuf),
    #[error(transparent)]
    Netcdf(#[from] netcrust::Error),
    #[error(transparent)]
    Core(#[from] rustwx_core::RustwxError),
    #[error(transparent)]
    Store(#[from] rw_store::RwStoreError),
}
