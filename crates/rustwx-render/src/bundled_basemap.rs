//! Binary Natural Earth layers that standalone hosts can materialize beside
//! their runtime data store.
//!
//! Natural Earth data is public domain. Keeping these two worldwide context
//! layers in the renderer crate lets a packaged application behave the same
//! as a developer checkout without depending on Cargo's source directory at
//! runtime.

/// The two files required by `shapefile::ShapeReader` for an indexed layer.
#[derive(Debug, Clone, Copy)]
pub struct BundledShapefile {
    pub shp: &'static [u8],
    pub shx: &'static [u8],
}

/// Worldwide first-order administrative boundaries (states, provinces,
/// regions, cantons, and their country-specific equivalents) at Natural
/// Earth's 1:10m detail.
pub fn bundled_natural_earth_admin1_10m() -> BundledShapefile {
    BundledShapefile {
        shp: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/basemap/natural_earth_10m/ne_10m_admin_1_states_provinces_lines.shp"
        )),
        shx: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/basemap/natural_earth_10m/ne_10m_admin_1_states_provinces_lines.shx"
        )),
    }
}

/// Worldwide lake polygons/shorelines at Natural Earth's 1:10m detail.
pub fn bundled_natural_earth_lakes_10m() -> BundledShapefile {
    BundledShapefile {
        shp: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/basemap/natural_earth_10m/ne_10m_lakes.shp"
        )),
        shx: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/basemap/natural_earth_10m/ne_10m_lakes.shx"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_context_layers_are_real_indexed_shapefiles() {
        let admin1 = bundled_natural_earth_admin1_10m();
        let lakes = bundled_natural_earth_lakes_10m();

        assert!(admin1.shp.len() > 7_000_000);
        assert!(admin1.shx.len() > 80_000);
        assert!(lakes.shp.len() > 2_000_000);
        assert!(lakes.shx.len() > 10_000);
        assert_eq!(&admin1.shp[..4], &[0, 0, 0x27, 0x0a]);
        assert_eq!(&lakes.shp[..4], &[0, 0, 0x27, 0x0a]);
    }
}
