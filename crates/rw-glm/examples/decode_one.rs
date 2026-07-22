//! Decode a single GOES GLM L2 LCFA granule and print a summary.
//!
//! Used to verify the vendored pure-Rust HDF5 reader against NOAA's
//! post-2026-07-09 granules, which are written with shuffle + deflate
//! filters (the legacy rustwx 0.4.4 reader cannot decode those and reports
//! the variables as missing).
//!
//! Usage: cargo run -p rw-glm --example decode_one -- <granule.nc>
fn main() {
    let path = std::env::args().nth(1).expect("usage: decode_one <granule.nc>");
    let path = std::path::Path::new(&path);
    match rw_glm::granule::decode_granule(path) {
        Ok(granule) => {
            println!("OK  {}", path.display());
            println!("  flashes: {}", granule.flashes.len());
            if let Some(first) = granule.flashes.first() {
                println!(
                    "  first flash: lat={:.3} lon={:.3} energy={:?}",
                    first.lat, first.lon, first.energy
                );
            }
        }
        Err(err) => {
            println!("FAIL {}", path.display());
            println!("  error: {err}");
            std::process::exit(1);
        }
    }
}
