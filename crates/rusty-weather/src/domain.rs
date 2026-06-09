#![allow(dead_code)]

use crate::region::RegionPreset;
use rustwx_products::named_geometry::find_built_in_country_domain;
use rustwx_products::shared_context::DomainSpec;

pub fn domain_from_region_or_country(
    region: RegionPreset,
    country: Option<&str>,
) -> Result<DomainSpec, Box<dyn std::error::Error>> {
    if let Some(country) = country.map(str::trim).filter(|value| !value.is_empty()) {
        return find_built_in_country_domain(country).ok_or_else(|| {
            format!(
                "unknown --country '{country}'; use `named_geometry domains --kind country` to list available country slugs"
            )
            .into()
        });
    }

    Ok(DomainSpec::new(region.slug(), region.bounds()))
}

pub fn requested_domain_slug(region: RegionPreset, country: Option<&str>) -> String {
    country
        .and_then(find_built_in_country_domain)
        .map(|domain| domain.slug)
        .unwrap_or_else(|| region.slug().to_string())
}
