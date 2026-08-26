//! The ABI L2 cloud-product suite for DA-grade ingest: product/variable
//! tables for ACHA, ACM, ACTP, COD, CPS and CTP, and decode entry points
//! that read the product's primary variable TOGETHER with its `DQF`
//! quality-flag companion and gate every non-good pixel to NaN, with the
//! counts recorded. Nothing is ever fabricated: a pixel the quality flags
//! condemn is NaN plus a count, never a value.
//!
//! Product tokens, primary variable names and sector availability were
//! verified against live `noaa-goes19` bucket listings and downloaded
//! granules on 2026-08-05 (day 216/2026): `HT` (ACHA, m), `BCM` (ACM),
//! `Phase` (ACTP), `COD` (COD), `CPS` (CPS, µm — the older `PSD` name does
//! not appear in current files), `PRES` (CTP, hPa). COD and CTP publish no
//! mesoscale sector under any prefix variant; the other four publish
//! CONUS, full-disk and both mesoscale views.
//!
//! ## Public product namespace: the `l2_` prefix
//!
//! The satellite catalog already publishes twenty-one named imagery
//! products plus `c01`..`c16` raw ABI channels through
//! [`GoesAbiProduct`](crate::product::GoesAbiProduct). Those are rendered
//! radiance products; these are retrieved geophysical quantities, and the
//! two namespaces met head-on at exactly one token:
//!
//! * **`cloud_top_phase`** already resolves to
//!   [`GoesAbiProduct::CloudPhase`](crate::product::GoesAbiProduct), the
//!   ABI **C11 8.4 µm brightness temperature** — an L1b radiance, not a
//!   retrieval. Serving both under one name would let a request for a
//!   retrieved phase class silently return a temperature in kelvin.
//!
//! `cloud_particle_size` is a near miss worth naming: ABI channel 6 is
//! called "Cloud Particle Size 2.2 µm", but it is published as `c06`, so
//! the reflectance and the CPS retrieval never share a token.
//!
//! The resolution is a namespace, not a rename war. Every L2 cloud
//! product owns an `l2_`-prefixed catalog id, which no channel or
//! composite id can ever take, and the mapping is fixed:
//!
//! | NOAA family | catalog id ([`CloudProduct::catalog_id`]) | store slug ([`CloudProduct::slug`]) | primary variable |
//! | --- | --- | --- | --- |
//! | `ABI-L2-ACHA` | `l2_cloud_top_height`    | `acha` | `HT`    |
//! | `ABI-L2-ACM`  | `l2_clear_sky_mask`      | `acm`  | `BCM`   |
//! | `ABI-L2-ACTP` | `l2_cloud_top_phase`     | `actp` | `Phase` |
//! | `ABI-L2-COD`  | `l2_cloud_optical_depth` | `cod`  | `COD`   |
//! | `ABI-L2-CPS`  | `l2_cloud_particle_size` | `cps`  | `CPS`   |
//! | `ABI-L2-CTP`  | `l2_cloud_top_pressure`  | `ctp`  | `PRES`  |
//!
//! [`CloudProduct::parse`] additionally accepts the unambiguous family
//! codes (`acha`, `acm`, `actp`, `cod`, `cps`, `ctp`), their `abi_l2_*`
//! spellings, and long names that the channel catalog does not claim. It
//! deliberately does **not** accept `cloud_top_phase`, `phase`, `height`
//! or `pressure`: the first collides outright, and the other three are
//! too generic to survive the next L2 family that lands.
//! `cloud_slugs_never_collide_with_the_channel_catalog` in this module
//! keeps that guarantee mechanical rather than aspirational.
//!
//! ## Reading: windows and previews, never a dense full disk
//!
//! The suite plugs into the existing native-source architecture. A
//! granule is archived byte-exactly next to the channel imagery of the
//! same frame ([`archive_goes_l2_source`](crate::archive::archive_goes_l2_source)),
//! and every read goes through one of three bounded doors:
//!
//! * [`read_archived_cloud_window`] — the tile/analysis door. It decodes
//!   only the requested native rectangle of the primary variable and the
//!   same rectangle of `DQF`, from an archived frame resolved by exact
//!   frame id, using the retained NOAA object key as the scene identity
//!   so the storage basename is never reparsed.
//! * [`read_archived_cloud_preview`] — the overview door. It decimates
//!   both planes on one globally aligned stride
//!   ([`automatic_preview_stride`](crate::archive::automatic_preview_stride)),
//!   so each preview pixel is a real native pixel carrying its own real
//!   `DQF`. Nothing is averaged across a quality boundary.
//! * [`read_cloud_product_field`] — the dense door, deliberately capped
//!   at [`MAX_DENSE_CLOUD_PLANE_CELLS`]. That admits every CONUS and
//!   mesoscale plane on the 2 km fixed grid and refuses a full disk
//!   (5,424 x 5,424 = 29,419,776 cells), pointing the caller at the two
//!   doors above. The refusal is this crate's own budget; the vendored
//!   NetCDF reader's array ceiling is untouched and unrelated.
//!
//! ## Provenance
//!
//! These are NOAA public-domain granules, not a derived house product.
//! [`CloudProduct::source`] records the provider, program, bucket family
//! and object-key shape each plane came from, and
//! [`cloud_product_catalog`] carries it alongside every descriptor, so a
//! catalog consumer can attribute a value without guessing.
//!
//! ## The two DQF conventions, measured
//!
//! The six products carry two incompatible DQF encodings, so the gate is
//! product-aware ([`DqfRule`]):
//!
//! * **Enumerated** (ACHA, ACM, CTP): a small enum published as
//!   `flag_values` with no `flag_masks`; `0` = good-quality. Good is
//!   exactly `DQF == 0`; everything else — including an unreadable or fill
//!   DQF — gates the pixel. This is the literal fail-closed rule. (Checked
//!   directly on the fixture ACHAM1 granule: `flag_values = [0 1 2 3 4]`,
//!   `valid_range = [0 3]`, no `flag_masks` attribute.)
//! * **ACTP** takes the same enumerated rule, but its DQF is really a
//!   bitfield: `flag_masks` with six paired bits, bit 0 =
//!   `overall_degraded_quality_qf`. The rule is exactly — not
//!   approximately — right here, because on the fixture ACTPC granule
//!   every one of the 352,589 valid non-zero DQF values has bit 0 set (the
//!   values present are 3, 5, 9, 13, 17, 21, 25, 29), and the 47,162 fill
//!   pixels decode to NaN, which is never good. `DQF == 0` and "overall
//!   good" name the same pixels on real data.
//! * **Bitfield** (COD and CPS, the DCOMP pair): a `u16` of paired
//!   provenance and degradation bits. Measured on
//!   `OR_ABI-L2-CODC-M6_G19_s20262161801170...` (and its CPS twin, whose
//!   DQF plane is bit-identical): every daytime pixel carries bit 2
//!   (`not_night_algorithm_pixel_qf`), and **every cloudy retrieval in the
//!   granule — 1,481,473 of them — carries bits 4 and 32**
//!   (`degraded_quality_qf`, `nonconvergence`). A `DQF != 0` gate, or even
//!   an any-quality-bit gate, therefore condemns 100% of actual cloud
//!   retrievals: fail-closed to the letter and useless in fact. What such
//!   a gate keeps is *not* clear sky. It is the single DQF value in the
//!   granule carrying no degradation bit at all, `2`, and ACTP calls all
//!   189,364 of those pixels cloudy (liquid 185,506, supercooled 1,048,
//!   mixed 2,810, clear 0) while DCOMP publishes fill or exactly 0.0 COD
//!   for every one of them. Zero retrievals survive and nothing usable is
//!   left. The default [`DqfRule::Bitfield`] mask instead condemns the
//!   three degradation causes that mark a compromised scene — snow/sea-ice
//!   surface (8), twilight (16) and sun glint (64) — plus any missing/fill
//!   DQF, and keeps the wholesale bits.
//!
//! ## The DCOMP default condemn mask: SETTLED (owner ruling, 2026-08-05)
//!
//! [`DCOMP_CONDEMN_DEFAULT`] is `8 | 16 | 64` — snow/sea-ice surface,
//! twilight and sun glint — plus any missing/fill DQF, since an unreadable
//! quality flag is never a good one. It deliberately keeps the wholesale
//! `degraded_quality_qf` and `nonconvergence` bits, which the algorithm
//! sets on every single retrieval and which therefore discriminate
//! nothing. On the measured granule the default retains 1,361,938 of the
//! 1,481,473 retrievals (~92%).
//!
//! The ruling's reasoning: the condemned bits mark conditions whose
//! retrievals are *biased* rather than merely noisy, and a biased
//! retrieval would contaminate an analysis in a way a noisy one does not.
//!
//! This settles the DEFAULT. It does not foreclose a later A/B on which
//! bits deserve to condemn, nor on inflating observation error versus
//! dropping the pixel outright. [`DCOMP_CONDEMN_STRICT`] restores the
//! literal rule for consumers who want it, and any custom mask can be
//! passed through the `*_with_rule` entry points; counts are recorded and
//! gated pixels are NaN either way.

use std::error::Error;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::abi::{
    GoesAbiField, GoesAbiScene, read_goes_abi_field_strided_from_scene,
    read_goes_abi_field_window_from_scene, read_goes_abi_scene, read_goes_abi_scene_with_identity,
};
use crate::archive::{automatic_preview_stride, resolve_native_cloud_frame};
use crate::s3::Sector;

/// The L2 cloud-suite products this module knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CloudProduct {
    /// ABI-L2-ACHA: cloud-top height (`HT`, m).
    CloudTopHeight,
    /// ABI-L2-ACM: clear-sky mask (`BCM`, 0 = clear, 1 = cloudy).
    ClearSkyMask,
    /// ABI-L2-ACTP: cloud-top phase (`Phase`, 0..=5).
    CloudTopPhase,
    /// ABI-L2-COD: cloud optical depth (`COD`, unitless).
    OpticalDepth,
    /// ABI-L2-CPS: cloud particle size (`CPS`, µm).
    ParticleSize,
    /// ABI-L2-CTP: cloud-top pressure (`PRES`, hPa).
    CloudTopPressure,
}

/// Default condemn mask for the DCOMP (COD/CPS) DQF bitfield: snow/sea-ice
/// surface (8), twilight (16), sun glint (64). Settled by owner ruling on
/// 2026-08-05; see the module docs for the measurement and the reasoning.
pub const DCOMP_CONDEMN_DEFAULT: u16 = 8 | 16 | 64;

/// The literal fail-closed mask for the DCOMP bitfield: every bit except
/// the two day/night provenance bits condemns. Measured to keep zero
/// retrievals on real GOES-19 granules — what survives is the lone
/// no-degradation-bit value `2`, which ACTP calls cloudy everywhere it
/// occurs and for which DCOMP publishes no optical depth. Provided for
/// consumers who want the literal rule, not as a usable cloud gate and not
/// as a clear-sky filter.
pub const DCOMP_CONDEMN_STRICT: u16 = !0b11;

/// The catalog-id namespace every L2 cloud product lives in. No ABI
/// channel or RGB composite id may ever start with it; see the module
/// docs for why the namespace exists.
pub const CLOUD_CATALOG_PREFIX: &str = "l2_";

/// The largest single plane a dense [`read_cloud_product_field`] may
/// materialize, in cells. 8,388,608 admits the CONUS plane on the L2
/// suite's 2 km fixed grid (2,500 x 1,500 = 3,750,000) and every
/// mesoscale plane (250 x 250 on the measured ACHA granule), and refuses
/// a full disk (5,424 x 5,424 = 29,419,776), which belongs in
/// [`read_archived_cloud_window`] or [`read_archived_cloud_preview`].
/// Each dense read holds two such planes at once — the primary and its
/// `DQF` companion.
pub const MAX_DENSE_CLOUD_PLANE_CELLS: usize = 8_388_608;

/// The default cell budget for a decimated preview plane.
pub const DEFAULT_CLOUD_PREVIEW_CELLS: usize = 2_097_152;

/// How a product's DQF plane marks a pixel good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DqfRule {
    /// Good is exactly `DQF == 0`; anything else (including fill) gates.
    Enumerated,
    /// Good is a finite DQF with no condemned bit set; a missing/fill DQF
    /// gates.
    Bitfield { condemn: u16 },
}

impl DqfRule {
    /// Whether one decoded DQF value marks its pixel good. Decoded fill /
    /// out-of-range DQF arrives here as NaN and is never good.
    pub fn is_good(self, dqf: f32) -> bool {
        match self {
            Self::Enumerated => dqf == 0.0,
            Self::Bitfield { condemn } => {
                dqf.is_finite()
                    && (0.0..=f32::from(u16::MAX)).contains(&dqf)
                    && (dqf as u16) & condemn == 0
            }
        }
    }

    /// Stable slug for catalog and log output.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Enumerated => "enumerated",
            Self::Bitfield { .. } => "bitfield",
        }
    }

    /// The condemn mask, for the bitfield rule only.
    pub fn condemn_mask(self) -> Option<u16> {
        match self {
            Self::Enumerated => None,
            Self::Bitfield { condemn } => Some(condemn),
        }
    }
}

impl CloudProduct {
    pub const ALL: [Self; 6] = [
        Self::CloudTopHeight,
        Self::ClearSkyMask,
        Self::CloudTopPhase,
        Self::OpticalDepth,
        Self::ParticleSize,
        Self::CloudTopPressure,
    ];

    /// Every accepted spelling, catalog id first. See the module docs for
    /// the tokens deliberately left out of this table.
    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::CloudTopHeight => &[
                "l2_cloud_top_height",
                "acha",
                "abi_l2_acha",
                "cloud_top_height",
            ],
            Self::ClearSkyMask => &[
                "l2_clear_sky_mask",
                "acm",
                "abi_l2_acm",
                "clear_sky_mask",
                "cloud_mask",
                "binary_cloud_mask",
            ],
            Self::CloudTopPhase => &["l2_cloud_top_phase", "actp", "abi_l2_actp"],
            Self::OpticalDepth => &[
                "l2_cloud_optical_depth",
                "cod",
                "abi_l2_cod",
                "cloud_optical_depth",
                "optical_depth",
            ],
            Self::ParticleSize => &[
                "l2_cloud_particle_size",
                "cps",
                "abi_l2_cps",
                "cloud_particle_size",
                "particle_size",
            ],
            Self::CloudTopPressure => &[
                "l2_cloud_top_pressure",
                "ctp",
                "abi_l2_ctp",
                "cloud_top_pressure",
            ],
        }
    }

    /// The public catalog id, in the [`CLOUD_CATALOG_PREFIX`] namespace.
    pub fn catalog_id(self) -> &'static str {
        self.aliases()[0]
    }

    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Self::ALL
            .into_iter()
            .find(|product| product.aliases().contains(&normalized.as_str()))
    }

    /// The product family token inside S3 prefixes and filenames.
    pub fn family(self) -> &'static str {
        match self {
            Self::CloudTopHeight => "ACHA",
            Self::ClearSkyMask => "ACM",
            Self::CloudTopPhase => "ACTP",
            Self::OpticalDepth => "COD",
            Self::ParticleSize => "CPS",
            Self::CloudTopPressure => "CTP",
        }
    }

    /// Store-safe lowercase slug: archive manifest key and the name
    /// inside content-addressed archive filenames.
    pub fn slug(self) -> &'static str {
        match self {
            Self::CloudTopHeight => "acha",
            Self::ClearSkyMask => "acm",
            Self::CloudTopPhase => "actp",
            Self::OpticalDepth => "cod",
            Self::ParticleSize => "cps",
            Self::CloudTopPressure => "ctp",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::CloudTopHeight => "Cloud-top Height · ABI L2 ACHA",
            Self::ClearSkyMask => "Clear-sky Mask · ABI L2 ACM",
            Self::CloudTopPhase => "Cloud-top Phase · ABI L2 ACTP",
            Self::OpticalDepth => "Cloud Optical Depth · ABI L2 COD",
            Self::ParticleSize => "Cloud Particle Size · ABI L2 CPS",
            Self::CloudTopPressure => "Cloud-top Pressure · ABI L2 CTP",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::CloudTopHeight => {
                "Retrieved cloud-top height above the ellipsoid, DQF-gated on the enumerated good-quality flag."
            }
            Self::ClearSkyMask => {
                "Binary clear-sky mask: 0 clear, 1 cloudy. Categorical — never interpolate it."
            }
            Self::CloudTopPhase => {
                "Retrieved cloud-top phase class: 0 clear, 1 liquid, 2 supercooled, 3 mixed, 4 ice, 5 unknown. Categorical — never interpolate it. This is the ACTP retrieval, not the C11 8.4 µm brightness temperature published as `cloud_phase`."
            }
            Self::OpticalDepth => {
                "Daytime DCOMP cloud optical depth, gated on the measured degradation-cause mask rather than the literal any-bit rule, which retains no retrievals at all."
            }
            Self::ParticleSize => {
                "Daytime DCOMP cloud particle effective radius, gated exactly as its COD twin: the two products publish a bit-identical DQF plane."
            }
            Self::CloudTopPressure => {
                "Retrieved cloud-top pressure, DQF-gated on the enumerated good-quality flag."
            }
        }
    }

    /// The primary NetCDF variable, as named in current granules
    /// (verified against real GOES-19 files, 2026-08-05).
    pub fn primary_variable(self) -> &'static str {
        match self {
            Self::CloudTopHeight => "HT",
            Self::ClearSkyMask => "BCM",
            Self::CloudTopPhase => "Phase",
            Self::OpticalDepth => "COD",
            Self::ParticleSize => "CPS",
            Self::CloudTopPressure => "PRES",
        }
    }

    /// The quality-flag companion variable. One name across the suite.
    pub fn dqf_variable(self) -> &'static str {
        "DQF"
    }

    /// The published units, or `None` for the categorical products.
    pub fn units(self) -> Option<&'static str> {
        match self {
            Self::CloudTopHeight => Some("m"),
            Self::CloudTopPressure => Some("hPa"),
            Self::ParticleSize => Some("um"),
            Self::OpticalDepth | Self::ClearSkyMask | Self::CloudTopPhase => None,
        }
    }

    /// Whether the plane holds class codes rather than a continuous
    /// quantity. Categorical planes must be sampled nearest-neighbour;
    /// averaging two class codes invents a third.
    pub fn categorical(self) -> bool {
        matches!(self, Self::ClearSkyMask | Self::CloudTopPhase)
    }

    /// The sectors NOAA actually publishes for this product.
    pub fn sectors(self) -> Vec<Sector> {
        [
            Sector::FullDisk,
            Sector::Conus,
            Sector::Meso1,
            Sector::Meso2,
        ]
        .into_iter()
        .filter(|sector| self.abi_product(*sector).is_some())
        .collect()
    }

    /// The filename product token for a sector (`ABI-L2-ACHAC`,
    /// `ABI-L2-ACHAM1`, ...), or `None` where NOAA publishes no such
    /// sector: COD and CTP have no mesoscale product (verified against
    /// `noaa-goes19` listings, 2026-08-05).
    pub fn abi_product(self, sector: Sector) -> Option<String> {
        let sector_token = match sector {
            Sector::Conus => "C",
            Sector::FullDisk => "F",
            Sector::Meso1 | Sector::Meso2 => {
                if matches!(self, Self::OpticalDepth | Self::CloudTopPressure) {
                    return None;
                }
                if sector == Sector::Meso1 { "M1" } else { "M2" }
            }
        };
        Some(format!("ABI-L2-{}{}", self.family(), sector_token))
    }

    /// Recover the product and sector from a NOAA filename product token
    /// such as `ABI-L2-ACHAM1`. Exact: the token must be one this suite
    /// would itself construct, so an unrelated L2 family never resolves
    /// to a cloud product by prefix accident.
    pub fn from_abi_product(token: &str) -> Option<(Self, Sector)> {
        let token = token.trim().to_ascii_uppercase();
        for product in Self::ALL {
            for sector in [
                Sector::FullDisk,
                Sector::Conus,
                Sector::Meso1,
                Sector::Meso2,
            ] {
                if product.abi_product(sector).as_deref() == Some(token.as_str()) {
                    return Some((product, sector));
                }
            }
        }
        None
    }

    /// The product's default DQF gate. See the module docs for the
    /// measured basis of the DCOMP bitfield default.
    pub fn dqf_rule(self) -> DqfRule {
        match self {
            Self::OpticalDepth | Self::ParticleSize => DqfRule::Bitfield {
                condemn: DCOMP_CONDEMN_DEFAULT,
            },
            _ => DqfRule::Enumerated,
        }
    }

    /// Where this plane comes from. NOAA GOES ABI L2 is public-domain
    /// U.S. Government data; nothing here is a house product.
    pub fn source(self) -> CloudSourceIdentity {
        CloudSourceIdentity {
            provider: "National Oceanic and Atmospheric Administration (NOAA) / NESDIS".into(),
            program: "GOES-R Series ABI Level 2+ cloud products".into(),
            collection: format!("ABI-L2-{}", self.family()),
            bucket_pattern: "noaa-goes{16,17,18,19}".into(),
            object_key_pattern: format!(
                "ABI-L2-{family}{{sector}}/{{yyyy}}/{{ddd}}/{{hh}}/OR_ABI-L2-{family}{{sector}}-M{{mode}}_{{platform}}_s...nc",
                family = self.family()
            ),
            access: "AWS Open Data Registry; anonymous HTTPS, no authentication.".into(),
            rights: "U.S. Government work in the public domain in the United States; \
                     redistributed under NOAA Open Data Dissemination. Credit NOAA as the \
                     source and do not present derived output as an official NOAA product."
                .into(),
            rights_url: "https://www.noaa.gov/information-technology/open-data-dissemination"
                .into(),
        }
    }

    /// The catalog descriptor for this product, provenance included.
    pub fn descriptor(self) -> CloudProductDescriptor {
        let rule = self.dqf_rule();
        CloudProductDescriptor {
            id: self.catalog_id().to_string(),
            title: self.title().to_string(),
            description: self.description().to_string(),
            abi_family: self.family().to_string(),
            store_slug: self.slug().to_string(),
            primary_variable: self.primary_variable().to_string(),
            quality_variable: self.dqf_variable().to_string(),
            units: self.units().map(str::to_string),
            categorical: self.categorical(),
            sectors: self
                .sectors()
                .into_iter()
                .map(|sector| sector.slug().to_string())
                .collect(),
            dqf_rule: rule.slug().to_string(),
            dqf_condemn_mask: rule.condemn_mask(),
            source: self.source(),
        }
    }
}

/// Where a cloud plane came from, recorded so a catalog consumer can
/// attribute a value without guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudSourceIdentity {
    pub provider: String,
    pub program: String,
    pub collection: String,
    pub bucket_pattern: String,
    pub object_key_pattern: String,
    pub access: String,
    pub rights: String,
    pub rights_url: String,
}

/// One entry of the L2 cloud catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudProductDescriptor {
    pub id: String,
    pub title: String,
    pub description: String,
    pub abi_family: String,
    pub store_slug: String,
    pub primary_variable: String,
    pub quality_variable: String,
    pub units: Option<String>,
    pub categorical: bool,
    pub sectors: Vec<String>,
    pub dqf_rule: String,
    pub dqf_condemn_mask: Option<u16>,
    pub source: CloudSourceIdentity,
}

/// The L2 cloud catalog, in [`CloudProduct::ALL`] order.
pub fn cloud_product_catalog() -> Vec<CloudProductDescriptor> {
    CloudProduct::ALL
        .into_iter()
        .map(CloudProduct::descriptor)
        .collect()
}

/// What the DQF gate did to one decoded plane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DqfReport {
    /// Pixels in the plane.
    pub total: usize,
    /// Pixels whose primary value was already NaN before the gate (fill /
    /// out of valid range).
    pub primary_missing: usize,
    /// Pixels whose DQF itself decoded to NaN (fill / out of range).
    /// Always gated: an unreadable quality flag is not a good one.
    pub dqf_missing: usize,
    /// Pixels the rule condemned (includes `dqf_missing`).
    pub dqf_bad: usize,
    /// Pixels the gate newly forced to NaN (condemned AND previously
    /// finite).
    pub masked: usize,
    /// Finite pixels remaining after the gate.
    pub finite: usize,
}

/// A rectangle of the native fixed grid, in cell indices — the unit an
/// archived read decodes. Grouping it keeps the archive door's signature
/// readable and matches the `x_start, x_count, y_start, y_count` order the
/// rest of the crate's windowed readers take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudWindow {
    pub x_start: usize,
    pub x_count: usize,
    pub y_start: usize,
    pub y_count: usize,
}

impl CloudWindow {
    pub fn new(x_start: usize, x_count: usize, y_start: usize, y_count: usize) -> Self {
        Self {
            x_start,
            x_count,
            y_start,
            y_count,
        }
    }

    /// Cells this window decodes, per plane. An archived read decodes two
    /// planes — the primary and its `DQF` companion.
    pub fn cells(self) -> usize {
        self.x_count.saturating_mul(self.y_count)
    }
}

/// A decoded, DQF-gated cloud-product plane.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudProductField {
    /// The primary variable with every condemned pixel forced to NaN.
    pub field: GoesAbiField,
    pub product: CloudProduct,
    pub dqf: DqfReport,
}

/// Read and gate a full cloud-product plane using the product's default
/// DQF rule. Bounded by [`MAX_DENSE_CLOUD_PLANE_CELLS`].
pub fn read_cloud_product_field(
    path: impl AsRef<Path>,
    product: CloudProduct,
) -> Result<CloudProductField, Box<dyn Error>> {
    read_cloud_product_field_with_rule(path, product, product.dqf_rule())
}

/// Read and gate a full cloud-product plane with an explicit DQF rule.
/// Bounded by [`MAX_DENSE_CLOUD_PLANE_CELLS`].
pub fn read_cloud_product_field_with_rule(
    path: impl AsRef<Path>,
    product: CloudProduct,
    rule: DqfRule,
) -> Result<CloudProductField, Box<dyn Error>> {
    let scene = read_goes_abi_scene(path.as_ref())?;
    read_cloud_product_field_from_scene(&scene, product, rule)
}

/// Read and gate a whole plane from an already-established scene, refusing
/// any grid above [`MAX_DENSE_CLOUD_PLANE_CELLS`].
pub fn read_cloud_product_field_from_scene(
    scene: &GoesAbiScene,
    product: CloudProduct,
    rule: DqfRule,
) -> Result<CloudProductField, Box<dyn Error>> {
    let nx = scene.fixed_grid.nx;
    let ny = scene.fixed_grid.ny;
    let cells = nx.saturating_mul(ny);
    if cells > MAX_DENSE_CLOUD_PLANE_CELLS {
        return Err(boxed_error(format!(
            "{} plane {nx}x{ny} is {cells} cells, above the {MAX_DENSE_CLOUD_PLANE_CELLS}-cell \
             dense read budget; use read_archived_cloud_window for an exact rectangle or \
             read_archived_cloud_preview for a decimated overview",
            product.catalog_id()
        )));
    }
    read_cloud_product_field_window_from_scene(scene, product, rule, 0, nx, 0, ny)
}

/// Read and gate a window of a cloud-product plane using the product's
/// default DQF rule. Window arguments follow
/// [`read_goes_abi_field_window`](crate::abi::read_goes_abi_field_window).
pub fn read_cloud_product_field_window(
    path: impl AsRef<Path>,
    product: CloudProduct,
    x_start: usize,
    x_count: usize,
    y_start: usize,
    y_count: usize,
) -> Result<CloudProductField, Box<dyn Error>> {
    read_cloud_product_field_window_with_rule(
        path,
        product,
        product.dqf_rule(),
        x_start,
        x_count,
        y_start,
        y_count,
    )
}

/// Read and gate a window of a cloud-product plane with an explicit DQF
/// rule.
pub fn read_cloud_product_field_window_with_rule(
    path: impl AsRef<Path>,
    product: CloudProduct,
    rule: DqfRule,
    x_start: usize,
    x_count: usize,
    y_start: usize,
    y_count: usize,
) -> Result<CloudProductField, Box<dyn Error>> {
    let scene = read_goes_abi_scene(path.as_ref())?;
    read_cloud_product_field_window_from_scene(
        &scene, product, rule, x_start, x_count, y_start, y_count,
    )
}

/// Read and gate a window from an already-established scene.
///
/// This is the archive-safe door: it opens `scene.path` and never
/// reparses that storage basename, so the retained NOAA object key stays
/// the authoritative product identity.
pub fn read_cloud_product_field_window_from_scene(
    scene: &GoesAbiScene,
    product: CloudProduct,
    rule: DqfRule,
    x_start: usize,
    x_count: usize,
    y_start: usize,
    y_count: usize,
) -> Result<CloudProductField, Box<dyn Error>> {
    let mut field = read_goes_abi_field_window_from_scene(
        scene,
        product.primary_variable(),
        x_start,
        x_count,
        y_start,
        y_count,
    )?;
    let dqf = read_goes_abi_field_window_from_scene(
        scene,
        product.dqf_variable(),
        x_start,
        x_count,
        y_start,
        y_count,
    )?;
    let report = gate_by_dqf(&mut field.values, &dqf.values, rule)?;
    Ok(CloudProductField {
        field,
        product,
        dqf: report,
    })
}

/// Read and gate a decimated overview of a whole plane from an
/// already-established scene.
///
/// Primary and `DQF` are decimated on the same stride, so every surviving
/// value is a real native pixel judged by that same pixel's own quality
/// flag. Nothing is averaged, which keeps the categorical products
/// (`BCM`, `Phase`) honest and keeps a good pixel from inheriting a
/// neighbour's condemnation.
pub fn read_cloud_product_preview_from_scene(
    scene: &GoesAbiScene,
    product: CloudProduct,
    rule: DqfRule,
    maximum_cells: usize,
) -> Result<CloudProductField, Box<dyn Error>> {
    let step = automatic_preview_stride(scene.fixed_grid.nx, scene.fixed_grid.ny, maximum_cells);
    let mut field =
        read_goes_abi_field_strided_from_scene(scene, product.primary_variable(), step)?;
    let dqf = read_goes_abi_field_strided_from_scene(scene, product.dqf_variable(), step)?;
    let report = gate_by_dqf(&mut field.values, &dqf.values, rule)?;
    Ok(CloudProductField {
        field,
        product,
        dqf: report,
    })
}

/// Open an archived L2 granule as a scene, using the retained NOAA object
/// key as its identity, and confirm the archived bytes really are the
/// requested product.
fn open_archived_cloud_scene(
    store_root: &Path,
    platform: &str,
    sector: &str,
    product: CloudProduct,
    frame: &str,
) -> Result<(GoesAbiScene, String), Box<dyn Error>> {
    let manifest = resolve_native_cloud_frame(store_root, platform, sector, &[product], frame)?;
    let source = manifest.l2_product_source(product)?;
    let path = manifest.l2_product_path(store_root, product)?;
    let scene = read_goes_abi_scene_with_identity(&path, &source.object_key)?;
    match CloudProduct::from_abi_product(&scene.product) {
        Some((archived, _)) if archived == product => {}
        _ => {
            return Err(boxed_error(format!(
                "native frame {} maps {} to object {}, which is product {}",
                manifest.frame_id,
                product.catalog_id(),
                source.object_key,
                scene.product
            )));
        }
    }
    Ok((scene, manifest.frame_id.clone()))
}

/// Decode one native rectangle of an archived L2 cloud granule, gated by
/// the same rectangle of its `DQF` companion.
///
/// This is the door a windowed tile renderer uses: it resolves an exact
/// frame, decodes only the requested cells, and never materializes the
/// full plane.
pub fn read_archived_cloud_window(
    store_root: &Path,
    platform: &str,
    sector: &str,
    product: CloudProduct,
    frame: &str,
    window: CloudWindow,
) -> Result<CloudProductField, Box<dyn Error>> {
    let (scene, _frame_id) =
        open_archived_cloud_scene(store_root, platform, sector, product, frame)?;
    read_cloud_product_field_window_from_scene(
        &scene,
        product,
        product.dqf_rule(),
        window.x_start,
        window.x_count,
        window.y_start,
        window.y_count,
    )
}

/// Decode a decimated overview of an archived L2 cloud granule, bounded
/// by `maximum_cells` (pass [`DEFAULT_CLOUD_PREVIEW_CELLS`] for the
/// default budget).
pub fn read_archived_cloud_preview(
    store_root: &Path,
    platform: &str,
    sector: &str,
    product: CloudProduct,
    frame: &str,
    maximum_cells: usize,
) -> Result<CloudProductField, Box<dyn Error>> {
    let (scene, _frame_id) =
        open_archived_cloud_scene(store_root, platform, sector, product, frame)?;
    read_cloud_product_preview_from_scene(&scene, product, product.dqf_rule(), maximum_cells)
}

/// Force every pixel `rule` condemns to NaN, in place, and account for
/// every pixel. Pure; the decode entry points feed it and tests pin it.
pub fn gate_by_dqf(
    values: &mut [f32],
    dqf: &[f32],
    rule: DqfRule,
) -> Result<DqfReport, Box<dyn Error>> {
    if values.len() != dqf.len() {
        return Err(boxed_error(format!(
            "DQF plane length {} does not match primary plane length {}",
            dqf.len(),
            values.len()
        )));
    }
    let mut report = DqfReport {
        total: values.len(),
        ..DqfReport::default()
    };
    for (value, &flag) in values.iter_mut().zip(dqf.iter()) {
        let primary_was_finite = value.is_finite();
        if !primary_was_finite {
            report.primary_missing += 1;
        }
        if !flag.is_finite() {
            report.dqf_missing += 1;
        }
        if !rule.is_good(flag) {
            report.dqf_bad += 1;
            if primary_was_finite {
                report.masked += 1;
            }
            *value = f32::NAN;
        }
        if value.is_finite() {
            report.finite += 1;
        }
    }
    Ok(report)
}

fn boxed_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{AbiFixedGrid, AbiSector, GoesImagerProjection};
    use crate::geostationary::SweepAngleAxis;
    use crate::goes::GoesSatellite;
    use crate::product::GoesAbiProduct;
    use crate::s3::product_hour_prefix;
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;

    #[test]
    fn product_tokens_match_live_bucket_keys() {
        // Pinned against listings taken from noaa-goes19 on 2026-08-05.
        assert_eq!(
            CloudProduct::CloudTopHeight
                .abi_product(Sector::Conus)
                .as_deref(),
            Some("ABI-L2-ACHAC")
        );
        assert_eq!(
            CloudProduct::CloudTopHeight
                .abi_product(Sector::Meso1)
                .as_deref(),
            Some("ABI-L2-ACHAM1")
        );
        assert_eq!(
            CloudProduct::ClearSkyMask
                .abi_product(Sector::Meso2)
                .as_deref(),
            Some("ABI-L2-ACMM2")
        );
        assert_eq!(
            CloudProduct::CloudTopPhase
                .abi_product(Sector::FullDisk)
                .as_deref(),
            Some("ABI-L2-ACTPF")
        );
        assert_eq!(
            CloudProduct::OpticalDepth
                .abi_product(Sector::Conus)
                .as_deref(),
            Some("ABI-L2-CODC")
        );
        // NOAA publishes no mesoscale COD or CTP.
        assert_eq!(CloudProduct::OpticalDepth.abi_product(Sector::Meso1), None);
        assert_eq!(
            CloudProduct::CloudTopPressure.abi_product(Sector::Meso2),
            None
        );
        assert_eq!(
            CloudProduct::CloudTopPressure
                .abi_product(Sector::Conus)
                .as_deref(),
            Some("ABI-L2-CTPC")
        );
        assert_eq!(
            CloudProduct::OpticalDepth.sectors(),
            vec![Sector::FullDisk, Sector::Conus]
        );
        assert_eq!(
            CloudProduct::CloudTopPhase.sectors(),
            vec![
                Sector::FullDisk,
                Sector::Conus,
                Sector::Meso1,
                Sector::Meso2
            ]
        );
    }

    #[test]
    fn abi_product_tokens_round_trip_exactly() {
        for product in CloudProduct::ALL {
            for sector in product.sectors() {
                let token = product.abi_product(sector).unwrap();
                assert_eq!(
                    CloudProduct::from_abi_product(&token),
                    Some((product, sector)),
                    "{token} must round-trip"
                );
            }
        }
        // Neighbouring L2 families must never resolve by prefix accident.
        assert_eq!(CloudProduct::from_abi_product("ABI-L2-CMIPC"), None);
        assert_eq!(CloudProduct::from_abi_product("ABI-L2-ACHA"), None);
        assert_eq!(CloudProduct::from_abi_product("ABI-L2-CODM1"), None);
        assert_eq!(CloudProduct::from_abi_product("ABI-L2-ACTPX"), None);
    }

    #[test]
    fn cloud_prefixes_match_observed_key_layout() {
        use crate::goes::GoesSatellite;
        let hour = Utc.with_ymd_and_hms(2026, 8, 4, 18, 0, 0).unwrap();
        // Real key, 2026-08-05 listing:
        // ABI-L2-ACHAC/2026/216/18/OR_ABI-L2-ACHAC-M6_G19_s20262161801170_...
        assert_eq!(
            product_hour_prefix("ABI-L2-ACHAC", &GoesSatellite::G19, 6, hour),
            "ABI-L2-ACHAC/2026/216/18/OR_ABI-L2-ACHAC-M6_G19_"
        );
        // Mesoscale: shared directory prefix, sector digit kept in the
        // filename token. Real key:
        // ABI-L2-ACHAM/2026/216/18/OR_ABI-L2-ACHAM1-M6_G19_s20262161801249_...
        assert_eq!(
            product_hour_prefix("ABI-L2-ACHAM1", &GoesSatellite::G19, 6, hour),
            "ABI-L2-ACHAM/2026/216/18/OR_ABI-L2-ACHAM1-M6_G19_"
        );
    }

    #[test]
    fn parse_accepts_family_codes_and_catalog_ids() {
        assert_eq!(
            CloudProduct::parse("ACHA"),
            Some(CloudProduct::CloudTopHeight)
        );
        assert_eq!(
            CloudProduct::parse("l2-cloud-top-phase"),
            Some(CloudProduct::CloudTopPhase)
        );
        assert_eq!(
            CloudProduct::parse("ABI-L2-CPS"),
            Some(CloudProduct::ParticleSize)
        );
        assert_eq!(CloudProduct::parse("cod"), Some(CloudProduct::OpticalDepth));
        assert_eq!(CloudProduct::parse("bogus"), None);
    }

    /// The whole point of the `l2_` namespace: one request token can
    /// never mean both a retrieval and a radiance.
    #[test]
    fn cloud_slugs_never_collide_with_the_channel_catalog() {
        for product in CloudProduct::ALL {
            assert!(
                product.catalog_id().starts_with(CLOUD_CATALOG_PREFIX),
                "{} must live in the l2_ namespace",
                product.catalog_id()
            );
            for alias in product.aliases() {
                assert_eq!(
                    GoesAbiProduct::parse(alias),
                    None,
                    "cloud alias {alias} is also a channel-catalog product"
                );
                assert_eq!(
                    CloudProduct::parse(alias),
                    Some(product),
                    "cloud alias {alias} must resolve to its own product"
                );
            }
        }
        // ... and nothing the channel catalog publishes resolves here.
        for product in GoesAbiProduct::NAMED {
            assert_eq!(CloudProduct::parse(&product.slug()), None);
        }
        for channel in 1..=16u8 {
            assert_eq!(
                CloudProduct::parse(&GoesAbiProduct::RawChannel(channel).slug()),
                None
            );
        }
        // The one token that actually collided, pinned in both
        // directions so a future rename cannot silently reopen it.
        assert_eq!(
            GoesAbiProduct::parse("cloud_top_phase"),
            Some(GoesAbiProduct::CloudPhase)
        );
        assert_eq!(CloudProduct::parse("cloud_top_phase"), None);
        for generic in ["phase", "height", "pressure"] {
            assert_eq!(
                CloudProduct::parse(generic),
                None,
                "{generic} is too generic to own"
            );
        }
    }

    #[test]
    fn catalog_records_variables_units_and_noaa_provenance() {
        let catalog = cloud_product_catalog();
        assert_eq!(catalog.len(), CloudProduct::ALL.len());
        let phase = catalog
            .iter()
            .find(|entry| entry.id == "l2_cloud_top_phase")
            .expect("ACTP is catalogued");
        assert_eq!(phase.primary_variable, "Phase");
        assert_eq!(phase.quality_variable, "DQF");
        assert_eq!(phase.units, None);
        assert!(phase.categorical);
        assert_eq!(phase.dqf_rule, "enumerated");
        assert_eq!(phase.dqf_condemn_mask, None);
        assert_eq!(phase.source.collection, "ABI-L2-ACTP");

        let cod = catalog
            .iter()
            .find(|entry| entry.id == "l2_cloud_optical_depth")
            .expect("COD is catalogued");
        assert_eq!(cod.dqf_rule, "bitfield");
        assert_eq!(cod.dqf_condemn_mask, Some(DCOMP_CONDEMN_DEFAULT));
        assert!(!cod.categorical);
        assert_eq!(cod.sectors, vec!["fulldisk", "conus"]);

        for entry in &catalog {
            assert!(entry.source.provider.contains("NOAA"));
            assert!(entry.source.rights.contains("public domain"));
            assert!(entry.source.bucket_pattern.contains("noaa-goes"));
            assert!(entry.source.object_key_pattern.contains(&entry.abi_family));
        }
    }

    #[test]
    fn enumerated_rule_is_literal_fail_closed() {
        let rule = DqfRule::Enumerated;
        assert!(rule.is_good(0.0));
        assert!(!rule.is_good(1.0));
        assert!(!rule.is_good(3.0));
        assert!(!rule.is_good(f32::NAN), "unreadable DQF is never good");
    }

    #[test]
    fn bitfield_rule_ignores_provenance_and_condemns_causes() {
        let rule = DqfRule::Bitfield {
            condemn: DCOMP_CONDEMN_DEFAULT,
        };
        // Three DQF values the fixture CODC granule really contains, and
        // what each one actually is (recomputed from raw integers):
        //   2   — 189,364 px, the only value in the granule with no
        //         degradation bit set. NOT clear sky: ACTP calls every one
        //         cloudy (liquid 185,506, supercooled 1,048, mixed 2,810,
        //         clear 0), yet DCOMP publishes fill or exactly 0.0 COD
        //         for all of them, so none is a retrieval.
        //   134 — 1,775,498 px, day + degraded + ice-phase bits but no bit
        //         32, so it sits outside the 1,481,473 retrieval
        //         population entirely; 1,627,710 are ACTP-clear and not
        //         one pixel carries COD > 0 (COD is fill for 158,154 and
        //         exactly 0.0 for the other 1,617,344).
        //   678 — 561,187 px, the genuine wholesale-flagged cloudy
        //         retrievals: all ice, all COD > 0.
        // All three must pass the default gate.
        assert!(rule.is_good(2.0));
        assert!(rule.is_good(134.0));
        assert!(rule.is_good(678.0));
        // Glint (64), twilight (16), snow/ice surface (8) condemn.
        assert!(!rule.is_good(742.0));
        assert!(!rule.is_good(2.0 + 16.0));
        assert!(!rule.is_good(2.0 + 8.0));
        assert!(!rule.is_good(f32::NAN), "fill DQF is never good");

        let strict = DqfRule::Bitfield {
            condemn: DCOMP_CONDEMN_STRICT,
        };
        assert!(strict.is_good(2.0), "provenance bits never condemn");
        assert!(!strict.is_good(134.0), "strict condemns wholesale bits");
    }

    #[test]
    fn gate_accounts_for_every_pixel() {
        let mut values = vec![1.0, 2.0, f32::NAN, 4.0, 5.0];
        let dqf = vec![0.0, 1.0, 0.0, f32::NAN, 0.0];
        let report = gate_by_dqf(&mut values, &dqf, DqfRule::Enumerated).unwrap();
        assert_eq!(
            report,
            DqfReport {
                total: 5,
                primary_missing: 1,
                dqf_missing: 1,
                dqf_bad: 2,
                masked: 2,
                finite: 2,
            }
        );
        assert_eq!(values[0], 1.0);
        assert!(values[1].is_nan(), "condemned pixel gated to NaN");
        assert!(values[3].is_nan(), "fill DQF gates fail-closed");
        assert_eq!(values[4], 5.0);

        let mut short = vec![1.0];
        assert!(gate_by_dqf(&mut short, &dqf, DqfRule::Enumerated).is_err());
    }

    /// The DCOMP bitfield gate must survive an out-of-range DQF without
    /// wrapping a `u16` cast into a value that happens to look good.
    #[test]
    fn bitfield_rule_rejects_values_outside_the_u16_domain() {
        let rule = DqfRule::Bitfield {
            condemn: DCOMP_CONDEMN_DEFAULT,
        };
        assert!(!rule.is_good(-1.0));
        assert!(!rule.is_good(65_536.0));
        assert!(!rule.is_good(f32::INFINITY));
        assert!(!rule.is_good(f32::NEG_INFINITY));
    }

    fn scene_with_grid(nx: usize, ny: usize) -> GoesAbiScene {
        let start_time_utc = Utc.with_ymd_and_hms(2026, 8, 4, 18, 1, 17).unwrap();
        GoesAbiScene {
            path: PathBuf::from("unreadable-on-purpose.nc"),
            product: "ABI-L2-ACHAF".into(),
            sector: AbiSector::FullDisk,
            channel: None,
            satellite: GoesSatellite::G19,
            start_time_utc,
            end_time_utc: start_time_utc + chrono::Duration::seconds(571),
            projection: GoesImagerProjection {
                perspective_point_height_m: 35_786_023.0,
                semi_major_axis_m: 6_378_137.0,
                semi_minor_axis_m: 6_356_752.314_14,
                longitude_of_projection_origin_deg: -75.0,
                sweep_angle_axis: SweepAngleAxis::X,
            },
            fixed_grid: AbiFixedGrid {
                nx,
                ny,
                x_scan_rad: vec![0.0; nx],
                y_scan_rad: vec![0.0; ny],
            },
        }
    }

    /// A full-disk 2 km plane must be refused before a single byte is
    /// decoded — the scene path here does not even exist, so reaching the
    /// reader at all would surface as a different error.
    #[test]
    fn dense_reads_refuse_a_full_disk_plane_before_decoding() {
        let scene = scene_with_grid(5_424, 5_424);
        let error = read_cloud_product_field_from_scene(
            &scene,
            CloudProduct::CloudTopHeight,
            DqfRule::Enumerated,
        )
        .expect_err("a full disk must not be read densely");
        let message = error.to_string();
        assert!(message.contains("29419776"), "{message}");
        assert!(message.contains("read_archived_cloud_window"), "{message}");
        assert!(message.contains("read_archived_cloud_preview"), "{message}");
        assert!(message.contains("l2_cloud_top_height"), "{message}");
    }

    #[test]
    fn dense_reads_admit_conus_and_mesoscale_grids() {
        // 2 km CONUS and mesoscale planes stay inside the budget; only
        // the grid check is exercised here, so the failure below is the
        // reader refusing a path that does not exist, not the budget.
        for (nx, ny) in [(2_500, 1_500), (500, 500)] {
            assert!(nx * ny <= MAX_DENSE_CLOUD_PLANE_CELLS);
            let scene = scene_with_grid(nx, ny);
            let message = read_cloud_product_field_from_scene(
                &scene,
                CloudProduct::CloudTopHeight,
                DqfRule::Enumerated,
            )
            .expect_err("the fixture scene has no file behind it")
            .to_string();
            assert!(
                !message.contains("dense read budget"),
                "{nx}x{ny} must pass the budget: {message}"
            );
        }
    }
}
