#![allow(dead_code)]

use clap::ValueEnum;
use rustwx_products::shared_context::DomainSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RegionPreset {
    Global,
    NorthAmerica,
    SouthAmerica,
    Europe,
    Africa,
    Asia,
    Australia,
    Antarctica,
    Midwest,
    Conus,
    Alaska,
    Hawaii,
    PuertoRico,
    California,
    CaliforniaSquare,
    RenoSquare,
    PacificNorthwest,
    CaliforniaSouthwest,
    FireWeatherWest,
    RockiesHighPlains,
    Southeast,
    SouthernPlains,
    Oklahoma,
    IllinoisToKansas,
    GulfToKansas,
    Northeast,
    GreatLakes,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitRegionPreset {
    pub slug: &'static str,
    pub label: &'static str,
    pub bounds: (f64, f64, f64, f64),
}

impl SplitRegionPreset {
    pub fn domain(self) -> DomainSpec {
        DomainSpec::new(self.slug, self.bounds)
    }
}

pub const US_SPLIT_REGION_PRESETS: &[SplitRegionPreset] = &[
    split_region(
        "pacific_northwest",
        "Pacific Northwest",
        (-125.0, -110.0, 41.0, 49.5),
    ),
    split_region(
        "california_southwest",
        "California / Southwest",
        (-125.0, -108.0, 31.0, 41.5),
    ),
    split_region(
        "rockies_high_plains",
        "Rockies / High Plains",
        (-112.0, -96.0, 37.0, 49.5),
    ),
    split_region(
        "southern_plains",
        "Southern Plains",
        (-109.0, -90.0, 25.0, 40.5),
    ),
    split_region("great_lakes", "Great Lakes", (-97.5, -72.0, 39.0, 50.5)),
    split_region("southeast", "Southeast", (-96.0, -72.0, 24.0, 38.5)),
    split_region("northeast", "Northeast", (-84.5, -65.0, 36.0, 48.5)),
];

pub fn us_split_region_domains() -> Vec<DomainSpec> {
    US_SPLIT_REGION_PRESETS
        .iter()
        .copied()
        .map(SplitRegionPreset::domain)
        .collect()
}

pub fn conus_plus_us_split_region_domains() -> Vec<DomainSpec> {
    let mut domains = Vec::with_capacity(1 + US_SPLIT_REGION_PRESETS.len());
    domains.push(DomainSpec::new(
        RegionPreset::Conus.slug(),
        RegionPreset::Conus.bounds(),
    ));
    domains.extend(us_split_region_domains());
    domains
}

const fn split_region(
    slug: &'static str,
    label: &'static str,
    bounds: (f64, f64, f64, f64),
) -> SplitRegionPreset {
    SplitRegionPreset {
        slug,
        label,
        bounds,
    }
}

impl RegionPreset {
    pub fn bounds(self) -> (f64, f64, f64, f64) {
        match self {
            Self::Global => (-180.0, 179.999, -90.0, 90.0),
            Self::NorthAmerica => (-170.0, -50.0, 5.0, 84.0),
            Self::SouthAmerica => (-82.0, -34.0, -56.0, 13.0),
            Self::Europe => (-25.0, 45.0, 34.0, 72.0),
            Self::Africa => (-20.0, 55.0, -35.0, 38.0),
            Self::Asia => (25.0, 179.999, -10.0, 82.0),
            Self::Australia => (110.0, 180.0, -50.0, 0.0),
            Self::Antarctica => (-180.0, 179.999, -90.0, -60.0),
            Self::Midwest => (-104.0, -74.0, 28.0, 49.0),
            Self::Conus => (-127.0, -66.0, 23.0, 51.5),
            Self::Alaska => (-180.0, -128.0, 50.0, 73.5),
            Self::Hawaii => (-161.5, -154.0, 18.0, 23.5),
            Self::PuertoRico => (-68.5, -63.5, 17.0, 20.0),
            Self::California => (-124.9, -113.8, 31.9, 42.5),
            Self::CaliforniaSquare => (-124.9, -113.7, 31.8, 42.7),
            Self::RenoSquare => (-123.1, -116.1, 36.1, 43.1),
            Self::PacificNorthwest => (-125.0, -110.0, 41.0, 49.5),
            Self::CaliforniaSouthwest => (-125.0, -108.0, 31.0, 41.5),
            Self::FireWeatherWest => (-126.5, -101.0, 28.0, 50.5),
            Self::RockiesHighPlains => (-112.0, -96.0, 37.0, 49.5),
            Self::Southeast => (-96.0, -72.0, 24.0, 38.5),
            Self::SouthernPlains => (-109.0, -90.0, 25.0, 40.5),
            Self::Oklahoma => (-103.75, -93.5, 32.75, 38.25),
            Self::IllinoisToKansas => (-100.0, -84.75, 36.25, 43.25),
            Self::GulfToKansas => (-103.5, -90.0, 25.0, 40.5),
            Self::Northeast => (-84.5, -65.0, 36.0, 48.5),
            Self::GreatLakes => (-97.5, -72.0, 39.0, 50.5),
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::NorthAmerica => "north_america",
            Self::SouthAmerica => "south_america",
            Self::Europe => "europe",
            Self::Africa => "africa",
            Self::Asia => "asia",
            Self::Australia => "australia",
            Self::Antarctica => "antarctica",
            Self::Midwest => "midwest",
            Self::Conus => "conus",
            Self::Alaska => "alaska",
            Self::Hawaii => "hawaii",
            Self::PuertoRico => "puerto_rico",
            Self::California => "california",
            Self::CaliforniaSquare => "california_square",
            Self::RenoSquare => "reno_square",
            Self::PacificNorthwest => "pacific_northwest",
            Self::CaliforniaSouthwest => "california_southwest",
            Self::FireWeatherWest => "fire_weather_west",
            Self::RockiesHighPlains => "rockies_high_plains",
            Self::Southeast => "southeast",
            Self::SouthernPlains => "southern_plains",
            Self::Oklahoma => "oklahoma",
            Self::IllinoisToKansas => "illinois_to_kansas",
            Self::GulfToKansas => "gulf_to_kansas",
            Self::Northeast => "northeast",
            Self::GreatLakes => "great_lakes",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RegionPreset, US_SPLIT_REGION_PRESETS};
    use std::collections::HashSet;

    #[test]
    fn global_region_slug_is_stable() {
        assert_eq!(RegionPreset::Global.slug(), "global");
        assert_eq!(
            RegionPreset::Global.bounds(),
            (-180.0, 179.999, -90.0, 90.0)
        );
    }

    #[test]
    fn california_square_contains_california_bounds() {
        let ca = RegionPreset::California.bounds();
        let square = RegionPreset::CaliforniaSquare.bounds();
        assert!(square.0 <= ca.0);
        assert!(square.1 >= ca.1);
        assert!(square.2 <= ca.2);
        assert!(square.3 >= ca.3);
    }

    #[test]
    fn california_square_slug_is_stable() {
        assert_eq!(RegionPreset::CaliforniaSquare.slug(), "california_square");
    }

    #[test]
    fn reno_square_is_centered_near_reno() {
        let (west, east, south, north) = RegionPreset::RenoSquare.bounds();
        let center_lon = (west + east) / 2.0;
        let center_lat = (south + north) / 2.0;
        assert!((center_lon + 119.8).abs() < 0.5);
        assert!((center_lat - 39.5).abs() < 0.5);
    }

    #[test]
    fn split_region_slugs_are_unique() {
        let mut seen = HashSet::new();
        for region in US_SPLIT_REGION_PRESETS {
            assert!(
                seen.insert(region.slug),
                "duplicate split-region slug {}",
                region.slug
            );
        }
    }
}
