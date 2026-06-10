#![allow(dead_code)]

//! Store -> render glue shared by the render-side bins (`rw_render`, and
//! Task 6's `rw_batch`): open one stored hour, resolve plot-recipe
//! `FieldSelector`s and derived recipe slugs against the stored variable
//! metadata (built from each variable's selector JSON, not hardcoded
//! tables, so coverage is provable), and feed the EXISTING rustwx-products
//! render paths — `render_direct_recipes_from_selected_fields` for direct
//! recipes and `render_derived_recipes_from_store_grids` for derived/heavy
//! recipes. No render logic lives here; this module only reads fields and
//! reports which requested products cannot resolve against the store.
//!
//! Reads are full-field (`read_full_2d` via `read_field_2d`/`read_grid_2d`,
//! ~3.6 ms per field): the direct render path crops in render space from
//! the full grid (`crop_direct_fields_for_domain` inside the products
//! crate) and the derived path crops values after the projected-domain
//! classification, so a store-side windowed read would change the data the
//! proven render paths see. Windowed reads stay a later optimization for
//! when a render lane learns to consume pre-windowed grids.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rustwx_core::{FieldSelector, SelectedField2D};
use rustwx_models::{LatestRun, plot_recipe_fetch_plan};
use rustwx_products::derived::{
    DerivedBatchRequest, DerivedRenderedRecipe, StoreProductGrid,
    render_derived_recipes_from_store_grids,
};
use rustwx_products::direct::{
    DirectBatchRequest, DirectRenderedRecipe, render_direct_recipes_from_selected_fields,
};
use rw_store::error::RwStoreError;
use rw_store::grid::GridFile;
use rw_store::ingest::{StoredField2D, derived_selector_slug, read_field_2d, read_grid_2d};
use rw_store::reader::HourReader;

/// One stored hour opened for rendering: the hour reader, the run grid,
/// and the resolution maps built from the stored variable metadata.
pub struct StoreFieldSource {
    hour_path: PathBuf,
    reader: HourReader,
    grid: GridFile,
    /// `FieldSelector` -> stored 2D variable name. First write wins on
    /// selector collisions, mirroring GRIB file-order semantics: the sfc
    /// file's two APCP accumulations share the plain TotalPrecipitation
    /// selector, and the run total (`apcp_run_total`, stored first) is the
    /// one the direct lane's extraction would have picked.
    selector_vars: HashMap<FieldSelector, String>,
    /// Derived/heavy variable slugs present in this hour (store order),
    /// keyed from each variable's `{"derived": slug}` selector marker.
    derived_slugs: Vec<String>,
}

impl StoreFieldSource {
    /// Open `<store_root>/<model_slug>/<run_slug>/f{hour:03}.rws` plus the
    /// run's `grid.rwg` and build the selector/derived resolution maps.
    pub fn open(
        store_root: &Path,
        model_slug: &str,
        run_slug: &str,
        hour: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let run_dir = store_root.join(model_slug).join(run_slug);
        let hour_path = run_dir.join(format!("f{hour:03}.rws"));
        let reader = HourReader::open(&hour_path)
            .map_err(|err| format!("open {}: {err}", hour_path.display()))?;
        let grid_path = run_dir.join("grid.rwg");
        let grid = GridFile::open(&grid_path)
            .map_err(|err| format!("open {}: {err}", grid_path.display()))?;
        if reader.meta().grid_hash != grid.hash {
            return Err(format!(
                "hour {} was written against grid {} but {} holds {}",
                hour_path.display(),
                reader.meta().grid_hash,
                grid_path.display(),
                grid.hash
            )
            .into());
        }

        let mut selector_vars = HashMap::new();
        let mut derived_slugs = Vec::new();
        for var in &reader.meta().variables {
            if var.kind != "surface2d" {
                continue;
            }
            if let Some(slug) = derived_selector_slug(&var.selector) {
                derived_slugs.push(slug.to_string());
                continue;
            }
            match serde_json::from_value::<FieldSelector>(var.selector.clone()) {
                Ok(selector) => {
                    selector_vars
                        .entry(selector)
                        .or_insert_with(|| var.name.clone());
                }
                Err(err) => {
                    return Err(format!(
                        "stored variable '{}' has a selector that is neither a FieldSelector \
                         nor a derived marker: {err}",
                        var.name
                    )
                    .into());
                }
            }
        }

        Ok(Self {
            hour_path,
            reader,
            grid,
            selector_vars,
            derived_slugs,
        })
    }

    pub fn hour_path(&self) -> &Path {
        &self.hour_path
    }

    /// The stored 2D variable carrying this selector, if any.
    pub fn resolve(&self, selector: &FieldSelector) -> Option<&str> {
        self.selector_vars.get(selector).map(String::as_str)
    }

    /// Read one selector's full field back as the `SelectedField2D` the
    /// render lanes consume (bit-exact f32 round-trip of the extraction).
    pub fn fetch(
        &self,
        selector: &FieldSelector,
    ) -> Result<SelectedField2D, Box<dyn std::error::Error>> {
        let name = self
            .resolve(selector)
            .ok_or_else(|| format!("no stored variable carries selector {}", selector.key()))?;
        Ok(read_field_2d(&self.reader, &self.grid, name)?)
    }

    /// Derived/heavy recipe slugs stored in this hour, in store order.
    pub fn derived_slugs(&self) -> &[String] {
        &self.derived_slugs
    }

    /// Read one precomputed derived/heavy grid by recipe slug.
    pub fn derived_grid(&self, slug: &str) -> Result<StoredField2D, RwStoreError> {
        read_grid_2d(&self.reader, &self.grid, slug)
    }

    /// The full hour grid (coordinates from `grid.rwg`).
    pub fn full_grid(&self) -> rustwx_core::LatLonGrid {
        rustwx_core::LatLonGrid {
            shape: rustwx_core::GridShape {
                nx: self.grid.nx,
                ny: self.grid.ny,
            },
            lat_deg: self.grid.lat.clone(),
            lon_deg: self.grid.lon.clone(),
        }
    }

    pub fn projection(&self) -> Option<&rustwx_core::GridProjection> {
        self.grid.projection.as_ref()
    }

    /// Provenance strings for the rendered-recipe reports (never pixels).
    pub fn fetch_key(&self) -> String {
        let meta = self.reader.meta();
        format!(
            "rw-store:{}:{}:f{:03}",
            meta.model, meta.run, meta.forecast_hour
        )
    }
}

/// One requested product that cannot render from this store hour, with the
/// reason (missing selector(s) or an unstored derived slug). The blocker
/// pattern: record why, never invent a substitute.
#[derive(Debug, Clone)]
pub struct StoreRenderSkip {
    pub slug: String,
    pub reason: String,
}

/// Outcome of one direct-lane store render pass.
pub struct DirectStoreOutcome {
    pub rendered: Vec<DirectRenderedRecipe>,
    pub skipped: Vec<StoreRenderSkip>,
}

/// Render the requested direct recipes from stored fields through the
/// direct lane's own from-selected-fields entry (the same planning, crop,
/// render-request build, and save path the GRIB lane runs). A recipe is
/// renderable iff every selector in its fetch plan resolves in the store;
/// unresolvable recipes are returned as skips with the missing selectors.
pub fn render_direct_recipes_from_store(
    source: &StoreFieldSource,
    request: &DirectBatchRequest,
    latest: &LatestRun,
    recipe_slugs: &[String],
) -> Result<DirectStoreOutcome, Box<dyn std::error::Error>> {
    let mut renderable = Vec::new();
    let mut skipped = Vec::new();
    let mut needed = Vec::<FieldSelector>::new();
    for slug in recipe_slugs {
        let plan = match plot_recipe_fetch_plan(slug, request.model) {
            Ok(plan) => plan,
            Err(err) => {
                skipped.push(StoreRenderSkip {
                    slug: slug.clone(),
                    reason: format!("no fetch plan for {}: {err}", request.model),
                });
                continue;
            }
        };
        let selectors = plan.selectors();
        let missing: Vec<String> = selectors
            .iter()
            .filter(|selector| source.resolve(selector).is_none())
            .map(|selector| selector.key())
            .collect();
        if missing.is_empty() {
            for selector in selectors {
                if !needed.contains(&selector) {
                    needed.push(selector);
                }
            }
            renderable.push(slug.clone());
        } else {
            skipped.push(StoreRenderSkip {
                slug: slug.clone(),
                reason: format!("missing stored selector(s): {}", missing.join(", ")),
            });
        }
    }
    if renderable.is_empty() {
        return Ok(DirectStoreOutcome {
            rendered: Vec::new(),
            skipped,
        });
    }

    let mut extracted = HashMap::with_capacity(needed.len());
    for selector in needed {
        extracted.insert(selector, source.fetch(&selector)?);
    }
    let rendered = render_direct_recipes_from_selected_fields(
        request,
        latest,
        &renderable,
        &extracted,
        "rw-store",
        source.hour_path().display().to_string(),
        source.fetch_key(),
    )?;
    Ok(DirectStoreOutcome { rendered, skipped })
}

/// Outcome of one derived/heavy-lane store render pass.
pub struct DerivedStoreOutcome {
    pub rendered: Vec<DerivedRenderedRecipe>,
    pub skipped: Vec<StoreRenderSkip>,
}

/// Render the requested derived/heavy recipes from their precomputed store
/// grids through the derived lane's store-render seam (the same projected
/// crop, styles, scales, and save path the GRIB lane runs). A recipe is
/// renderable iff its slug-named grid exists in the hour; `theta_e_2m_10m_winds`
/// additionally needs the stored 10 m u/v planes for its barb overlay.
pub fn render_derived_recipes_from_store(
    source: &StoreFieldSource,
    request: &DerivedBatchRequest,
    cycle_utc: u8,
    recipe_slugs: &[String],
) -> Result<DerivedStoreOutcome, Box<dyn std::error::Error>> {
    use rustwx_core::CanonicalField;

    let mut grids = Vec::new();
    let mut renderable = Vec::new();
    let mut skipped = Vec::new();
    let mut winds: Option<(Vec<f64>, Vec<f64>)> = None;
    for slug in recipe_slugs {
        let stored = match source.derived_grid(slug) {
            Ok(stored) => stored,
            Err(RwStoreError::UnknownVariable(_)) => {
                skipped.push(StoreRenderSkip {
                    slug: slug.clone(),
                    reason: "not stored: ingest did not realize this recipe grid".to_string(),
                });
                continue;
            }
            Err(err) => return Err(format!("read derived grid '{slug}': {err}").into()),
        };
        if slug == "theta_e_2m_10m_winds" && winds.is_none() {
            let u10 = FieldSelector::height_agl(CanonicalField::UWind, 10);
            let v10 = FieldSelector::height_agl(CanonicalField::VWind, 10);
            if source.resolve(&u10).is_none() || source.resolve(&v10).is_none() {
                skipped.push(StoreRenderSkip {
                    slug: slug.clone(),
                    reason: "stored grid present but the 10 m u/v planes its barb overlay \
                             needs are not stored"
                        .to_string(),
                });
                continue;
            }
            let to_f64 =
                |field: SelectedField2D| field.values.iter().map(|&v| f64::from(v)).collect();
            winds = Some((to_f64(source.fetch(&u10)?), to_f64(source.fetch(&v10)?)));
        }
        grids.push(StoreProductGrid {
            slug: slug.clone(),
            units: stored.units,
            values: stored.values.iter().map(|&v| f64::from(v)).collect(),
        });
        renderable.push(slug.clone());
    }
    if renderable.is_empty() {
        return Ok(DerivedStoreOutcome {
            rendered: Vec::new(),
            skipped,
        });
    }

    let mut sub_request = request.clone();
    sub_request.recipe_slugs = renderable;
    let full_grid = source.full_grid();
    let rendered = render_derived_recipes_from_store_grids(
        &sub_request,
        cycle_utc,
        &full_grid,
        source.projection(),
        &grids,
        winds
            .as_ref()
            .map(|(u10, v10)| (u10.as_slice(), v10.as_slice())),
        vec![source.fetch_key()],
    )?;
    Ok(DerivedStoreOutcome { rendered, skipped })
}
