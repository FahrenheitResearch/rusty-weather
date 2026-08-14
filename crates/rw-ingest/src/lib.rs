//! Live GRIB -> `.rws` ingest as a library: the exact fetch/extract/derive/
//! write flow the `rw_ingest`/`rw_batch` bins have always run, extracted
//! from their `#[path]`-shared modules so interactive hosts (the
//! rusty-weather-ui shell) can run ingests in-process on background
//! threads with progress events and cancellation.
//!
//! Layout mirrors the old bin-side modules one-to-one:
//! - [`ingest_hour`] (re-exported at the crate root): per-hour
//!   [`fetch_hour`] / [`process_fetched_hour`] / [`ingest_hour()`],
//!   [`IngestConfig`], [`planned_store_variables`], [`parse_hours`].
//! - [`ingest_profile`]: what one run fetches/extracts/computes/stores
//!   (`full` / `sounding` / `view` / `surface` / `analysis` presets + overrides +
//!   validation).
//! - [`ingest_compute`]: the derived/heavy precompute over the products
//!   decode lane.
//! - [`size_estimate`]: exact (`walk_hour_sizes`) and predictive
//!   (`estimate` against a [`size_estimate::Calibration`]) sizing.
//! - [`throttle`]: polite scheduling — the bins' process-wide knobs plus
//!   the per-thread / dedicated-pool variants for interactive hosts.
//! - [`events`] (re-exported at the crate root): [`IngestEvent`] progress
//!   stream, [`IngestStage`], [`IngestError`] (with a `Cancelled` variant),
//!   and [`print_event`] — the sink that reproduces the bins' historical
//!   stdout/stderr lines byte-for-byte.

mod events;
pub mod ingest_hour;
pub mod throttle;

// Child modules of `ingest_hour` historically; kept reachable both ways.
pub use ingest_hour::ingest_compute;
pub use ingest_hour::ingest_profile;
pub use ingest_hour::size_estimate;

pub use events::{IngestError, IngestEvent, IngestStage, NEVER_CANCEL, print_event};
pub use ingest_hour::{
    FetchedHour, IngestConfig, IngestedHour, PlannedStoreVariables, SpilledFetchedHour,
    VolumeSummary, cache_state, fetch_hour, ingest_hour as ingest_hour_serial, parse_hours,
    planned_store_variables, process_fetched_hour, validate_forecast_hours,
};

/// Short git SHA (plus `-dirty`) of the build that produced this crate, the
/// same stamp `write_hour_from_fields_with_derived` records in `run.json`.
pub fn build_sha() -> &'static str {
    env!("RW_BUILD_SHA")
}

/// One product file to fetch for an hour and the roles its messages serve.
/// The per-hour flow has two extraction roles — a pressure-source file (the
/// 3D isobaric volumes + render-grade isobaric planes + the prs-side thermo
/// decode) and a surface-source file (the 2D surface set + the surface-side
/// thermo decode). HRRR splits them across two physical files (`prs`/`sfc`);
/// GFS's single `pgrb2.0p25` carries both, so one entry sets both roles;
/// RTMA/URMA and HIRESW set only the surface role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductFetch {
    /// Product token passed to [`rustwx_core::ModelRunRequest::new`].
    pub product: &'static str,
    /// The 2D surface field extraction reads this file.
    pub surface_source: bool,
    /// The 3D volume / isobaric-plane extraction reads this file.
    pub pressure_source: bool,
    /// `.idx` substring patterns selecting only the GRIB messages this file's
    /// roles need. Empty = fetch the whole file (HRRR/GFS — preserving their
    /// historical byte-identical whole-file fetch). Non-empty = the fetch path
    /// passes these as `FetchRequest.variable_patterns`. Registered AWS,
    /// Google, and ECMWF sources use their `.idx`/`.index` sidecars for ranged
    /// GETs of the matched messages (cache keyed by the pattern set — see
    /// `rustwx_io::fetch_bytes_with_cache`); NOMADS currently treats the same
    /// patterns as pinned inventory evidence but downloads the whole file.
    /// RRFS-A's NA files are 4.3 GB
    /// (`prs-na`) + 9.1 GB (`nat-na`), so subsetting is mandatory: the surface
    /// subset is ~1.8% of the file, the pressure subset ~86% (the isobaric
    /// volumes are inherently most of the pressure file).
    pub idx_patterns: &'static [&'static str],
}

/// Whether this build has a complete fetch/extract plan for a model.
///
/// This is intentionally an ingest-only status rather than a query or HTTP
/// contract. Service layers can expose it without duplicating the fetch-plan
/// truth, while retaining freedom to define their own public response schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestSupportStatus {
    Ready,
    Unsupported,
}

/// Evidence level for an ingest adapter, independent of whether its fetch
/// plan is enabled. This is the machine-readable source of truth for service
/// capability responses; consumers must not infer verification from model
/// names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestVerificationLevel {
    /// A real provider payload has completed the download -> ingest -> store
    /// validation path for this adapter.
    LiveVerified,
    /// URL/cadence and captured provider-inventory fixtures are verified, but
    /// a full real-payload store round trip is not yet part of this evidence.
    FixtureVerified,
    /// The adapter is implemented and test-covered without a pinned live
    /// provider inventory/round trip at the current contract level.
    ImplementedUnverified,
    /// No complete ingest adapter exists.
    Unsupported,
}

impl IngestVerificationLevel {
    /// Stable wire/config spelling. Prefer this over formatting the debug
    /// representation in service adapters.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LiveVerified => "live_verified",
            Self::FixtureVerified => "fixture_verified",
            Self::ImplementedUnverified => "implemented_unverified",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Honest restrictions on an otherwise ready ingest lane. Callers can use
/// these typed values to constrain controls without maintaining a second
/// model-name allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestCapabilityLimitation {
    /// The model publishes analyses, not positive forecast leads.
    AnalysisOnly,
    /// The product has no pressure-level/isobaric payload.
    SurfaceOnly,
    /// Only the post-processed ensemble mean is ingested, not individual
    /// members, spread, probability, PMM/LPMM, or other statistics.
    EnsembleMeanOnly,
    /// The fetch plan ingests the unperturbed/control member only. The
    /// upstream ensemble also publishes perturbed members and statistics,
    /// but they are not represented by this RWS lane.
    EnsembleControlMemberOnly,
    /// The post-processed product publishes only a documented subset of
    /// pressure levels; manifests report the levels actually realized.
    SparsePressureLevels,
    /// Derived/heavy diagnostics are disabled because the published source
    /// either has incompatible aggregation semantics or omits a required
    /// native input.
    DerivedProductsDisabled,
    /// This fetch plan is pinned to the CONUS product/domain.
    ConusOnly,
    /// The upstream provider labels this feed preliminary/pre-operational.
    PreOperationalFeed,
}

impl IngestCapabilityLimitation {
    /// Stable wire/config spelling. Prefer this over formatting the debug
    /// representation in service adapters.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnalysisOnly => "analysis_only",
            Self::SurfaceOnly => "surface_only",
            Self::EnsembleMeanOnly => "ensemble_mean_only",
            Self::EnsembleControlMemberOnly => "ensemble_control_member_only",
            Self::SparsePressureLevels => "sparse_pressure_levels",
            Self::DerivedProductsDisabled => "derived_products_disabled",
            Self::ConusOnly => "conus_only",
            Self::PreOperationalFeed => "pre_operational_feed",
        }
    }
}

/// Typed, brand-neutral description of a model's ingest availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelIngestCapability {
    pub model: rustwx_core::ModelId,
    pub status: IngestSupportStatus,
    pub verification: IngestVerificationLevel,
    /// Empty when [`status`](Self::status) is [`IngestSupportStatus::Unsupported`].
    pub products: Vec<ProductFetch>,
    /// Empty for an unrestricted full-profile ingest lane.
    pub limitations: Vec<IngestCapabilityLimitation>,
}

/// The per-model fetch plan: which product file(s) one hour downloads and
/// which extraction roles each serves. HRRR keeps its historical two-file
/// pair (pressure = `prs`, surface = `sfc`) in that exact order so its
/// fetch URLs and extraction sequence stay byte-identical. RRFS Public also
/// uses a two-file pair, but both files are already on the same CONUS grid. A
/// one-file plan may serve both roles (GFS and the supported ensemble-mean
/// products) or only the surface role (RTMA/URMA/HIRESW).
///
/// Models that are not ingest-supported (see [`ingest_supported`]) return an
/// error rather than a plan — callers gate on `ingest_supported` first, so
/// this is a defensive guard, not the primary check.
pub fn fetch_plan(model: rustwx_core::ModelId) -> Result<Vec<ProductFetch>, IngestError> {
    use rustwx_core::ModelId;
    match model {
        ModelId::Hrrr | ModelId::HrrrAk => Ok(vec![
            ProductFetch {
                product: "prs",
                surface_source: false,
                pressure_source: true,
                idx_patterns: &[],
            },
            ProductFetch {
                product: "sfc",
                surface_source: true,
                pressure_source: false,
                idx_patterns: &[],
            },
        ]),
        ModelId::Gfs | ModelId::Gdas => Ok(vec![ProductFetch {
            product: "pgrb2.0p25",
            surface_source: true,
            pressure_source: true,
            idx_patterns: &[],
        }]),
        // ECCC publishes GDPS as one complete GRIB2 object per field/level.
        // These are logical extraction families: fetch_hour expands each into
        // an exact, profile-dependent ordered component bundle and caches both
        // the component objects and assembled message stream.
        ModelId::Gdps => Ok(vec![
            ProductFetch {
                product: "rws-pressure",
                surface_source: false,
                pressure_source: true,
                idx_patterns: &[],
            },
            ProductFetch {
                product: "rws-surface",
                surface_source: true,
                pressure_source: false,
                idx_patterns: &[],
            },
        ]),
        ModelId::Gefs => Ok(vec![ProductFetch {
            product: "pgrb2ap5/gec00",
            surface_source: true,
            pressure_source: true,
            idx_patterns: GEFS_CONTROL_IDX_PATTERNS,
        }]),
        ModelId::Aigfs => Ok(vec![
            ProductFetch {
                product: "pres",
                surface_source: false,
                pressure_source: true,
                idx_patterns: NOAA_AI_PRESSURE_IDX_PATTERNS,
            },
            ProductFetch {
                product: "sfc",
                surface_source: true,
                pressure_source: false,
                idx_patterns: NOAA_AI_SURFACE_IDX_PATTERNS,
            },
        ]),
        ModelId::Aigefs => Ok(vec![
            ProductFetch {
                product: "pres/avg",
                surface_source: false,
                pressure_source: true,
                idx_patterns: NOAA_AI_PRESSURE_IDX_PATTERNS,
            },
            ProductFetch {
                product: "sfc/avg",
                surface_source: true,
                pressure_source: false,
                idx_patterns: NOAA_AI_SURFACE_IDX_PATTERNS,
            },
        ]),
        ModelId::Hgefs => Ok(vec![
            ProductFetch {
                product: "pres/avg",
                surface_source: false,
                pressure_source: true,
                idx_patterns: NOAA_AI_PRESSURE_IDX_PATTERNS,
            },
            ProductFetch {
                product: "sfc/avg",
                surface_source: true,
                pressure_source: false,
                idx_patterns: NOAA_AI_SURFACE_IDX_PATTERNS,
            },
        ]),
        ModelId::EcmwfOpenData => Ok(vec![ProductFetch {
            product: "oper",
            surface_source: true,
            pressure_source: true,
            idx_patterns: IFS_OPER_IDX_PATTERNS,
        }]),
        // AIFS Single v2 publishes one atmospheric `oper` GRIB2 file per
        // six-hour lead. ECMWF's line-delimited JSON `.index` provides exact
        // byte ranges, so fetch only the surface and pressure parameters the
        // store can normalize instead of downloading the ~86 MB whole file.
        ModelId::Aifs => Ok(vec![ProductFetch {
            product: "oper",
            surface_source: true,
            pressure_source: true,
            idx_patterns: AIFS_OPER_IDX_PATTERNS,
        }]),
        ModelId::Rap => Ok(vec![ProductFetch {
            product: "awp130pgrb",
            surface_source: true,
            pressure_source: true,
            // Keep RAP whole-file for now: the shared gridded fetch path
            // documents old subset attempts missing pressure-level winds.
            idx_patterns: &[],
        }]),
        // HIRESW's operational CONUS ARW 2.5 km product is a rich 2-D
        // surface/native file, not a pressure-volume product. Keep it on the
        // surface role only; the analysis profile stores the fields it can
        // represent without fabricating an isobaric source.
        ModelId::Hiresw => Ok(vec![ProductFetch {
            product: "arw_2p5km/conus",
            surface_source: true,
            pressure_source: false,
            idx_patterns: HIRESW_CONUS_IDX_PATTERNS,
        }]),
        // HREF/SREF/REFS post-processed means are single-file products that
        // carry both 2-D fields and a sparse, explicitly published set of
        // isobaric levels. The same bytes serve both extraction roles. Do not
        // substitute spread/probability/PMM products: their semantics differ.
        ModelId::Href => Ok(vec![ProductFetch {
            product: "ensprod/conus/mean",
            surface_source: true,
            pressure_source: true,
            idx_patterns: HREF_MEAN_IDX_PATTERNS,
        }]),
        ModelId::Sref => Ok(vec![ProductFetch {
            product: "ensprod/pgrb212/mean_3hrly",
            surface_source: true,
            pressure_source: true,
            idx_patterns: SREF_MEAN_IDX_PATTERNS,
        }]),
        ModelId::Refs => Ok(vec![ProductFetch {
            product: "mean-conus",
            surface_source: true,
            pressure_source: true,
            idx_patterns: REFS_MEAN_IDX_PATTERNS,
        }]),
        // RTMA/URMA CONUS analyses are surface-only. Their dedicated NOAA
        // public S3 buckets carry `.idx` sidecars, so this plan range-fetches
        // only the fields represented by `IngestProfile::analysis()` and
        // never aliases the surface bytes into a pressure role.
        ModelId::Rtma | ModelId::Urma => Ok(vec![ProductFetch {
            product: "2dvaranl_ndfd",
            surface_source: true,
            pressure_source: false,
            idx_patterns: SURFACE_ANALYSIS_IDX_PATTERNS,
        }]),
        // NBM core CONUS blend: one 2.5 km surface file per native forecast
        // step. The files are ~160-200 MB and
        // mostly deterministic fields interleaved with probabilistic
        // companions, so range-subset the exact fields the store can decode.
        ModelId::Nbm => Ok(vec![ProductFetch {
            product: "core/co",
            surface_source: true,
            pressure_source: false,
            idx_patterns: NBM_CORE_IDX_PATTERNS,
        }]),
        ModelId::Nam => Ok(vec![ProductFetch {
            product: "awip3d",
            surface_source: true,
            pressure_source: true,
            idx_patterns: &[],
        }]),
        // RRFS-A: the only files carrying RRFS surface fields are the NA pair
        // (recon-verified — `prslev.conus` is pressure-only, `natlev.conus`
        // 404s). Both are the SAME rotated-pole grid (GRIB template 1,
        // 4881x2961); they are cropped to a CONUS box at ingest (see
        // `model_crop_box`). The files are huge (prs-na 4.3 GB, nat-na 9.1 GB),
        // so each carries `.idx` patterns to subset-fetch only the messages its
        // role needs.
        ModelId::RrfsA => Ok(vec![
            ProductFetch {
                product: "prs-na",
                surface_source: false,
                pressure_source: true,
                idx_patterns: RRFS_PRS_IDX_PATTERNS,
            },
            ProductFetch {
                product: "nat-na",
                surface_source: true,
                pressure_source: false,
                idx_patterns: RRFS_NAT_IDX_PATTERNS,
            },
        ]),
        // RRFS Public's preliminary 3 km CONUS feed publishes a conventional
        // pressure-level file plus a separate 2-D file on the same 1799x1059
        // grid. The inventories use the same field/level spellings as the
        // RRFS-A subset selectors, including honest trailing one-hour APCP,
        // UH, and wind maxima. Keep this adapter distinct from RRFS-A: its
        // provider prefix, grid, cycle cadence, and operational status differ.
        ModelId::RrfsPublic => Ok(vec![
            ProductFetch {
                product: "prs-conus",
                surface_source: false,
                pressure_source: true,
                idx_patterns: RRFS_PRS_IDX_PATTERNS,
            },
            ProductFetch {
                product: "2dfld-conus",
                surface_source: true,
                pressure_source: false,
                idx_patterns: RRFS_NAT_IDX_PATTERNS,
            },
        ]),
        other => Err(events::other(format!(
            "model '{other}' has no ingest fetch plan (not ingest-supported)"
        ))),
    }
}

/// GEFS control-member fields normalized by the current RWS plan. Bare
/// pressure-variable tokens intentionally select every published isobaric
/// level; level realization remains a decode/store concern.
const GEFS_CONTROL_IDX_PATTERNS: &[&str] = &[
    "HGT", "TMP", "RH", "UGRD", "VGRD", "PRES", "PRMSL", "APCP", "PWAT", "TCDC", "CRAIN", "CSNOW",
    "CICEP", "CFRZR",
];

/// Shared pressure inventory for NOAA's operational AI model products.
/// Moisture is specific humidity (`SPFH`), converted to canonical dewpoint by
/// rustwx-io before the RWS volume is written.
const NOAA_AI_PRESSURE_IDX_PATTERNS: &[&str] = &["HGT", "TMP", "SPFH", "UGRD", "VGRD"];

/// Shared surface inventory for NOAA AIGFS/AIGEFS/HGEFS. These products do
/// not publish 2-m dewpoint or static surface height, so capability metadata
/// keeps terrain-dependent derived products disabled.
const NOAA_AI_SURFACE_IDX_PATTERNS: &[&str] = &[
    "TMP:2 m above ground",
    "UGRD:10 m above ground",
    "VGRD:10 m above ground",
    "PRMSL:mean sea level",
    "APCP:surface",
];

/// IFS Open Data `oper` parameters selected from ECMWF's JSON `.index`.
/// Pressure moisture is `q`; the I/O layer normalizes it to dewpoint. The
/// source inventory is pinned under `tests/fixtures` and the explicit byte
/// offsets allow bounded range acquisition.
const IFS_OPER_IDX_PATTERNS: &[&str] = &[
    "param=2t",
    "param=2d",
    "param=10u",
    "param=10v",
    "param=10fg",
    "param=msl",
    "param=sp",
    "param=tp",
    "param=tcc",
    "param=tcwv",
    "param=t",
    "param=q",
    "param=u",
    "param=v",
    "param=gh",
];

/// ECMWF AIFS Single v2 `oper` parameter selection.
///
/// These exact `param=...` predicates are interpreted against ECMWF's
/// newline-delimited JSON index by rustwx-io. The inventory is pinned by the
/// focused provider fixture under `tests/fixtures`. AIFS pressure moisture is
/// specific humidity (`q`); rustwx-io converts it to the canonical dewpoint
/// volume, while the derived decoder consumes it directly.
const AIFS_OPER_IDX_PATTERNS: &[&str] = &[
    "param=2t",
    "param=2d",
    "param=10u",
    "param=10v",
    "param=msl",
    "param=sp",
    "param=tp",
    "param=tcc",
    "param=lcc",
    "param=mcc",
    "param=hcc",
    "param=t",
    "param=q",
    "param=u",
    "param=v",
    "param=gh",
];

/// `.idx` message selection for the RTMA/URMA CONUS
/// `2dvaranl_ndfd` surface analysis.
///
/// Field and level strings are pinned to NOAA's current operational index
/// inventory (verified 2026-08-10). SPFH, WDIR/WIND, CEIL, and URMA's HTSGW
/// are intentionally omitted because the store already receives U/V winds
/// and has no matching surface-plan selector for the other records.
const SURFACE_ANALYSIS_IDX_PATTERNS: &[&str] = &[
    "HGT:surface",
    "PRES:surface",
    "TMP:2 m above ground",
    "DPT:2 m above ground",
    "UGRD:10 m above ground",
    "VGRD:10 m above ground",
    "GUST:10 m above ground",
    "VIS:surface",
    "TCDC:entire atmosphere",
];

/// Operational HIRESW CONUS ARW 2.5 km surface/native inventory.
///
/// Captured from NOAA NOMADS `hiresw.t00z.arw_2p5km.f24.conus.grib2.idx`
/// on 2026-08-11. This intentionally contains no isobaric selectors: the
/// file publishes no pressure-level volume. Extra native surface fields are
/// retained so explicitly named surface profiles can use them later without
/// changing the acquisition contract.
const HIRESW_CONUS_IDX_PATTERNS: &[&str] = &[
    "MSLET",
    "RH:2 m above ground",
    "PWAT:entire atmosphere",
    "APCP:surface",
    "REFD:1000 m above ground",
    "MXUPHL:5000-2000 m above ground",
    "TCDC:entire atmosphere",
    "TMP:2 m above ground",
    "DPT:2 m above ground",
    "UGRD:10 m above ground",
    "VGRD:10 m above ground",
    "PRES:surface",
    "HGT:surface",
    "GUST:surface",
    "CRAIN:surface",
    "REFC:entire atmosphere",
    "VIS:surface",
];

/// HREF CONUS weighted-ensemble-mean messages used by the sparse sounding
/// and 2-D store lanes. The source file itself contains only mean records.
const HREF_MEAN_IDX_PATTERNS: &[&str] = &[
    "MSLET", "HGT", "UGRD", "VGRD", "TMP", "DPT", "RH", "PWAT", "VIS", "LCDC", "MCDC", "HCDC",
    "TCDC", "APCP", "CRAIN", "CFRZR", "CICEP", "CSNOW",
];

/// SREF grid-212 three-hourly weighted-ensemble-mean messages. The upstream
/// file is one run-wide GRIB containing every native forecast step, so a warm
/// fetch-cache entry is reused while forecast-hour-aware extraction selects
/// one valid time.
const SREF_MEAN_IDX_PATTERNS: &[&str] = &[
    "PRMSL", "HGT", "UGRD", "VGRD", "ABSV", "TMP", "DPT", "RH", "CAPE", "CIN", "PWAT", "VIS",
    "APCP", "CRAIN", "CFRZR", "CICEP", "CSNOW",
];

/// REFS CONUS post-processed weighted-ensemble-mean messages. Individual
/// members and alternate statistics deliberately remain outside this plan.
const REFS_MEAN_IDX_PATTERNS: &[&str] = &[
    "MSLET", "HGT", "UGRD", "VGRD", "TMP", "DPT", "RH", "PWAT", "VIS", "LCDC", "MCDC", "HCDC",
    "TCDC", "APCP", "CRAIN", "CFRZR", "CICEP", "CSNOW",
];

/// `.idx` message selection for NBM `core/co`.
///
/// NBM publishes wind speed/direction rather than U/V components; the I/O
/// layer synthesizes `u_10m`/`v_10m`, so both WIND and WDIR are required.
/// The deterministic directive drops percentile, probability, and spread
/// records sharing the same variable/level tokens.
const NBM_CORE_IDX_PATTERNS: &[&str] = &[
    rustwx_io::IDX_DETERMINISTIC_ONLY,
    "TMP:2 m above ground",
    "DPT:2 m above ground",
    "RH:2 m above ground",
    "GUST:10 m above ground",
    "WIND:10 m above ground",
    "WDIR:10 m above ground",
    "APCP:surface",
    "PWAT:entire atmosphere",
    "VIS:surface",
];

/// `.idx` message-selection patterns for the RRFS-A `prs-na` (pressure) file:
/// the isobaric volume field types the ingest plan decodes (T/RH/DPT for the
/// dewpoint→rh fallback, U/V, geopotential height) plus absolute vorticity
/// (the sparse per-level planes). Each is a bare `VARIABLE` token (no level
/// filter) so it matches every isobaric level of that field via
/// [`wx_core::download::find_entries`]; the level subset the profile stores is
/// realized at decode.
///
/// **Pattern format (load-bearing):** `find_entries` parses a pattern as
/// `VARIABLE[:level-substring]`, matching the GRIB variable name EXACTLY and the
/// level as a substring — it does NOT wrap the token in `.idx` field colons.
/// A `:VAR:` form would split at the leading colon into an empty variable name
/// and match nothing (the fetcher would then silently fall back to a whole-file
/// GET — a ~4 GB regression). So these are bare `TMP`, `RH`, … exactly as every
/// other model's `idx_patterns` are written.
///
/// Over-fetches the few stratospheric levels above the stored set (substring
/// patterns can't express "≥100 mb"), so the realized subset is ~69% of the
/// file (measured against the live f001 `.idx`, 2026-06-11) — the isobaric
/// volumes ARE most of the pressure file.
const RRFS_PRS_IDX_PATTERNS: &[&str] = &["TMP", "RH", "DPT", "UGRD", "VGRD", "HGT", "ABSV"];

/// `.idx` message-selection patterns for the RRFS-A `nat-na` (surface) file: the
/// 2D surface set the ingest plan extracts plus the trailing 1 h window messages
/// (APCP 0-1 h acc, MXUPHL 2-5 km 0-1 h max, WIND 10 m 0-1 h max). `MSLET`
/// (mean sea level) covers the `mslp` selector (`PARAMETER_MSLP` matches it).
///
/// Same `VARIABLE[:level-substring]` format as [`RRFS_PRS_IDX_PATTERNS`] — bare
/// variable names and `VAR:level` tokens, never `:VAR:level:` (see that doc for
/// why colon-wrapping silently disables subsetting). Level-qualified entries
/// pin the height/surface variant (e.g. `TMP:2 m above ground`); bare entries
/// (`REFC`, `MSLET`, `PWAT`, the categorical precip flags) match the single
/// message of that variable.
///
/// `CAPE` (bare) pulls the native CAPE planes (surface / 90-0 mb ML /
/// 255-0 mb MU, plus a harmless 180-0 mb layer) that the heavy native-ECAPE
/// ratio recipes consume; `TCDC:entire atmosphere` is deliberately
/// level-qualified — bare `TCDC` would drag ~60 per-hybrid-level cloud planes
/// (hundreds of MB) instead of the 2 entire-atmosphere messages. LCDC/MCDC/HCDC
/// exist in `natlev.na` only at their cloud-layer levels (live-idx verified
/// 2026-06-11 — the original recon's "only TCDC" claim was wrong), so bare
/// tokens are exact.
///
/// Tiny: ~2.6% of the 9.2 GB file (measured against the live f001 `.idx`:
/// 33 messages, ~226 MB).
const RRFS_NAT_IDX_PATTERNS: &[&str] = &[
    "TMP:2 m above ground",
    "DPT:2 m above ground",
    "RH:2 m above ground",
    "SPFH:2 m above ground",
    "UGRD:10 m above ground",
    "VGRD:10 m above ground",
    "REFC",
    "MSLET",
    "PRES:surface",
    "HGT:surface",
    "GUST:surface",
    "PWAT",
    "APCP:surface",
    "CRAIN",
    "CSNOW",
    "CICEP",
    "CFRZR",
    "VIS:surface",
    "MXUPHL:5000-2000 m above ground",
    "MXUPHL:3000-0 m above ground",
    "WIND:10 m above ground",
    "MAXUW:10 m above ground",
    "MAXVW:10 m above ground",
    "CAPE",
    "TCDC:entire atmosphere",
    "LCDC",
    "MCDC",
    "HCDC",
];

/// Validate that a profile's required extraction roles exist in a model's
/// fetch plan. This is the model-aware counterpart to
/// [`ingest_profile::IngestProfile::validate`], which validates only the
/// profile's internal shape.
pub fn validate_ingest_profile_for_model(
    model: rustwx_core::ModelId,
    profile: &ingest_profile::IngestProfile,
) -> Result<(), IngestError> {
    profile.validate().map_err(events::other)?;
    let plan = fetch_plan(model)?;
    if !plan.iter().any(|product| product.surface_source) {
        return Err(events::other(format!(
            "model '{model}' has no surface-source product in its ingest plan"
        )));
    }
    if profile.needs_prs() && !plan.iter().any(|product| product.pressure_source) {
        return Err(events::other(format!(
            "model '{model}' is surface-only, but profile '{}' requests pressure-level or derived inputs; use --profile surface for forecast fields or --profile analysis for analysis products",
            profile.describe()
        )));
    }
    if matches!(
        model,
        rustwx_core::ModelId::Aigefs
            | rustwx_core::ModelId::Hgefs
            | rustwx_core::ModelId::Href
            | rustwx_core::ModelId::Sref
            | rustwx_core::ModelId::Refs
    ) && (profile.derived || profile.heavy)
    {
        return Err(events::other(format!(
            "model '{model}' ingests a post-processed ensemble mean; derived/heavy diagnostics from mean state fields are not ensemble-mean diagnostics; use --profile sounding or disable both derived and heavy",
        )));
    }
    if matches!(
        model,
        rustwx_core::ModelId::Gefs
            | rustwx_core::ModelId::Aigfs
            | rustwx_core::ModelId::EcmwfOpenData
            | rustwx_core::ModelId::Aifs
    ) && (profile.derived || profile.heavy)
    {
        return Err(events::other(format!(
            "model '{model}' public ingest products do not publish native surface orography in each forecast file; derived/heavy diagnostics remain disabled until a verified static-field join exists; use --profile sounding or disable both derived and heavy",
        )));
    }
    Ok(())
}

/// The geographic CONUS crop box for a model whose native ingest domain is
/// larger than CONUS (RRFS-A's North America rotated-pole grid), as
/// `(west, east, south, north)` degrees. `None` = no crop (HRRR, GFS — the
/// native grid is already the store grid).
///
/// RRFS-A: the NA files (4881x2961, GRIB template 1 rotated-pole, unrotated to
/// curvilinear geographic by grib-core) cover all of North America. Cropping to
/// this box at ingest keeps the store HRRR-class (~5.1M cells, ~2.7x HRRR)
/// instead of 14.5M. The box is chosen so RRFS-CONUS coverage ⊇ HRRR-CONUS:
/// HRRR's CONUS Lambert grid spans roughly lat 21.1..52.6, lon -134.1..-60.9;
/// these bounds (21.0..53.5, -134.5..-60.5) bound it with a small margin.
/// Realized on the native rotated index grid as a contiguous block
/// (~1736x2931 cells; the rotated grid is skewed so the index block
/// over-covers the geographic rectangle — the true rectangle is fully inside).
/// Determinism: the crop index range is a pure function of the grid's per-cell
/// coordinates (first/last row+col whose geographic point lies in the box),
/// computed once per hour — no per-run floating-point branch.
pub fn model_crop_box(model: rustwx_core::ModelId) -> Option<(f64, f64, f64, f64)> {
    use rustwx_core::ModelId;
    match model {
        ModelId::RrfsA => Some((-134.5, -60.5, 21.0, 53.5)),
        _ => None,
    }
}

/// Whether this crate can ingest `model` today. Backed by [`fetch_plan`]:
/// a model is ingest-supported exactly when a per-model fetch plan exists
/// for it (HRRR's `prs`/`sfc` pair, GFS's single `pgrb2.0p25`). UI pickers
/// gate enablement on this so the list self-updates as plans land.
pub fn ingest_supported(model: rustwx_core::ModelId) -> bool {
    fetch_plan(model).is_ok()
}

/// Whether a product can be range-subset through at least one registered
/// source in the current runtime. Pattern presence alone is insufficient:
/// NOMADS sidecars are inventory evidence, while NOMADS acquisition remains
/// whole-file by design.
pub fn indexed_subset_available(model: rustwx_core::ModelId, product: &ProductFetch) -> bool {
    !product.idx_patterns.is_empty()
        && rustwx_models::model_summary(model)
            .sources
            .iter()
            .any(|source| rustwx_io::source_supports_indexed_subset_fetch(source.id))
}

/// Whether the remote ingest path can acquire `model` from `source`.
///
/// The model registry also describes local archive adapters. Those are useful
/// to other callers but are not interchangeable with the GRIB acquisition path
/// used by the scheduler.
pub fn model_source_ingest_supported(
    model: rustwx_core::ModelId,
    source: rustwx_core::SourceId,
) -> bool {
    ingest_supported(model)
        && rustwx_models::model_summary(model)
            .sources
            .iter()
            .any(|candidate| candidate.id == source)
        && (model != rustwx_core::ModelId::Aifs || source == rustwx_core::SourceId::Ecmwf)
}

/// Return the authoritative ingest capability for one model.
///
/// Service and query layers should consume this seam instead of maintaining a
/// second hard-coded support list. Forecast-hour cadence remains authoritative
/// in `rustwx_models::supported_forecast_hours` because it is cycle-dependent.
pub fn model_ingest_capability(model: rustwx_core::ModelId) -> ModelIngestCapability {
    let limitations = match model {
        rustwx_core::ModelId::Rtma | rustwx_core::ModelId::Urma => vec![
            IngestCapabilityLimitation::AnalysisOnly,
            IngestCapabilityLimitation::SurfaceOnly,
        ],
        rustwx_core::ModelId::Hiresw => vec![
            IngestCapabilityLimitation::SurfaceOnly,
            IngestCapabilityLimitation::ConusOnly,
        ],
        rustwx_core::ModelId::Nbm => vec![
            IngestCapabilityLimitation::SurfaceOnly,
            IngestCapabilityLimitation::ConusOnly,
        ],
        rustwx_core::ModelId::Gefs => vec![
            IngestCapabilityLimitation::EnsembleControlMemberOnly,
            IngestCapabilityLimitation::SparsePressureLevels,
            IngestCapabilityLimitation::DerivedProductsDisabled,
        ],
        rustwx_core::ModelId::Aigfs | rustwx_core::ModelId::EcmwfOpenData => vec![
            IngestCapabilityLimitation::SparsePressureLevels,
            IngestCapabilityLimitation::DerivedProductsDisabled,
        ],
        rustwx_core::ModelId::Aigefs | rustwx_core::ModelId::Hgefs => vec![
            IngestCapabilityLimitation::EnsembleMeanOnly,
            IngestCapabilityLimitation::SparsePressureLevels,
            IngestCapabilityLimitation::DerivedProductsDisabled,
        ],
        rustwx_core::ModelId::Href | rustwx_core::ModelId::Sref => vec![
            IngestCapabilityLimitation::EnsembleMeanOnly,
            IngestCapabilityLimitation::SparsePressureLevels,
            IngestCapabilityLimitation::DerivedProductsDisabled,
            IngestCapabilityLimitation::ConusOnly,
        ],
        rustwx_core::ModelId::Refs => vec![
            IngestCapabilityLimitation::EnsembleMeanOnly,
            IngestCapabilityLimitation::SparsePressureLevels,
            IngestCapabilityLimitation::DerivedProductsDisabled,
            IngestCapabilityLimitation::ConusOnly,
            IngestCapabilityLimitation::PreOperationalFeed,
        ],
        rustwx_core::ModelId::RrfsPublic => vec![
            IngestCapabilityLimitation::ConusOnly,
            IngestCapabilityLimitation::PreOperationalFeed,
        ],
        rustwx_core::ModelId::Aifs => vec![
            IngestCapabilityLimitation::SparsePressureLevels,
            IngestCapabilityLimitation::DerivedProductsDisabled,
        ],
        rustwx_core::ModelId::Gdps => {
            vec![IngestCapabilityLimitation::SparsePressureLevels]
        }
        _ => Vec::new(),
    };
    match fetch_plan(model) {
        Ok(products) => {
            let verification = match model {
                rustwx_core::ModelId::Hrrr
                | rustwx_core::ModelId::Gfs
                | rustwx_core::ModelId::Gdps
                | rustwx_core::ModelId::Rap
                | rustwx_core::ModelId::Nam
                | rustwx_core::ModelId::RrfsA
                | rustwx_core::ModelId::Gefs
                | rustwx_core::ModelId::Aigfs
                | rustwx_core::ModelId::Aigefs
                | rustwx_core::ModelId::Hgefs
                | rustwx_core::ModelId::EcmwfOpenData => IngestVerificationLevel::LiveVerified,
                rustwx_core::ModelId::HrrrAk
                | rustwx_core::ModelId::Gdas
                | rustwx_core::ModelId::Nbm
                | rustwx_core::ModelId::Rtma
                | rustwx_core::ModelId::Urma
                | rustwx_core::ModelId::Hiresw
                | rustwx_core::ModelId::Href
                | rustwx_core::ModelId::Sref
                | rustwx_core::ModelId::Refs
                | rustwx_core::ModelId::RrfsPublic
                | rustwx_core::ModelId::Aifs => IngestVerificationLevel::FixtureVerified,
                _ => IngestVerificationLevel::ImplementedUnverified,
            };
            ModelIngestCapability {
                model,
                status: IngestSupportStatus::Ready,
                verification,
                products,
                limitations,
            }
        }
        Err(_) => ModelIngestCapability {
            model,
            status: IngestSupportStatus::Unsupported,
            verification: IngestVerificationLevel::Unsupported,
            products: Vec::new(),
            limitations,
        },
    }
}

/// Return one capability row for every built-in model, in registry order.
///
/// This is the complete ingest support table for service/UI consumers. It
/// includes unsupported rows deliberately, so callers can explain a gated
/// model without maintaining a separate allowlist.
pub fn model_ingest_capabilities() -> Vec<ModelIngestCapability> {
    rustwx_models::built_in_models()
        .iter()
        .map(|summary| model_ingest_capability(summary.id))
        .collect()
}

/// Crate-local profiling scope: expands to `puffin::profile_scope!` under
/// the `profiling` feature and to nothing otherwise, so call sites stay
/// clean and headless bins compile puffin out entirely.
#[cfg(feature = "profiling")]
macro_rules! profile_scope {
    ($($arg:tt)*) => {
        puffin::profile_scope!($($arg)*);
    };
}
#[cfg(not(feature = "profiling"))]
macro_rules! profile_scope {
    ($($arg:tt)*) => {};
}
pub(crate) use profile_scope;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sha_is_stamped() {
        assert!(!build_sha().is_empty());
    }

    #[test]
    fn ingest_verification_level_has_stable_wire_names() {
        assert_eq!(
            IngestVerificationLevel::LiveVerified.as_str(),
            "live_verified"
        );
        assert_eq!(
            IngestVerificationLevel::FixtureVerified.as_str(),
            "fixture_verified"
        );
        assert_eq!(
            IngestVerificationLevel::ImplementedUnverified.as_str(),
            "implemented_unverified"
        );
        assert_eq!(IngestVerificationLevel::Unsupported.as_str(), "unsupported");
        assert_eq!(
            IngestCapabilityLimitation::EnsembleControlMemberOnly.as_str(),
            "ensemble_control_member_only"
        );
    }

    #[test]
    fn fetch_plan_models_are_ingest_supported() {
        use rustwx_core::ModelId;
        let enabled = [
            ModelId::Hrrr,
            ModelId::HrrrAk,
            ModelId::Rap,
            ModelId::Gfs,
            ModelId::Gdps,
            ModelId::Gdas,
            ModelId::Gefs,
            ModelId::Aigfs,
            ModelId::Aigefs,
            ModelId::Hgefs,
            ModelId::EcmwfOpenData,
            ModelId::Aifs,
            ModelId::Nam,
            ModelId::Hiresw,
            ModelId::Href,
            ModelId::Sref,
            ModelId::RrfsA,
            ModelId::RrfsPublic,
            ModelId::Refs,
            ModelId::Nbm,
            ModelId::Rtma,
            ModelId::Urma,
        ];
        for model in enabled {
            assert!(
                ingest_supported(model),
                "{model} should be ingest-supported"
            );
        }
        // Every other user-facing model stays gated until its fetch plan lands.
        for model in rustwx_models::supported_models() {
            if !enabled.contains(&model) {
                assert!(
                    !ingest_supported(model),
                    "{model} must stay gated until its fetch plan exists"
                );
            }
        }
    }

    #[test]
    fn model_ingest_capability_reports_ready_and_unsupported_models() {
        use rustwx_core::ModelId;

        let nbm = model_ingest_capability(ModelId::Nbm);
        assert_eq!(nbm.model, ModelId::Nbm);
        assert_eq!(nbm.status, IngestSupportStatus::Ready);
        assert_eq!(nbm.verification, IngestVerificationLevel::FixtureVerified);
        assert_eq!(nbm.products.len(), 1);
        assert_eq!(
            nbm.limitations,
            vec![
                IngestCapabilityLimitation::SurfaceOnly,
                IngestCapabilityLimitation::ConusOnly,
            ]
        );

        for model in [ModelId::Aigefs, ModelId::Hgefs] {
            let capability = model_ingest_capability(model);
            assert_eq!(
                capability.verification,
                IngestVerificationLevel::LiveVerified
            );
            assert!(
                capability
                    .limitations
                    .contains(&IngestCapabilityLimitation::EnsembleMeanOnly)
            );
            assert!(
                capability
                    .limitations
                    .contains(&IngestCapabilityLimitation::SparsePressureLevels)
            );
            assert!(
                capability
                    .limitations
                    .contains(&IngestCapabilityLimitation::DerivedProductsDisabled)
            );
        }

        let gefs = model_ingest_capability(ModelId::Gefs);
        assert_eq!(gefs.verification, IngestVerificationLevel::LiveVerified);
        assert_eq!(
            gefs.limitations,
            vec![
                IngestCapabilityLimitation::EnsembleControlMemberOnly,
                IngestCapabilityLimitation::SparsePressureLevels,
                IngestCapabilityLimitation::DerivedProductsDisabled,
            ]
        );

        for model in [ModelId::Aigfs, ModelId::EcmwfOpenData] {
            let capability = model_ingest_capability(model);
            assert_eq!(
                capability.verification,
                IngestVerificationLevel::LiveVerified
            );
            assert_eq!(
                capability.limitations,
                vec![
                    IngestCapabilityLimitation::SparsePressureLevels,
                    IngestCapabilityLimitation::DerivedProductsDisabled,
                ]
            );
        }

        let rtma = model_ingest_capability(ModelId::Rtma);
        assert_eq!(rtma.model, ModelId::Rtma);
        assert_eq!(rtma.status, IngestSupportStatus::Ready);
        assert_eq!(rtma.verification, IngestVerificationLevel::FixtureVerified);
        assert_eq!(rtma.products.len(), 1);
        assert_eq!(
            rtma.limitations,
            vec![
                IngestCapabilityLimitation::AnalysisOnly,
                IngestCapabilityLimitation::SurfaceOnly,
            ]
        );

        let hiresw = model_ingest_capability(ModelId::Hiresw);
        assert_eq!(hiresw.status, IngestSupportStatus::Ready);
        assert_eq!(
            hiresw.limitations,
            vec![
                IngestCapabilityLimitation::SurfaceOnly,
                IngestCapabilityLimitation::ConusOnly,
            ]
        );

        let href = model_ingest_capability(ModelId::Href);
        assert_eq!(href.status, IngestSupportStatus::Ready);
        assert_eq!(href.verification, IngestVerificationLevel::FixtureVerified);
        assert!(
            href.limitations
                .contains(&IngestCapabilityLimitation::EnsembleMeanOnly)
        );
        assert!(
            href.limitations
                .contains(&IngestCapabilityLimitation::SparsePressureLevels)
        );
        assert!(
            href.limitations
                .contains(&IngestCapabilityLimitation::DerivedProductsDisabled)
        );

        let refs = model_ingest_capability(ModelId::Refs);
        assert_eq!(refs.status, IngestSupportStatus::Ready);
        assert!(
            refs.limitations
                .contains(&IngestCapabilityLimitation::PreOperationalFeed)
        );

        let rrfs_public = model_ingest_capability(ModelId::RrfsPublic);
        assert_eq!(rrfs_public.status, IngestSupportStatus::Ready);
        assert_eq!(
            rrfs_public.verification,
            IngestVerificationLevel::FixtureVerified
        );

        let aifs = model_ingest_capability(ModelId::Aifs);
        assert_eq!(aifs.status, IngestSupportStatus::Ready);
        assert_eq!(aifs.verification, IngestVerificationLevel::FixtureVerified);
        assert_eq!(
            aifs.limitations,
            vec![
                IngestCapabilityLimitation::SparsePressureLevels,
                IngestCapabilityLimitation::DerivedProductsDisabled,
            ]
        );
        assert_eq!(
            rrfs_public.limitations,
            vec![
                IngestCapabilityLimitation::ConusOnly,
                IngestCapabilityLimitation::PreOperationalFeed,
            ]
        );

        for model in [ModelId::Rap, ModelId::Nam] {
            assert_eq!(
                model_ingest_capability(model).verification,
                IngestVerificationLevel::LiveVerified,
                "{model} completed an official-payload ingest and deep store validation"
            );
        }
        for model in [ModelId::HrrrAk, ModelId::Gdas] {
            assert_eq!(
                model_ingest_capability(model).verification,
                IngestVerificationLevel::FixtureVerified,
                "{model} has pinned official URL, cadence, and inventory evidence"
            );
        }

        let unsupported = model_ingest_capability(ModelId::WrfGdex);
        assert_eq!(unsupported.status, IngestSupportStatus::Unsupported);
        assert_eq!(
            unsupported.verification,
            IngestVerificationLevel::Unsupported
        );
        assert!(unsupported.products.is_empty());
        assert!(unsupported.limitations.is_empty());
    }

    #[test]
    fn model_ingest_capability_table_is_complete_and_exact() {
        use rustwx_core::ModelId;

        let table = model_ingest_capabilities();
        assert_eq!(table.len(), rustwx_models::built_in_models().len());
        assert_eq!(
            table.iter().map(|row| row.model).collect::<Vec<_>>(),
            rustwx_models::built_in_models()
                .iter()
                .map(|summary| summary.id)
                .collect::<Vec<_>>()
        );

        let ready = table
            .iter()
            .filter(|row| row.status == IngestSupportStatus::Ready)
            .map(|row| row.model)
            .collect::<Vec<_>>();
        assert_eq!(
            ready,
            vec![
                ModelId::Hrrr,
                ModelId::HrrrAk,
                ModelId::Gfs,
                ModelId::Gdps,
                ModelId::Gdas,
                ModelId::Gefs,
                ModelId::Aigfs,
                ModelId::Aigefs,
                ModelId::Hgefs,
                ModelId::EcmwfOpenData,
                ModelId::Aifs,
                ModelId::Rap,
                ModelId::Nam,
                ModelId::Hiresw,
                ModelId::Href,
                ModelId::Sref,
                ModelId::Rtma,
                ModelId::Urma,
                ModelId::Nbm,
                ModelId::RrfsA,
                ModelId::RrfsPublic,
                ModelId::Refs,
            ]
        );

        let unsupported = table
            .iter()
            .filter(|row| row.status == IngestSupportStatus::Unsupported)
            .map(|row| row.model)
            .collect::<Vec<_>>();
        assert_eq!(unsupported, vec![ModelId::RrfsFireWx, ModelId::WrfGdex]);
        assert!(table.iter().all(|row| {
            (row.status == IngestSupportStatus::Ready && !row.products.is_empty())
                || (row.status == IngestSupportStatus::Unsupported
                    && row.products.is_empty()
                    && row.verification == IngestVerificationLevel::Unsupported)
        }));
    }

    #[test]
    fn whole_file_models_carry_no_idx_patterns() {
        use rustwx_core::ModelId;
        // These plans intentionally keep whole-file fetches (empty patterns).
        for model in [
            ModelId::Hrrr,
            ModelId::HrrrAk,
            ModelId::Rap,
            ModelId::Gfs,
            ModelId::Gdas,
            ModelId::Nam,
        ] {
            for entry in fetch_plan(model).expect("plan") {
                assert!(
                    entry.idx_patterns.is_empty(),
                    "{model} product {} must fetch whole-file (no idx subset)",
                    entry.product
                );
            }
        }
        // Whole-file first-wave models have no crop box.
        assert!(model_crop_box(ModelId::Hrrr).is_none());
        assert!(model_crop_box(ModelId::HrrrAk).is_none());
        assert!(model_crop_box(ModelId::Rap).is_none());
        assert!(model_crop_box(ModelId::Gfs).is_none());
        assert!(model_crop_box(ModelId::Gdas).is_none());
        assert!(model_crop_box(ModelId::Nam).is_none());
    }

    #[test]
    fn global_wave_fetch_plans_carry_verified_index_selectors() {
        use rustwx_core::ModelId;

        for model in [
            ModelId::Gefs,
            ModelId::Aigfs,
            ModelId::Aigefs,
            ModelId::Hgefs,
            ModelId::EcmwfOpenData,
        ] {
            for product in fetch_plan(model).expect("global-wave plan") {
                assert!(
                    !product.idx_patterns.is_empty(),
                    "{model} product {} must carry its pinned selector inventory",
                    product.product
                );
            }
        }

        assert!(indexed_subset_available(
            ModelId::Gefs,
            &fetch_plan(ModelId::Gefs).unwrap()[0]
        ));
        assert!(indexed_subset_available(
            ModelId::EcmwfOpenData,
            &fetch_plan(ModelId::EcmwfOpenData).unwrap()[0]
        ));
        for model in [ModelId::Aigfs, ModelId::Aigefs, ModelId::Hgefs] {
            assert!(
                fetch_plan(model)
                    .unwrap()
                    .iter()
                    .all(|product| !indexed_subset_available(model, product)),
                "{model} is NOMADS-only and must report whole-file acquisition"
            );
        }
    }

    #[test]
    fn fetch_plan_rrfs_a_is_the_na_pair_with_subset_patterns() {
        use rustwx_core::ModelId;
        let plan = fetch_plan(ModelId::RrfsA).expect("RRFS-A plan");
        assert_eq!(plan.len(), 2, "RRFS-A fetches prs-na + nat-na");
        // Pressure role first (the historical pressure-then-surface order).
        assert_eq!(plan[0].product, "prs-na");
        assert!(plan[0].pressure_source && !plan[0].surface_source);
        assert_eq!(plan[1].product, "nat-na");
        assert!(plan[1].surface_source && !plan[1].pressure_source);
        // Both files are huge → both MUST subset-fetch.
        assert!(
            !plan[0].idx_patterns.is_empty() && !plan[1].idx_patterns.is_empty(),
            "RRFS-A NA files (4.3+9.1 GB) must subset-fetch"
        );
        // The surface plan must reach the trailing 1 h window messages and the
        // honest MSLET→mslp message.
        let nat = plan[1].idx_patterns;
        assert!(nat.iter().any(|p| p.contains("APCP:surface")));
        assert!(nat.iter().any(|p| p.contains("MXUPHL:5000-2000 m")));
        assert!(nat.iter().any(|p| p.contains("WIND:10 m above ground")));
        assert!(nat.iter().any(|p| p.contains("MSLET")));
        // The pressure plan must reach the isobaric volume field types.
        let prs = plan[0].idx_patterns;
        for need in ["TMP", "RH", "UGRD", "VGRD", "HGT"] {
            assert!(prs.contains(&need), "prs subset missing {need}");
        }
    }

    #[test]
    fn fetch_plan_nbm_uses_deterministic_indexed_core_conus() {
        use rustwx_core::ModelId;

        let plan = fetch_plan(ModelId::Nbm).expect("NBM plan");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].product, "core/co");
        assert!(plan[0].surface_source);
        assert!(!plan[0].pressure_source);
        assert_eq!(
            plan[0].idx_patterns.first().copied(),
            Some(rustwx_io::IDX_DETERMINISTIC_ONLY)
        );
        assert!(plan[0].idx_patterns.contains(&"WIND:10 m above ground"));
        assert!(plan[0].idx_patterns.contains(&"WDIR:10 m above ground"));
        assert!(plan[0].idx_patterns.contains(&"APCP:surface"));
    }

    #[test]
    fn fetch_plan_rtma_urma_is_surface_only_and_matches_operational_indexes() {
        use rustwx_core::ModelId;

        let cases = [
            (
                ModelId::Rtma,
                include_str!("../tests/fixtures/rtma2p5_2dvaranl_ndfd.idx"),
            ),
            (
                ModelId::Urma,
                include_str!("../tests/fixtures/urma2p5_2dvaranl_ndfd.idx"),
            ),
        ];
        for (model, idx) in cases {
            let plan = fetch_plan(model).expect("analysis fetch plan");
            assert_eq!(plan.len(), 1);
            assert_eq!(plan[0].product, "2dvaranl_ndfd");
            assert!(plan[0].surface_source);
            assert!(!plan[0].pressure_source);
            assert_eq!(plan[0].idx_patterns, SURFACE_ANALYSIS_IDX_PATTERNS);
            for pattern in plan[0].idx_patterns {
                let matched = idx.lines().any(|line| {
                    let parts = line.split(':').collect::<Vec<_>>();
                    parts.len() >= 5 && idx_line_matches(pattern, parts[3], parts[4])
                });
                assert!(matched, "{model} index has no row for {pattern}");
            }
        }
    }

    #[test]
    fn fetch_plan_hiresw_is_surface_only_and_matches_operational_index() {
        use rustwx_core::ModelId;

        let idx = include_str!("../tests/fixtures/hiresw.t00z.arw_2p5km.f24.conus.grib2.idx");
        let plan = fetch_plan(ModelId::Hiresw).expect("HIRESW fetch plan");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].product, "arw_2p5km/conus");
        assert!(plan[0].surface_source);
        assert!(!plan[0].pressure_source);
        assert_eq!(plan[0].idx_patterns, HIRESW_CONUS_IDX_PATTERNS);
        assert_patterns_match_index("HIRESW", plan[0].idx_patterns, idx);
        assert!(
            !idx.lines().any(|line| line.contains(":TMP:500 mb:")),
            "the supported HIRESW file must not be represented as a pressure volume"
        );
    }

    #[test]
    fn ensemble_mean_fetch_plans_match_captured_operational_indexes() {
        use rustwx_core::ModelId;

        let cases = [
            (
                ModelId::Href,
                "ensprod/conus/mean",
                HREF_MEAN_IDX_PATTERNS,
                include_str!("../tests/fixtures/href.t00z.conus.mean.f24.grib2.idx"),
            ),
            (
                ModelId::Sref,
                "ensprod/pgrb212/mean_3hrly",
                SREF_MEAN_IDX_PATTERNS,
                include_str!("../tests/fixtures/sref.t03z.pgrb212.mean_3hrly.excerpt.idx"),
            ),
            (
                ModelId::Refs,
                "mean-conus",
                REFS_MEAN_IDX_PATTERNS,
                include_str!("../tests/fixtures/refs.t00z.mean.f24.conus.grib2.idx"),
            ),
        ];
        for (model, product, patterns, idx) in cases {
            let plan = fetch_plan(model).expect("ensemble-mean fetch plan");
            assert_eq!(plan.len(), 1);
            assert_eq!(plan[0].product, product);
            assert!(plan[0].surface_source && plan[0].pressure_source);
            assert_eq!(plan[0].idx_patterns, patterns);
            assert_patterns_match_index(&model.to_string(), patterns, idx);
            assert!(
                idx.lines().all(|line| line.contains(":wt ens mean")),
                "{model} fixture must contain only weighted-ensemble-mean records"
            );
        }
    }

    #[test]
    fn rrfs_public_fetch_plan_matches_captured_provider_indexes() {
        use rustwx_core::ModelId;

        let pressure =
            include_str!("../tests/fixtures/rrfs.t00z.prslev.3km.f024.conus.grib2.excerpt.idx");
        let surface =
            include_str!("../tests/fixtures/rrfs.t00z.2dfld.3km.f024.conus.grib2.excerpt.idx");
        let plan = fetch_plan(ModelId::RrfsPublic).expect("RRFS Public fetch plan");
        assert_eq!(plan.len(), 2);

        assert_eq!(plan[0].product, "prs-conus");
        assert!(plan[0].pressure_source && !plan[0].surface_source);
        assert_eq!(plan[0].idx_patterns, RRFS_PRS_IDX_PATTERNS);
        assert_patterns_match_index("RRFS Public pressure", plan[0].idx_patterns, pressure);

        assert_eq!(plan[1].product, "2dfld-conus");
        assert!(plan[1].surface_source && !plan[1].pressure_source);
        assert_eq!(plan[1].idx_patterns, RRFS_NAT_IDX_PATTERNS);
        assert_patterns_match_index("RRFS Public surface", plan[1].idx_patterns, surface);

        for evidence in [
            ":APCP:surface:23-24 hour acc fcst:",
            ":MXUPHL:5000-2000 m above ground:23-24 hour max fcst:",
            ":WIND:10 m above ground:23-24 hour max fcst:",
        ] {
            assert!(
                surface.contains(evidence),
                "missing native window: {evidence}"
            );
        }
    }

    fn assert_patterns_match_index(label: &str, patterns: &[&str], idx: &str) {
        for pattern in patterns {
            let matched = idx.lines().any(|line| {
                let parts = line.split(':').collect::<Vec<_>>();
                parts.len() >= 5 && idx_line_matches(pattern, parts[3], parts[4])
            });
            assert!(matched, "{label} index has no row for {pattern}");
        }
    }

    #[test]
    fn surface_only_models_require_the_analysis_profile() {
        use rustwx_core::ModelId;

        for model in [ModelId::Rtma, ModelId::Urma] {
            validate_ingest_profile_for_model(model, &ingest_profile::IngestProfile::analysis())
                .expect("analysis profile is compatible");
            let message =
                validate_ingest_profile_for_model(model, &ingest_profile::IngestProfile::full())
                    .expect_err("full profile requires a pressure product")
                    .to_string();
            assert!(message.contains("surface-only"), "got: {message}");
            assert!(message.contains("--profile analysis"), "got: {message}");
        }

        validate_ingest_profile_for_model(ModelId::Nbm, &ingest_profile::IngestProfile::surface())
            .expect("NBM accepts the complete direct surface profile");
        let message =
            validate_ingest_profile_for_model(ModelId::Nbm, &ingest_profile::IngestProfile::view())
                .expect_err("NBM view requires a pressure product")
                .to_string();
        assert!(message.contains("surface-only"), "got: {message}");

        validate_ingest_profile_for_model(
            ModelId::Hiresw,
            &ingest_profile::IngestProfile::analysis(),
        )
        .expect("HIRESW accepts the surface-only analysis profile");
        let message = validate_ingest_profile_for_model(
            ModelId::Hiresw,
            &ingest_profile::IngestProfile::sounding(),
        )
        .expect_err("HIRESW has no isobaric source")
        .to_string();
        assert!(message.contains("surface-only"), "got: {message}");
    }

    #[test]
    fn ensemble_mean_models_reject_nonlinear_derived_stages() {
        use rustwx_core::ModelId;

        for model in [
            ModelId::Aigefs,
            ModelId::Hgefs,
            ModelId::Href,
            ModelId::Sref,
            ModelId::Refs,
        ] {
            validate_ingest_profile_for_model(model, &ingest_profile::IngestProfile::sounding())
                .expect("sparse ensemble-mean soundings are supported");

            let message =
                validate_ingest_profile_for_model(model, &ingest_profile::IngestProfile::full())
                    .expect_err("derived diagnostics from mean state must be rejected")
                    .to_string();
            assert!(message.contains("post-processed ensemble mean"));
            assert!(message.contains("not ensemble-mean diagnostics"));

            let mut raw_fields = ingest_profile::IngestProfile::full();
            raw_fields.derived = false;
            raw_fields.heavy = false;
            validate_ingest_profile_for_model(model, &raw_fields)
                .expect("raw mean fields and sparse pressure levels are supported");
        }
    }

    #[test]
    fn aifs_accepts_raw_soundings_but_rejects_unverified_terrain_derived_stages() {
        use rustwx_core::ModelId;

        validate_ingest_profile_for_model(
            ModelId::Aifs,
            &ingest_profile::IngestProfile::sounding(),
        )
        .expect("AIFS sparse pressure volumes and surface state are supported");

        let message = validate_ingest_profile_for_model(
            ModelId::Aifs,
            &ingest_profile::IngestProfile::full(),
        )
        .expect_err("AIFS derived/heavy stages need a verified static-orography join")
        .to_string();
        assert!(message.contains("surface orography"), "got: {message}");
        assert!(message.contains("--profile sounding"), "got: {message}");
    }

    #[test]
    fn other_terrain_incomplete_global_models_reject_derived_stages() {
        use rustwx_core::ModelId;

        for model in [ModelId::Gefs, ModelId::Aigfs, ModelId::EcmwfOpenData] {
            validate_ingest_profile_for_model(model, &ingest_profile::IngestProfile::sounding())
                .expect("sparse pressure volumes and surface state are supported");
            let message =
                validate_ingest_profile_for_model(model, &ingest_profile::IngestProfile::full())
                    .expect_err("derived/heavy stages need a verified static-orography join")
                    .to_string();
            assert!(message.contains("surface orography"), "got: {message}");
            assert!(message.contains("--profile sounding"), "got: {message}");
        }
    }

    #[test]
    fn rrfs_a_has_a_conus_crop_box_bounding_hrrr() {
        use rustwx_core::ModelId;
        let (w, e, s, n) = model_crop_box(ModelId::RrfsA).expect("RRFS-A crop box");
        // (west, east, south, north). Must bound HRRR's CONUS coverage with a
        // margin so RRFS-CONUS ⊇ HRRR-CONUS.
        assert!(w <= -134.0 && e >= -61.0, "lon box must bound HRRR conus");
        assert!(s <= 21.1 && n >= 52.6, "lat box must bound HRRR conus");
        assert!(w < e && s < n, "box must be well-ordered");
    }

    /// Replica of `wx_core::download::find_entries`'s match rule (split the
    /// pattern on its FIRST colon into an exact variable name + a level
    /// substring) so this crate can assert its `.idx` patterns actually select
    /// messages WITHOUT taking a dev-dep on wx-core. Kept deliberately tiny and
    /// in lock-step with the real parser; the real parser is itself covered by
    /// a regression test in `vendor/wx-core/src/download/idx.rs`.
    fn idx_line_matches(pattern: &str, variable: &str, level: &str) -> bool {
        let (var_pat, level_pat) = match pattern.find(':') {
            Some(i) => (&pattern[..i], Some(&pattern[i + 1..])),
            None => (pattern, None),
        };
        variable == var_pat && level_pat.is_none_or(|lp| level.contains(lp))
    }

    fn idx_has_row(idx: &str, variable: &str, level: &str) -> bool {
        idx.lines().any(|line| {
            let mut parts = line.split(':');
            matches!(
                (
                    parts.nth(3),
                    parts.next(),
                ),
                (Some(found_variable), Some(found_level))
                    if found_variable == variable && found_level == level
            )
        })
    }

    fn pressure_levels(idx: &str, variable: &str, minimum_hpa: u16) -> Vec<u16> {
        let mut levels = idx
            .lines()
            .filter_map(|line| {
                let parts = line.split(':').collect::<Vec<_>>();
                if parts.get(3).copied() != Some(variable) {
                    return None;
                }
                parts
                    .get(4)?
                    .strip_suffix(" mb")?
                    .parse::<u16>()
                    .ok()
                    .filter(|level| *level >= minimum_hpa && *level <= 1_000)
            })
            .collect::<Vec<_>>();
        levels.sort_unstable();
        levels.dedup();
        levels
    }

    fn assert_inventory_identity(idx: &str, lines: usize, date_stamp: &str) {
        assert_eq!(idx.lines().count(), lines);
        assert!(idx.lines().all(|line| {
            line.split(':').nth(2) == Some(date_stamp)
                && line
                    .split(':')
                    .nth(1)
                    .is_some_and(|offset| offset.parse::<u64>().is_ok())
        }));
    }

    #[test]
    fn noaa_wave1_official_indexes_pin_urls_products_and_inventory() {
        use rustwx_core::{CycleSpec, ModelId, ModelRunRequest, SourceId};

        let hrrr_prs = include_str!("../tests/fixtures/hrrr-ak.t00z.wrfprsf01.grib2.idx");
        let hrrr_sfc = include_str!("../tests/fixtures/hrrr-ak.t00z.wrfsfcf01.grib2.idx");
        let rap = include_str!("../tests/fixtures/rap.t00z.awp130pgrbf01.grib2.idx");
        let nam = include_str!("../tests/fixtures/nam.t00z.awip3d01.tm00.grib2.idx");
        let gdas = include_str!("../tests/fixtures/gdas.t00z.pgrb2.0p25.f003.idx");

        assert_inventory_identity(hrrr_prs, 702, "d=2026081200");
        assert_inventory_identity(hrrr_sfc, 169, "d=2026081200");
        assert_inventory_identity(rap, 355, "d=2026081200");
        assert_inventory_identity(nam, 656, "d=2026081200");
        assert_inventory_identity(gdas, 743, "d=2026081200");

        let cycle = CycleSpec::new("20260812", 0).unwrap();
        let cases = [
            (
                ModelId::HrrrAk,
                1,
                "prs",
                "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260812/alaska/hrrr.t00z.wrfprsf01.ak.grib2.idx",
            ),
            (
                ModelId::HrrrAk,
                1,
                "sfc",
                "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260812/alaska/hrrr.t00z.wrfsfcf01.ak.grib2.idx",
            ),
            (
                ModelId::Rap,
                1,
                "awp130pgrb",
                "https://noaa-rap-pds.s3.amazonaws.com/rap.20260812/rap.t00z.awp130pgrbf01.grib2.idx",
            ),
            (
                ModelId::Nam,
                1,
                "awip3d",
                "https://noaa-nam-pds.s3.amazonaws.com/nam.20260812/nam.t00z.awip3d01.tm00.grib2.idx",
            ),
            (
                ModelId::Gdas,
                3,
                "pgrb2.0p25",
                "https://noaa-gfs-bdp-pds.s3.amazonaws.com/gdas.20260812/00/atmos/gdas.t00z.pgrb2.0p25.f003.idx",
            ),
        ];
        for (model, hour, product, expected_idx_url) in cases {
            let request = ModelRunRequest::new(model, cycle.clone(), hour, product).unwrap();
            let urls = rustwx_models::resolve_urls(&request).unwrap();
            let aws = urls
                .iter()
                .find(|candidate| candidate.source == SourceId::Aws)
                .expect("official AWS route must resolve");
            assert_eq!(format!("{}.idx", aws.grib_url), expected_idx_url);
        }

        for (idx, required_rows) in [
            (
                hrrr_sfc,
                &[
                    ("TMP", "2 m above ground"),
                    ("DPT", "2 m above ground"),
                    ("RH", "2 m above ground"),
                    ("UGRD", "10 m above ground"),
                    ("VGRD", "10 m above ground"),
                    ("MSLMA", "mean sea level"),
                    ("PRES", "surface"),
                    ("HGT", "surface"),
                ][..],
            ),
            (
                rap,
                &[
                    ("TMP", "2 m above ground"),
                    ("DPT", "2 m above ground"),
                    ("RH", "2 m above ground"),
                    ("UGRD", "10 m above ground"),
                    ("VGRD", "10 m above ground"),
                    ("MSLMA", "mean sea level"),
                    ("PRES", "surface"),
                    ("HGT", "surface"),
                ][..],
            ),
            (
                nam,
                &[
                    ("TMP", "2 m above ground"),
                    ("RH", "2 m above ground"),
                    ("UGRD", "10 m above ground"),
                    ("VGRD", "10 m above ground"),
                    ("PRMSL", "mean sea level"),
                    ("PRES", "surface"),
                    ("HGT", "surface"),
                ][..],
            ),
            (
                gdas,
                &[
                    ("TMP", "2 m above ground"),
                    ("DPT", "2 m above ground"),
                    ("RH", "2 m above ground"),
                    ("UGRD", "10 m above ground"),
                    ("VGRD", "10 m above ground"),
                    ("PRMSL", "mean sea level"),
                    ("PRES", "surface"),
                    ("HGT", "surface"),
                ][..],
            ),
        ] {
            for (variable, level) in required_rows {
                assert!(
                    idx_has_row(idx, variable, level),
                    "missing {variable}:{level}"
                );
            }
        }
        assert!(
            !idx_has_row(nam, "DPT", "2 m above ground"),
            "awip3d publishes 2 m RH, not a native 2 m dewpoint"
        );
    }

    #[test]
    fn noaa_wave1_inventory_pins_pressure_levels_and_accumulation_windows() {
        let hrrr_prs = include_str!("../tests/fixtures/hrrr-ak.t00z.wrfprsf01.grib2.idx");
        let hrrr_sfc = include_str!("../tests/fixtures/hrrr-ak.t00z.wrfsfcf01.grib2.idx");
        let rap = include_str!("../tests/fixtures/rap.t00z.awp130pgrbf01.grib2.idx");
        let nam = include_str!("../tests/fixtures/nam.t00z.awip3d01.tm00.grib2.idx");
        let gdas = include_str!("../tests/fixtures/gdas.t00z.pgrb2.0p25.f003.idx");

        let hrrr_levels = (50..=1_000).step_by(25).collect::<Vec<_>>();
        let sounding_levels = (100..=1_000).step_by(25).collect::<Vec<_>>();
        for variable in ["TMP", "DPT", "RH", "UGRD", "VGRD", "HGT"] {
            assert_eq!(pressure_levels(hrrr_prs, variable, 50), hrrr_levels);
        }
        for variable in ["TMP", "RH", "UGRD", "VGRD", "HGT"] {
            assert_eq!(pressure_levels(rap, variable, 100), sounding_levels);
        }
        assert!(pressure_levels(rap, "DPT", 100).is_empty());

        for variable in ["TMP", "RH", "UGRD", "VGRD", "HGT"] {
            assert_eq!(pressure_levels(nam, variable, 50), hrrr_levels);
        }
        assert_eq!(
            pressure_levels(nam, "DPT", 50),
            vec![300, 400, 500, 700, 850, 1_000]
        );
        assert_eq!(
            pressure_levels(gdas, "TMP", 100),
            vec![
                100, 150, 200, 250, 300, 350, 400, 450, 500, 550, 600, 650, 700, 750, 800, 850,
                900, 925, 950, 975, 1_000,
            ]
        );
        for variable in ["RH", "UGRD", "VGRD", "HGT"] {
            assert_eq!(
                pressure_levels(gdas, variable, 100),
                pressure_levels(gdas, "TMP", 100)
            );
        }
        assert!(pressure_levels(gdas, "DPT", 100).is_empty());

        assert!(hrrr_sfc.contains(":APCP:surface:0-1 hour acc fcst:"));
        assert!(rap.contains(":APCP:surface:0-1 hour acc fcst:"));
        assert!(nam.contains(":APCP:surface:0-1 hour acc fcst:"));
        assert!(gdas.contains(":APCP:surface:0-3 hour acc fcst:"));
    }

    /// REGRESSION (root cause of the first live ingest fetching whole 4.3+9.1 GB
    /// files instead of subsetting): the patterns MUST be bare
    /// `VARIABLE[:level]` tokens, NOT colon-wrapped `:VARIABLE:level:`.
    /// `find_entries` splits on the first colon, so a leading colon yields an
    /// empty variable name that matches nothing, and the fetcher then silently
    /// falls back to a whole-file GET. This test proves the live-idx field rows
    /// (variable, level taken verbatim from the 2026-06-11 f001 `.idx` files)
    /// are selected by the current patterns, and that the old `:VAR:` framing
    /// would select nothing.
    #[test]
    fn rrfs_a_idx_patterns_select_real_idx_rows() {
        // (variable, level) rows that must be reachable, verbatim from the live
        // natlev.na / prslev.na .idx files.
        let nat_rows = [
            ("TMP", "2 m above ground"),
            ("DPT", "2 m above ground"),
            ("UGRD", "10 m above ground"),
            ("VGRD", "10 m above ground"),
            ("REFC", "entire atmosphere (considered as a single layer)"),
            ("MSLET", "mean sea level"),
            ("APCP", "surface"),
            ("MXUPHL", "5000-2000 m above ground"),
            ("MXUPHL", "3000-0 m above ground"),
            ("WIND", "10 m above ground"),
            // Native CAPE planes for the heavy native-ECAPE ratio recipes.
            ("CAPE", "surface"),
            ("CAPE", "90-0 mb above ground"),
            ("CAPE", "255-0 mb above ground"),
            // Cloud cover: TCDC entire-atmosphere + the per-layer LCDC set
            // (natlev carries them; the original recon missed them).
            ("TCDC", "entire atmosphere (considered as a single layer)"),
            ("LCDC", "low cloud layer"),
            ("MCDC", "middle cloud layer"),
            ("HCDC", "high cloud layer"),
        ];
        // Bare `TCDC` must NOT be a pattern: natlev carries ~60 per-hybrid-level
        // TCDC planes and an unqualified token would fetch them all.
        assert!(
            !RRFS_NAT_IDX_PATTERNS.contains(&"TCDC"),
            "TCDC must stay level-qualified (entire atmosphere)"
        );
        for (var, level) in nat_rows {
            assert!(
                RRFS_NAT_IDX_PATTERNS
                    .iter()
                    .any(|p| idx_line_matches(p, var, level)),
                "nat patterns select nothing for {var}:{level}"
            );
        }
        // prslev.na isobaric rows (one example level per field type).
        let prs_rows = [
            ("TMP", "500 mb"),
            ("RH", "850 mb"),
            ("DPT", "700 mb"),
            ("UGRD", "250 mb"),
            ("VGRD", "250 mb"),
            ("HGT", "500 mb"),
            ("ABSV", "500 mb"),
        ];
        for (var, level) in prs_rows {
            assert!(
                RRFS_PRS_IDX_PATTERNS
                    .iter()
                    .any(|p| idx_line_matches(p, var, level)),
                "prs patterns select nothing for {var}:{level}"
            );
        }
        // No pattern may be colon-wrapped — that is the exact shape that
        // silently disabled subsetting and must never regress.
        for pattern in RRFS_NAT_IDX_PATTERNS.iter().chain(RRFS_PRS_IDX_PATTERNS) {
            assert!(
                !pattern.starts_with(':') && !pattern.ends_with(':'),
                "idx pattern {pattern:?} is colon-wrapped (matches nothing in find_entries)"
            );
            // And prove the colon-wrapped form really would match nothing, so
            // the guard above is meaningful and not cosmetic.
            let wrapped = format!(":{pattern}:");
            assert!(
                !idx_line_matches(&wrapped, "TMP", "2 m above ground")
                    && !idx_line_matches(&wrapped, "REFC", "entire atmosphere"),
                "colon-wrapped {wrapped:?} unexpectedly matched"
            );
        }
    }

    #[test]
    fn fetch_plan_hrrr_is_the_historical_two_file_pair() {
        use rustwx_core::ModelId;
        for (model, pressure, surface) in [
            (ModelId::Hrrr, "prs", "sfc"),
            (ModelId::HrrrAk, "prs", "sfc"),
            (ModelId::Aigfs, "pres", "sfc"),
            (ModelId::Aigefs, "pres/avg", "sfc/avg"),
            (ModelId::Hgefs, "pres/avg", "sfc/avg"),
        ] {
            let plan = fetch_plan(model).expect("split pressure/surface plan");
            assert_eq!(plan.len(), 2, "{model} fetches pressure + surface");
            // Order is load-bearing: pressure (prs) first, surface (sfc) second,
            // matching the historical fetch sequence.
            assert_eq!(plan[0].product, pressure);
            assert!(plan[0].pressure_source && !plan[0].surface_source);
            assert_eq!(plan[1].product, surface);
            assert!(plan[1].surface_source && !plan[1].pressure_source);
        }
    }

    #[test]
    fn fetch_plan_gdps_uses_ordered_logical_component_families() {
        let plan = fetch_plan(rustwx_core::ModelId::Gdps).expect("GDPS plan");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].product, "rws-pressure");
        assert!(plan[0].pressure_source && !plan[0].surface_source);
        assert_eq!(plan[1].product, "rws-surface");
        assert!(plan[1].surface_source && !plan[1].pressure_source);
        assert!(plan.iter().all(|product| product.idx_patterns.is_empty()));

        let capability = model_ingest_capability(rustwx_core::ModelId::Gdps);
        assert_eq!(capability.status, IngestSupportStatus::Ready);
        assert_eq!(
            capability.verification,
            IngestVerificationLevel::LiveVerified
        );
        assert_eq!(
            capability.limitations,
            vec![IngestCapabilityLimitation::SparsePressureLevels]
        );
    }

    #[test]
    fn fetch_plan_single_file_models_serve_both_roles() {
        use rustwx_core::ModelId;
        for (model, product) in [
            (ModelId::Rap, "awp130pgrb"),
            (ModelId::Gfs, "pgrb2.0p25"),
            (ModelId::Gdas, "pgrb2.0p25"),
            (ModelId::Gefs, "pgrb2ap5/gec00"),
            (ModelId::EcmwfOpenData, "oper"),
            (ModelId::Aifs, "oper"),
            (ModelId::Nam, "awip3d"),
            (ModelId::Href, "ensprod/conus/mean"),
            (ModelId::Sref, "ensprod/pgrb212/mean_3hrly"),
            (ModelId::Refs, "mean-conus"),
        ] {
            let plan = fetch_plan(model).expect("single-file plan");
            assert_eq!(plan.len(), 1, "{model} fetches one product file");
            assert_eq!(plan[0].product, product);
            assert!(
                plan[0].surface_source && plan[0].pressure_source,
                "the one {model} file serves both the surface and pressure roles"
            );
        }
    }

    #[test]
    fn fetch_plan_rap_is_one_file_serving_both_roles() {
        use rustwx_core::ModelId;
        let plan = fetch_plan(ModelId::Rap).expect("RAP plan");
        assert_eq!(plan.len(), 1, "RAP fetches a single awp130pgrb file");
        assert_eq!(plan[0].product, "awp130pgrb");
        assert!(
            plan[0].surface_source && plan[0].pressure_source,
            "the one RAP file serves both the surface and pressure roles"
        );
        assert!(plan[0].idx_patterns.is_empty());
    }

    #[test]
    fn fetch_plan_aifs_uses_the_verified_ecmwf_json_index_inventory() {
        use rustwx_core::ModelId;

        let plan = fetch_plan(ModelId::Aifs).expect("AIFS plan");
        assert_eq!(plan.len(), 1);
        let oper = &plan[0];
        assert_eq!(oper.product, "oper");
        assert!(oper.surface_source && oper.pressure_source);
        assert_eq!(
            oper.idx_patterns,
            &[
                "param=2t",
                "param=2d",
                "param=10u",
                "param=10v",
                "param=msl",
                "param=sp",
                "param=tp",
                "param=tcc",
                "param=lcc",
                "param=mcc",
                "param=hcc",
                "param=t",
                "param=q",
                "param=u",
                "param=v",
                "param=gh",
            ]
        );

        let fixture = include_str!("../tests/fixtures/aifs-single.20260810T0000.f024.oper.index");
        let rows = fixture
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSON row"))
            .collect::<Vec<_>>();
        for pattern in oper.idx_patterns {
            let param = pattern
                .strip_prefix("param=")
                .expect("AIFS patterns are exact param predicates");
            assert!(
                rows.iter().any(|row| row["param"] == param),
                "fixture must contain an official row for {pattern}"
            );
        }
        let pressure_levels = rows
            .iter()
            .filter(|row| row["param"] == "q")
            .filter_map(|row| row["levelist"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            pressure_levels,
            vec![
                "925", "1000", "250", "50", "700", "150", "300", "500", "100", "850", "200", "400",
                "600",
            ]
        );
    }

    #[test]
    fn global_wave_fetch_plans_match_captured_official_indexes() {
        use rustwx_core::ModelId;

        let gefs = include_str!("../tests/fixtures/gefs.20260812.t00z.f024.idx");
        let plan = fetch_plan(ModelId::Gefs).expect("GEFS plan");
        assert_patterns_match_index("GEFS control", plan[0].idx_patterns, gefs);
        assert!(
            gefs.lines()
                .filter(|line| !line.trim().is_empty())
                .all(|line| line.contains("ENS=low-res ctl")),
            "the pinned GEFS lane must contain only the control member"
        );

        let noaa_cases = [
            (
                ModelId::Aigfs,
                include_str!("../tests/fixtures/aigfs.20260812.t00z.f024.pres.idx"),
                include_str!("../tests/fixtures/aigfs.20260812.t00z.f024.sfc.idx"),
                false,
            ),
            (
                ModelId::Aigefs,
                include_str!("../tests/fixtures/aigefs.20260812.t00z.f024.pres.avg.idx"),
                include_str!("../tests/fixtures/aigefs.20260812.t00z.f024.sfc.avg.idx"),
                true,
            ),
            (
                ModelId::Hgefs,
                include_str!("../tests/fixtures/hgefs.20260812.t00z.f024.pres.avg.idx"),
                include_str!("../tests/fixtures/hgefs.20260812.t00z.f024.sfc.avg.idx"),
                true,
            ),
        ];
        for (model, pressure, surface, expect_mean) in noaa_cases {
            let plan = fetch_plan(model).expect("NOAA AI plan");
            assert_patterns_match_index(
                &format!("{model} pressure"),
                plan[0].idx_patterns,
                pressure,
            );
            assert_patterns_match_index(&format!("{model} surface"), plan[1].idx_patterns, surface);
            if expect_mean {
                assert!(
                    pressure
                        .lines()
                        .chain(surface.lines())
                        .filter(|line| !line.trim().is_empty())
                        .all(|line| line.ends_with(":ens mean")),
                    "{model} fixtures must contain post-processed mean rows only"
                );
            }
        }

        let ifs = include_str!("../tests/fixtures/ifs.20260812.t00z.f024.oper.index");
        let rows = ifs
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("valid ECMWF JSON row")
            })
            .collect::<Vec<_>>();
        let ifs_plan = fetch_plan(ModelId::EcmwfOpenData).expect("IFS plan");
        for pattern in ifs_plan[0].idx_patterns {
            let param = pattern
                .strip_prefix("param=")
                .expect("IFS patterns are exact param predicates");
            assert!(
                rows.iter().any(|row| row["param"] == param),
                "IFS fixture must contain an official row for {pattern}"
            );
        }
        let mut q_levels = rows
            .iter()
            .filter(|row| row["param"] == "q" && row["levtype"] == "pl")
            .filter_map(|row| row["levelist"].as_str())
            .map(|level| level.parse::<u16>().expect("numeric pressure level"))
            .collect::<Vec<_>>();
        q_levels.sort_unstable();
        assert_eq!(
            q_levels,
            vec![
                10, 50, 100, 150, 200, 250, 300, 400, 500, 600, 700, 850, 925, 1000
            ]
        );
    }

    #[test]
    fn fetch_plan_rejects_unsupported_model() {
        use rustwx_core::ModelId;
        let err = fetch_plan(ModelId::WrfGdex).expect_err("WRF GDEX has no fetch plan");
        assert!(!err.is_cancelled());
        assert!(
            err.to_string().contains("no ingest fetch plan"),
            "got: {err}"
        );
    }

    /// The GFS fetch-plan token resolves to a well-formed AWS GRIB URL
    /// through the same `ModelRunRequest` -> `resolve_urls` path the ingest
    /// fetch uses. AWS is GFS source priority 2 (NOMADS is 1), so the test
    /// picks the AWS entry explicitly and asserts the exact archive URL.
    #[test]
    fn gfs_fetch_plan_token_resolves_a_well_formed_aws_url() {
        use rustwx_core::{CycleSpec, ModelId, ModelRunRequest, SourceId};
        let plan = fetch_plan(ModelId::Gfs).expect("GFS plan");
        let cycle = CycleSpec::new("20260414", 18).expect("valid cycle");
        let request =
            ModelRunRequest::new(ModelId::Gfs, cycle, 12, plan[0].product).expect("GFS request");
        let urls = rustwx_models::resolve_urls(&request).expect("GFS urls resolve");
        let aws = urls
            .iter()
            .find(|url| url.source == SourceId::Aws)
            .expect("AWS is a GFS source");
        assert_eq!(
            aws.grib_url,
            "https://noaa-gfs-bdp-pds.s3.amazonaws.com/gfs.20260414/18/atmos/gfs.t18z.pgrb2.0p25.f012"
        );
    }

    #[test]
    fn added_fetch_plan_tokens_resolve_well_formed_urls() {
        use rustwx_core::{CycleSpec, ModelId, ModelRunRequest, SourceId};
        let cycle = CycleSpec::new("20260414", 18).expect("valid cycle");
        let cases = [
            (
                ModelId::HrrrAk,
                6,
                "prs",
                SourceId::Aws,
                "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260414/alaska/hrrr.t18z.wrfprsf06.ak.grib2",
            ),
            (
                ModelId::Rap,
                6,
                "awp130pgrb",
                SourceId::Aws,
                "https://noaa-rap-pds.s3.amazonaws.com/rap.20260414/rap.t18z.awp130pgrbf06.grib2",
            ),
            (
                ModelId::Gdas,
                3,
                "pgrb2.0p25",
                SourceId::Aws,
                "https://noaa-gfs-bdp-pds.s3.amazonaws.com/gdas.20260414/18/atmos/gdas.t18z.pgrb2.0p25.f003",
            ),
            (
                ModelId::Gefs,
                6,
                "pgrb2ap5/gec00",
                SourceId::Aws,
                "https://noaa-gefs-pds.s3.amazonaws.com/gefs.20260414/18/atmos/pgrb2ap5/gec00.t18z.pgrb2a.0p50.f006",
            ),
            (
                ModelId::Aigfs,
                6,
                "pres",
                SourceId::Nomads,
                "https://nomads.ncep.noaa.gov/pub/data/nccf/com/aigfs/prod/aigfs.20260414/18/model/atmos/grib2/aigfs.t18z.pres.f006.grib2",
            ),
            (
                ModelId::Aigefs,
                6,
                "sfc/avg",
                SourceId::Nomads,
                "https://nomads.ncep.noaa.gov/pub/data/nccf/com/aigefs/prod/aigefs.20260414/18/ensstat/products/atmos/grib2/aigefs.t18z.sfc.avg.f006.grib2",
            ),
            (
                ModelId::Hgefs,
                6,
                "pres/avg",
                SourceId::Nomads,
                "https://nomads.ncep.noaa.gov/pub/data/nccf/com/hgefs/prod/hgefs.20260414/18/ensstat/products/atmos/grib2/hgefs.t18z.pres.avg.f006.grib2",
            ),
            (
                ModelId::EcmwfOpenData,
                6,
                "oper",
                SourceId::Ecmwf,
                "https://data.ecmwf.int/forecasts/20260414/18z/ifs/0p25/oper/20260414180000-6h-oper-fc.grib2",
            ),
            (
                ModelId::Nam,
                6,
                "awip3d",
                SourceId::Aws,
                "https://noaa-nam-pds.s3.amazonaws.com/nam.20260414/nam.t18z.awip3d06.tm00.grib2",
            ),
        ];
        for (model, hour, product, source, expected) in cases {
            let request =
                ModelRunRequest::new(model, cycle.clone(), hour, product).expect("request");
            let urls = rustwx_models::resolve_urls(&request).expect("urls resolve");
            let resolved = urls
                .iter()
                .find(|url| url.source == source)
                .unwrap_or_else(|| panic!("{model} {source:?} URL missing"));
            assert_eq!(resolved.grib_url, expected);
        }
    }
}
