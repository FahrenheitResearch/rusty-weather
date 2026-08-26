use rustwx_core::MAX_GRID_CELLS;
use serde::{Deserialize, Serialize};

/// Exact-time samples and stored variables are addressed by u16 ids in the
/// v1 store contract. These are representation boundaries, not query policy.
const FULL_U16_NAMESPACE: usize = u16::MAX as usize + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingPolicy {
    #[default]
    Strict,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TimeRange {
    /// Inclusive UTC Unix timestamp.
    pub start_unix: Option<i64>,
    /// Exclusive UTC Unix timestamp.
    pub end_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryLimits {
    pub max_catalog_entries: usize,
    /// Maximum physical samples retained in one run snapshot.
    pub max_time_points: usize,
    /// Maximum samples selected by one point or temporal request.
    pub max_selected_time_points: usize,
    pub max_variables: usize,
    /// Maximum cells decoded by non-temporal grid reductions and windows.
    pub max_reduction_cells: usize,
    /// Maximum native-grid cells reduced by one temporal-grid request.
    pub max_temporal_reduction_cells: usize,
    /// Maximum fixed and dynamic values allocated by one temporal result.
    pub max_temporal_output_values: usize,
    pub max_point_values: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            // Direct/library queries have no hidden operational policy. A
            // server can still pass explicit admission budgets through
            // `open_with_limits`; checked arithmetic and fallible output
            // allocation remain authoritative here.
            max_catalog_entries: usize::MAX,
            max_time_points: FULL_U16_NAMESPACE,
            max_selected_time_points: FULL_U16_NAMESPACE,
            max_variables: FULL_U16_NAMESPACE,
            max_reduction_cells: MAX_GRID_CELLS,
            max_temporal_reduction_cells: MAX_GRID_CELLS,
            max_temporal_output_values: usize::MAX,
            max_point_values: usize::MAX,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimePoint {
    pub storage_slot: u16,
    pub lead_seconds: u64,
    pub valid_unix: i64,
    /// Store filenames never cross the public DTO boundary.
    #[serde(skip)]
    pub(crate) file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProvenance {
    /// Backward-compatible acquisition-lane identity.
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast_producer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licensing_publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_provider: Option<String>,
    #[serde(default)]
    pub transport_is_mirror: bool,
    pub roles: Vec<String>,
    pub products: Vec<String>,
}

impl SourceProvenance {
    /// Identity whose terms and attribution govern the acquired bytes. Legacy
    /// manifests retain the historical `provider` fallback.
    pub fn licensing_publisher_identity(&self) -> &str {
        self.licensing_publisher
            .as_deref()
            .unwrap_or(&self.provider)
    }
}

impl From<rw_store::RwsSourceProvenance> for SourceProvenance {
    fn from(value: rw_store::RwsSourceProvenance) -> Self {
        Self {
            provider: value.provider,
            forecast_producer: value.forecast_producer,
            licensing_publisher: value.licensing_publisher,
            transport_provider: value.transport_provider,
            transport_is_mirror: value.transport_is_mirror,
            roles: value.roles,
            products: value.products,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAttribution {
    pub provider: String,
    pub copyright_statement: String,
    pub notice: String,
    pub source_url: String,
    pub license: String,
    pub license_url: String,
    pub terms_url: String,
    pub modification_notice: String,
    pub disclaimer: String,
}

pub fn ecmwf_provider_attribution() -> ProviderAttribution {
    let notice = "This service is based on data and products of the European Centre for Medium-Range Weather Forecasts (ECMWF).";
    ProviderAttribution {
        provider: "European Centre for Medium-Range Weather Forecasts (ECMWF)".into(),
        copyright_statement: notice.into(),
        notice: notice.into(),
        source_url: "https://www.ecmwf.int/".into(),
        license: "This ECMWF data is published under a Creative Commons Attribution 4.0 International (CC BY 4.0).".into(),
        license_url: "https://creativecommons.org/licenses/by/4.0/".into(),
        terms_url: "https://apps.ecmwf.int/datasets/licences/general/".into(),
        modification_notice: "The ECMWF source data has been subset, normalized, and re-encoded by this service.".into(),
        disclaimer: "ECMWF does not accept any liability whatsoever for any error or omission in the data, their availability, or for any loss or damage arising from their use.".into(),
    }
}

pub fn noaa_provider_attribution() -> ProviderAttribution {
    ProviderAttribution {
        provider: "National Oceanic and Atmospheric Administration (NOAA) / National Weather Service (NWS)".into(),
        copyright_statement: "NOAA/NWS data and products are U.S. Government works in the public domain unless specifically noted otherwise.".into(),
        notice: "This service uses NOAA/NWS data and products. Credit NOAA/NWS as the source, do not imply NOAA/NWS endorsement, and do not present modified output as an official government product.".into(),
        source_url: "https://www.noaa.gov/information-technology/open-data-dissemination".into(),
        license: "Public domain in the United States unless specifically noted; contributed or third-party archive holdings may carry separate terms.".into(),
        license_url: "https://www.weather.gov/disclaimer/".into(),
        terms_url: "https://www.weather.gov/disclaimer/".into(),
        modification_notice: "The NOAA source data has been subset, normalized, derived, and re-encoded by this service; this output is not an official NOAA/NWS product.".into(),
        disclaimer: "NOAA/NWS data is furnished as-is without warranties of accuracy, timeliness, completeness, merchantability, or fitness for a particular purpose; delivery is not guaranteed.".into(),
    }
}

pub fn eccc_provider_attribution() -> ProviderAttribution {
    ProviderAttribution {
        provider: "Environment and Climate Change Canada (ECCC) / Meteorological Service of Canada (MSC)".into(),
        copyright_statement: "Contains information licenced under the Data Server End-use Licence of Environment and Climate Change Canada.".into(),
        notice: "Data Source: Environment and Climate Change Canada".into(),
        source_url: "https://eccc-msc.github.io/open-data/msc-data/nwp_gdps/readme_gdps-datamart_en/".into(),
        license: "Environment and Climate Change Canada Data Servers End-use Licence, version 2.1; worldwide, royalty-free use including commercial copying, modification, publication, and distribution, subject to attribution and the licence terms.".into(),
        license_url: "https://eccc-msc.github.io/open-data/licence/readme_en/".into(),
        terms_url: "https://eccc-msc.github.io/open-data/licence/readme_en/".into(),
        modification_notice: "The ECCC source objects have been selected, combined, normalized, and re-encoded by this service; this output is not an official ECCC/MSC product.".into(),
        disclaimer: "The source information is licensed as-is without warranties; ECCC and other contributors disclaim liability to the maximum extent permitted by the licence.".into(),
    }
}

/// Attribution and operational caveat for the experimental HRDPS-West 1 km
/// feed on ECCC's non-operational DD-Alpha service.
pub fn hrdps_west_provider_attribution() -> ProviderAttribution {
    let mut attribution = eccc_provider_attribution();
    attribution.source_url =
        "https://eccc-msc.github.io/open-data/msc-data/nwp_hrdps/readme_hrdps-datamart-alpha_en/"
            .into();
    attribution.disclaimer = "The source information is licensed as-is without warranties. HRDPS-West is published on ECCC's experimental, non-operational DD-Alpha service with only 24 hours of rolling source history; availability and completeness are not guaranteed."
        .into();
    attribution
}

/// Attribution for ECCC's Regional Ensemble Prediction System (REPS).
pub fn reps_provider_attribution() -> ProviderAttribution {
    let mut attribution = eccc_provider_attribution();
    attribution.source_url =
        "https://eccc-msc.github.io/open-data/msc-data/nwp_reps/readme_reps-datamart_en/".into();
    attribution
}

pub fn cma_provider_attribution() -> ProviderAttribution {
    ProviderAttribution {
        provider: "China Meteorological Administration (CMA)".into(),
        copyright_statement: "CMA is the producing centre for CMA GRAPES GEPS v1.3; its WMO discovery record declares the feed as WMO core data.".into(),
        notice: "Data source: China Meteorological Administration (CMA), distributed through WIS2.".into(),
        source_url: "https://wis2node.wis.cma.cn/oapi/collections/discovery-metadata/items/urn%3Awmo%3Amd%3Acn-cma%3Adata.core.weather.prediction.forecast.medium-range.probabilistic.global?f=json".into(),
        license: "WMO Unified Data Policy core data: free and unrestricted international exchange without charge and with no conditions on use.".into(),
        license_url: "https://public.wmo.int/wmo-unified-data-policy-resolution-res1".into(),
        terms_url: "https://public.wmo.int/wmo-unified-data-policy-resolution-res1".into(),
        modification_notice: "The CMA source statistics have been selected, normalized, and re-encoded by this service; this output is not an official CMA product.".into(),
        disclaimer: "Availability and interpretation remain subject to the authoritative CMA/WMO metadata; this service supplies transformed output without implying CMA or WMO endorsement.".into(),
    }
}

pub fn dwd_provider_attribution() -> ProviderAttribution {
    ProviderAttribution {
        provider: "Deutscher Wetterdienst (DWD)".into(),
        copyright_statement:
            "DWD Open Data is made available under the Creative Commons Attribution 4.0 International licence."
                .into(),
        notice: "Source: Deutscher Wetterdienst".into(),
        source_url:
            "https://www.dwd.de/EN/ourservices/nwp_forecast_data/nwp_forecast_data.html".into(),
        license: "Creative Commons Attribution 4.0 International (CC BY 4.0).".into(),
        license_url: "https://creativecommons.org/licenses/by/4.0/".into(),
        terms_url: "https://www.dwd.de/EN/service/legal_notice/legal_notice.html".into(),
        modification_notice: "The DWD source objects have been selected, combined, normalized, and re-encoded by this service; this output is not an official DWD product.".into(),
        disclaimer: "DWD Open Data is provided under the applicable licence and legal notice without any service guarantee; users remain responsible for checking fitness for their purpose.".into(),
    }
}

pub fn roshydromet_provider_attribution() -> ProviderAttribution {
    ProviderAttribution {
        provider: "Federal Service for Hydrometeorology and Environmental Monitoring (Roshydromet)".into(),
        copyright_statement: "Roshydromet is the producing centre for ICON-Ru13/6N29; its WMO discovery record declares the feed as WMO core data.".into(),
        notice: "Data source: Roshydromet WIPPS Designated Centre Moscow, distributed through WIS2.".into(),
        source_url: "https://meteoinfo.ru/en/wis2-srf-products-of-wipps-dc-moscow".into(),
        license: "WMO Unified Data Policy core data: free and unrestricted international exchange without charge and with no conditions on use.".into(),
        license_url: "https://public.wmo.int/wmo-unified-data-policy-resolution-res1".into(),
        terms_url: "https://public.wmo.int/wmo-unified-data-policy-resolution-res1".into(),
        modification_notice: "The Roshydromet source bulletins have been selected, unwrapped, combined, normalized, and re-encoded by this service; this output is not an official Roshydromet or WMO product.".into(),
        disclaimer: "Availability and interpretation remain subject to the authoritative Roshydromet/WMO metadata; this service supplies transformed output without implying Roshydromet or WMO endorsement.".into(),
    }
}

/// Attribution for ECCC's provider-published GEPS statistical products.
///
/// The licence and required source notice are shared with the other ECCC
/// Datamart products, but the product documentation URL must identify GEPS
/// rather than the GDPS page used by the generic ECCC attribution.
pub fn geps_provider_attribution() -> ProviderAttribution {
    let mut attribution = eccc_provider_attribution();
    attribution.source_url =
        "https://eccc-msc.github.io/open-data/msc-data/nwp_geps/readme_geps-datamart_en/".into();
    attribution
}

pub fn cptec_provider_attribution() -> ProviderAttribution {
    ProviderAttribution {
        provider: "Center for Weather Forecast and Climate Studies (CPTEC) / National Institute for Space Research (INPE), Brazil".into(),
        copyright_statement: "CPTEC/INPE is the producing and publishing organization for these operational WRF, BRAMS, and Eta forecast files.".into(),
        notice: "Data source: CPTEC/INPE; transported from the official CPTEC Data Server.".into(),
        source_url: "https://www3.cptec.inpe.br/dimnt/base-de-dados/previsoes-cptec/".into(),
        license: "INPE's Open Data Plan publishes Eta South America as daily open data under Brazil's Open Data Policy (Decreto 8.777/2016); no model-directory-specific licence statement was observed, so users should verify current publisher terms for their use.".into(),
        license_url: "https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/dados-abertos".into(),
        terms_url: "https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/dados-abertos".into(),
        modification_notice: "The CPTEC/INPE source messages have been byte-range selected, normalized, and re-encoded by this service; this output is not an official CPTEC/INPE product.".into(),
        disclaimer: "Availability and interpretation remain subject to the authoritative CPTEC/INPE publication; this service supplies transformed output without implying CPTEC/INPE endorsement.".into(),
    }
}

/// Attribution for ECCC's experimental GDPS-GEML AI-emulator feed. The
/// licence and required notice are shared with Datamart, while this exact
/// documentation URL keeps the experimental product identity unambiguous.
pub fn gdps_geml_provider_attribution() -> ProviderAttribution {
    let mut attribution = eccc_provider_attribution();
    attribution.source_url =
        "https://eccc-msc.github.io/open-data/msc-data/nwp_gdps/readme_gdps-geml-datamart_en/"
            .into();
    attribution
}

/// The single attribution owed to one acquired source.
///
/// Resolution is deliberately most-specific-first and scoped to this one
/// source: a source published on a feed that carries its own product page or
/// operational caveat owes exactly that attribution, never the generic
/// publisher attribution in addition to it. Whole-vector matching cannot
/// express this, because it lets one source's specific feed identity suppress
/// or duplicate another source's owed notice.
fn provider_attribution_for_source(source: &SourceProvenance) -> Option<ProviderAttribution> {
    // Model-specific feed identities remain carried on the acquisition-lane
    // provider, so they are resolved before the publisher identity.
    match source.provider.as_str() {
        "eccc-msc-gdps-geml-datamart" => return Some(gdps_geml_provider_attribution()),
        "eccc-msc-hrdps-west-dd-alpha" => return Some(hrdps_west_provider_attribution()),
        _ => {}
    }
    match source.licensing_publisher_identity() {
        "ecmwf" | "ecmwf-open-data" => Some(ecmwf_provider_attribution()),
        "noaa"
        | "noaa-nws"
        | "noaa-ncep"
        | "noaa-nomads"
        | "noaa-ncei"
        | "noaa-aws-public-data"
        | "noaa-google-public-data"
        | "noaa-microsoft-azure-public-data"
        // Backward-compatible identities written by the first
        // provenance-capable development snapshots.
        | "aws-public-data"
        | "google-public-data"
        | "microsoft-azure-public-data" => Some(noaa_provider_attribution()),
        "eccc" | "eccc-msc" | "eccc-msc-datamart" => {
            if source
                .products
                .iter()
                .any(|product| product == "rws-published-statistics")
            {
                Some(geps_provider_attribution())
            } else if source
                .products
                .iter()
                .any(|product| product == "rws-reps-provider-statistics")
            {
                Some(reps_provider_attribution())
            } else {
                Some(eccc_provider_attribution())
            }
        }
        "cma" | "cma-wis2-core-data" => Some(cma_provider_attribution()),
        "dwd" | "dwd-open-data" => Some(dwd_provider_attribution()),
        "roshydromet" | "roshydromet-wipps-dc" => Some(roshydromet_provider_attribution()),
        "cptec-inpe" | "inpe" => Some(cptec_provider_attribution()),
        _ => None,
    }
}

/// Every attribution owed by this run's acquired sources, in first-source
/// order and without repeats. Sources that resolve to the same attribution
/// contribute it exactly once.
pub fn provider_attributions_for_provenance(
    sources: &[SourceProvenance],
) -> Vec<ProviderAttribution> {
    let mut attributions: Vec<ProviderAttribution> = Vec::with_capacity(4);
    for source in sources {
        let Some(attribution) = provider_attribution_for_source(source) else {
            continue;
        };
        if !attributions.contains(&attribution) {
            attributions.push(attribution);
        }
    }
    attributions
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDescriptor {
    pub model: String,
    pub run: String,
    pub schema: String,
    pub snapshot_id: String,
    pub grid_hash: String,
    pub nx: usize,
    pub ny: usize,
    pub exact_time_axis: bool,
    pub origin_unix: Option<i64>,
    pub sample_count: usize,
    pub first_valid_unix: Option<i64>,
    pub last_valid_unix: Option<i64>,
    #[serde(default)]
    pub source_provenance: Vec<SourceProvenance>,
    #[serde(default)]
    pub provider_attributions: Vec<ProviderAttribution>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridPoint {
    pub requested_latitude: f64,
    pub requested_longitude: f64,
    pub x: usize,
    pub y: usize,
    pub grid_latitude: f32,
    pub grid_longitude: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableCapability {
    pub name: String,
    pub units: String,
    pub kind: String,
    pub codec: String,
    pub levels_hpa: Vec<u16>,
    pub selector: serde_json::Value,
    pub available_slots: Vec<u16>,
    pub available_samples: usize,
    pub expected_samples: usize,
    pub coverage: f64,
    pub point_series: bool,
    pub pressure_profile: bool,
    /// This pressure-level variable can be returned for multiple exact stored
    /// times by the bounded profile-cycle query.
    pub profile_cycle: bool,
    /// This stored variable can be returned in a bounded geographic-domain
    /// window with cropped coordinates and exact projection metadata.
    pub geographic_window: bool,
    pub scalar_temporal_reduction: bool,
    pub temporal: crate::VariableTemporalCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub model: String,
    pub run_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCatalogEntry {
    pub run: RunDescriptor,
    pub variable_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointSeriesRequest {
    pub latitude: f64,
    pub longitude: f64,
    pub variables: Vec<String>,
    #[serde(default)]
    pub time: TimeRange,
    #[serde(default)]
    pub missing_policy: MissingPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointVariableSeries {
    pub name: String,
    pub units: String,
    pub values: Vec<Option<f32>>,
    pub available_samples: usize,
    pub expected_samples: usize,
    pub coverage: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointSeriesResult {
    pub run: RunDescriptor,
    pub point: GridPoint,
    pub axis: Vec<TimePoint>,
    pub variables: Vec<PointVariableSeries>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileRequest {
    pub latitude: f64,
    pub longitude: f64,
    pub storage_slot: u16,
    pub variables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PressureProfile {
    pub name: String,
    pub units: String,
    pub levels_hpa: Vec<u16>,
    pub values: Vec<Option<f32>>,
    pub available_levels: usize,
    pub expected_levels: usize,
    pub coverage: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileResult {
    pub run: RunDescriptor,
    pub point: GridPoint,
    pub time: TimePoint,
    pub variables: Vec<PressureProfile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileCycleRequest {
    pub latitude: f64,
    pub longitude: f64,
    /// Pressure-level variables decoded as complete vertical profiles.
    pub variables: Vec<String>,
    /// Surface variables sampled at the same nearest native-grid point.
    #[serde(default)]
    pub surface_variables: Vec<String>,
    #[serde(default)]
    pub time: TimeRange,
    #[serde(default)]
    pub missing_policy: MissingPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCycleSampleStatus {
    /// Every requested pressure and surface variable is present at this stored time.
    Complete,
    /// Some requested pressure or surface values are present and some are absent.
    Partial,
    /// None of the requested pressure or surface values are present at this stored time.
    Gap,
}

/// One surface value colocated with a cycle sounding.
///
/// This intentionally matches the typed `SurfaceSample` carried by the
/// Community Cache profile payload while remaining HTTP- and protocol-neutral.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileSurfaceSample {
    pub variable: String,
    pub units: String,
    pub value: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileCycleSample {
    pub time: TimePoint,
    /// Sanitized acquisition provenance from this exact manifest hour entry.
    pub source_provenance: Vec<SourceProvenance>,
    pub status: ProfileCycleSampleStatus,
    /// Available profiles in request order, with a level axis bound to this time.
    pub variables: Vec<PressureProfile>,
    /// Absent requested pressure variables in request order.
    pub missing_variables: Vec<String>,
    /// Surface values in requested order. Missing/non-finite values remain explicit `None`.
    pub surface_samples: Vec<ProfileSurfaceSample>,
    /// Requested surface variables absent or non-finite at this stored time.
    pub missing_surface_variables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileCycleResult {
    pub run: RunDescriptor,
    pub point: GridPoint,
    pub requested_variables: Vec<String>,
    pub requested_surface_variables: Vec<String>,
    pub requested_time: TimeRange,
    pub missing_policy: MissingPolicy,
    /// One entry for every selected stored time, in deterministic physical-time order.
    pub samples: Vec<ProfileCycleSample>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScalarTemporalRequest {
    pub variable: String,
    #[serde(default)]
    pub time: TimeRange,
    #[serde(default)]
    pub missing_policy: MissingPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScalarTemporalResult {
    pub run: RunDescriptor,
    pub variable: String,
    pub units: String,
    pub nx: usize,
    pub ny: usize,
    pub axis: Vec<TimePoint>,
    pub expected_samples: usize,
    pub missing_variable_slots: Vec<u16>,
    pub minimum: Vec<Option<f32>>,
    pub maximum: Vec<Option<f32>>,
    pub range: Vec<Option<f32>>,
    /// Arithmetic mean of finite stored samples; it is not time-weighted.
    pub sample_mean: Vec<Option<f64>>,
    /// Index into `axis`; ties retain the earliest index.
    pub argmin_time_index: Vec<Option<u32>>,
    /// Index into `axis`; ties retain the earliest index.
    pub argmax_time_index: Vec<Option<u32>>,
    pub finite_count: Vec<u32>,
    pub coverage: Vec<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_query_defaults_have_no_old_operational_ceilings() {
        let limits = QueryLimits::default();
        assert_eq!(limits.max_catalog_entries, usize::MAX);
        assert!(limits.max_time_points > 4_096);
        assert!(limits.max_selected_time_points > 4_096);
        assert!(limits.max_variables > 64);
        assert!(limits.max_reduction_cells > 4_000_000);
        assert!(limits.max_temporal_reduction_cells > 4_000_000);
        assert_eq!(limits.max_temporal_output_values, usize::MAX);
        assert_eq!(limits.max_point_values, usize::MAX);
    }

    fn provenance(provider: &str) -> SourceProvenance {
        SourceProvenance {
            provider: provider.into(),
            forecast_producer: None,
            licensing_publisher: None,
            transport_provider: None,
            transport_is_mirror: false,
            roles: vec!["surface".into()],
            products: vec!["product".into()],
        }
    }

    /// The structured shape ingest actually writes for ECCC acquisitions: an
    /// explicit licensing publisher alongside the acquisition-lane provider.
    fn structured_eccc_provenance(provider: &str) -> SourceProvenance {
        SourceProvenance {
            provider: provider.into(),
            forecast_producer: Some("eccc-msc".into()),
            licensing_publisher: Some("eccc".into()),
            transport_provider: Some("eccc-datamart".into()),
            transport_is_mirror: false,
            roles: vec!["surface".into()],
            products: vec!["product".into()],
        }
    }

    fn provenance_with_product(provider: &str, product: &str) -> SourceProvenance {
        SourceProvenance {
            provider: provider.into(),
            forecast_producer: None,
            licensing_publisher: None,
            transport_provider: None,
            transport_is_mirror: false,
            roles: vec!["surface".into()],
            products: vec![product.into()],
        }
    }

    #[test]
    fn provider_attributions_cover_noaa_mirrors_and_ecmwf_without_duplicates() {
        let sources = vec![
            provenance("noaa-nomads"),
            provenance("noaa-aws-public-data"),
            provenance("ecmwf-open-data"),
        ];
        let attributions = provider_attributions_for_provenance(&sources);
        assert_eq!(attributions.len(), 2);
        assert!(
            attributions
                .iter()
                .any(|item| item.provider.contains("ECMWF"))
        );
        let noaa = attributions
            .iter()
            .find(|item| item.provider.contains("NOAA"))
            .expect("NOAA attribution");
        assert!(noaa.notice.contains("do not imply NOAA/NWS endorsement"));
        assert!(
            noaa.modification_notice
                .contains("not an official NOAA/NWS product")
        );
    }

    #[test]
    fn legacy_noaa_mirror_identity_keeps_attribution() {
        let attributions = provider_attributions_for_provenance(&[provenance("aws-public-data")]);
        assert_eq!(attributions, vec![noaa_provider_attribution()]);
    }

    #[test]
    fn eccc_provenance_emits_required_source_and_modification_notice_once() {
        let attributions = provider_attributions_for_provenance(&[
            provenance("eccc-msc-datamart"),
            provenance("eccc-msc-datamart"),
        ]);
        assert_eq!(attributions, vec![eccc_provider_attribution()]);
        assert_eq!(
            attributions[0].notice,
            "Data Source: Environment and Climate Change Canada"
        );
        assert!(
            attributions[0]
                .modification_notice
                .contains("not an official")
        );
        assert!(attributions[0].license.contains("version 2.1"));
    }

    #[test]
    fn reps_provenance_uses_the_exact_product_documentation_url() {
        let sources = [SourceProvenance {
            provider: "eccc-msc-datamart".into(),
            forecast_producer: None,
            licensing_publisher: None,
            transport_provider: None,
            transport_is_mirror: false,
            roles: vec!["surface".into()],
            products: vec!["rws-reps-provider-statistics".into()],
        }];
        let attributions = provider_attributions_for_provenance(&sources);
        assert_eq!(attributions, vec![reps_provider_attribution()]);
        assert_eq!(
            attributions[0].source_url,
            "https://eccc-msc.github.io/open-data/msc-data/nwp_reps/readme_reps-datamart_en/"
        );
        assert_eq!(
            attributions[0].notice,
            "Data Source: Environment and Climate Change Canada"
        );
        assert!(attributions[0].license.contains("version 2.1"));
    }

    #[test]
    fn cma_wis2_core_data_emits_owner_policy_and_transport_attribution() {
        let attributions = provider_attributions_for_provenance(&[
            provenance("cma-wis2-core-data"),
            provenance("cma-wis2-core-data"),
        ]);
        assert_eq!(attributions, vec![cma_provider_attribution()]);
        assert!(attributions[0].provider.contains("CMA"));
        assert!(attributions[0].license.contains("WMO Unified Data Policy"));
        assert!(attributions[0].notice.contains("WIS2"));
        assert!(
            attributions[0]
                .modification_notice
                .contains("not an official")
        );
    }

    #[test]
    fn dwd_provenance_emits_cc_by_source_and_modification_notice_once() {
        let attributions = provider_attributions_for_provenance(&[
            provenance("dwd-open-data"),
            provenance("dwd-open-data"),
        ]);
        assert_eq!(attributions, vec![dwd_provider_attribution()]);
        assert_eq!(attributions[0].notice, "Source: Deutscher Wetterdienst");
        assert!(attributions[0].license.contains("CC BY 4.0"));
        assert!(
            attributions[0]
                .modification_notice
                .contains("not an official DWD product")
        );
    }

    #[test]
    fn roshydromet_provenance_emits_wmo_core_policy_once() {
        let attributions = provider_attributions_for_provenance(&[
            provenance("roshydromet-wipps-dc"),
            provenance("roshydromet-wipps-dc"),
        ]);
        assert_eq!(attributions, vec![roshydromet_provider_attribution()]);
        assert!(attributions[0].notice.contains("Roshydromet"));
        assert!(
            attributions[0]
                .license
                .contains("WMO Unified Data Policy core")
        );
        assert!(attributions[0].modification_notice.contains("unwrapped"));
    }

    #[test]
    fn geps_provenance_uses_the_exact_product_documentation_url() {
        let attributions = provider_attributions_for_provenance(&[provenance_with_product(
            "eccc-msc-datamart",
            "rws-published-statistics",
        )]);
        assert_eq!(attributions, vec![geps_provider_attribution()]);
        assert_eq!(
            attributions[0].source_url,
            "https://eccc-msc.github.io/open-data/msc-data/nwp_geps/readme_geps-datamart_en/"
        );
        assert_eq!(
            attributions[0].notice,
            "Data Source: Environment and Climate Change Canada"
        );
        assert!(attributions[0].license.contains("version 2.1"));
    }

    #[test]
    fn cptec_provenance_keeps_producer_transport_and_cautious_open_data_terms() {
        let attributions = provider_attributions_for_provenance(&[
            provenance("cptec-inpe"),
            provenance("cptec-inpe"),
        ]);
        assert_eq!(attributions, vec![cptec_provider_attribution()]);
        assert!(attributions[0].provider.contains("CPTEC"));
        assert!(attributions[0].notice.contains("CPTEC Data Server"));
        assert!(attributions[0].copyright_statement.contains("Eta"));
        assert!(attributions[0].license.contains("Decreto 8.777/2016"));
        assert!(
            attributions[0]
                .license
                .contains("no model-directory-specific")
        );
        assert!(!attributions[0].license.contains("CC BY"));
        assert!(
            attributions[0]
                .modification_notice
                .contains("not an official CPTEC/INPE product")
        );
    }

    #[test]
    fn gdps_geml_provenance_uses_the_exact_experimental_product_page() {
        let attributions =
            provider_attributions_for_provenance(&[provenance("eccc-msc-gdps-geml-datamart")]);
        assert_eq!(attributions, vec![gdps_geml_provider_attribution()]);
        assert_eq!(
            attributions[0].source_url,
            "https://eccc-msc.github.io/open-data/msc-data/nwp_gdps/readme_gdps-geml-datamart_en/"
        );
        assert_eq!(
            attributions[0].notice,
            "Data Source: Environment and Climate Change Canada"
        );
        assert!(attributions[0].license.contains("version 2.1"));
        assert!(attributions[0].modification_notice.contains("normalized"));
    }

    #[test]
    fn structured_publisher_drives_attribution_independently_of_transport() {
        let sources = [SourceProvenance {
            provider: "cloud-object-lane".into(),
            forecast_producer: Some("noaa-ncep".into()),
            licensing_publisher: Some("noaa".into()),
            transport_provider: Some("aws-asdi".into()),
            transport_is_mirror: true,
            roles: vec!["surface".into()],
            products: vec!["pgrb2".into()],
        }];
        let attributions = provider_attributions_for_provenance(&sources);
        assert_eq!(attributions.len(), 1);
        assert!(attributions[0].provider.contains("NOAA"));

        let serialized = serde_json::to_value(&sources[0]).unwrap();
        assert_eq!(serialized["forecast_producer"], "noaa-ncep");
        assert_eq!(serialized["licensing_publisher"], "noaa");
        assert_eq!(serialized["transport_provider"], "aws-asdi");
        assert_eq!(serialized["transport_is_mirror"], true);
    }

    #[test]
    fn hrdps_west_provenance_surfaces_dd_alpha_status_and_retention() {
        let attributions =
            provider_attributions_for_provenance(&[provenance("eccc-msc-hrdps-west-dd-alpha")]);
        assert_eq!(attributions, vec![hrdps_west_provider_attribution()]);
        assert_eq!(
            attributions[0].source_url,
            "https://eccc-msc.github.io/open-data/msc-data/nwp_hrdps/readme_hrdps-datamart-alpha_en/"
        );
        assert_eq!(
            attributions[0].notice,
            "Data Source: Environment and Climate Change Canada"
        );
        assert!(attributions[0].license.contains("version 2.1"));
        assert!(
            attributions[0]
                .disclaimer
                .contains("non-operational DD-Alpha")
        );
        assert!(attributions[0].disclaimer.contains("24 hours"));
    }

    #[test]
    fn structured_gdps_geml_identity_emits_the_experimental_attribution_once() {
        let attributions = provider_attributions_for_provenance(&[structured_eccc_provenance(
            "eccc-msc-gdps-geml-datamart",
        )]);
        assert_eq!(attributions, vec![gdps_geml_provider_attribution()]);
        assert_eq!(
            attributions
                .iter()
                .filter(|item| **item == gdps_geml_provider_attribution())
                .count(),
            1
        );
    }

    #[test]
    fn structured_hrdps_west_identity_keeps_only_the_dd_alpha_attribution() {
        let attributions = provider_attributions_for_provenance(&[structured_eccc_provenance(
            "eccc-msc-hrdps-west-dd-alpha",
        )]);
        assert_eq!(attributions, vec![hrdps_west_provider_attribution()]);
        assert!(!attributions.contains(&eccc_provider_attribution()));
        assert!(
            attributions[0]
                .disclaimer
                .contains("non-operational DD-Alpha")
        );
    }

    #[test]
    fn mixed_eccc_sources_keep_the_dd_alpha_caveat_and_one_generic_attribution() {
        let attributions = provider_attributions_for_provenance(&[
            structured_eccc_provenance("eccc-msc-hrdps-west-dd-alpha"),
            structured_eccc_provenance("eccc-msc-datamart"),
            structured_eccc_provenance("eccc-msc-datamart"),
        ]);
        assert_eq!(attributions.len(), 2);
        assert_eq!(
            attributions
                .iter()
                .filter(|item| **item == hrdps_west_provider_attribution())
                .count(),
            1
        );
        assert_eq!(
            attributions
                .iter()
                .filter(|item| **item == eccc_provider_attribution())
                .count(),
            1
        );
    }

    #[test]
    fn legacy_mixed_eccc_sources_resolve_the_same_two_attributions() {
        let attributions = provider_attributions_for_provenance(&[
            provenance("eccc-msc-hrdps-west-dd-alpha"),
            provenance("eccc-msc-datamart"),
        ]);
        assert_eq!(attributions.len(), 2);
        assert!(attributions.contains(&hrdps_west_provider_attribution()));
        assert!(attributions.contains(&eccc_provider_attribution()));
    }
}
