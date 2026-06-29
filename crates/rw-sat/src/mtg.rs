//! EUMETSAT Meteosat Third Generation (MTG) collection metadata and
//! OpenSearch product discovery.
//!
//! The public OpenSearch endpoint can discover current MTG FCI/LI product IDs
//! without credentials. Downloading the product bytes remains credential-gated
//! by EUMETSAT's Data Store API, so this module deliberately stops at product
//! discovery and stable URL construction.

use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Public EUMETSAT Data Store OpenSearch endpoint.
pub const EUMETSAT_OPENSEARCH_URL: &str = "https://api.eumetsat.int/data/search-products/1.0.0/os";
/// Public collection browse endpoint.
pub const EUMETSAT_BROWSE_COLLECTIONS_URL: &str =
    "https://api.eumetsat.int/data/browse/collections";
/// Human product navigator base.
pub const EUMETSAT_PRODUCT_PAGE_URL: &str = "https://data.eumetsat.int/product";
/// Credentialed product-download endpoint base.
pub const EUMETSAT_DOWNLOAD_COLLECTIONS_URL: &str =
    "https://api.eumetsat.int/data/download/1.0.0/collections";
/// OAuth-style token endpoint used by EUMDAC.
pub const EUMETSAT_TOKEN_URL: &str = "https://api.eumetsat.int/token";
/// Environment variable used by the CLI for the EUMETSAT API consumer key.
pub const EUMETSAT_CONSUMER_KEY_ENV: &str = "EUMETSAT_CONSUMER_KEY";
/// Environment variable used by the CLI for the EUMETSAT API consumer secret.
pub const EUMETSAT_CONSUMER_SECRET_ENV: &str = "EUMETSAT_CONSUMER_SECRET";

/// MTG collections that are immediately relevant to live imagery/lightning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MtgCollection {
    FciL1cNormal,
    FciL1cHigh,
    LiLightningEventsFiltered,
    LiLightningGroups,
    LiLightningFlashes,
    LiAccumulatedFlashes,
    LiAccumulatedFlashArea,
    LiAccumulatedFlashRadiance,
}

impl MtgCollection {
    pub const ALL: &'static [MtgCollection] = &[
        MtgCollection::FciL1cNormal,
        MtgCollection::FciL1cHigh,
        MtgCollection::LiLightningEventsFiltered,
        MtgCollection::LiLightningGroups,
        MtgCollection::LiLightningFlashes,
        MtgCollection::LiAccumulatedFlashes,
        MtgCollection::LiAccumulatedFlashArea,
        MtgCollection::LiAccumulatedFlashRadiance,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            MtgCollection::FciL1cNormal => "fci-l1c",
            MtgCollection::FciL1cHigh => "fci-l1c-hr",
            MtgCollection::LiLightningEventsFiltered => "li-events",
            MtgCollection::LiLightningGroups => "li-groups",
            MtgCollection::LiLightningFlashes => "li-flashes",
            MtgCollection::LiAccumulatedFlashes => "li-accumulated-flashes",
            MtgCollection::LiAccumulatedFlashArea => "li-accumulated-flash-area",
            MtgCollection::LiAccumulatedFlashRadiance => "li-accumulated-flash-radiance",
        }
    }

    pub fn collection_id(self) -> &'static str {
        match self {
            MtgCollection::FciL1cNormal => "EO:EUM:DAT:0662",
            MtgCollection::FciL1cHigh => "EO:EUM:DAT:0665",
            MtgCollection::LiLightningEventsFiltered => "EO:EUM:DAT:0690",
            MtgCollection::LiLightningGroups => "EO:EUM:DAT:0782",
            MtgCollection::LiLightningFlashes => "EO:EUM:DAT:0691",
            MtgCollection::LiAccumulatedFlashes => "EO:EUM:DAT:0686",
            MtgCollection::LiAccumulatedFlashArea => "EO:EUM:DAT:0687",
            MtgCollection::LiAccumulatedFlashRadiance => "EO:EUM:DAT:0688",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            MtgCollection::FciL1cNormal => {
                "FCI Level 1c Normal Resolution Image Data - MTG - 0 degree"
            }
            MtgCollection::FciL1cHigh => "FCI Level 1c High Resolution Image Data - MTG - 0 degree",
            MtgCollection::LiLightningEventsFiltered => {
                "LI Lightning Events Filtered - MTG - 0 degree"
            }
            MtgCollection::LiLightningGroups => "LI Lightning Groups - MTG - 0 degree",
            MtgCollection::LiLightningFlashes => "LI Lightning Flashes - MTG - 0 degree",
            MtgCollection::LiAccumulatedFlashes => "LI Accumulated Flashes - MTG - 0 degree",
            MtgCollection::LiAccumulatedFlashArea => "LI Accumulated Flash Area - MTG - 0 degree",
            MtgCollection::LiAccumulatedFlashRadiance => {
                "LI Accumulated Flash Radiance - MTG - 0 degree"
            }
        }
    }

    pub fn instrument(self) -> &'static str {
        match self {
            MtgCollection::FciL1cNormal | MtgCollection::FciL1cHigh => "fci",
            _ => "li",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let normalized = normalize_token(value);
        Self::ALL.iter().copied().find(|collection| {
            normalize_token(collection.slug()) == normalized
                || normalize_token(collection.collection_id()) == normalized
        })
    }

    pub fn collection_browse_url(self) -> String {
        format!(
            "{}/{}",
            EUMETSAT_BROWSE_COLLECTIONS_URL,
            query_encode(collection_id(self))
        )
    }

    pub fn product_page_url(self) -> String {
        format!(
            "{}/{}",
            EUMETSAT_PRODUCT_PAGE_URL,
            query_encode(collection_id(self))
        )
    }

    pub fn product_download_url(self, product_id: &str) -> String {
        format!(
            "{}/{}/products/{}",
            EUMETSAT_DOWNLOAD_COLLECTIONS_URL,
            query_encode(collection_id(self)),
            query_encode(product_id)
        )
    }
}

impl fmt::Display for MtgCollection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.slug(), self.collection_id())
    }
}

/// Product-search request for EUMETSAT OpenSearch.
#[derive(Debug, Clone)]
pub struct MtgSearchRequest {
    pub collection: MtgCollection,
    pub dt_start: DateTime<Utc>,
    pub dt_end: DateTime<Utc>,
    pub count: usize,
}

impl MtgSearchRequest {
    pub fn new(
        collection: MtgCollection,
        dt_start: DateTime<Utc>,
        dt_end: DateTime<Utc>,
        count: usize,
    ) -> Self {
        Self {
            collection,
            dt_start,
            dt_end,
            count,
        }
    }

    pub fn url(&self) -> String {
        let count = self.count.clamp(1, 500);
        format!(
            "{base}?pi={pi}&dtstart={start}&dtend={end}&c={count}&sort=start%2Ctime%2C0&set=brief&format=json",
            base = EUMETSAT_OPENSEARCH_URL,
            pi = query_encode(self.collection.collection_id()),
            start = query_encode(&self.dt_start.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            end = query_encode(&self.dt_end.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtgProductLink {
    pub href: String,
    pub title: Option<String>,
    pub media_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtgProduct {
    pub id: String,
    pub date: Option<String>,
    pub updated: Option<String>,
    pub data_links: Vec<MtgProductLink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtgSearchResult {
    pub collection: MtgCollection,
    pub total_results: usize,
    pub products: Vec<MtgProduct>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EumetsatCredentials {
    pub consumer_key: String,
    pub consumer_secret: String,
}

impl EumetsatCredentials {
    pub fn new(consumer_key: impl Into<String>, consumer_secret: impl Into<String>) -> Self {
        Self {
            consumer_key: consumer_key.into(),
            consumer_secret: consumer_secret.into(),
        }
    }

    pub fn from_env() -> Result<Self, Box<dyn Error>> {
        let key = env::var(EUMETSAT_CONSUMER_KEY_ENV).map_err(|_| {
            boxed_error(format!(
                "missing {EUMETSAT_CONSUMER_KEY_ENV}; pass --consumer-key or set the env var"
            ))
        })?;
        let secret = env::var(EUMETSAT_CONSUMER_SECRET_ENV).map_err(|_| {
            boxed_error(format!(
                "missing {EUMETSAT_CONSUMER_SECRET_ENV}; pass --consumer-secret or set the env var"
            ))
        })?;
        Ok(Self::new(key, secret))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EumetsatAccessToken {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub expires_at_unix: u64,
}

impl EumetsatAccessToken {
    pub fn bearer_header(&self) -> String {
        format!("Bearer {}", self.access_token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedMtgProduct {
    pub collection: MtgCollection,
    pub product_id: String,
    pub path: PathBuf,
    pub filename: String,
    pub bytes: u64,
    pub content_length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MtgPackageManifest {
    pub package_path: PathBuf,
    pub package_filename: String,
    pub entry_count: usize,
    pub file_count: usize,
    pub netcdf_count: usize,
    pub fci_count: usize,
    pub li_count: usize,
    pub entries: Vec<MtgPackageEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MtgPackageEntry {
    pub name: String,
    pub safe_path: Option<String>,
    pub is_file: bool,
    pub size_bytes: u64,
    pub compressed_size: u64,
    pub compression: String,
    pub kind: String,
    pub instrument: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MtgUnpackResult {
    pub manifest: MtgPackageManifest,
    pub out_dir: PathBuf,
    pub netcdf_only: bool,
    pub extracted: Vec<MtgExtractedEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MtgExtractedEntry {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
}

pub fn search_products(
    agent: &ureq::Agent,
    request: &MtgSearchRequest,
) -> Result<MtgSearchResult, Box<dyn Error>> {
    let url = request.url();
    let mut response = agent.get(&url).call()?;
    let text = response.body_mut().read_to_string()?;
    let feed: SearchResponse = serde_json::from_str(&text)?;
    Ok(MtgSearchResult {
        collection: request.collection,
        total_results: feed.total_results.unwrap_or_default(),
        products: feed
            .features
            .unwrap_or_default()
            .into_iter()
            .map(MtgProduct::from)
            .collect(),
    })
}

pub fn inspect_package(path: &Path) -> Result<MtgPackageManifest, Box<dyn Error>> {
    let file = fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut entries = Vec::with_capacity(archive.len());

    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        let safe_path = entry
            .enclosed_name()
            .map(|path| path.to_string_lossy().replace('\\', "/"));
        let kind = classify_package_entry(&name).to_string();
        let instrument = infer_package_instrument(&name).map(str::to_string);
        entries.push(MtgPackageEntry {
            name,
            safe_path,
            is_file: entry.is_file(),
            size_bytes: entry.size(),
            compressed_size: entry.compressed_size(),
            compression: format!("{:?}", entry.compression()),
            kind,
            instrument,
        });
    }

    let file_count = entries.iter().filter(|entry| entry.is_file).count();
    let netcdf_count = entries
        .iter()
        .filter(|entry| entry.kind == "netcdf")
        .count();
    let fci_count = entries
        .iter()
        .filter(|entry| entry.instrument.as_deref() == Some("fci"))
        .count();
    let li_count = entries
        .iter()
        .filter(|entry| entry.instrument.as_deref() == Some("li"))
        .count();
    Ok(MtgPackageManifest {
        package_path: path.to_path_buf(),
        package_filename: path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string()),
        entry_count: entries.len(),
        file_count,
        netcdf_count,
        fci_count,
        li_count,
        entries,
    })
}

pub fn unpack_package(
    package_path: &Path,
    out_dir: &Path,
    netcdf_only: bool,
) -> Result<MtgUnpackResult, Box<dyn Error>> {
    fs::create_dir_all(out_dir)?;
    let file = fs::File::open(package_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut extracted = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        if netcdf_only && classify_package_entry(&name) != "netcdf" {
            continue;
        }
        let Some(safe_path) = entry.enclosed_name() else {
            continue;
        };
        let target = out_dir.join(safe_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&target)?;
        let bytes = io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
        extracted.push(MtgExtractedEntry {
            name,
            path: target,
            bytes,
        });
    }

    Ok(MtgUnpackResult {
        manifest: inspect_package(package_path)?,
        out_dir: out_dir.to_path_buf(),
        netcdf_only,
        extracted,
    })
}

pub fn request_access_token(
    agent: &ureq::Agent,
    credentials: &EumetsatCredentials,
    validity_period_secs: u64,
) -> Result<EumetsatAccessToken, Box<dyn Error>> {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!(
        "{}:{}",
        credentials.consumer_key, credentials.consumer_secret
    ));
    let mut response = agent
        .post(EUMETSAT_TOKEN_URL)
        .header("Authorization", format!("Basic {basic}"))
        .header("Accept", "application/json")
        .send_form([
            ("grant_type", "client_credentials".to_string()),
            ("validity_period", validity_period_secs.to_string()),
        ])?;
    let text = response.body_mut().read_to_string()?;
    let token: TokenResponse = serde_json::from_str(&text)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    Ok(EumetsatAccessToken {
        access_token: token.access_token,
        token_type: token.token_type.unwrap_or_else(|| "Bearer".to_string()),
        expires_in: token.expires_in,
        expires_at_unix: now.saturating_add(token.expires_in),
    })
}

pub fn download_product(
    agent: &ureq::Agent,
    collection: MtgCollection,
    product_id: &str,
    token: &EumetsatAccessToken,
    out_dir: &Path,
) -> Result<DownloadedMtgProduct, Box<dyn Error>> {
    fs::create_dir_all(out_dir)?;
    let url = collection.product_download_url(product_id);
    let mut response = agent
        .get(&url)
        .header("Authorization", token.bearer_header())
        .call()?;
    let filename = response
        .headers()
        .get("Content-Disposition")
        .and_then(|value| value.to_str().ok())
        .and_then(content_disposition_filename)
        .unwrap_or_else(|| format!("{}.zip", sanitize_filename(product_id)));
    let content_length = response
        .headers()
        .get("Content-Length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let target = out_dir.join(&filename);
    let partial = out_dir.join(format!("{filename}.partial"));
    let mut file = fs::File::create(&partial)?;
    let bytes = io::copy(&mut response.body_mut().as_reader(), &mut file)?;
    file.sync_all()?;
    drop(file);
    if let Some(expected) = content_length {
        if expected != bytes {
            let _ = fs::remove_file(&partial);
            return Err(boxed_error(format!(
                "downloaded byte count mismatch for {product_id}: expected {expected}, got {bytes}"
            )));
        }
    }
    fs::rename(&partial, &target)?;
    Ok(DownloadedMtgProduct {
        collection,
        product_id: product_id.to_string(),
        path: target,
        filename,
        bytes,
        content_length,
    })
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(rename = "totalResults")]
    total_results: Option<usize>,
    features: Option<Vec<SearchFeature>>,
}

#[derive(Debug, Deserialize)]
struct SearchFeature {
    id: String,
    properties: Option<SearchProperties>,
}

#[derive(Debug, Deserialize)]
struct SearchProperties {
    date: Option<String>,
    updated: Option<String>,
    links: Option<SearchLinks>,
}

#[derive(Debug, Deserialize)]
struct SearchLinks {
    data: Option<Vec<SearchLink>>,
}

#[derive(Debug, Deserialize)]
struct SearchLink {
    href: String,
    title: Option<String>,
    #[serde(rename = "type")]
    media_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    expires_in: u64,
}

impl From<SearchFeature> for MtgProduct {
    fn from(feature: SearchFeature) -> Self {
        let (date, updated, data_links) = match feature.properties {
            Some(properties) => {
                let links = properties
                    .links
                    .and_then(|links| links.data)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|link| MtgProductLink {
                        href: link.href,
                        title: link.title,
                        media_type: link.media_type,
                    })
                    .collect();
                (properties.date, properties.updated, links)
            }
            None => (None, None, Vec::new()),
        };
        Self {
            id: feature.id,
            date,
            updated,
            data_links,
        }
    }
}

fn collection_id(collection: MtgCollection) -> &'static str {
    collection.collection_id()
}

fn normalize_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn query_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn content_disposition_filename(value: &str) -> Option<String> {
    value.split(';').find_map(|part| {
        let part = part.trim();
        let raw = part.strip_prefix("filename=")?;
        let raw = raw.trim_matches('"');
        (!raw.is_empty()).then(|| sanitize_filename(raw))
    })
}

fn sanitize_filename(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_control()
            || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let out = out.trim_matches([' ', '.']);
    if out.is_empty() {
        "eumetsat-product".to_string()
    } else {
        out.to_string()
    }
}

fn classify_package_entry(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".nc") || lower.ends_with(".nc4") || lower.ends_with(".netcdf") {
        "netcdf"
    } else if lower.ends_with(".xml") {
        "xml"
    } else if lower.ends_with(".json") {
        "json"
    } else if lower.ends_with(".txt") || lower.ends_with(".md") {
        "text"
    } else {
        "other"
    }
}

fn infer_package_instrument(name: &str) -> Option<&'static str> {
    let normalized = normalize_token(name);
    if normalized.contains("fci") {
        Some("fci")
    } else if normalized.contains("li") || normalized.contains("lightning") {
        Some("li")
    } else {
        None
    }
}

fn boxed_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Write;

    #[test]
    fn mtg_collection_ids_are_the_live_eumetsat_ids() {
        assert_eq!(
            MtgCollection::FciL1cNormal.collection_id(),
            "EO:EUM:DAT:0662"
        );
        assert_eq!(
            MtgCollection::LiLightningFlashes.collection_id(),
            "EO:EUM:DAT:0691"
        );
        assert_eq!(
            MtgCollection::LiLightningGroups.collection_id(),
            "EO:EUM:DAT:0782"
        );
        assert_eq!(
            MtgCollection::parse("li-flashes"),
            Some(MtgCollection::LiLightningFlashes)
        );
        assert_eq!(
            MtgCollection::parse("EO:EUM:DAT:0662"),
            Some(MtgCollection::FciL1cNormal)
        );
    }

    #[test]
    fn opensearch_url_uses_json_brief_results_and_descending_time_sort() {
        let request = MtgSearchRequest::new(
            MtgCollection::FciL1cNormal,
            Utc.with_ymd_and_hms(2026, 6, 14, 18, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap(),
            2,
        );
        let url = request.url();
        assert!(url.contains("pi=EO%3AEUM%3ADAT%3A0662"), "{url}");
        assert!(url.contains("dtstart=2026-06-14T18%3A00%3A00Z"), "{url}");
        assert!(url.contains("dtend=2026-06-15T00%3A00%3A00Z"), "{url}");
        assert!(url.contains("sort=start%2Ctime%2C0"), "{url}");
        assert!(url.ends_with("&set=brief&format=json"), "{url}");
    }

    #[test]
    fn product_download_url_includes_collection_and_product_id() {
        let product_id = "W_XX-EUMETSAT-Darmstadt,IMG+SAT,MTI1+FCI.nc";
        let url = MtgCollection::FciL1cNormal.product_download_url(product_id);
        assert!(
            url.contains("/collections/EO%3AEUM%3ADAT%3A0662/products/W_XX-EUMETSAT"),
            "{url}"
        );
        assert!(url.contains("Darmstadt%2CIMG%2BSAT"), "{url}");
    }

    #[test]
    fn parses_token_response_and_bearer_header() {
        let token: TokenResponse = serde_json::from_str(
            r#"{"access_token":"abc","token_type":"Bearer","expires_in":3600}"#,
        )
        .unwrap();
        assert_eq!(token.access_token, "abc");
        assert_eq!(token.token_type.as_deref(), Some("Bearer"));
        assert_eq!(token.expires_in, 3600);

        let access = EumetsatAccessToken {
            access_token: "abc".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            expires_at_unix: 1,
        };
        assert_eq!(access.bearer_header(), "Bearer abc");
    }

    #[test]
    fn content_disposition_and_product_ids_make_safe_filenames() {
        assert_eq!(
            content_disposition_filename(r#"attachment; filename="MTG:bad/name.zip""#),
            Some("MTG_bad_name.zip".to_string())
        );
        assert_eq!(
            sanitize_filename("W_XX-EUMETSAT-Darmstadt,IMG+SAT,MTI1+LI"),
            "W_XX-EUMETSAT-Darmstadt,IMG+SAT,MTI1+LI"
        );
        assert_eq!(sanitize_filename("..."), "eumetsat-product");
    }

    #[test]
    fn inspects_and_unpacks_mtg_zip_packages_safely() {
        let root = std::env::temp_dir().join(format!("rw-sat-mtg-package-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let package = root.join("mtg-test.zip");
        let file = fs::File::create(&package).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("W_XX_EUMT_FCI_example.nc", options).unwrap();
        zip.write_all(b"fci-netcdf").unwrap();
        zip.start_file("metadata/product.xml", options).unwrap();
        zip.write_all(b"<xml />").unwrap();
        zip.start_file("../unsafe_li_example.nc", options).unwrap();
        zip.write_all(b"unsafe").unwrap();
        zip.finish().unwrap();

        let manifest = inspect_package(&package).unwrap();
        assert_eq!(manifest.entry_count, 3);
        assert_eq!(manifest.netcdf_count, 2);
        assert_eq!(manifest.fci_count, 1);
        assert_eq!(manifest.li_count, 1);
        assert!(
            manifest
                .entries
                .iter()
                .any(|entry| entry.name == "../unsafe_li_example.nc" && entry.safe_path.is_none())
        );

        let out = root.join("out");
        let result = unpack_package(&package, &out, true).unwrap();
        assert_eq!(result.extracted.len(), 1, "unsafe entry must be skipped");
        assert!(out.join("W_XX_EUMT_FCI_example.nc").is_file());
        assert!(!out.join("metadata/product.xml").exists());
        assert!(!root.join("unsafe_li_example.nc").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_brief_search_response() {
        let json = r#"{
            "totalResults": 1,
            "features": [{
                "id": "MTG_PRODUCT",
                "properties": {
                    "date": "2026-06-15T00:10:03Z/2026-06-15T00:19:35Z",
                    "updated": "2026-06-15T00:23:58.811Z",
                    "links": {
                        "data": [{
                            "href": "https://api.eumetsat.int/data/download/products/MTG_PRODUCT",
                            "title": "Product download",
                            "type": "application/octet-stream"
                        }]
                    }
                }
            }]
        }"#;
        let feed: SearchResponse = serde_json::from_str(json).unwrap();
        let product = MtgProduct::from(feed.features.unwrap().remove(0));
        assert_eq!(product.id, "MTG_PRODUCT");
        assert_eq!(product.data_links.len(), 1);
        assert_eq!(
            product.data_links[0].title.as_deref(),
            Some("Product download")
        );
    }
}
