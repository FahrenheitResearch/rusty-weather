//! GDEX (NSF NCAR GDEX / THREDDS) data-source core — Stage 1a plumbing.
//!
//! GDEX's web "Data Access" pages are a UI over a THREDDS Data Server (TDS).
//! The machine API is the TDS catalog: every `catalog.html` has a
//! `catalog.xml` twin. This module provides the tested, UI-free plumbing:
//!
//! 1. **Catalog crawl** — recursively fetch `catalog.xml`
//!    ([`fetch_and_parse_catalog`] for one level; [`crawl_dataset`] for a full,
//!    disk-cached crawl). `<catalogRef xlink:href>` = subdir (resolved relative
//!    and recursed), `<dataset urlPath>` = a leaf file (kept when the urlPath
//!    ends with a data extension, dropping the stray scan `dump`).
//! 2. **NCSS metadata + subset URL** — [`fetch_ncss_dataset`] parses the grid
//!    `dataset.xml` (variables, lat/lon box, time span); [`ncss_subset_url`]
//!    builds a subset request. NCSS grid rejects `accept=netcdf4` (HTTP 400) —
//!    this module always requests `accept=netcdf` (classic NetCDF-3, which
//!    `netcrust` reads).
//! 3. **Resumable download** — [`download_to_path`] streams a URL to a
//!    canonical-URL-bound temp with HTTP `Range` / `If-Range`, validates its
//!    identity sidecar and final size, then installs it with backup/rollback
//!    replacement. Local paths sanitize `:` -> `_`
//!    (the leaf filenames
//!    carry `:` which is illegal on NTFS).
//! 4. **GDEX-strength retry** — the server 503s and returns empty 200 bodies
//!    under load; every catalog / NCSS / download-start request retries 5xx and
//!    empty bodies with backoff.
//!
//! ## Ingest handoff (Stage 1b seam)
//!
//! This crate is deliberately UI- and store-agnostic. A download stops at a
//! local path ([`DownloadOutcome::path`]); the host application decides how
//! that path is imported or opened:
//!
//! ```ignore
//! let outcome = rw_gdex::download_leaf(&leaf, &cache_dir)?;
//! host_import(outcome.path)?;
//! ```
//!
//! This keeps the THREDDS protocol reusable by desktop, CLI, and service
//! front ends without creating a dependency back into any of them.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration as StdDuration;

use chrono::Utc;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, HeaderName, IF_RANGE, LAST_MODIFIED,
    RANGE,
};
use serde::{Deserialize, Serialize};

use std::sync::OnceLock;

#[derive(Debug, thiserror::Error)]
pub enum GdexError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("GDEX XML parse failed: {0}")]
    Xml(#[from] quick_xml::DeError),
    #[error("GDEX cache JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("downloaded {url} size mismatch: expected {expected} bytes, got {actual}")]
    DownloadSizeMismatch {
        url: String,
        expected: u64,
        actual: u64,
    },
    #[error("download cancelled: {url}")]
    DownloadCancelled { url: String },
    #[error("GDEX catalog worker panicked")]
    WorkerPanic,
}

pub type Result<T> = std::result::Result<T, GdexError>;

const HTTP_CONNECT_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const HTTP_METADATA_TIMEOUT: StdDuration = StdDuration::from_secs(25);
const HTTP_USER_AGENT: &str = concat!("rusty-weather/", env!("CARGO_PKG_VERSION"));
const MAX_METADATA_BODY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CATALOG_CACHE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DOWNLOAD_SIDECAR_BYTES: u64 = 64 * 1024;
const MAX_DOWNLOAD_URL_BYTES: usize = 16 * 1024;

fn build_http_client(timeout: Option<StdDuration>) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(HTTP_USER_AGENT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .pool_idle_timeout(StdDuration::from_secs(15))
        .timeout(timeout)
        .build()?)
}

fn metadata_http_client() -> Result<reqwest::blocking::Client> {
    static CLIENT: OnceLock<std::result::Result<reqwest::blocking::Client, String>> =
        OnceLock::new();
    match CLIENT.get_or_init(|| {
        build_http_client(Some(HTTP_METADATA_TIMEOUT)).map_err(|error| error.to_string())
    }) {
        Ok(client) => Ok(client.clone()),
        Err(error) => Err(gdex_error(format!(
            "construct GDEX metadata HTTP client: {error}"
        ))),
    }
}

fn download_http_client() -> Result<reqwest::blocking::Client> {
    static CLIENT: OnceLock<std::result::Result<reqwest::blocking::Client, String>> =
        OnceLock::new();
    // Multi-GB transfers must not share the old 180-second whole-request
    // deadline. Reqwest 0.12's blocking ClientBuilder has no separate
    // read-idle timeout, so connects remain bounded while body reads rely on
    // the transport/OS to surface a dead peer; cancellation is observed
    // between reads once a blocked read returns. An async-client migration can
    // add a true read-idle deadline without reviving a whole-body deadline.
    match CLIENT.get_or_init(|| build_http_client(None).map_err(|error| error.to_string())) {
        Ok(client) => Ok(client.clone()),
        Err(error) => Err(gdex_error(format!(
            "construct GDEX download HTTP client: {error}"
        ))),
    }
}

fn for_each_concurrent<T, F>(items: &[T], max_workers: usize, f: F) -> Result<()>
where
    T: Sync,
    F: Fn(&T) -> Result<()> + Sync,
{
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    if items.is_empty() {
        return Ok(());
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error: Mutex<Option<(usize, GdexError)>> = Mutex::new(None);
    let workers = max_workers.max(1).min(items.len());

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(|| loop {
                if failed.load(Ordering::Relaxed) {
                    break;
                }
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= items.len() {
                    break;
                }
                if let Err(err) = f(&items[index]) {
                    failed.store(true, Ordering::Relaxed);
                    if let Ok(mut slot) = first_error.lock() {
                        if slot.as_ref().is_none_or(|(earliest, _)| index < *earliest) {
                            *slot = Some((index, err));
                        }
                    }
                }
            }));
        }
        for handle in handles {
            if handle.join().is_err() {
                failed.store(true, Ordering::Relaxed);
                if let Ok(mut slot) = first_error.lock() {
                    if slot.is_none() {
                        *slot = Some((usize::MAX, GdexError::WorkerPanic));
                    }
                }
            }
        }
    });

    match first_error.into_inner() {
        Ok(Some((_, err))) => Err(err),
        _ => Ok(()),
    }
}


/// TDS catalog base (a dataset's crawl entry point lives under here).
const CATALOG_BASE: &str = "https://tds.gdex.ucar.edu/thredds/catalog/";
/// Direct-download service base (`HTTPServer`). `download_url = FILESERVER_BASE + urlPath`.
const FILESERVER_BASE: &str = "https://tds.gdex.ucar.edu/thredds/fileServer/";
/// NetCDF Subset Service grid base. `ncss_url = NCSS_GRID_BASE + urlPath`.
const NCSS_GRID_BASE: &str = "https://tds.gdex.ucar.edu/thredds/ncss/grid/";

/// Leaf extensions kept by the crawl (drops the scan-root `dump` entry).
const DATA_EXTENSIONS: &[&str] = &[".nc", ".grb", ".grib", ".grb2"];

/// Crawl concurrency cap — the server is flaky; be polite (doc §5).
const GDEX_CRAWL_CONCURRENCY: usize = 4;

/// Backoff schedule for the GDEX retry wrapper. `len + 1` total attempts (6):
/// one initial try plus five retries at 2s, 3s, 6s, 6s, 6s.
const GDEX_RETRY_BACKOFFS: &[StdDuration] = &[
    StdDuration::from_secs(2),
    StdDuration::from_secs(3),
    StdDuration::from_secs(6),
    StdDuration::from_secs(6),
    StdDuration::from_secs(6),
];

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// One physical file discovered by the catalog crawl.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leaf {
    /// Display name (the leaf `<dataset name>`, e.g. `wrf2d_d01_2080-01-01_00:00:00.nc`).
    pub name: String,
    /// THREDDS `urlPath` (e.g. `files/g/d612005/future2D/208001/...nc`).
    pub url_path: String,
    /// Full direct-download URL (`fileServer` + urlPath).
    pub download_url: String,
    /// NCSS grid base URL (append `/dataset.xml` for metadata; feed to
    /// [`ncss_subset_url`] for a subset).
    pub ncss_url: String,
    /// File size in bytes from `<dataSize>` when the catalog advertised it
    /// (decimal units: Mbytes = 1e6). `None` if absent.
    pub size_bytes: Option<u64>,
    /// Modification timestamp from `<date type="modified">` when present.
    pub date: Option<String>,
}

/// One level of a crawl: the child catalogs to recurse into, plus the leaves
/// found at this level. Stage 1b can drive lazy per-node tree expansion with
/// this directly instead of a full up-front [`crawl_dataset`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCatalog {
    /// The catalog URL this was parsed from.
    pub catalog_url: String,
    /// Resolved absolute URLs of child `catalog.xml` documents (TDS-internal
    /// only; external `<catalogRef>` web links are filtered out).
    pub child_catalog_urls: Vec<String>,
    /// Data-file leaves discovered at this level.
    pub leaves: Vec<Leaf>,
}

/// A full-crawl result, serialized to the on-disk crawl cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCache {
    /// Dataset id (e.g. `d612005`).
    pub dataset_id: String,
    /// RFC-3339 timestamp of the crawl.
    pub crawled_at: String,
    /// Every data-file leaf under the dataset.
    pub leaves: Vec<Leaf>,
}

/// A variable advertised by an NCSS grid `dataset.xml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NcssVariable {
    /// Variable / grid name (e.g. `T2`).
    pub name: String,
    /// Units string when present (e.g. `K`).
    pub units: Option<String>,
    /// Human description when present (e.g. `TEMP at 2 M`).
    pub description: Option<String>,
}

/// Geographic bounding box (degrees) as advertised by NCSS `<LatLonBox>` and
/// consumed by [`NcssSubset::bbox`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LatLonBox {
    /// Northern edge (degrees north).
    pub north: f64,
    /// Southern edge (degrees north).
    pub south: f64,
    /// Eastern edge (degrees east).
    pub east: f64,
    /// Western edge (degrees east).
    pub west: f64,
}

/// Time coverage from NCSS `<TimeSpan>` (ISO-8601 strings, verbatim).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeSpan {
    /// Start time.
    pub begin: String,
    /// End time.
    pub end: String,
}

/// Parsed NCSS grid metadata, enough to drive a subset UI.
#[derive(Clone, Debug, PartialEq)]
pub struct NcssGridDataset {
    /// Data variables (grids).
    pub variables: Vec<NcssVariable>,
    /// Full-grid lat/lon box when advertised.
    pub lat_lon_box: Option<LatLonBox>,
    /// Time coverage when advertised.
    pub time_span: Option<TimeSpan>,
}

/// An NCSS subset request. Spatial and temporal fields are optional; omitting
/// them requests the full grid / all times.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NcssSubset {
    /// Variables to include (each becomes a `var=` parameter).
    pub vars: Vec<String>,
    /// Optional spatial bounding box.
    pub bbox: Option<LatLonBox>,
    /// A single time (`time=`); takes precedence over the range fields.
    pub time: Option<String>,
    /// Range start (`time_start=`), used only when [`NcssSubset::time`] is `None`.
    pub time_start: Option<String>,
    /// Range end (`time_end=`), used only when [`NcssSubset::time`] is `None`.
    pub time_end: Option<String>,
}

/// Result of a [`download_to_path`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadOutcome {
    /// The final local path (temp renamed into place).
    pub path: PathBuf,
    /// Bytes on disk after the download.
    pub bytes: u64,
    /// True when an existing URL-bound partial was resumed via `Range`.
    pub resumed: bool,
    /// True when the file already existed at the expected size (no transfer).
    pub cache_hit: bool,
}

// ---------------------------------------------------------------------------
// Dataset registry
// ---------------------------------------------------------------------------

/// One GDEX dataset the in-app browser can open. The registry is the single
/// source of truth for dataset ids and their user-facing text — the browser's
/// picker rows, tree-root label, and attribution header all render from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GdexDataset {
    /// GDEX/RDA dataset id (e.g. `d612005`) — the THREDDS scan root lives at
    /// `files/g/<id>/` (see [`dataset_catalog_url`]).
    pub id: &'static str,
    /// Short display name (picker rows and the browser's tree root).
    pub label: &'static str,
    /// One-line description (picker hover text).
    pub blurb: &'static str,
    /// What the dataset's leaves are ("NetCDF" / "GRIB1") — a UI hint only;
    /// the import gate decides per file.
    pub format: &'static str,
    /// Attribution line shown in the browser's header.
    pub attribution: &'static str,
}

impl GdexDataset {
    /// The dataset's crawl entry-point catalog URL.
    pub fn catalog_url(&self) -> String {
        dataset_catalog_url(self.id)
    }
}

/// The datasets offered by the browser's picker. The FIRST entry is the
/// default (CONUS II — the dataset the pre-picker browser was hardwired to;
/// its downloads stay flat in the cache root for that reason).
///
/// Both ids verified against the live TDS on 2026-07-09:
/// `https://tds.gdex.ucar.edu/thredds/catalog/files/g/<id>/catalog.xml`.
/// ERA-20C (`d626000`) answers with `dataFormat: GRIB-1`, Rights "Freely
/// Available", and nine `e20c.oper.*` subtrees.
pub const GDEX_DATASETS: &[GdexDataset] = &[
    GdexDataset {
        id: "d612005",
        label: "CONUS II (d612005)",
        blurb: "NSF NCAR CONUS II regional climate WRF — present + future periods, NetCDF",
        format: "NetCDF",
        attribution: "NSF NCAR GDEX · CONUS II regional climate WRF (present + future) · \
                      CC-BY 4.0 · DOI 10.5065/49SN-8E08",
    },
    GdexDataset {
        id: "d626000",
        label: "ERA-20C (d626000)",
        blurb: "ECMWF ERA-20C — 20th-century reanalysis 1900-2010, GRIB1",
        format: "GRIB1",
        attribution: "NSF NCAR GDEX · ECMWF ERA-20C 20th-century reanalysis (1900-2010) · \
                      GRIB1 · rda.ucar.edu/datasets/d626000",
    },
];

/// Look a registry dataset up by id.
pub fn dataset_by_id(id: &str) -> Option<&'static GdexDataset> {
    GDEX_DATASETS.iter().find(|dataset| dataset.id == id)
}

/// The dataset id embedded in a THREDDS `urlPath` (`files/g/<id>/...`), when
/// the path has that shape. Lets download destinations be derived from the
/// LEAF being downloaded rather than from whatever dataset the picker shows.
/// The returned value is safe as one cache-directory component; malformed
/// remote dot/path segments are rejected.
pub fn dataset_id_from_url_path(url_path: &str) -> Option<&str> {
    let rest = url_path.strip_prefix("files/g/")?;
    let id = rest.split('/').next()?;
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        None
    } else {
        Some(id)
    }
}

// ---------------------------------------------------------------------------
// URL builders
// ---------------------------------------------------------------------------

/// The crawl entry-point catalog URL for a dataset id (e.g. `d612005`).
pub fn dataset_catalog_url(dataset_id: &str) -> String {
    format!("{CATALOG_BASE}files/g/{dataset_id}/catalog.xml")
}

/// The NCSS grid metadata URL (`.../ncss/grid/<urlPath>/dataset.xml`).
pub fn ncss_dataset_url(url_path: &str) -> String {
    format!("{NCSS_GRID_BASE}{url_path}/dataset.xml")
}

/// Build the NCSS subset request URL. Always ends with `accept=netcdf`
/// (classic NetCDF-3 — the grid endpoint rejects `netcdf4`). Parameters are
/// emitted in a fixed order (vars, bbox, time, accept) for deterministic URLs.
pub fn ncss_subset_url(url_path: &str, subset: &NcssSubset) -> String {
    let mut params: Vec<(&str, String)> = Vec::new();
    for var in &subset.vars {
        params.push(("var", var.clone()));
    }
    if let Some(bbox) = &subset.bbox {
        params.push(("north", fmt_coord(bbox.north)));
        params.push(("south", fmt_coord(bbox.south)));
        params.push(("east", fmt_coord(bbox.east)));
        params.push(("west", fmt_coord(bbox.west)));
    }
    if let Some(time) = &subset.time {
        params.push(("time", time.clone()));
    } else {
        if let Some(start) = &subset.time_start {
            params.push(("time_start", start.clone()));
        }
        if let Some(end) = &subset.time_end {
            params.push(("time_end", end.clone()));
        }
    }
    params.push(("accept", "netcdf".to_owned()));

    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode_query_value(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{NCSS_GRID_BASE}{url_path}?{query}")
}

/// Format a bbox coordinate without a trailing `.0` (server accepts both;
/// this keeps URLs tidy and tests deterministic).
fn fmt_coord(value: f64) -> String {
    if value.fract() == 0.0 && value.is_finite() {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Percent-encode a query-parameter value (RFC-3986 unreserved passes through).
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Retry wrapper (5xx / empty-200-body -> retry with backoff)
// ---------------------------------------------------------------------------

/// Classification of an HTTP response for the GDEX retry policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponseClass {
    /// 2xx with a non-empty body — use it.
    Accept,
    /// 5xx, or an empty 2xx body — transient, retry with backoff.
    Retry,
    /// A definite answer that is not success (4xx, redirects we won't follow).
    Fatal,
}

/// The heart of the GDEX retry policy: 5xx and empty 2xx bodies are transient.
fn classify_response(status: u16, body_empty: bool) -> ResponseClass {
    if (500..600).contains(&status) {
        ResponseClass::Retry
    } else if !(200..300).contains(&status) {
        ResponseClass::Fatal
    } else if body_empty {
        ResponseClass::Retry
    } else {
        ResponseClass::Accept
    }
}

/// One attempt's outcome for [`with_retry`].
enum Attempt<T> {
    /// Success — stop and return the value.
    Accept(T),
    /// Transient failure — sleep (if attempts remain) and try again.
    Retry(GdexError),
    /// Permanent failure — stop and return the error immediately.
    Fatal(GdexError),
}

/// Drive `attempt` up to `backoffs.len() + 1` times, sleeping the matching
/// backoff between transient retries. Returns the first `Accept`, any `Fatal`,
/// or the last `Retry` error once attempts are exhausted.
fn with_retry<T>(
    backoffs: &[StdDuration],
    mut attempt: impl FnMut(usize) -> Attempt<T>,
) -> Result<T> {
    let mut last: Option<GdexError> = None;
    for index in 0..=backoffs.len() {
        match attempt(index) {
            Attempt::Accept(value) => return Ok(value),
            Attempt::Fatal(err) => return Err(err),
            Attempt::Retry(err) => {
                last = Some(err);
                if index < backoffs.len() && !backoffs[index].is_zero() {
                    thread::sleep(backoffs[index]);
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| gdex_error("gdex request exhausted retries")))
}

/// A GDEX protocol error carried on the shared [`GdexError::Io`] variant
/// (no new enum variant needed for this module).
fn gdex_error(message: impl Into<String>) -> GdexError {
    GdexError::Io(io::Error::other(message.into()))
}

/// Fetch a text resource (catalog.xml / dataset.xml) with the GDEX retry
/// policy on the bounded metadata client (these documents run to hundreds of
/// KB and should never inherit the unbounded streaming-download deadline).
fn gdex_fetch_text(url: &str) -> Result<String> {
    let client = metadata_http_client()?;
    with_retry(GDEX_RETRY_BACKOFFS, |_| {
        match client.get(url).send() {
            Err(err) => Attempt::Retry(GdexError::Http(err)),
            Ok(mut response) => {
                let status = response.status().as_u16();
                if (500..600).contains(&status) {
                    return Attempt::Retry(gdex_error(format!(
                        "gdex {url}: status {status}"
                    )));
                }
                if !(200..300).contains(&status) {
                    return Attempt::Fatal(gdex_error(format!(
                        "gdex {url}: status {status}"
                    )));
                }
                if response
                    .content_length()
                    .is_some_and(|length| length > MAX_METADATA_BODY_BYTES)
                {
                    return Attempt::Fatal(gdex_error(format!(
                        "gdex {url}: metadata body exceeds {MAX_METADATA_BODY_BYTES} bytes"
                    )));
                }
                let mut bytes = Vec::new();
                match response
                    .by_ref()
                    .take(MAX_METADATA_BODY_BYTES + 1)
                    .read_to_end(&mut bytes)
                {
                    Err(error) => Attempt::Retry(GdexError::Io(error)),
                    Ok(_) if bytes.len() as u64 > MAX_METADATA_BODY_BYTES => {
                        Attempt::Fatal(gdex_error(format!(
                            "gdex {url}: metadata body exceeds {MAX_METADATA_BODY_BYTES} bytes"
                        )))
                    }
                    Ok(_) => match String::from_utf8(bytes) {
                        Err(error) => Attempt::Fatal(gdex_error(format!(
                            "gdex {url}: metadata is not UTF-8: {error}"
                        ))),
                        Ok(body) if body.trim().is_empty() => Attempt::Retry(gdex_error(format!(
                            "gdex {url}: empty metadata body"
                        ))),
                        Ok(body) => Attempt::Accept(body),
                    },
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Catalog crawl
// ---------------------------------------------------------------------------

/// Fetch and parse a single catalog level.
pub fn fetch_and_parse_catalog(catalog_url: &str) -> Result<ParsedCatalog> {
    let xml = gdex_fetch_text(catalog_url)?;
    parse_catalog(&xml, catalog_url)
}

/// Parse catalog XML already in hand (the offline-testable core of
/// [`fetch_and_parse_catalog`]).
fn parse_catalog(xml: &str, catalog_url: &str) -> Result<ParsedCatalog> {
    let catalog: CatalogXml = quick_xml::de::from_str(xml)?;
    let mut leaves = Vec::new();
    let mut child_urls = Vec::new();
    for cref in &catalog.catalog_refs {
        push_child_catalog(cref, catalog_url, &mut child_urls);
    }
    for dataset in &catalog.datasets {
        collect_from_dataset(dataset, catalog_url, &mut leaves, &mut child_urls);
    }
    child_urls.sort();
    child_urls.dedup();
    Ok(ParsedCatalog {
        catalog_url: catalog_url.to_owned(),
        child_catalog_urls: child_urls,
        leaves,
    })
}

/// Walk one `<dataset>` subtree, collecting leaves and child catalog URLs.
fn collect_from_dataset(
    dataset: &DatasetXml,
    catalog_url: &str,
    leaves: &mut Vec<Leaf>,
    child_urls: &mut Vec<String>,
) {
    for cref in &dataset.catalog_refs {
        push_child_catalog(cref, catalog_url, child_urls);
    }
    if let Some(url_path) = &dataset.url_path {
        if has_data_extension(url_path) {
            leaves.push(make_leaf(dataset, url_path));
        }
    }
    for nested in &dataset.datasets {
        collect_from_dataset(nested, catalog_url, leaves, child_urls);
    }
}

/// Resolve a `<catalogRef>` href and keep it only if it is a TDS-internal
/// catalog (external web links are dropped).
fn push_child_catalog(cref: &CatalogRefXml, catalog_url: &str, child_urls: &mut Vec<String>) {
    let Some(href) = &cref.href else {
        return;
    };
    let resolved = resolve_relative(catalog_url, href);
    if is_tds_catalog_url(&resolved) {
        child_urls.push(resolved);
    }
}

/// Build a [`Leaf`] from a leaf `<dataset>` and its `urlPath`.
fn make_leaf(dataset: &DatasetXml, url_path: &str) -> Leaf {
    let name = if dataset.name.trim().is_empty() {
        url_path.rsplit('/').next().unwrap_or(url_path).to_owned()
    } else {
        dataset.name.clone()
    };
    Leaf {
        name,
        url_path: url_path.to_owned(),
        download_url: format!("{FILESERVER_BASE}{url_path}"),
        ncss_url: format!("{NCSS_GRID_BASE}{url_path}"),
        size_bytes: dataset.data_size.as_ref().and_then(data_size_to_bytes),
        date: dataset
            .dates
            .iter()
            .find_map(|date| date.value.as_ref().map(|value| value.trim().to_owned())),
    }
}

/// `true` when `url_path` ends with a whitelisted data extension.
fn has_data_extension(url_path: &str) -> bool {
    let lower = url_path.to_ascii_lowercase();
    DATA_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// `true` when a resolved catalogRef points back into this TDS's catalog tree.
fn is_tds_catalog_url(url: &str) -> bool {
    url.starts_with(CATALOG_BASE)
}

/// Convert a `<dataSize units="...">` into bytes (decimal SI units).
fn data_size_to_bytes(data_size: &DataSizeXml) -> Option<u64> {
    let value: f64 = data_size.value.as_ref()?.trim().parse().ok()?;
    let factor = match data_size
        .units
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("bytes") => 1.0,
        Some("kbytes") => 1_000.0,
        Some("mbytes") => 1_000_000.0,
        Some("gbytes") => 1_000_000_000.0,
        Some("tbytes") => 1_000_000_000_000.0,
        _ => 1.0,
    };
    Some((value * factor) as u64)
}

/// Resolve `href` against the current catalog URL (RFC-3986-ish: absolute and
/// scheme-relative pass through; otherwise join to the base directory and
/// collapse `.`/`..`).
fn resolve_relative(base_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_owned();
    }
    let (scheme_authority, base_path) = split_scheme_authority(base_url);
    if let Some(rest) = href.strip_prefix("//") {
        let scheme = base_url.split("://").next().unwrap_or("https");
        return format!("{scheme}://{rest}");
    }
    let joined = if href.starts_with('/') {
        href.to_owned()
    } else {
        let base_dir = match base_path.rfind('/') {
            Some(index) => &base_path[..=index],
            None => "/",
        };
        format!("{base_dir}{href}")
    };
    format!("{scheme_authority}{}", normalize_path(&joined))
}

/// Split `https://host/path` into (`https://host`, `/path`).
fn split_scheme_authority(url: &str) -> (String, String) {
    if let Some(scheme_end) = url.find("://") {
        let after = scheme_end + 3;
        if let Some(slash) = url[after..].find('/') {
            let authority_end = after + slash;
            (
                url[..authority_end].to_owned(),
                url[authority_end..].to_owned(),
            )
        } else {
            (url.to_owned(), "/".to_owned())
        }
    } else {
        (String::new(), url.to_owned())
    }
}

/// Collapse `.` and `..` segments in an absolute path.
fn normalize_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut out = String::from("/");
    out.push_str(&segments.join("/"));
    out
}

/// Recursively crawl a dataset and cache the flat leaf list to disk as JSON.
/// With `refresh == false` a valid cache is returned without touching the
/// network; `refresh == true` always re-crawls and rewrites the cache.
pub fn crawl_dataset(dataset_id: &str, cache_dir: &Path, refresh: bool) -> Result<CatalogCache> {
    validate_dataset_id(dataset_id)?;
    let cache_path = catalog_cache_path(cache_dir, dataset_id);
    if !refresh {
        if let Some(cached) = read_catalog_cache(&cache_path)? {
            return Ok(cached);
        }
    }
    let leaves = crawl_from(&dataset_catalog_url(dataset_id))?;
    let cache = CatalogCache {
        dataset_id: dataset_id.to_owned(),
        crawled_at: Utc::now().to_rfc3339(),
        leaves,
    };
    write_catalog_cache(&cache_path, &cache)?;
    Ok(cache)
}

/// Reject a caller-supplied dataset id unless it is exactly one conservative
/// path/URL component. The public crawl API uses the id in both places, so
/// accepting separators, dot components, or percent escapes would permit a
/// cache-path traversal even though the built-in registry itself is trusted.
fn validate_dataset_id(dataset_id: &str) -> Result<()> {
    if dataset_id.is_empty()
        || dataset_id.len() > 128
        || !dataset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(gdex_error(format!(
            "invalid GDEX dataset id {dataset_id:?}: expected one ASCII alphanumeric/_/- component"
        )));
    }
    Ok(())
}

/// Breadth-first crawl from a starting catalog URL, capping concurrency and
/// guarding against revisits.
fn crawl_from(start_url: &str) -> Result<Vec<Leaf>> {
    let mut frontier = vec![start_url.to_owned()];
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start_url.to_owned());
    let mut all_leaves = Vec::new();

    while !frontier.is_empty() {
        let parsed: Mutex<Vec<ParsedCatalog>> = Mutex::new(Vec::new());
        for_each_concurrent(&frontier, GDEX_CRAWL_CONCURRENCY, |url| {
            let level = fetch_and_parse_catalog(url)?;
            if let Ok(mut guard) = parsed.lock() {
                guard.push(level);
            }
            Ok(())
        })?;

        let mut next = Vec::new();
        for level in parsed.into_inner().unwrap_or_default() {
            all_leaves.extend(level.leaves);
            for child in level.child_catalog_urls {
                if visited.insert(child.clone()) {
                    next.push(child);
                }
            }
        }
        frontier = next;
    }
    Ok(all_leaves)
}

/// Path of the on-disk crawl cache for a dataset.
fn catalog_cache_path(cache_dir: &Path, dataset_id: &str) -> PathBuf {
    cache_dir.join(format!("gdex_catalog_{dataset_id}.json"))
}

/// Read a crawl cache. Missing file -> `Ok(None)`; a corrupt file is treated
/// as a miss (`Ok(None)`) so a bad cache never wedges the crawl.
fn read_catalog_cache(path: &Path) -> Result<Option<CatalogCache>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if file.metadata()?.len() > MAX_CATALOG_CACHE_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_CATALOG_CACHE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CATALOG_CACHE_BYTES {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Write a crawl cache as pretty JSON (creating the cache dir if needed).
fn write_catalog_cache(path: &Path, cache: &CatalogCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cache)?;
    fs::write(path, json)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// NCSS grid metadata
// ---------------------------------------------------------------------------

/// Fetch and parse an NCSS grid `dataset.xml` for a leaf's `urlPath`.
pub fn fetch_ncss_dataset(url_path: &str) -> Result<NcssGridDataset> {
    let xml = gdex_fetch_text(&ncss_dataset_url(url_path))?;
    parse_ncss_dataset(&xml)
}

/// Parse NCSS grid `dataset.xml` already in hand (offline-testable core).
fn parse_ncss_dataset(xml: &str) -> Result<NcssGridDataset> {
    let parsed: GridDatasetXml = quick_xml::de::from_str(xml)?;
    let mut variables: Vec<NcssVariable> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for grid_set in &parsed.grid_sets {
        for grid in &grid_set.grids {
            if !seen.insert(grid.name.clone()) {
                continue;
            }
            let units = grid.find_attr("units");
            let description = grid
                .desc
                .clone()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| grid.find_attr("description"));
            variables.push(NcssVariable {
                name: grid.name.clone(),
                units,
                description,
            });
        }
    }
    Ok(NcssGridDataset {
        variables,
        lat_lon_box: parsed.lat_lon_box.map(|box_| LatLonBox {
            north: box_.north,
            south: box_.south,
            east: box_.east,
            west: box_.west,
        }),
        time_span: parsed.time_span.map(|span| TimeSpan {
            begin: span.begin,
            end: span.end,
        }),
    })
}

// ---------------------------------------------------------------------------
// Streaming resumable download
// ---------------------------------------------------------------------------

const SAFE_LEAF_FALLBACK: &str = "_download";

/// Sanitize an untrusted catalog leaf name into exactly one ordinary local
/// path component. `:` (and the other Windows-reserved characters) become
/// `_`; trailing spaces/dots and Windows device stems are also neutralized.
/// The GDEX leaves carry `:` in timestamps (`..._00:00:00.nc`), which is
/// illegal on NTFS.
pub fn sanitize_leaf_filename(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|ch| match ch {
            ':' | '<' | '>' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            other if (other as u32) < 0x20 => '_',
            other => other,
        })
        .collect();
    while matches!(sanitized.chars().last(), Some(' ' | '.')) {
        sanitized.pop();
    }
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        return SAFE_LEAF_FALLBACK.to_owned();
    }
    if has_windows_reserved_device_stem(&sanitized) {
        sanitized.insert(0, '_');
    }
    sanitized
}

fn has_windows_reserved_device_stem(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(|ch| ch == ' ' || ch == '.');
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) {
        return true;
    }
    let bytes = upper.as_bytes();
    bytes.len() == 4
        && (&bytes[..3] == b"COM" || &bytes[..3] == b"LPT")
        && matches!(bytes[3], b'1'..=b'9')
}

/// The local download path for a leaf under `cache_dir` (with the filename
/// sanitized for NTFS). The defensive component check keeps the destination
/// directly below `cache_dir` even if the sanitizer is later changed.
pub fn local_path_for_leaf(cache_dir: &Path, leaf: &Leaf) -> PathBuf {
    let filename = sanitize_leaf_filename(&leaf.name);
    let mut components = Path::new(&filename).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) => cache_dir.join(component),
        _ => cache_dir.join(SAFE_LEAF_FALLBACK),
    }
}

const DOWNLOAD_SIDECAR_SCHEMA: &str = "rw-gdex.download.v1";

/// Remote identity recorded beside a partial or completed download. URL
/// identity is mandatory; HTTP validators are retained whenever the server
/// supplies them. `final_size` is only set on the sidecar beside an installed
/// destination, which prevents a partial record from masquerading as a cache
/// entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DownloadSidecar {
    schema: String,
    canonical_url: String,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    last_modified: Option<String>,
    #[serde(default)]
    total_bytes: Option<u64>,
    #[serde(default)]
    final_size: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RemoteObjectMetadata {
    total_bytes: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
}

impl DownloadSidecar {
    fn new(canonical_url: &str, remote: Option<&RemoteObjectMetadata>) -> Self {
        let mut sidecar = Self {
            schema: DOWNLOAD_SIDECAR_SCHEMA.to_owned(),
            canonical_url: canonical_url.to_owned(),
            etag: None,
            last_modified: None,
            total_bytes: None,
            final_size: None,
        };
        if let Some(remote) = remote {
            sidecar.merge_remote(remote);
        }
        sidecar
    }

    fn merge_remote(&mut self, remote: &RemoteObjectMetadata) {
        if let Some(value) = &remote.etag {
            self.etag = Some(value.clone());
        }
        if let Some(value) = &remote.last_modified {
            self.last_modified = Some(value.clone());
        }
        if let Some(value) = remote.total_bytes {
            self.total_bytes = Some(value);
        }
    }

    fn if_range_value(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|etag| !etag.starts_with("W/"))
            .or(self.last_modified.as_deref())
    }
}

fn canonical_download_url(url: &str) -> Result<String> {
    let mut parsed = reqwest::Url::parse(url)
        .map_err(|error| gdex_error(format!("invalid download URL '{url}': {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(gdex_error(format!(
            "invalid download URL '{url}': only http and https are supported"
        )));
    }
    parsed.set_fragment(None);
    if parsed.as_str().len() > MAX_DOWNLOAD_URL_BYTES {
        return Err(gdex_error(format!(
            "download URL exceeds {MAX_DOWNLOAD_URL_BYTES} bytes"
        )));
    }
    Ok(parsed.to_string())
}

fn stable_url_hash(url: &str) -> u64 {
    // FNV-1a is deliberately implemented here rather than using DefaultHasher,
    // whose output is not a persistent-storage contract. A sidecar still
    // checks the complete canonical URL, so even a theoretical hash collision
    // is quarantined instead of appended.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in url.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn path_with_suffix(path: &Path, suffix: &str) -> Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        gdex_error(format!(
            "download path '{}' has no ordinary file name",
            path.display()
        ))
    })?;
    let mut suffixed = file_name.to_os_string();
    suffixed.push(suffix);
    Ok(path.with_file_name(suffixed))
}

/// Stable, collision-safe partial path for a `(canonical URL, destination)`
/// pair. The complete destination filename is retained before the URL hash, so
/// `field.nc` and `field.grb` never collapse to the same `field.download` path.
/// Callers may use this to display progress for the same temp used by the core.
pub fn download_partial_path(url: &str, dest: &Path) -> Result<PathBuf> {
    let canonical_url = canonical_download_url(url)?;
    path_with_suffix(
        dest,
        &format!(".{:016x}.download", stable_url_hash(&canonical_url)),
    )
}

fn partial_sidecar_path(partial_path: &Path) -> Result<PathBuf> {
    path_with_suffix(partial_path, ".json")
}

fn final_sidecar_path(dest: &Path) -> Result<PathBuf> {
    path_with_suffix(dest, ".gdex.json")
}

fn sidecar_matches_remote(
    sidecar: &DownloadSidecar,
    canonical_url: &str,
    remote: &RemoteObjectMetadata,
) -> bool {
    if sidecar.schema != DOWNLOAD_SIDECAR_SCHEMA || sidecar.canonical_url != canonical_url {
        return false;
    }
    if let (Some(saved), Some(current)) = (sidecar.total_bytes, remote.total_bytes) {
        if saved != current {
            return false;
        }
    }
    if let Some(current) = remote.etag.as_deref() {
        return sidecar.etag.as_deref() == Some(current);
    }
    if let Some(current) = remote.last_modified.as_deref() {
        return sidecar.last_modified.as_deref() == Some(current);
    }
    true
}

fn read_download_sidecar(path: &Path) -> Result<Option<DownloadSidecar>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_DOWNLOAD_SIDECAR_BYTES {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes).ok())
}

fn stage_download_sidecar(path: &Path, sidecar: &DownloadSidecar) -> Result<PathBuf> {
    static SIDECAR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(sidecar)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_DOWNLOAD_SIDECAR_BYTES {
        return Err(gdex_error(format!(
            "download identity sidecar exceeds {MAX_DOWNLOAD_SIDECAR_BYTES} bytes"
        )));
    }
    for _ in 0..1024 {
        let sequence = SIDECAR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = path_with_suffix(
            path,
            &format!(".stage-{}-{sequence}", std::process::id()),
        )?;
        let mut file = match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = file
            .write_all(&bytes)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
        {
            drop(file);
            let _ = fs::remove_file(&candidate);
            return Err(error.into());
        }
        drop(file);
        return Ok(candidate);
    }
    Err(gdex_error(format!(
        "could not reserve a sidecar staging file beside '{}'",
        path.display()
    )))
}

fn write_download_sidecar(path: &Path, sidecar: &DownloadSidecar) -> Result<()> {
    let stage = stage_download_sidecar(path, sidecar)?;
    if let Err(error) = replace_validated_temp(&stage, path) {
        let _ = fs::remove_file(&stage);
        return Err(error.into());
    }
    Ok(())
}

fn quarantine_existing(path: &Path) -> Result<Option<PathBuf>> {
    static QUARANTINE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    for _ in 0..1024 {
        let sequence = QUARANTINE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = path_with_suffix(
            path,
            &format!(".invalid-{}-{sequence}", std::process::id()),
        )?;
        match fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match fs::rename(path, &candidate) {
            Ok(()) => return Ok(Some(candidate)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(gdex_error(format!(
        "could not quarantine invalid download state '{}'",
        path.display()
    )))
}

fn quarantine_partial(partial_path: &Path, sidecar_path: &Path) -> Result<()> {
    // Move the bytes first so an interrupted quarantine can never leave an
    // anonymous partial eligible for append. A stranded sidecar is harmless
    // and is quarantined on the next preflight.
    let _ = quarantine_existing(partial_path)?;
    let _ = quarantine_existing(sidecar_path)?;
    Ok(())
}

#[derive(Debug)]
struct PreparedPartial {
    have: u64,
    sidecar: DownloadSidecar,
}

fn prepare_partial(
    partial_path: &Path,
    sidecar_path: &Path,
    canonical_url: &str,
    remote: Option<&RemoteObjectMetadata>,
) -> Result<PreparedPartial> {
    let metadata = match fs::symlink_metadata(partial_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let Some(metadata) = metadata else {
        // A sidecar without its byte file is not resumable state.
        let _ = quarantine_existing(sidecar_path)?;
        return Ok(PreparedPartial {
            have: 0,
            sidecar: DownloadSidecar::new(canonical_url, remote),
        });
    };
    let saved = read_download_sidecar(sidecar_path)?;
    let valid = metadata.file_type().is_file()
        && saved.as_ref().is_some_and(|sidecar| {
            sidecar.final_size.is_none()
                && remote
                    .map(|remote| sidecar_matches_remote(sidecar, canonical_url, remote))
                    .unwrap_or_else(|| {
                        sidecar.schema == DOWNLOAD_SIDECAR_SCHEMA
                            && sidecar.canonical_url == canonical_url
                    })
        });
    let have = metadata.len();
    let oversized = saved
        .as_ref()
        .and_then(|sidecar| sidecar.total_bytes)
        .or_else(|| remote.and_then(|remote| remote.total_bytes))
        .is_some_and(|total| have > total);
    if !valid || oversized {
        quarantine_partial(partial_path, sidecar_path)?;
        return Ok(PreparedPartial {
            have: 0,
            sidecar: DownloadSidecar::new(canonical_url, remote),
        });
    }

    let Some(mut sidecar) = saved else {
        // Keep this defensive even though `valid` above already rejects None:
        // malformed or concurrently replaced metadata must never become a
        // production panic path.
        quarantine_partial(partial_path, sidecar_path)?;
        return Ok(PreparedPartial {
            have: 0,
            sidecar: DownloadSidecar::new(canonical_url, remote),
        });
    };
    if let Some(remote) = remote {
        sidecar.merge_remote(remote);
    }
    sidecar.final_size = None;
    write_download_sidecar(sidecar_path, &sidecar)?;
    Ok(PreparedPartial { have, sidecar })
}

fn trusted_final_cache_len(
    dest: &Path,
    sidecar_path: &Path,
    canonical_url: &str,
    remote: Option<&RemoteObjectMetadata>,
) -> Result<Option<u64>> {
    let metadata = match fs::symlink_metadata(dest) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(sidecar) = read_download_sidecar(sidecar_path)? else {
        return Ok(None);
    };
    let actual = metadata.len();
    if sidecar.schema != DOWNLOAD_SIDECAR_SCHEMA
        || sidecar.canonical_url != canonical_url
        || sidecar.final_size != Some(actual)
        || sidecar.total_bytes != Some(actual)
        || remote
            .and_then(|remote| remote.total_bytes)
            .is_some_and(|current_total| current_total != actual)
        || remote.is_some_and(|remote| !sidecar_matches_remote(&sidecar, canonical_url, remote))
    {
        return Ok(None);
    }
    Ok(Some(actual))
}

/// Download a leaf to `cache_dir`, choosing the local path from its (sanitized)
/// name. The returned [`DownloadOutcome::path`] is the ingest seam for Stage 1b.
pub fn download_leaf(leaf: &Leaf, cache_dir: &Path) -> Result<DownloadOutcome> {
    let dest = local_path_for_leaf(cache_dir, leaf);
    download_to_path(&leaf.download_url, &dest)
}

/// Chunk size for the cancellable copy loop — large enough that syscall
/// overhead is negligible on a multi-GB stream, small enough that a cancel
/// lands within a fraction of a second at typical GDEX throughput.
const COPY_CHUNK_BYTES: usize = 64 * 1024;

/// How [`copy_with_cancel`] ended. Both ends carry the bytes written (and
/// flushed) to the writer during this call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyEnd {
    /// The reader hit EOF — the copy ran to completion.
    Complete(u64),
    /// The cancel flag was observed between chunks — the copy stopped early.
    /// Everything read so far was written and flushed; the writer is left
    /// intact (the resume contract depends on the partial temp surviving).
    Cancelled(u64),
}

/// `io::copy` with a cooperative cancel check between chunks. Flushes the
/// writer on both exits (completion and cancel) so a partial temp holds every
/// byte it claims; never truncates or deletes anything.
fn copy_with_cancel(
    reader: &mut impl Read,
    writer: &mut impl Write,
    cancel: &AtomicBool,
) -> io::Result<CopyEnd> {
    let mut buf = vec![0u8; COPY_CHUNK_BYTES];
    let mut written: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            writer.flush()?;
            return Ok(CopyEnd::Cancelled(written));
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => {
                writer.flush()?;
                return Ok(CopyEnd::Complete(written));
            }
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        writer.write_all(&buf[..n])?;
        written += n as u64;
    }
}

/// Strictly parsed HTTP byte-range metadata. A resumed body is only safe to
/// append when its advertised start matches the local temp length, and a 416
/// is only proof of completion when the server advertises that exact length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParsedContentRange {
    Satisfied { start: u64, end: u64, total: u64 },
    Unsatisfied { total: u64 },
}

fn parse_content_range(value: &str) -> Option<ParsedContentRange> {
    let mut parts = value.split_whitespace();
    let unit = parts.next()?;
    let range = parts.next()?;
    if !unit.eq_ignore_ascii_case("bytes") || parts.next().is_some() {
        return None;
    }
    let (bounds, total) = range.split_once('/')?;
    let total = total.parse::<u64>().ok()?;
    if bounds == "*" {
        return Some(ParsedContentRange::Unsatisfied { total });
    }
    let (start, end) = bounds.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    if start > end || end >= total {
        return None;
    }
    Some(ParsedContentRange::Satisfied { start, end, total })
}

fn response_content_length(
    response: &reqwest::blocking::Response,
    url: &str,
) -> Result<Option<u64>> {
    response
        .headers()
        .get(CONTENT_LENGTH)
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| gdex_error(format!("gdex {url}: invalid Content-Length header")))
        })
        .transpose()
}

fn response_header_text(
    response: &reqwest::blocking::Response,
    header: &HeaderName,
    label: &str,
    url: &str,
) -> Result<Option<String>> {
    response
        .headers()
        .get(header)
        .map(|value| {
            let value = value.to_str().map_err(|_| {
                gdex_error(format!("gdex {url}: invalid {label} response header"))
            })?;
            if value.trim().is_empty() {
                return Err(gdex_error(format!(
                    "gdex {url}: empty {label} response header"
                )));
            }
            Ok(value.to_owned())
        })
        .transpose()
}

fn response_remote_metadata(
    response: &reqwest::blocking::Response,
    url: &str,
    total_bytes: Option<u64>,
) -> Result<RemoteObjectMetadata> {
    Ok(RemoteObjectMetadata {
        total_bytes,
        etag: response_header_text(response, &ETAG, "ETag", url)?,
        last_modified: response_header_text(response, &LAST_MODIFIED, "Last-Modified", url)?,
    })
}

fn required_content_range(
    response: &reqwest::blocking::Response,
    url: &str,
) -> Result<ParsedContentRange> {
    let value = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| gdex_error(format!("gdex {url}: response omitted Content-Range")))?;
    parse_content_range(value).ok_or_else(|| {
        gdex_error(format!(
            "gdex {url}: malformed Content-Range header '{value}'"
        ))
    })
}

/// Install a completely validated temp without deleting an existing
/// destination first. Windows cannot rename over an existing file, so move
/// the old file to a same-directory backup, install the temp, and roll the old
/// file back if that second rename fails.
fn replace_validated_temp(temp: &Path, dest: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dest) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return fs::rename(temp, dest);
        }
        Err(error) => return Err(error),
    }

    static REPLACEMENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    if dest.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("download destination '{}' has no file name", dest.display()),
        ));
    }
    let mut backup = None;
    for _ in 0..1024 {
        let sequence = REPLACEMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".gdex-replace-{}-{sequence}.bak",
            std::process::id()
        ));
        match fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                backup = Some(candidate);
                break;
            }
            Err(error) => return Err(error),
        }
    }
    let backup = backup.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "could not reserve a replacement backup beside '{}'",
                dest.display()
            ),
        )
    })?;

    fs::rename(dest, &backup)?;
    match fs::rename(temp, dest) {
        Ok(()) => {
            // The new validated destination is already live. A cleanup error
            // should not turn a successful download into a retry that replaces
            // it again; leave the uniquely named old copy for manual recovery.
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(install_error) => match fs::rename(&backup, dest) {
            Ok(()) => Err(install_error),
            Err(rollback_error) => Err(io::Error::other(format!(
                "could not install validated temp '{}' ({install_error}); could not restore old destination from '{}' ({rollback_error})",
                temp.display(),
                backup.display()
            ))),
        },
    }
}

/// Stream `url` to `dest`, resuming an interrupted, URL-bound temp via HTTP
/// `Range` when the server supports it. Partial and installed files carry
/// identity sidecars, so a same-sized object from another URL or revision is
/// never appended or mistaken for a cache hit. Never buffers the body in
/// memory.
pub fn download_to_path(url: &str, dest: &Path) -> Result<DownloadOutcome> {
    download_to_path_with_cancel(url, dest, &AtomicBool::new(false))
}

#[cfg(test)]
fn head_content_length(url: &str, cancel: &AtomicBool) -> Option<u64> {
    head_remote_metadata(url, cancel).and_then(|metadata| metadata.total_bytes)
}

/// [`download_to_path`] with cooperative cancellation and persistent remote
/// identity. The partial is URL-bound, validators are checked before every
/// append/promotion, and a protocol mismatch is quarantined then retried once
/// from byte zero. Cancellation keeps the paired partial and sidecar intact.
pub fn download_to_path_with_cancel(
    url: &str,
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<DownloadOutcome> {
    let canonical_url = canonical_download_url(url)?;
    let cancelled = || GdexError::DownloadCancelled {
        url: canonical_url.clone(),
    };
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let partial_path = download_partial_path(&canonical_url, dest)?;
    let partial_sidecar = partial_sidecar_path(&partial_path)?;
    let final_sidecar = final_sidecar_path(dest)?;
    let mut remote = head_remote_metadata(&canonical_url, cancel);
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    if let Some(bytes) = trusted_final_cache_len(
        dest,
        &final_sidecar,
        &canonical_url,
        remote.as_ref(),
    )? {
        return Ok(DownloadOutcome {
            path: dest.to_owned(),
            bytes,
            resumed: false,
            cache_hit: true,
        });
    }

    let download_client = download_http_client()?;
    for restart_index in 0..=1 {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let prepared = prepare_partial(
            &partial_path,
            &partial_sidecar,
            &canonical_url,
            remote.as_ref(),
        )?;
        match download_once(
            &download_client,
            &canonical_url,
            &partial_path,
            &partial_sidecar,
            prepared,
            cancel,
        )? {
            DownloadAttempt::Complete {
                resumed,
                sidecar,
            } => {
                let metadata = fs::symlink_metadata(&partial_path)?;
                if !metadata.file_type().is_file() {
                    quarantine_partial(&partial_path, &partial_sidecar)?;
                    return Err(gdex_error(format!(
                        "gdex {canonical_url}: validated partial path is not a regular file"
                    )));
                }
                let final_len = metadata.len();
                if let Some(total) = sidecar.total_bytes {
                    if final_len != total {
                        quarantine_partial(&partial_path, &partial_sidecar)?;
                        return Err(GdexError::DownloadSizeMismatch {
                            url: canonical_url.clone(),
                            expected: total,
                            actual: final_len,
                        });
                    }
                }
                install_validated_download(
                    &partial_path,
                    &partial_sidecar,
                    dest,
                    &final_sidecar,
                    sidecar,
                    final_len,
                )?;
                return Ok(DownloadOutcome {
                    path: dest.to_owned(),
                    bytes: final_len,
                    resumed,
                    cache_hit: false,
                });
            }
            DownloadAttempt::Restart(reason) => {
                quarantine_partial(&partial_path, &partial_sidecar)?;
                if restart_index == 1 {
                    return Err(gdex_error(format!(
                        "gdex {canonical_url}: response remained unsafe after a clean restart: {reason}"
                    )));
                }
                remote = head_remote_metadata(&canonical_url, cancel);
            }
        }
    }
    Err(gdex_error(format!(
        "gdex {canonical_url}: exhausted safe download restarts"
    )))
}

enum DownloadAttempt {
    Complete {
        resumed: bool,
        sidecar: DownloadSidecar,
    },
    Restart(String),
}

fn download_once(
    client: &reqwest::blocking::Client,
    canonical_url: &str,
    partial_path: &Path,
    partial_sidecar_path: &Path,
    prepared: PreparedPartial,
    cancel: &AtomicBool,
) -> Result<DownloadAttempt> {
    let cancelled = || GdexError::DownloadCancelled {
        url: canonical_url.to_owned(),
    };
    let want_resume = prepared.have > 0;

    // Persist the URL and best validators known so far before a response body
    // can create or append any bytes.
    write_download_sidecar(partial_sidecar_path, &prepared.sidecar)?;

    // Retry request setup/status only. If a body read breaks, the bytes already
    // flushed remain paired with this sidecar for the caller's next resume.
    let mut response = with_retry(GDEX_RETRY_BACKOFFS, |_| {
        if cancel.load(Ordering::Relaxed) {
            return Attempt::Fatal(cancelled());
        }
        // Byte offsets and Content-Length must describe the stored bytes, not
        // a transparently decoded transfer representation.
        let mut request = client
            .get(canonical_url)
            .header(ACCEPT_ENCODING, "identity");
        if want_resume {
            request = request.header(RANGE, format!("bytes={}-", prepared.have));
            if let Some(validator) = prepared.sidecar.if_range_value() {
                request = request.header(IF_RANGE, validator);
            }
        }
        match request.send() {
            Err(error) => Attempt::Retry(GdexError::Http(error)),
            Ok(response) => {
                let status = response.status().as_u16();
                if (500..600).contains(&status)
                    || (status == 200 && response.content_length() == Some(0))
                {
                    Attempt::Retry(gdex_error(format!(
                        "gdex {canonical_url}: status {status} or empty body"
                    )))
                } else if matches!(status, 200 | 206 | 416) {
                    Attempt::Accept(response)
                } else {
                    Attempt::Fatal(gdex_error(format!(
                        "gdex {canonical_url}: status {status}"
                    )))
                }
            }
        }
    })?;

    let status = response.status().as_u16();
    let response_length = match response_content_length(&response, canonical_url) {
        Ok(length) => length,
        Err(error) => return Ok(DownloadAttempt::Restart(error.to_string())),
    };
    match status {
        416 => download_416(
            response,
            canonical_url,
            partial_sidecar_path,
            prepared,
            want_resume,
        ),
        206 => download_206(
            &mut response,
            canonical_url,
            partial_path,
            partial_sidecar_path,
            prepared,
            want_resume,
            response_length,
            cancel,
        ),
        200 => download_200(
            &mut response,
            canonical_url,
            partial_path,
            partial_sidecar_path,
            response_length,
            cancel,
        ),
        _ => Ok(DownloadAttempt::Restart(format!(
            "unsupported download status {status}"
        ))),
    }
}

fn download_416(
    response: reqwest::blocking::Response,
    canonical_url: &str,
    partial_sidecar_path: &Path,
    prepared: PreparedPartial,
    want_resume: bool,
) -> Result<DownloadAttempt> {
    if !want_resume {
        return Ok(DownloadAttempt::Restart(
            "unexpected 416 without a resumable partial".to_owned(),
        ));
    }
    let range = match required_content_range(&response, canonical_url) {
        Ok(range) => range,
        Err(error) => return Ok(DownloadAttempt::Restart(error.to_string())),
    };
    let ParsedContentRange::Unsatisfied { total } = range else {
        return Ok(DownloadAttempt::Restart(
            "416 did not contain 'bytes */total'".to_owned(),
        ));
    };
    let response_remote = match response_remote_metadata(&response, canonical_url, Some(total)) {
        Ok(metadata) => metadata,
        Err(error) => return Ok(DownloadAttempt::Restart(error.to_string())),
    };
    if !sidecar_matches_remote(&prepared.sidecar, canonical_url, &response_remote) {
        return Ok(DownloadAttempt::Restart(
            "416 validators identify a different remote object".to_owned(),
        ));
    }
    if prepared.have != total {
        return Ok(DownloadAttempt::Restart(format!(
            "416 reports {total} remote bytes but the partial has {}",
            prepared.have
        )));
    }
    let mut sidecar = prepared.sidecar;
    sidecar.merge_remote(&response_remote);
    write_download_sidecar(partial_sidecar_path, &sidecar)?;
    Ok(DownloadAttempt::Complete {
        resumed: true,
        sidecar,
    })
}

#[allow(clippy::too_many_arguments)]
fn download_206(
    response: &mut reqwest::blocking::Response,
    canonical_url: &str,
    partial_path: &Path,
    partial_sidecar_path: &Path,
    prepared: PreparedPartial,
    want_resume: bool,
    response_length: Option<u64>,
    cancel: &AtomicBool,
) -> Result<DownloadAttempt> {
    if !want_resume {
        return Ok(DownloadAttempt::Restart(
            "unexpected 206 without a Range request".to_owned(),
        ));
    }
    let range = match required_content_range(response, canonical_url) {
        Ok(range) => range,
        Err(error) => return Ok(DownloadAttempt::Restart(error.to_string())),
    };
    let ParsedContentRange::Satisfied { start, end, total } = range else {
        return Ok(DownloadAttempt::Restart(
            "206 did not contain 'bytes start-end/total'".to_owned(),
        ));
    };
    if start != prepared.have {
        return Ok(DownloadAttempt::Restart(format!(
            "resumed range starts at {start}, local partial ends at {}",
            prepared.have
        )));
    }
    let range_length = match end
        .checked_sub(start)
        .and_then(|length| length.checked_add(1))
    {
        Some(length) => length,
        None => {
            return Ok(DownloadAttempt::Restart(
                "byte-range length overflow".to_owned(),
            ));
        }
    };
    if response_length.is_some_and(|length| length != range_length) {
        return Ok(DownloadAttempt::Restart(format!(
            "206 Content-Length {response_length:?} disagrees with its {range_length}-byte range"
        )));
    }
    let response_remote = match response_remote_metadata(response, canonical_url, Some(total)) {
        Ok(metadata) => metadata,
        Err(error) => return Ok(DownloadAttempt::Restart(error.to_string())),
    };
    if !sidecar_matches_remote(&prepared.sidecar, canonical_url, &response_remote) {
        return Ok(DownloadAttempt::Restart(
            "206 validators identify a different remote object".to_owned(),
        ));
    }
    let current = fs::symlink_metadata(partial_path)?;
    if !current.file_type().is_file() || current.len() != prepared.have {
        return Ok(DownloadAttempt::Restart(
            "partial changed after resume preflight".to_owned(),
        ));
    }
    let mut sidecar = prepared.sidecar;
    sidecar.merge_remote(&response_remote);
    write_download_sidecar(partial_sidecar_path, &sidecar)?;
    let mut file = fs::OpenOptions::new().append(true).open(partial_path)?;
    let copied = match copy_with_cancel(response, &mut file, cancel)? {
        CopyEnd::Cancelled(_) => {
            drop(file);
            return Err(GdexError::DownloadCancelled {
                url: canonical_url.to_owned(),
            });
        }
        CopyEnd::Complete(copied) => copied,
    };
    drop(file);
    if copied != range_length {
        return Ok(DownloadAttempt::Restart(format!(
            "206 body supplied {copied} bytes for its advertised {range_length}-byte range"
        )));
    }
    let final_len = fs::symlink_metadata(partial_path)?.len();
    if final_len != total {
        return Ok(DownloadAttempt::Restart(format!(
            "resumed partial ended at {final_len} bytes, expected {total}"
        )));
    }
    Ok(DownloadAttempt::Complete {
        resumed: true,
        sidecar,
    })
}

fn download_200(
    response: &mut reqwest::blocking::Response,
    canonical_url: &str,
    partial_path: &Path,
    partial_sidecar_path: &Path,
    response_length: Option<u64>,
    cancel: &AtomicBool,
) -> Result<DownloadAttempt> {
    // A full response is authoritative whether this was a fresh GET or
    // If-Range correctly rejected a stale validator. Do not retain old
    // validators when the new response omits them.
    let response_remote = match response_remote_metadata(response, canonical_url, response_length)
    {
        Ok(metadata) => metadata,
        Err(error) => return Ok(DownloadAttempt::Restart(error.to_string())),
    };
    let mut sidecar = DownloadSidecar::new(canonical_url, Some(&response_remote));
    write_download_sidecar(partial_sidecar_path, &sidecar)?;
    let mut file = fs::File::create(partial_path)?;
    if let CopyEnd::Cancelled(_) = copy_with_cancel(response, &mut file, cancel)? {
        drop(file);
        return Err(GdexError::DownloadCancelled {
            url: canonical_url.to_owned(),
        });
    }
    drop(file);
    let final_len = fs::symlink_metadata(partial_path)?.len();
    if final_len == 0 {
        return Ok(DownloadAttempt::Restart(
            "server returned an empty 200 body".to_owned(),
        ));
    }
    if let Some(total) = response_remote.total_bytes {
        if final_len != total {
            return Ok(DownloadAttempt::Restart(format!(
                "full body supplied {final_len} bytes, expected {total}"
            )));
        }
    } else {
        sidecar.total_bytes = Some(final_len);
        write_download_sidecar(partial_sidecar_path, &sidecar)?;
    }
    Ok(DownloadAttempt::Complete {
        resumed: false,
        sidecar,
    })
}

fn install_validated_download(
    partial_path: &Path,
    partial_sidecar_path: &Path,
    dest: &Path,
    final_sidecar_path: &Path,
    mut sidecar: DownloadSidecar,
    final_len: u64,
) -> Result<()> {
    sidecar.total_bytes = Some(final_len);
    sidecar.final_size = Some(final_len);
    let staged_sidecar = stage_download_sidecar(final_sidecar_path, &sidecar)?;

    // Remove the old identity record before replacing its file, otherwise a
    // concurrent reader could briefly match new bytes to stale metadata. If
    // file replacement rolls back, restore that old record as well.
    let old_sidecar = match quarantine_existing(final_sidecar_path) {
        Ok(path) => path,
        Err(error) => {
            let _ = fs::remove_file(&staged_sidecar);
            return Err(error);
        }
    };
    if let Err(error) = replace_validated_temp(partial_path, dest) {
        let _ = fs::remove_file(&staged_sidecar);
        if let Some(old_sidecar) = old_sidecar {
            let _ = fs::rename(old_sidecar, final_sidecar_path);
        }
        return Err(error.into());
    }
    if let Err(error) = replace_validated_temp(&staged_sidecar, final_sidecar_path) {
        let _ = fs::remove_file(&staged_sidecar);
        return Err(error.into());
    }

    let _ = fs::remove_file(partial_sidecar_path);
    if let Some(old_sidecar) = old_sidecar {
        let _ = fs::remove_file(old_sidecar);
    }
    Ok(())
}

/// Best-effort remote identity via bounded HEAD retries. A failed or
/// metadata-poor HEAD does not block the stream; GET/Content-Range remains
/// authoritative.
fn head_remote_metadata(url: &str, cancel: &AtomicBool) -> Option<RemoteObjectMetadata> {
    let client = metadata_http_client().ok()?;
    with_retry(GDEX_RETRY_BACKOFFS, |_| {
        if cancel.load(Ordering::Relaxed) {
            return Attempt::Fatal(GdexError::DownloadCancelled {
                url: url.to_owned(),
            });
        }
        match client
            .head(url)
            .header(ACCEPT_ENCODING, "identity")
            .send()
        {
            Err(error) => Attempt::Retry(GdexError::Http(error)),
            Ok(response) => {
                let status = response.status().as_u16();
                if (500..600).contains(&status) {
                    Attempt::Retry(gdex_error(format!("gdex HEAD {url}: status {status}")))
                } else if !(200..300).contains(&status) {
                    Attempt::Fatal(gdex_error(format!("gdex HEAD {url}: status {status}")))
                } else {
                    match response_content_length(&response, url)
                        .and_then(|total| response_remote_metadata(&response, url, total))
                    {
                        Ok(metadata) => Attempt::Accept(metadata),
                        Err(error) => Attempt::Fatal(error),
                    }
                }
            }
        }
    })
    .ok()
}

// ---------------------------------------------------------------------------
// quick-xml serde models (THREDDS InvCatalog + NCSS gridDataset)
// ---------------------------------------------------------------------------
//
// Namespaced attributes are matched with an `alias` for the un-prefixed name so
// parsing is robust to quick-xml's raw-name handling either way.

#[derive(Debug, Deserialize)]
struct CatalogXml {
    #[serde(rename = "catalogRef", default)]
    catalog_refs: Vec<CatalogRefXml>,
    #[serde(rename = "dataset", default)]
    datasets: Vec<DatasetXml>,
}

#[derive(Debug, Deserialize)]
struct DatasetXml {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "@urlPath")]
    url_path: Option<String>,
    #[serde(rename = "catalogRef", default)]
    catalog_refs: Vec<CatalogRefXml>,
    #[serde(rename = "dataset", default)]
    datasets: Vec<DatasetXml>,
    #[serde(rename = "dataSize")]
    data_size: Option<DataSizeXml>,
    #[serde(rename = "date", default)]
    dates: Vec<DateXml>,
}

#[derive(Debug, Deserialize)]
struct CatalogRefXml {
    #[serde(rename = "@xlink:href", alias = "@href")]
    href: Option<String>,
    #[serde(rename = "@xlink:title", alias = "@title", default)]
    #[allow(dead_code)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DataSizeXml {
    #[serde(rename = "@units")]
    units: Option<String>,
    #[serde(rename = "$text", default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DateXml {
    #[serde(rename = "$text", default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GridDatasetXml {
    #[serde(rename = "gridSet", default)]
    grid_sets: Vec<GridSetXml>,
    #[serde(rename = "LatLonBox")]
    lat_lon_box: Option<LatLonBoxXml>,
    #[serde(rename = "TimeSpan")]
    time_span: Option<TimeSpanXml>,
}

#[derive(Debug, Deserialize)]
struct GridSetXml {
    #[serde(rename = "grid", default)]
    grids: Vec<GridXml>,
}

#[derive(Debug, Deserialize)]
struct GridXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@desc", default)]
    desc: Option<String>,
    #[serde(rename = "attribute", default)]
    attributes: Vec<GridAttrXml>,
}

impl GridXml {
    /// Value of the child `<attribute name="{name}" value="...">`, if present
    /// and non-empty.
    fn find_attr(&self, name: &str) -> Option<String> {
        self.attributes
            .iter()
            .find(|attr| attr.name == name)
            .and_then(|attr| attr.value.clone())
            .filter(|value| !value.trim().is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct GridAttrXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@value", default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LatLonBoxXml {
    west: f64,
    east: f64,
    south: f64,
    north: f64,
}

#[derive(Debug, Deserialize)]
struct TimeSpanXml {
    begin: String,
    end: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const CATALOG_TOP: &str = include_str!("fixtures/gdex_catalog_top.xml");
    const CATALOG_LEAF: &str = include_str!("fixtures/gdex_catalog_leaf.xml");
    const NCSS_DATASET: &str = include_str!("fixtures/gdex_ncss_dataset.xml");
    const CATALOG_ERA20C_TOP: &str = include_str!("fixtures/gdex_catalog_era20c_top.xml");
    const CATALOG_ERA20C_DECADE: &str = include_str!("fixtures/gdex_catalog_era20c_decade.xml");

    /// Tiny deterministic HTTP/1.1 server for download protocol tests. Each
    /// response corresponds to one request (normally HEAD, then GET) and closes
    /// the connection so the blocking client's pool cannot hide a request.
    fn spawn_http_server(responses: Vec<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let address = listener.local_addr().expect("local test server address");
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept local test request");
                let mut request = [0u8; 8192];
                let _ = stream.read(&mut request);
                stream
                    .write_all(&response)
                    .expect("write local test response");
                stream.flush().expect("flush local test response");
            }
        });
        format!("http://{address}/file")
    }

    fn seed_partial(url: &str, dest: &Path, bytes: &[u8], total: Option<u64>) -> (PathBuf, PathBuf) {
        let canonical_url = canonical_download_url(url).expect("test URL canonicalizes");
        let partial = download_partial_path(url, dest).expect("test partial path");
        let sidecar_path = partial_sidecar_path(&partial).expect("test sidecar path");
        fs::write(&partial, bytes).expect("seed partial bytes");
        let remote = RemoteObjectMetadata {
            total_bytes: total,
            etag: None,
            last_modified: None,
        };
        let sidecar = DownloadSidecar::new(&canonical_url, Some(&remote));
        write_download_sidecar(&sidecar_path, &sidecar).expect("seed partial identity");
        (partial, sidecar_path)
    }

    const TOP_URL: &str = "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/catalog.xml";
    const LEAF_URL: &str =
        "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/208001/catalog.xml";
    const ERA20C_TOP_URL: &str =
        "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d626000/catalog.xml";
    const ERA20C_DECADE_URL: &str = "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d626000/e20c.oper.an.sfc.3hr/1900_1909/catalog.xml";

    #[test]
    fn bounded_worker_pool_visits_every_item() {
        let items: Vec<usize> = (0..32).collect();
        let seen = std::sync::atomic::AtomicUsize::new(0);
        for_each_concurrent(&items, 4, |_| {
            seen.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .expect("all work succeeds");
        assert_eq!(seen.load(Ordering::Relaxed), items.len());
    }

    #[test]
    fn bounded_worker_pool_propagates_worker_error() {
        let items: Vec<usize> = (0..8).collect();
        let result = for_each_concurrent(&items, 3, |item| {
            if *item == 2 {
                Err(gdex_error("worker failed"))
            } else {
                Ok(())
            }
        });
        assert!(
            matches!(result, Err(GdexError::Io(err)) if err.to_string().contains("worker failed"))
        );
    }

    #[test]
    fn dataset_registry_ids_are_unique_and_urls_well_formed() {
        // The default (first) entry is CONUS II — the pre-picker dataset whose
        // downloads are grandfathered flat in the cache root.
        assert_eq!(GDEX_DATASETS[0].id, "d612005");

        let mut seen = HashSet::new();
        for dataset in GDEX_DATASETS {
            assert!(seen.insert(dataset.id), "duplicate id {}", dataset.id);
            assert_eq!(
                dataset.catalog_url(),
                format!("{CATALOG_BASE}files/g/{}/catalog.xml", dataset.id)
            );
            assert_eq!(dataset_by_id(dataset.id), Some(dataset));
            assert!(!dataset.label.is_empty());
            assert!(!dataset.attribution.is_empty());
        }
        assert_eq!(dataset_by_id("d000000"), None);

        // The two shipped datasets, by exact URL (verified live 2026-07-09).
        assert_eq!(dataset_catalog_url("d612005"), TOP_URL);
        assert_eq!(dataset_catalog_url("d626000"), ERA20C_TOP_URL);
    }

    #[test]
    fn dataset_id_parses_from_url_path_shapes() {
        assert_eq!(
            dataset_id_from_url_path(
                "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
            ),
            Some("d612005")
        );
        assert_eq!(
            dataset_id_from_url_path(
                "files/g/d626000/e20c.oper.an.sfc.3hr/1900_1909/e20c.oper.an.sfc.3hr.128_151_msl.regn80sc.1900010100_1900123121.grb"
            ),
            Some("d626000")
        );
        // The bare scan root and non-TDS shapes yield nothing.
        assert_eq!(dataset_id_from_url_path("files/g/"), None);
        assert_eq!(dataset_id_from_url_path("other/d612005/x.nc"), None);
        assert_eq!(dataset_id_from_url_path("files/g/../outside.nc"), None);
        assert_eq!(dataset_id_from_url_path("files/g/./inside.nc"), None);
        assert_eq!(dataset_id_from_url_path("files/g/d626000%2F../x.grb"), None);
        assert_eq!(dataset_id_from_url_path("files/g/d626000\\..\\x.grb"), None);
    }

    #[test]
    fn public_crawl_dataset_ids_are_one_safe_component() {
        for valid in ["d612005", "dataset_2", "ERA-20C"] {
            assert!(validate_dataset_id(valid).is_ok(), "{valid} should be valid");
        }
        for invalid in [
            "",
            ".",
            "..",
            "../outside",
            "folder/d612005",
            "folder\\d612005",
            "d612005%2F..",
            "with space",
        ] {
            assert!(
                validate_dataset_id(invalid).is_err(),
                "{invalid:?} must not reach a URL or cache path"
            );
        }
    }

    #[test]
    fn partial_path_keeps_full_filename_and_binds_canonical_url() {
        let dest_nc = Path::new("cache/field.nc");
        let with_fragment = download_partial_path(
            "https://example.test/data/file.nc?run=1#display-only",
            dest_nc,
        )
        .expect("valid URL");
        let without_fragment =
            download_partial_path("https://example.test/data/file.nc?run=1", dest_nc)
                .expect("valid URL");
        assert_eq!(with_fragment, without_fragment, "fragments are not HTTP identity");
        assert!(
            with_fragment
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("field.nc.") && name.ends_with(".download"))
        );

        let other_query =
            download_partial_path("https://example.test/data/file.nc?run=2", dest_nc)
                .expect("valid URL");
        let other_extension = download_partial_path(
            "https://example.test/data/file.nc?run=1",
            Path::new("cache/field.grb"),
        )
        .expect("valid URL");
        assert_ne!(with_fragment, other_query, "query changes remote identity");
        assert_ne!(with_fragment, other_extension, "full destination name is retained");
    }

    #[test]
    fn validator_checks_reject_same_size_different_objects() {
        let canonical = "https://example.test/file.nc";
        let original = RemoteObjectMetadata {
            total_bytes: Some(100),
            etag: Some("\"revision-a\"".to_owned()),
            last_modified: Some("Wed, 08 Jul 2026 12:00:00 GMT".to_owned()),
        };
        let sidecar = DownloadSidecar::new(canonical, Some(&original));
        assert!(sidecar_matches_remote(&sidecar, canonical, &original));

        let changed_etag = RemoteObjectMetadata {
            total_bytes: Some(100),
            etag: Some("\"revision-b\"".to_owned()),
            last_modified: original.last_modified.clone(),
        };
        assert!(
            !sidecar_matches_remote(&sidecar, canonical, &changed_etag),
            "equal Content-Length is not object identity"
        );
        let changed_date_without_etag = RemoteObjectMetadata {
            total_bytes: Some(100),
            etag: None,
            last_modified: Some("Thu, 09 Jul 2026 12:00:00 GMT".to_owned()),
        };
        assert!(
            !sidecar_matches_remote(&sidecar, canonical, &changed_date_without_etag),
            "Last-Modified is the fallback validator when ETag is absent"
        );
        assert!(!sidecar_matches_remote(
            &sidecar,
            "https://example.test/other.nc",
            &original
        ));
    }

    #[test]
    fn era20c_top_catalog_lists_nine_subtrees_and_drops_the_dump_leaf() {
        let parsed = parse_catalog(CATALOG_ERA20C_TOP, ERA20C_TOP_URL).expect("era20c top parses");
        // The root's only <dataset urlPath> is the extensionless scan `dump`,
        // which the extension whitelist drops.
        assert!(parsed.leaves.is_empty(), "dump must not survive as a leaf");
        assert_eq!(parsed.child_catalog_urls.len(), 9, "nine e20c subtrees");
        for expected in [
            "/e20c.oper.an.sfc.3hr/catalog.xml",
            "/e20c.oper.an.sfc.6hr/catalog.xml",
            "/e20c.oper.an.pl.3hr/catalog.xml",
            "/e20c.oper.invariant/catalog.xml",
        ] {
            assert!(
                parsed
                    .child_catalog_urls
                    .iter()
                    .any(|url| url.ends_with(expected)),
                "missing subtree {expected}"
            );
        }
        assert!(
            parsed
                .child_catalog_urls
                .iter()
                .all(|url| url.contains("/files/g/d626000/")),
            "every child resolves inside the d626000 tree"
        );
    }

    #[test]
    fn era20c_decade_catalog_keeps_grb_leaves_with_size_and_date() {
        let parsed =
            parse_catalog(CATALOG_ERA20C_DECADE, ERA20C_DECADE_URL).expect("decade parses");
        assert!(parsed.child_catalog_urls.is_empty(), "a decade dir is flat");
        assert_eq!(parsed.leaves.len(), 3);

        let msl = parsed
            .leaves
            .iter()
            .find(|leaf| leaf.name.contains("128_151_msl"))
            .expect("msl leaf present");
        assert_eq!(
            msl.url_path,
            "files/g/d626000/e20c.oper.an.sfc.3hr/1900_1909/e20c.oper.an.sfc.3hr.128_151_msl.regn80sc.1900010100_1900123121.grb"
        );
        assert_eq!(
            msl.download_url,
            format!("{FILESERVER_BASE}{}", msl.url_path),
            "fileServer download URL builds for .grb exactly as for .nc"
        );
        // <dataSize units="Mbytes">299.3</dataSize> -> decimal bytes.
        assert_eq!(msl.size_bytes, Some(299_300_000));
        assert_eq!(msl.date.as_deref(), Some("2014-11-04T18:47:44Z"));
    }

    #[test]
    fn top_catalog_discovers_five_subtrees_and_no_leaves() {
        let parsed = parse_catalog(CATALOG_TOP, TOP_URL).expect("top catalog parses");
        assert!(parsed.leaves.is_empty(), "the scan root has no file leaves");
        assert_eq!(
            parsed.child_catalog_urls,
            vec![
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/INVARIANT/catalog.xml"
                    .to_owned(),
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/catalog.xml"
                    .to_owned(),
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future3D/catalog.xml"
                    .to_owned(),
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/hist2D/catalog.xml"
                    .to_owned(),
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/hist3D/catalog.xml"
                    .to_owned(),
            ],
            "the five CONUS II subtrees, resolved relative to the catalog URL"
        );
    }

    #[test]
    fn leaf_catalog_keeps_data_files_and_drops_the_dump_entry() {
        let parsed = parse_catalog(CATALOG_LEAF, LEAF_URL).expect("leaf catalog parses");
        assert!(
            parsed.child_catalog_urls.is_empty(),
            "a leaf catalog has no sub-catalogs"
        );
        // Six real .nc leaves; the synthetic no-extension `dump` is dropped.
        assert_eq!(parsed.leaves.len(), 6, "dump entry must be filtered out");
        assert!(
            !parsed
                .leaves
                .iter()
                .any(|leaf| leaf.url_path.ends_with("dump")),
            "the dump entry leaked through the extension whitelist"
        );

        let first = &parsed.leaves[0];
        assert_eq!(first.name, "wrf2d_d01_2080-01-01_00:00:00.nc");
        assert_eq!(
            first.url_path,
            "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
        );
        assert_eq!(
            first.download_url,
            "https://tds.gdex.ucar.edu/thredds/fileServer/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
        );
        assert_eq!(
            first.ncss_url,
            "https://tds.gdex.ucar.edu/thredds/ncss/grid/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
        );
        assert_eq!(first.size_bytes, Some(163_700_000));
        assert_eq!(first.date.as_deref(), Some("2024-03-25T21:34:51.095Z"));
    }

    #[test]
    fn relative_hrefs_resolve_against_the_catalog_url() {
        assert_eq!(
            resolve_relative(TOP_URL, "INVARIANT/catalog.xml"),
            "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/INVARIANT/catalog.xml"
        );
        // A parent-relative href collapses `..`.
        assert_eq!(
            resolve_relative(LEAF_URL, "../209912/catalog.xml"),
            "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/209912/catalog.xml"
        );
        // An absolute external link passes through unchanged (and would be
        // filtered by is_tds_catalog_url downstream).
        assert_eq!(
            resolve_relative(TOP_URL, "https://example.org/other.xml"),
            "https://example.org/other.xml"
        );
        assert!(!is_tds_catalog_url("https://example.org/other.xml"));
    }

    #[test]
    fn data_extension_whitelist_matches_expected_types() {
        assert!(has_data_extension("a/b/wrf2d_d01.nc"));
        assert!(has_data_extension("x.grb"));
        assert!(has_data_extension("x.grib"));
        assert!(has_data_extension("x.grb2"));
        assert!(has_data_extension("X.NC"));
        assert!(!has_data_extension("files/g/d626000/dump"));
        assert!(!has_data_extension("notes.txt"));
    }

    #[test]
    fn colon_sanitized_to_underscore_for_local_path() {
        assert_eq!(
            sanitize_leaf_filename("wrf2d_d01_2080-01-01_00:00:00.nc"),
            "wrf2d_d01_2080-01-01_00_00_00.nc"
        );
        let leaf = Leaf {
            name: "wrf2d_d01_2080-01-01_00:00:00.nc".to_owned(),
            url_path: "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc".to_owned(),
            download_url: String::new(),
            ncss_url: String::new(),
            size_bytes: None,
            date: None,
        };
        let path = local_path_for_leaf(Path::new("cache"), &leaf);
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("wrf2d_d01_2080-01-01_00_00_00.nc")
        );
    }

    #[test]
    fn untrusted_leaf_names_stay_one_safe_component_below_cache() {
        let cases = [
            ("", "_download"),
            (".", "_download"),
            ("..", "_download"),
            ("...  ", "_download"),
            ("../outside.nc", ".._outside.nc"),
            ("folder\\outside.nc", "folder_outside.nc"),
            ("field.nc. ", "field.nc"),
            ("CON", "_CON"),
            ("con.nc", "_con.nc"),
            ("aux .nc", "_aux .nc"),
            ("LPT9.grb", "_LPT9.grb"),
            ("com10.nc", "com10.nc"),
        ];
        let cache = Path::new("cache-root");
        for (untrusted, expected) in cases {
            let safe = sanitize_leaf_filename(untrusted);
            assert_eq!(safe, expected, "input {untrusted:?}");
            let components = Path::new(&safe).components().collect::<Vec<_>>();
            assert!(
                matches!(components.as_slice(), [Component::Normal(_)]),
                "{untrusted:?} sanitized to non-normal path {safe:?}"
            );
            let leaf = Leaf {
                name: untrusted.to_owned(),
                url_path: String::new(),
                download_url: String::new(),
                ncss_url: String::new(),
                size_bytes: None,
                date: None,
            };
            let path = local_path_for_leaf(cache, &leaf);
            assert_eq!(path.parent(), Some(cache), "input {untrusted:?}");
            assert_eq!(path.file_name().and_then(|name| name.to_str()), Some(expected));
        }
    }

    #[test]
    fn ncss_dataset_parses_variables_bbox_and_timespan() {
        let dataset = parse_ncss_dataset(NCSS_DATASET).expect("ncss dataset parses");
        let names: Vec<&str> = dataset
            .variables
            .iter()
            .map(|var| var.name.as_str())
            .collect();
        assert_eq!(names, vec!["ACDEWC", "T2", "TK", "U", "V"]);

        let t2 = dataset
            .variables
            .iter()
            .find(|var| var.name == "T2")
            .expect("T2 present");
        assert_eq!(t2.units.as_deref(), Some("K"));
        assert_eq!(t2.description.as_deref(), Some("TEMP at 2 M"));

        let bbox = dataset.lat_lon_box.expect("LatLonBox present");
        assert!((bbox.west - (-156.8905)).abs() < 1e-6);
        assert!((bbox.east - (-40.2612)).abs() < 1e-6);
        assert!((bbox.south - 15.0072).abs() < 1e-6);
        assert!((bbox.north - 73.2915).abs() < 1e-6);

        let span = dataset.time_span.expect("TimeSpan present");
        assert_eq!(span.begin, "2080-01-01T00:00:00Z");
        assert_eq!(span.end, "2080-01-01T00:00:00Z");
    }

    #[test]
    fn ncss_subset_url_is_deterministic_and_requests_classic_netcdf() {
        let url_path = "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc";

        // Vars only (no spatial/temporal narrowing).
        let vars_only = NcssSubset {
            vars: vec!["T2".to_owned()],
            ..NcssSubset::default()
        };
        assert_eq!(
            ncss_subset_url(url_path, &vars_only),
            "https://tds.gdex.ucar.edu/thredds/ncss/grid/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc?var=T2&accept=netcdf"
        );

        // Full subset: var + bbox + single time. Colons in the time value are
        // percent-encoded; accept=netcdf (NOT netcdf4) trails.
        let full = NcssSubset {
            vars: vec!["T2".to_owned()],
            bbox: Some(LatLonBox {
                north: 45.0,
                south: 40.0,
                east: -95.0,
                west: -100.0,
            }),
            time: Some("2080-01-01T00:00:00Z".to_owned()),
            ..NcssSubset::default()
        };
        assert_eq!(
            ncss_subset_url(url_path, &full),
            "https://tds.gdex.ucar.edu/thredds/ncss/grid/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc\
             ?var=T2&north=45&south=40&east=-95&west=-100&time=2080-01-01T00%3A00%3A00Z&accept=netcdf"
        );

        // A time range is honored when no single time is set.
        let ranged = NcssSubset {
            vars: vec!["T2".to_owned(), "U".to_owned()],
            time_start: Some("2080-01-01T00:00:00Z".to_owned()),
            time_end: Some("2080-01-01T06:00:00Z".to_owned()),
            ..NcssSubset::default()
        };
        let url = ncss_subset_url(url_path, &ranged);
        assert!(url.contains("var=T2&var=U"));
        assert!(url.contains("time_start=2080-01-01T00%3A00%3A00Z"));
        assert!(url.contains("time_end=2080-01-01T06%3A00%3A00Z"));
        assert!(url.ends_with("&accept=netcdf"));
    }

    #[test]
    fn response_classification_retries_5xx_and_empty_bodies() {
        assert_eq!(classify_response(503, false), ResponseClass::Retry);
        assert_eq!(classify_response(500, false), ResponseClass::Retry);
        assert_eq!(classify_response(200, true), ResponseClass::Retry);
        assert_eq!(classify_response(200, false), ResponseClass::Accept);
        assert_eq!(classify_response(404, false), ResponseClass::Fatal);
        assert_eq!(classify_response(400, false), ResponseClass::Fatal);
    }

    #[test]
    fn with_retry_stops_on_first_success() {
        use std::cell::Cell;
        // Two transient failures, then success on the third attempt.
        let calls = Cell::new(0usize);
        let no_sleep = [StdDuration::ZERO, StdDuration::ZERO, StdDuration::ZERO];
        let result: Result<&str> = with_retry(&no_sleep, |_| {
            let n = calls.get();
            calls.set(n + 1);
            if n < 2 {
                Attempt::Retry(gdex_error("transient"))
            } else {
                Attempt::Accept("ok")
            }
        });
        assert_eq!(result.expect("succeeds by the third try"), "ok");
        assert_eq!(calls.get(), 3, "must stop the instant it succeeds");
    }

    #[test]
    fn with_retry_returns_fatal_immediately() {
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let no_sleep = [StdDuration::ZERO, StdDuration::ZERO];
        let result: Result<&str> = with_retry(&no_sleep, |_| {
            calls.set(calls.get() + 1);
            Attempt::Fatal(gdex_error("permanent"))
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 1, "a fatal answer is not retried");
    }

    #[test]
    fn with_retry_exhausts_then_errors() {
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let no_sleep = [StdDuration::ZERO, StdDuration::ZERO];
        let result: Result<&str> = with_retry(&no_sleep, |_| -> Attempt<&str> {
            calls.set(calls.get() + 1);
            Attempt::Retry(gdex_error("always transient"))
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 3, "one initial try plus two backoff retries");
    }

    /// Yields one prebuilt chunk per `read` call; optionally sets a shared
    /// cancel flag as a side effect of returning the k-th chunk (1-based) —
    /// the user pressing Cancel while a chunk is in flight.
    struct ChunkReader {
        chunks: Vec<Vec<u8>>,
        next: usize,
        cancel_after: Option<(usize, std::sync::Arc<AtomicBool>)>,
    }

    impl ChunkReader {
        fn new(
            chunks: Vec<Vec<u8>>,
            cancel_after: Option<(usize, std::sync::Arc<AtomicBool>)>,
        ) -> Self {
            Self {
                chunks,
                next: 0,
                cancel_after,
            }
        }
    }

    impl Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.get(self.next) else {
                return Ok(0);
            };
            assert!(buf.len() >= chunk.len(), "test chunks fit the copy buffer");
            buf[..chunk.len()].copy_from_slice(chunk);
            self.next += 1;
            if let Some((after, flag)) = &self.cancel_after {
                if self.next == *after {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            Ok(chunk.len())
        }
    }

    /// Records written bytes and counts `flush` calls — the cancel contract
    /// requires the partial to be flushed before the copy returns.
    #[derive(Default)]
    struct FlushTracking {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for FlushTracking {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn copy_with_cancel_without_cancel_copies_everything() {
        let chunks: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 100]).collect();
        let expected: Vec<u8> = chunks.concat();
        let mut reader = ChunkReader::new(chunks, None);
        let mut writer = FlushTracking::default();

        let end = copy_with_cancel(&mut reader, &mut writer, &AtomicBool::new(false))
            .expect("in-memory copy cannot fail");

        assert_eq!(end, CopyEnd::Complete(expected.len() as u64));
        assert_eq!(
            writer.bytes, expected,
            "a never-cancelled copy is byte-identical"
        );
        assert!(writer.flushes >= 1, "completion flushes the writer");
    }

    #[test]
    fn copy_with_cancel_stops_between_chunks_and_keeps_flushed_partial() {
        use std::sync::Arc;
        let chunks: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 100]).collect();
        let expected_partial: Vec<u8> = chunks[..2].concat();
        let flag = Arc::new(AtomicBool::new(false));
        // The flag is set while chunk 2 is in flight: that chunk still lands,
        // then the check at the top of the next iteration stops the copy.
        let mut reader = ChunkReader::new(chunks, Some((2, flag.clone())));
        let mut writer = FlushTracking::default();

        let end =
            copy_with_cancel(&mut reader, &mut writer, &flag).expect("in-memory copy cannot fail");

        assert_eq!(end, CopyEnd::Cancelled(expected_partial.len() as u64));
        assert_eq!(
            writer.bytes, expected_partial,
            "every byte read before the cancel is written, none after"
        );
        assert!(
            writer.flushes >= 1,
            "the partial is flushed before returning"
        );
        assert_eq!(
            reader.next, 2,
            "no further chunks are pulled after the cancel"
        );
    }

    #[test]
    fn cancelled_copy_keeps_partial_temp_that_resumes_to_identical_bytes() {
        use std::sync::Arc;
        let dir = unique_temp_dir("gdex-cancel-partial");
        fs::create_dir_all(&dir).expect("temp dir");
        // Exercises the same create/append byte semantics as the URL-bound
        // partial used by download_to_path.
        let temp_path = dir.join("wrf2d_test.nc.download");
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let chunk = 4096;

        // Pass 1 — a fresh download (the 200 path: File::create) cancelled
        // after 10 chunks. The partial temp must survive, exactly as written.
        let chunks: Vec<Vec<u8>> = payload.chunks(chunk).map(<[u8]>::to_vec).collect();
        let flag = Arc::new(AtomicBool::new(false));
        let mut reader = ChunkReader::new(chunks, Some((10, flag.clone())));
        let mut file = fs::File::create(&temp_path).expect("create temp");
        let end = copy_with_cancel(&mut reader, &mut file, &flag).expect("file copy ok");
        drop(file);
        let have = (10 * chunk) as u64;
        assert_eq!(end, CopyEnd::Cancelled(have));
        assert_eq!(
            fs::metadata(&temp_path)
                .expect("temp survives the cancel")
                .len(),
            have,
            "the partial temp is kept on disk with every flushed byte"
        );

        // Pass 2 — the resume (the 206 path: append the remainder), as the
        // next download_to_path call does after its Range request.
        let rest: Vec<Vec<u8>> = payload[have as usize..]
            .chunks(chunk)
            .map(<[u8]>::to_vec)
            .collect();
        let mut reader = ChunkReader::new(rest, None);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&temp_path)
            .expect("append temp");
        let end = copy_with_cancel(&mut reader, &mut file, &AtomicBool::new(false))
            .expect("file copy ok");
        drop(file);
        assert_eq!(end, CopyEnd::Complete(payload.len() as u64 - have));
        assert_eq!(
            fs::read(&temp_path).expect("read back"),
            payload,
            "cancel + resume reassembles the exact original bytes"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_range_parser_is_strict_about_shape_and_bounds() {
        assert_eq!(
            parse_content_range("bytes 5-9/10"),
            Some(ParsedContentRange::Satisfied {
                start: 5,
                end: 9,
                total: 10,
            })
        );
        assert_eq!(
            parse_content_range("bytes */10"),
            Some(ParsedContentRange::Unsatisfied { total: 10 })
        );
        assert_eq!(parse_content_range("bytes 9-5/10"), None);
        assert_eq!(parse_content_range("bytes 5-10/10"), None);
        assert_eq!(parse_content_range("bytes 5-9/*"), None);
        assert_eq!(parse_content_range("items 5-9/10"), None);
    }

    #[test]
    fn completed_cache_requires_url_bound_sidecar_not_length_alone() {
        let dir = unique_temp_dir("gdex-final-identity");
        fs::create_dir_all(&dir).expect("temp dir");
        let dest = dir.join("field.nc");
        let sidecar_path = final_sidecar_path(&dest).expect("final sidecar path");
        fs::write(&dest, b"hello").expect("seed final bytes");
        assert_eq!(
            trusted_final_cache_len(&dest, &sidecar_path, "https://example.test/a", None)
                .expect("cache probe"),
            None,
            "same-length anonymous files are never trusted"
        );

        let remote = RemoteObjectMetadata {
            total_bytes: Some(5),
            etag: Some("\"a\"".to_owned()),
            last_modified: None,
        };
        let mut sidecar = DownloadSidecar::new("https://example.test/a", Some(&remote));
        sidecar.final_size = Some(5);
        write_download_sidecar(&sidecar_path, &sidecar).expect("write final identity");
        assert_eq!(
            trusted_final_cache_len(
                &dest,
                &sidecar_path,
                "https://example.test/a",
                Some(&remote)
            )
            .expect("cache probe"),
            Some(5)
        );

        let changed = RemoteObjectMetadata {
            total_bytes: Some(5),
            etag: Some("\"b\"".to_owned()),
            last_modified: None,
        };
        assert_eq!(
            trusted_final_cache_len(
                &dest,
                &sidecar_path,
                "https://example.test/a",
                Some(&changed)
            )
            .expect("cache probe"),
            None,
            "same length with a changed ETag is a cache miss"
        );
        assert_eq!(
            trusted_final_cache_len(&dest, &sidecar_path, "https://example.test/b", None)
                .expect("cache probe"),
            None,
            "a sidecar for another URL is a cache miss"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_partial_is_quarantined_before_a_fresh_get() {
        let url = spawn_http_server(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfresh".to_vec(),
        ]);
        let dir = unique_temp_dir("gdex-oversized-partial");
        fs::create_dir_all(&dir).expect("temp dir");
        let dest = dir.join("field.nc");
        let (partial, _) = seed_partial(&url, &dest, b"too-long", Some(5));

        let outcome = download_to_path(&url, &dest).expect("fresh restart succeeds");

        assert!(!outcome.resumed);
        assert_eq!(fs::read(&dest).expect("fresh file installed"), b"fresh");
        assert!(!partial.exists(), "oversized bytes cannot remain resumable");
        assert!(
            fs::read_dir(&dir)
                .expect("read temp dir")
                .filter_map(|entry| entry.ok())
                .any(|entry| entry.file_name().to_string_lossy().contains(".invalid-"))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn valid_206_resume_appends_exact_requested_tail() {
        let url = spawn_http_server(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 5-9/10\r\nContent-Length: 5\r\nConnection: close\r\n\r\nworld".to_vec(),
        ]);
        let dir = unique_temp_dir("gdex-valid-206");
        fs::create_dir_all(&dir).expect("temp dir");
        let dest = dir.join("field.nc");
        let (temp, sidecar) = seed_partial(&url, &dest, b"hello", Some(10));

        let outcome = download_to_path(&url, &dest).expect("valid resume");

        assert!(outcome.resumed);
        assert_eq!(outcome.bytes, 10);
        assert_eq!(fs::read(&dest).expect("read installed file"), b"helloworld");
        assert!(!temp.exists());
        assert!(!sidecar.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mismatched_206_start_is_quarantined_then_restarted_cleanly() {
        let url = spawn_http_server(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4/10\r\nContent-Length: 5\r\nConnection: close\r\n\r\nwrong".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfresh".to_vec(),
        ]);
        let dir = unique_temp_dir("gdex-wrong-206-start");
        fs::create_dir_all(&dir).expect("temp dir");
        let dest = dir.join("field.nc");
        let (temp, _) = seed_partial(&url, &dest, b"hello", Some(10));

        let outcome = download_to_path(&url, &dest).expect("clean restart succeeds");

        assert!(!outcome.resumed);
        assert_eq!(fs::read(&dest).expect("fresh file installed"), b"fresh");
        assert!(!temp.exists(), "unsafe original path is no longer resumable");
        assert!(
            fs::read_dir(&dir)
                .expect("read temp dir")
                .filter_map(|entry| entry.ok())
                .any(|entry| entry.file_name().to_string_lossy().contains(".invalid-")),
            "unsafe partial is retained under a quarantine name"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mismatched_206_body_length_is_quarantined_then_restarted() {
        let url = spawn_http_server(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 5-7/10\r\nConnection: close\r\n\r\nworld".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfresh".to_vec(),
        ]);
        let dir = unique_temp_dir("gdex-wrong-206-body");
        fs::create_dir_all(&dir).expect("temp dir");
        let dest = dir.join("field.nc");
        let (temp, _) = seed_partial(&url, &dest, b"hello", Some(10));

        let outcome = download_to_path(&url, &dest).expect("clean restart succeeds");

        assert!(!outcome.resumed);
        assert_eq!(fs::read(&dest).expect("fresh file installed"), b"fresh");
        assert!(!temp.exists(), "bad append is not resumable");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_416_total_promotes_only_the_complete_temp() {
        let url = spawn_http_server(vec![
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */5\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        ]);
        let dir = unique_temp_dir("gdex-valid-416");
        fs::create_dir_all(&dir).expect("temp dir");
        let dest = dir.join("field.nc");
        seed_partial(&url, &dest, b"hello", Some(5));

        let outcome = download_to_path(&url, &dest).expect("matching 416 total");

        assert!(outcome.resumed);
        assert_eq!(fs::read(&dest).expect("read installed file"), b"hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unproved_416_quarantines_partial_and_preserves_old_until_restart_finishes() {
        let url = spawn_http_server(vec![
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfresh".to_vec(),
        ]);
        let dir = unique_temp_dir("gdex-unproved-416");
        fs::create_dir_all(&dir).expect("temp dir");
        let dest = dir.join("field.nc");
        fs::write(&dest, b"known-old").expect("seed old destination");
        let (temp, _) = seed_partial(&url, &dest, b"junk", None);

        let outcome = download_to_path(&url, &dest).expect("safe full restart succeeds");

        assert!(!outcome.resumed);
        assert_eq!(fs::read(&dest).expect("replacement installed"), b"fresh");
        assert!(!temp.exists(), "unproved partial was quarantined");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_length_verifies_full_download_when_head_is_unavailable() {
        let url = spawn_http_server(vec![
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec(),
        ]);
        let dir = unique_temp_dir("gdex-get-length");
        fs::create_dir_all(&dir).expect("temp dir");
        let dest = dir.join("field.nc");

        let outcome = download_to_path(&url, &dest).expect("GET length is authoritative");

        assert_eq!(outcome.bytes, 5);
        assert_eq!(fs::read(&dest).expect("read installed file"), b"hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacement_failure_rolls_old_destination_back() {
        let dir = unique_temp_dir("gdex-replace-rollback");
        fs::create_dir_all(&dir).expect("temp dir");
        let dest = dir.join("field.nc");
        let missing_temp = dir.join("missing.download");
        fs::write(&dest, b"known-old").expect("seed old destination");

        replace_validated_temp(&missing_temp, &dest)
            .expect_err("missing validated temp must fail installation");

        assert_eq!(fs::read(&dest).expect("old destination restored"), b"known-old");
        let leftovers = fs::read_dir(&dir)
            .expect("read temp dir")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(leftovers, vec![dest.file_name().expect("destination name").to_os_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crawl_cache_round_trips_through_disk() {
        let dir = unique_temp_dir("gdex-cache-roundtrip");
        fs::create_dir_all(&dir).expect("temp dir");

        // A cold read of a never-written cache is a miss.
        let cache_path = catalog_cache_path(&dir, "d612005");
        assert_eq!(
            read_catalog_cache(&cache_path).expect("cold read ok"),
            None,
            "missing cache reads as None"
        );

        let cache = CatalogCache {
            dataset_id: "d612005".to_owned(),
            crawled_at: "2026-07-09T00:00:00+00:00".to_owned(),
            leaves: vec![Leaf {
                name: "wrf2d_d01_2080-01-01_00:00:00.nc".to_owned(),
                url_path: "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
                    .to_owned(),
                download_url: format!(
                    "{FILESERVER_BASE}files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
                ),
                ncss_url: format!(
                    "{NCSS_GRID_BASE}files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
                ),
                size_bytes: Some(163_700_000),
                date: Some("2024-03-25T21:34:51.095Z".to_owned()),
            }],
        };
        write_catalog_cache(&cache_path, &cache).expect("write cache");
        let read_back = read_catalog_cache(&cache_path)
            .expect("warm read ok")
            .expect("cache present after write");
        assert_eq!(read_back, cache, "cache must round-trip byte-for-byte");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A unique scratch directory under the OS temp dir (no external dep).
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{tag}-{}-{nanos}", std::process::id()))
    }

    // -----------------------------------------------------------------------
    // Live proofs — run ONCE on a build node, never in CI (they hit the
    // network; keep them LIGHT — no 156 MB pulls).
    //   cargo test -p rw-gdex -- --ignored
    // -----------------------------------------------------------------------

    #[test]
    #[ignore = "live: crawls GDEX; run once on a node"]
    fn live_crawl_d612005_top_and_one_month() {
        // Top scan root -> the five CONUS II subtrees.
        let top =
            fetch_and_parse_catalog(&dataset_catalog_url("d612005")).expect("live top catalog");
        eprintln!(
            "live top: {} child catalogs, {} leaves",
            top.child_catalog_urls.len(),
            top.leaves.len()
        );
        assert_eq!(top.child_catalog_urls.len(), 5, "five subtrees");
        assert!(
            top.child_catalog_urls
                .iter()
                .any(|url| url.ends_with("/future2D/catalog.xml"))
        );

        // One real month leaf catalog -> its .nc leaves (light: ~240 KB XML).
        let month = fetch_and_parse_catalog(LEAF_URL).expect("live month catalog");
        eprintln!("live future2D/208001: {} leaves", month.leaves.len());
        assert!(
            month.leaves.len() > 100,
            "a month has hundreds of hourly files"
        );
        assert!(
            month
                .leaves
                .iter()
                .all(|leaf| leaf.download_url.starts_with(FILESERVER_BASE))
        );
        assert!(
            month
                .leaves
                .iter()
                .any(|leaf| leaf.name == "wrf2d_d01_2080-01-01_00:00:00.nc")
        );
    }

    #[test]
    #[ignore = "live: lists the ERA-20C catalog root + one decade; run once on a node"]
    fn live_era20c_catalog_root_and_one_decade() {
        // Root scan -> the nine e20c.oper.* subtrees; the extensionless scan
        // `dump` must not surface as a leaf (light: ~8 KB XML).
        let top =
            fetch_and_parse_catalog(&dataset_catalog_url("d626000")).expect("live era20c top");
        eprintln!(
            "live d626000 top: {} child catalogs, {} leaves",
            top.child_catalog_urls.len(),
            top.leaves.len()
        );
        assert_eq!(top.child_catalog_urls.len(), 9, "nine e20c subtrees");
        assert!(top.leaves.is_empty(), "dump filtered at the root");
        assert!(
            top.child_catalog_urls
                .iter()
                .any(|url| url.ends_with("/e20c.oper.an.sfc.3hr/catalog.xml"))
        );

        // One decade dir of the 3-hourly surface analysis (~390 KB XML):
        // hundreds of .grb leaves, every one a fileServer download URL with an
        // advertised size — what the picker's Download button consumes.
        let decade = fetch_and_parse_catalog(ERA20C_DECADE_URL).expect("live era20c decade");
        eprintln!(
            "live e20c.oper.an.sfc.3hr/1900_1909: {} leaves",
            decade.leaves.len()
        );
        assert!(
            decade.leaves.len() > 500,
            "a decade dir carries hundreds of per-variable year files"
        );
        assert!(
            decade
                .leaves
                .iter()
                .all(|leaf| leaf.url_path.to_ascii_lowercase().ends_with(".grb")
                    && leaf.download_url.starts_with(FILESERVER_BASE))
        );
        assert!(
            decade.leaves.iter().any(|leaf| leaf.name
                == "e20c.oper.an.sfc.3hr.128_151_msl.regn80sc.1900010100_1900123121.grb"),
            "the fixture's msl leaf exists live"
        );
        assert!(
            decade.leaves.iter().all(|leaf| leaf.size_bytes.is_some()),
            "decade leaves advertise sizes (drives the progress bar)"
        );
    }

    #[test]
    #[ignore = "live: NCSS subset download; run once on a node"]
    fn live_ncss_tiny_subset_is_valid_netcdf3() {
        let url_path = "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc";

        // Confirm the grid metadata parses live.
        let meta = fetch_ncss_dataset(url_path).expect("live ncss dataset.xml");
        eprintln!("live ncss vars: {}", meta.variables.len());
        assert!(meta.variables.iter().any(|var| var.name == "T2"));

        // Tiny subset: one var, small bbox, single time.
        let subset = NcssSubset {
            vars: vec!["T2".to_owned()],
            bbox: Some(LatLonBox {
                north: 45.0,
                south: 40.0,
                east: -95.0,
                west: -100.0,
            }),
            time: Some("2080-01-01T00:00:00Z".to_owned()),
            ..NcssSubset::default()
        };
        let url = ncss_subset_url(url_path, &subset);
        let dir = unique_temp_dir("gdex-ncss-subset");
        let dest = dir.join("subset_T2.nc");
        let outcome = download_to_path(&url, &dest).expect("live subset download");
        eprintln!("live subset: {} bytes -> {}", outcome.bytes, dest.display());

        let bytes = fs::read(&dest).expect("read subset");
        assert!(bytes.len() > 100, "subset should be non-trivial");
        // Classic NetCDF-3 magic: 'C' 'D' 'F' 0x01.
        assert_eq!(&bytes[..4], b"CDF\x01", "not a classic NetCDF-3 file");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "live: Range probe (1 KB, not a full download); run once on a node"]
    fn live_fileserver_supports_range_resume() {
        let url = "https://tds.gdex.ucar.edu/thredds/fileServer/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc";

        // HEAD advertises the full size (recon: ~156 MB).
        let total = head_content_length(url, &AtomicBool::new(false)).expect("HEAD Content-Length");
        eprintln!("live HEAD Content-Length: {total}");
        assert!(total > 100_000_000, "the 00:00 file is ~156 MB");

        // A 1 KB partial GET must return 206 with exactly 1024 bytes — proof
        // the server honors Range (so an interrupted big download can resume).
        // Retry through the server's transient 503s (see the doc's flakiness
        // note) so the proof is stable.
        let client = download_http_client().expect("construct download client");
        let response = with_retry(GDEX_RETRY_BACKOFFS, |_| match client
            .get(url)
            .header(RANGE, "bytes=0-1023")
            .send()
        {
            Err(err) => Attempt::Retry(GdexError::Http(err)),
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (500..600).contains(&status) {
                    Attempt::Retry(gdex_error(format!("range probe: status {status}")))
                } else {
                    Attempt::Accept(resp)
                }
            }
        })
        .expect("range GET");
        assert_eq!(
            response.status().as_u16(),
            206,
            "expected 206 Partial Content"
        );
        let body = response.bytes().expect("range body");
        eprintln!("live Range bytes=0-1023 -> {} bytes", body.len());
        assert_eq!(body.len(), 1024, "Range must yield exactly 1024 bytes");
    }
}
