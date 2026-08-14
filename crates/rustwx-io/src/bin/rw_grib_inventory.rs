use std::env;
use std::path::PathBuf;

use grib_core::grib2::{Grib2File, level_name, parameter_name};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: rw_grib_inventory <GRIB2_FILE>")?;
    let bytes = std::fs::read(&path)?;
    let grib = Grib2File::from_bytes(&bytes)?;
    if grib.messages.is_empty() {
        return Err(format!("'{}' contains no GRIB2 messages", path.display()).into());
    }

    println!(
        "{}: {} bytes, {} message(s)",
        path.display(),
        bytes.len(),
        grib.messages.len()
    );
    for (index, message) in grib.messages.iter().enumerate() {
        println!(
            "{index:04} d/c/n={}/{}/{} {:<36} level={}:{} ({}) grid={} {}x{} scan=0x{:02x} resolution=0x{:02x} south_pole=({:.6},{:.6}) rotation={:.6} data_rep={} pdt={} forecast={}/{} ensemble={:?}/{:?}/{:?} derived={:?} percentile={:?} probability={:?}/{:?}/{:?}/{:?} statistics={:?}/{:?}/{:?}",
            message.discipline,
            message.product.parameter_category,
            message.product.parameter_number,
            parameter_name(
                message.discipline,
                message.product.parameter_category,
                message.product.parameter_number,
            ),
            message.product.level_type,
            message.product.level_value,
            level_name(message.product.level_type),
            message.grid.template,
            message.grid.nx,
            message.grid.ny,
            message.grid.scan_mode,
            message.grid.resolution_flags,
            message.grid.south_pole_lat,
            message.grid.south_pole_lon,
            message.grid.rotation_angle,
            message.data_rep.template,
            message.product.template,
            message.product.forecast_time,
            message.product.time_range_unit,
            message.product.ensemble_type,
            message.product.perturbation_number,
            message.product.num_forecasts_in_ensemble,
            message.product.derived_forecast_type,
            message.product.percentile_value,
            message.product.probability_number,
            message.product.total_number_of_probabilities,
            message.product.probability_type,
            (
                message.product.probability_lower_limit,
                message.product.probability_upper_limit,
            ),
            message.product.statistical_process_type,
            message.product.statistical_time_range_unit,
            message.product.time_range_length,
        );
    }
    Ok(())
}
