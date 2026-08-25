use serde::{Deserialize, Serialize};

pub const FORMAT_SPECIFICATION: SpecificationReference = SpecificationReference {
    authority: "WSR-88D Radar Operations Center",
    document: "2620001AD",
    build: "24.0",
    issued: "2025-08-19",
    references: "section 3.3.1; Figures 3-6, 3-8b, 3-14, 3-16; Tables III, VIII, IX; Appendix D",
};

pub const PRODUCT_SPECIFICATION: SpecificationReference = SpecificationReference {
    authority: "WSR-88D Radar Operations Center",
    document: "2620003AE",
    build: "24.0",
    issued: "2025-08-19",
    references: "section 18; Appendix C Formats I and V",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpecificationReference {
    pub authority: &'static str,
    pub document: &'static str,
    pub build: &'static str,
    pub issued: &'static str,
    pub references: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodeOptions {
    /// Four-character radar site identifier used only when the transport
    /// header does not carry a PIL from which it can be derived.
    pub site_hint: Option<String>,
    pub limits: DecodeLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeLimits {
    pub max_input_bytes: usize,
    pub max_decompressed_body_bytes: usize,
    pub max_scan_prefix_bytes: usize,
    pub max_layers: usize,
    pub max_packets_per_layer: usize,
    pub max_pages: usize,
    pub max_lines_per_page: usize,
    pub max_cells: usize,
    pub max_track_points_per_cell: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_decompressed_body_bytes: 16 * 1024 * 1024,
            max_scan_prefix_bytes: 512,
            max_layers: 64,
            max_packets_per_layer: 65_536,
            max_pages: 48,
            max_lines_per_page: 17,
            max_cells: 256,
            max_track_points_per_cell: 64,
        }
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "product", rename_all = "snake_case")]
pub enum NexradStormProduct {
    StormTracking(StormTrackingProduct),
    StormStructure(StormStructureProduct),
}

impl NexradStormProduct {
    #[must_use]
    pub fn identity(&self) -> &ProductIdentity {
        match self {
            Self::StormTracking(product) => &product.identity,
            Self::StormStructure(product) => &product.identity,
        }
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProductIdentity {
    pub message_code: i16,
    pub mnemonic: String,
    pub product_version: u8,
    pub radar_site: SiteIdentity,
    pub radar_location: GeographicPoint,
    pub radar_height_feet: i16,
    pub message_at_unix_ms: i64,
    pub volume_scan_at_unix_ms: i64,
    pub generated_at_unix_ms: i64,
    pub message_sequence: i16,
    pub volume_scan_number: i16,
    pub source_id: i16,
    pub destination_id: i16,
    pub operational_mode: i16,
    pub volume_coverage_pattern: i16,
    pub compression: Compression,
    pub transport: TransportIdentity,
    pub provenance: ProductProvenance,
    /// Explicitly reported operational oddities that are safe to isolate.
    /// An empty list means every relevant declared offset was consumed.
    pub validation_notices: Vec<ValidationNotice>,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "notice", rename_all = "snake_case")]
pub enum ValidationNotice {
    IgnoredOutOfRangeOptionalCellTrendOffset {
        offset_bytes: usize,
        message_length: usize,
    },
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteIdentity {
    pub site_id: Option<String>,
    pub source: SiteIdentitySource,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteIdentitySource {
    WmoProductIdentifier,
    CallerHint,
    SourceIdOnly,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TransportIdentity {
    pub wmo_heading: Option<String>,
    pub wmo_origin: Option<String>,
    pub product_identifier: Option<String>,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    None,
    Bzip2,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductProvenance {
    pub producer: String,
    pub format_specification: SpecificationReferenceOwned,
    pub product_specification: SpecificationReferenceOwned,
    pub supplied_geometry: SuppliedGeometry,
    pub geometry_statement: String,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecificationReferenceOwned {
    pub authority: String,
    pub document: String,
    pub build: String,
    pub issued: String,
    pub references: String,
}

impl From<SpecificationReference> for SpecificationReferenceOwned {
    fn from(value: SpecificationReference) -> Self {
        Self {
            authority: value.authority.to_owned(),
            document: value.document.to_owned(),
            build: value.build.to_owned(),
            issued: value.issued.to_owned(),
            references: value.references.to_owned(),
        }
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppliedGeometry {
    CentroidPointsAndTracks,
    CentroidPointsOnly,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeographicPoint {
    pub latitude_degrees: f64,
    pub longitude_degrees: f64,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct RadarRelativePosition {
    /// Signed eastward coordinate in exact quarter-kilometre units.
    pub i_quarter_km: i16,
    /// Signed northward coordinate in exact quarter-kilometre units.
    pub j_quarter_km: i16,
}

impl RadarRelativePosition {
    #[must_use]
    pub fn geographic_from(self, radar: GeographicPoint) -> GeographicPoint {
        // ROC 2620001AD section 3.3.3 defines I east and J north at 0.25 km.
        // The geographic conversion is explicitly our spherical derivation;
        // it is not an extra coordinate transmitted by the RPG.
        let east_m = f64::from(self.i_quarter_km) * 250.0;
        let north_m = f64::from(self.j_quarter_km) * 250.0;
        let distance = east_m.hypot(north_m);
        if distance == 0.0 {
            return radar;
        }
        let bearing = east_m.atan2(north_m);
        let angular = distance / 6_371_008.8_f64;
        let lat1 = radar.latitude_degrees.to_radians();
        let lon1 = radar.longitude_degrees.to_radians();
        let lat2 = (lat1.sin() * angular.cos() + lat1.cos() * angular.sin() * bearing.cos()).asin();
        let lon2 = lon1
            + (bearing.sin() * angular.sin() * lat1.cos())
                .atan2(angular.cos() - lat1.sin() * lat2.sin());
        GeographicPoint {
            latitude_degrees: lat2.to_degrees(),
            longitude_degrees: ((lon2.to_degrees() + 540.0) % 360.0) - 180.0,
        }
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StormTrackingProduct {
    pub identity: ProductIdentity,
    pub cells: Vec<TrackedStormCell>,
    pub forecast_interval_minutes: Option<u16>,
    pub number_of_past_volumes: Option<u16>,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackedStormCell {
    pub storm_id: String,
    pub current: TrackPoint,
    /// Packet order is preserved. Product 58 does not encode an exact time for
    /// each past point, so `valid_at_unix_ms` remains `None` rather than being
    /// invented from scan cadence.
    pub history_in_packet_order: Vec<TrackPoint>,
    pub forecasts: Vec<ForecastPoint>,
    pub motion: StormMotion,
    pub forecast_error_nautical_miles: Option<f32>,
    pub mean_error_nautical_miles: Option<f32>,
    pub stationary_radius_quarter_km: Option<i16>,
    pub tabular_current: Option<AzimuthRange>,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackPoint {
    pub position: RadarRelativePosition,
    pub geographic: GeographicPoint,
    pub valid_at_unix_ms: Option<i64>,
    pub radar_relative_provenance: CoordinateProvenance,
    pub geographic_derivation: CoordinateProvenance,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastPoint {
    pub point: TrackPoint,
    pub lead_minutes: Option<u16>,
    pub tabular_position: Option<AzimuthRange>,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateProvenance {
    RpgPacketQuarterKilometre,
    SphericalRadarCentricRwV1,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AzimuthRange {
    pub azimuth_degrees: u16,
    pub range_nautical_miles: u16,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StormMotion {
    New,
    Moving {
        /// Meteorological direction from which the storm moves.
        direction_from_degrees: u16,
        speed_knots: u16,
    },
    NoData,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StormStructureProduct {
    pub identity: ProductIdentity,
    pub cells: Vec<StormStructureCell>,
    pub reported_cell_count: Option<u16>,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StormStructureCell {
    pub storm_id: String,
    pub position: AzimuthRange,
    pub base_kft_agl: QualifiedHeight,
    pub top_kft_agl: QualifiedHeight,
    pub cell_based_vil_kg_m2: u16,
    pub maximum_reflectivity_dbz: u16,
    pub maximum_reflectivity_height_kft_agl: f32,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualifiedHeight {
    pub kft_agl: f32,
    pub qualifier: HeightQualifier,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeightQualifier {
    Exact,
    BelowLowestElevation,
    AboveHighestElevation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Level2DerivedGeometryRef {
    pub geometry_id: String,
    pub site_id: String,
    pub volume_scan_at_unix_ms: i64,
    pub centroid: GeographicPoint,
    pub provenance: DerivedGeometryProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedGeometryProvenance {
    pub source_kind: String,
    pub source_id: String,
    pub method_id: String,
    pub method_version: String,
    pub moment: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PairingOptions {
    pub maximum_time_delta_ms: i64,
    pub maximum_centroid_distance_m: f64,
}

impl Default for PairingOptions {
    fn default() -> Self {
        Self {
            maximum_time_delta_ms: 5 * 60 * 1_000,
            maximum_centroid_distance_m: 10_000.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeometryPairingResult {
    pub associations: Vec<StormGeometryAssociation>,
    pub unmatched_storm_ids: Vec<String>,
    pub unmatched_geometry_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StormGeometryAssociation {
    pub storm_id: String,
    pub tracking_product: ProductIdentity,
    pub authoritative_centroid: TrackPoint,
    pub derived_geometry: Level2DerivedGeometryRef,
    pub centroid_distance_m: f64,
    pub absolute_time_delta_ms: i64,
    pub method: AssociationMethod,
    pub provenance_statement: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssociationMethod {
    SameSiteTimeWindowNearestCentroidRwV1,
}
