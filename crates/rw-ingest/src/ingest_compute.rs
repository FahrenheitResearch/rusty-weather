//! Ingest-time derived + heavy precompute: decode the surface + pressure
//! thermodynamic pair from the already-fetched family GRIB bytes through
//! the SAME products decode lane the derived/heavy render paths use
//! (`rustwx_products::gridded::decode_store_thermo_pair` — same message
//! matching, same moisture preference with specific humidity first, same
//! f64 precision), then run all 29 non-heavy derived recipes
//! (`rustwx_products::derived::compute_store_derived_grids`) and all 16
//! heavy ECAPE-class recipes
//! (`rustwx_products::derived::compute_store_heavy_grids` — the exact
//! prep + kernel dispatch the heavy render lane runs) through the existing
//! products compute lanes, handing back f32 grids ready to store as
//! ordinary 2D variables named by recipe slug.
//!
//! No science lives here, and no input lives here either: an earlier
//! version assembled the compute inputs from the f32 extraction planes,
//! which rounded every input through f32 and — worse — sourced 2 m
//! moisture from dewpoint while the render lane's decode prefers the 2 m
//! specific-humidity message the HRRR sfc file actually carries. Stored
//! grids were therefore NOT the grids the render lane computes. Decoding
//! the pair through the render lane's own decoder makes the stored grids
//! bit-identical to a render-lane compute over the same files (the only
//! rounding is the single f64 -> f32 cast below, which is exactly the cast
//! the render lane applies when it builds its `Field2D`). Native CAPE
//! planes (the heavy native-ratio denominators) ride on the decoded
//! `SurfaceFields` the same way they do in the render lane.

use rayon::prelude::*;
use rustwx_products::derived::{
    StoreHeavyTiming, compute_store_derived_grids, compute_store_heavy_grids,
};
use rustwx_products::gridded::decode_store_thermo_pair_owned;

/// One derived grid ready to store: variable name (the recipe slug), display
/// units, and full-grid row-major values.
pub struct DerivedGrid2D {
    pub name: &'static str,
    pub units: String,
    pub values: Vec<f32>,
}

/// The decoded products-side compute inputs for one hour: the f64
/// `SurfaceFields`/`PressureFields` pair every store compute stage
/// (derived and heavy) consumes. Decoded once per hour by
/// [`decode_products_inputs`].
pub struct ProductsComputeInputs {
    pub surface: rustwx_products::gridded::SurfaceFields,
    pub pressure: rustwx_products::gridded::PressureFields,
}

/// Output of the heavy (ECAPE-class) compute stage: realized grids,
/// recipes skipped with the products lane's documented reason, the ECAPE
/// triplet's per-column failure count, and the products lane's per-kernel
/// timing breakdown for honest stage reporting.
pub struct HeavyGrids2D {
    pub grids: Vec<DerivedGrid2D>,
    pub skipped: Vec<(&'static str, String)>,
    pub ecape_failure_count: usize,
    pub timing: StoreHeavyTiming,
}

/// Decode the compute inputs from the fetched sfc + prs bytes via the
/// render lane's own thermo decoder. The optional pressure volumes are
/// skipped (no store-computed recipe consumes them); everything else —
/// including the native CAPE planes the heavy native-ratio recipes
/// divide by — is exactly what the render lane sees.
///
/// Takes the raw buffers by value so each is freed at its true last use
/// inside the decode (surface bytes after the surface decode, pressure
/// bytes once the parser owns its message copies) instead of riding
/// resident through both compute stages.
pub fn decode_products_inputs(
    surface_bytes: Vec<u8>,
    pressure_bytes: Vec<u8>,
) -> Result<ProductsComputeInputs, Box<dyn std::error::Error>> {
    let (surface, pressure) = decode_store_thermo_pair_owned(surface_bytes, pressure_bytes)?;
    Ok(ProductsComputeInputs { surface, pressure })
}

/// Run the shared non-heavy compute pass on the decoded inputs and convert
/// the grids to f32 — the same cast the render lane applies building its
/// `Field2D`, so the stored f32 grid equals the render lane's raster input
/// bit for bit. The expensive recipe kernels are rayon-parallel inside
/// rustwx-calc.
pub fn compute_derived_2d_from_inputs(
    inputs: &ProductsComputeInputs,
) -> Result<Vec<DerivedGrid2D>, Box<dyn std::error::Error>> {
    let grids = compute_store_derived_grids(&inputs.surface, &inputs.pressure)?;
    Ok(grids
        .into_iter()
        .map(|grid| DerivedGrid2D {
            name: grid.slug,
            units: grid.units,
            values: grid.values.par_iter().map(|&value| value as f32).collect(),
        })
        .collect())
}

/// Run the heavy ECAPE-class compute pass (the heavy render lane's exact
/// prep + kernels via `compute_store_heavy_grids`) on the decoded inputs
/// and convert the grids to f32. The native-CAPE ratio recipes realize
/// only when the decode found the matching native plane; otherwise they
/// come back in `skipped` with the products lane's documented reason.
pub fn compute_heavy_2d_from_inputs(
    inputs: &ProductsComputeInputs,
) -> Result<HeavyGrids2D, Box<dyn std::error::Error>> {
    let heavy = compute_store_heavy_grids(&inputs.surface, &inputs.pressure)?;
    Ok(HeavyGrids2D {
        grids: heavy
            .grids
            .into_iter()
            .map(|grid| DerivedGrid2D {
                name: grid.slug,
                units: grid.units,
                values: grid.values.par_iter().map(|&value| value as f32).collect(),
            })
            .collect(),
        skipped: heavy
            .skipped
            .into_iter()
            .map(|skip| (skip.slug, skip.reason))
            .collect(),
        ecape_failure_count: heavy.ecape_failure_count,
        timing: heavy.timing,
    })
}
