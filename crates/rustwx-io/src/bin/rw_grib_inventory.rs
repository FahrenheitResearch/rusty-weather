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
            "{index:04} d/c/n={}/{}/{} {:<36} level={}:{} ({}) grid={} {}x{} flags=0x{:02x} data_rep={} pdt={}",
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
            message.grid.resolution_flags,
            message.data_rep.template,
            message.product.template,
        );
    }
    Ok(())
}
