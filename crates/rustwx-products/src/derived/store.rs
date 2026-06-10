//! Store-ingest derived lane: compute every supported derived recipe grid
//! (non-heavy and heavy/ECAPE) from already-assembled surface + pressure
//! bundles, with no fetching, projection, or rendering involved.
//!
//! This is the same compute path the render/query lanes use. Non-heavy:
//! [`compute_derived_fields_generic`] does the shared input prep (pressure
//! volume, height-AGL assembly, grid spacing) once for all recipes, and
//! [`derived_query_field_from_computed`] maps each recipe to its grid and
//! units. Heavy: [`prepare_heavy_volume`] +
//! [`compute_ecape_map_fields_with_prepared_volume`] — the exact prep +
//! kernel dispatch the ECAPE/heavy render lane runs — produce the panel
//! fields, which are mapped to recipe slugs by their artifact slugs (the
//! two namespaces are identical for heavy recipes). Grids stored at ingest
//! are therefore bit-identical to what the corresponding render lane would
//! compute from the same inputs.

use crate::ecape::{EcapeMapFieldsTiming, compute_ecape_map_fields_with_prepared_volume_timed};
use crate::gridded::{PressureFields, SurfaceFields, prepare_heavy_volume_timed};

use super::compute::compute_derived_fields_generic;
use super::inventory::supported_derived_recipe_inventory;
use super::query::derived_query_field_from_computed;
use super::recipes::DerivedRecipe;

/// One derived grid computed for store ingest: the recipe slug (also the
/// store variable name), display units, and the full-grid row-major values.
#[derive(Debug, Clone)]
pub struct StoreDerivedGrid {
    pub slug: &'static str,
    pub units: String,
    pub values: Vec<f64>,
}

/// Slugs of every supported non-heavy derived recipe, in inventory order.
/// These are the grids [`compute_store_derived_grids`] realizes; heavy
/// (ECAPE-class) recipes stay out of the ingest path.
pub fn store_derived_recipe_slugs() -> Vec<&'static str> {
    supported_derived_recipe_inventory()
        .iter()
        .filter(|entry| !entry.heavy)
        .map(|entry| entry.slug)
        .collect()
}

/// Compute all non-heavy derived recipe grids in one shared pass over the
/// prepared surface + pressure inputs. Fails as a whole if the shared prep
/// or any recipe fails: ingest inputs carry the full field set, so a
/// missing dependency is a wiring bug, not an expected degradation.
pub fn compute_store_derived_grids(
    surface: &SurfaceFields,
    pressure: &PressureFields,
) -> Result<Vec<StoreDerivedGrid>, Box<dyn std::error::Error>> {
    let recipes = store_derived_recipe_slugs()
        .into_iter()
        .map(|slug| DerivedRecipe::parse(slug).map_err(std::io::Error::other))
        .collect::<Result<Vec<_>, _>>()?;
    let computed = compute_derived_fields_generic(surface, pressure, &recipes)?;
    let mut grids = Vec::with_capacity(recipes.len());
    for recipe in recipes {
        let query = derived_query_field_from_computed(surface.nx, surface.ny, recipe, &computed)?;
        grids.push(StoreDerivedGrid {
            slug: recipe.slug(),
            units: query.units,
            values: query.values,
        });
    }
    Ok(grids)
}

/// One heavy recipe skipped at store ingest, with the documented reason
/// (the blocker pattern: record why, never invent a substitute).
#[derive(Debug, Clone)]
pub struct StoreHeavySkip {
    pub slug: &'static str,
    pub reason: String,
}

/// Where the heavy stage's wall time went: the shared height-AGL prep plus
/// the per-kernel breakdown from the ECAPE lane. Observation only — the
/// kernels and their order are identical to the untimed render path.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreHeavyTiming {
    pub prepare_height_agl_ms: u128,
    pub kernels: EcapeMapFieldsTiming,
}

/// Output of [`compute_store_heavy_grids`]: realized heavy recipe grids in
/// inventory order, recipes skipped with reasons, the ECAPE triplet's
/// per-column failure count (cells whose parcel ascent failed — those carry
/// NaN in the grids, same as the render lane), and the stage timing.
#[derive(Debug, Clone)]
pub struct StoreHeavyGrids {
    pub grids: Vec<StoreDerivedGrid>,
    pub skipped: Vec<StoreHeavySkip>,
    pub ecape_failure_count: usize,
    pub timing: StoreHeavyTiming,
}

/// Slugs of every heavy (ECAPE-class) derived recipe, in inventory order.
/// These are the grids [`compute_store_heavy_grids`] realizes.
pub fn store_heavy_recipe_slugs() -> Vec<&'static str> {
    supported_derived_recipe_inventory()
        .iter()
        .filter(|entry| entry.heavy)
        .map(|entry| entry.slug)
        .collect()
}

/// The heavy recipes that depend on the model's *native* CAPE planes
/// (decoded from the surface GRIB's CAPE messages). These are the only
/// heavy recipes allowed to skip: the surface file may simply not carry a
/// matching CAPE message, the same optionality the render lane has.
const NATIVE_RATIO_SLUGS: [(&str, &str); 3] = [
    ("sb_ecape_native_cape_ratio", "surface-based (level type 1)"),
    ("ml_ecape_native_cape_ratio", "0-90 mb mixed layer"),
    ("mu_ecape_native_cape_ratio", "0-255 mb most-unstable layer"),
];

/// Compute every heavy (ECAPE-class) derived recipe grid through the
/// existing heavy lane: `prepare_heavy_volume` (height-AGL assembly, no
/// pressure broadcast) feeding `compute_ecape_map_fields_with_prepared_volume`
/// (ECAPE triplet, wind diagnostics, ML LCL, and the experimental
/// composites). The native-CAPE ratio recipes realize only when the
/// corresponding `surface.native_*cape_jkg` plane is present; their absence
/// is recorded as a skip, not an error. Any other missing recipe is a
/// wiring bug and fails the whole call.
pub fn compute_store_heavy_grids(
    surface: &SurfaceFields,
    pressure: &PressureFields,
) -> Result<StoreHeavyGrids, Box<dyn std::error::Error>> {
    let (prepared, prep_timing) = prepare_heavy_volume_timed(surface, pressure, false)?;
    let (fields, ecape_failure_count, kernel_timing) =
        compute_ecape_map_fields_with_prepared_volume_timed(surface, pressure, &prepared)?;
    drop(prepared);
    let timing = StoreHeavyTiming {
        prepare_height_agl_ms: prep_timing.prepare_height_agl_ms,
        kernels: kernel_timing,
    };

    let mut remaining: Vec<crate::shared_context::WeatherPanelField> = fields;
    let mut grids = Vec::new();
    let mut skipped = Vec::new();
    for slug in store_heavy_recipe_slugs() {
        match remaining
            .iter()
            .position(|field| field.artifact_slug() == slug)
        {
            Some(index) => {
                let field = remaining.swap_remove(index);
                grids.push(StoreDerivedGrid {
                    slug,
                    units: field.units,
                    values: field.values,
                });
            }
            None => match NATIVE_RATIO_SLUGS.iter().find(|(have, _)| *have == slug) {
                Some((_, layer)) => skipped.push(StoreHeavySkip {
                    slug,
                    reason: format!(
                        "surface inputs carry no native {layer} CAPE plane \
                         (no matching CAPE message in the surface GRIB)"
                    ),
                }),
                None => {
                    return Err(format!(
                        "heavy recipe '{slug}' missing from the ECAPE field set; \
                         this is a wiring bug, not a degradation"
                    )
                    .into());
                }
            },
        }
    }
    Ok(StoreHeavyGrids {
        grids,
        skipped,
        ecape_failure_count,
        timing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The heavy store inventory is the 16 ECAPE-class recipes, in
    /// inventory order, and every slug maps 1:1 onto a heavy-lane panel
    /// artifact slug (the mapping `compute_store_heavy_grids` relies on).
    #[test]
    fn heavy_store_slugs_pin_the_sixteen_ecape_recipes() {
        assert_eq!(
            store_heavy_recipe_slugs(),
            vec![
                "sbecape",
                "mlecape",
                "muecape",
                "sb_ecape_derived_cape_ratio",
                "ml_ecape_derived_cape_ratio",
                "mu_ecape_derived_cape_ratio",
                "sb_ecape_native_cape_ratio",
                "ml_ecape_native_cape_ratio",
                "mu_ecape_native_cape_ratio",
                "sbncape",
                "sbecin",
                "mlecin",
                "ecape_scp",
                "ecape_ehi_0_1km",
                "ecape_ehi_0_3km",
                "ecape_stp",
            ]
        );
    }

    /// Heavy + non-heavy store slugs partition the supported inventory:
    /// nothing is double-stored and nothing is dropped.
    #[test]
    fn heavy_and_derived_store_slugs_partition_the_inventory() {
        let mut combined = store_derived_recipe_slugs();
        combined.extend(store_heavy_recipe_slugs());
        combined.sort_unstable();
        let mut inventory: Vec<&str> = supported_derived_recipe_inventory()
            .iter()
            .map(|entry| entry.slug)
            .collect();
        inventory.sort_unstable();
        assert_eq!(combined, inventory);
    }
}
