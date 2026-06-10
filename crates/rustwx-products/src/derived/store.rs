//! Store-ingest derived lane: compute every supported non-heavy derived
//! recipe grid from already-assembled surface + pressure bundles, with no
//! fetching, projection, or rendering involved.
//!
//! This is the same compute path the render/query lanes use —
//! [`compute_derived_fields_generic`] does the shared input prep (pressure
//! volume, height-AGL assembly, grid spacing) once for all recipes, and
//! [`derived_query_field_from_computed`] maps each recipe to its grid and
//! units — so grids stored at ingest are bit-identical to what the derived
//! render lane would compute from the same inputs.

use crate::gridded::{PressureFields, SurfaceFields};

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
