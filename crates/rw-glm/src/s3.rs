//! Anonymous AWS S3 access to the NOAA GOES open-data buckets for GLM L2 LCFA
//! granules, over plain HTTPS — the listing/download primitives the follow
//! engine (`follow.rs`) drives.
//!
//! The paginated `ListObjectsV2` client (URL-query encoding, XML unescape,
//! continuation-token + `start-after` paging, the pure-Rust TLS agent) is
//! **copied from `crates/rw-sat/src/s3.rs`** rather than shared: per the plan
//! it is a small, stable helper and a premature `rw-net` abstraction is not
//! warranted while only two crates use it. Behaviour matches rw-sat's exactly;
//! only the GOES-prefix math is GLM-specific (`GLM-L2-LCFA/YYYY/DDD/HH/`).
//!
//! GLM keys under one hour prefix sort lexicographically by their `sYYYYDDDHHMMSSS`
//! scan-start token, so `start-after={last seen key}` returns exactly the newer
//! granules — the incremental-poll primitive.

use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rw_store::atomic::atomic_write_bytes;

/// One listed S3 object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Object {
    pub key: String,
    pub size_bytes: u64,
    pub last_modified: String,
}

/// Map a satellite name to its GOES open-data bucket. Accepts the same aliases
/// as rw-sat (`g19`/`goes19`/`noaa-goes19`).
pub fn bucket_for_satellite(satellite: &str) -> Result<String, Box<dyn Error>> {
    let normalized = satellite.trim().to_ascii_lowercase().replace('-', "");
    match normalized.as_str() {
        "g16" | "goes16" => Ok("noaa-goes16".to_string()),
        "g17" | "goes17" => Ok("noaa-goes17".to_string()),
        "g18" | "goes18" => Ok("noaa-goes18".to_string()),
        "g19" | "goes19" => Ok("noaa-goes19".to_string()),
        value if value.starts_with("noaagoes") => Ok(value.replacen("noaagoes", "noaa-goes", 1)),
        value if value.starts_with("noaa-goes") => Ok(value.to_string()),
        _ => Err(boxed_error(format!(
            "unsupported GOES satellite: {satellite}"
        ))),
    }
}

/// The GLM L2 LCFA hour-directory prefix `GLM-L2-LCFA/{year}/{doy:03}/{hour:02}/`
/// for a UTC `(year, day_of_year, hour)`. GLM is a full-disk instrument so —
/// unlike ABI — there is no sector/band specialization; every granule for the
/// hour lives under this one prefix.
pub fn glm_hour_prefix(year: i64, day_of_year: u32, hour: u32) -> String {
    format!("GLM-L2-LCFA/{year:04}/{day_of_year:03}/{hour:02}/")
}

pub fn object_url(bucket: &str, key: &str) -> String {
    format!("https://{bucket}.s3.amazonaws.com/{key}")
}

pub fn object_filename(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// List every object under `prefix`, following continuation tokens. When
/// `start_after` is given, only keys lexicographically greater are returned
/// (the incremental polling primitive). Copied from rw-sat `list_s3_objects`.
pub fn list_s3_objects(
    agent: &ureq::Agent,
    bucket: &str,
    prefix: &str,
    start_after: Option<&str>,
) -> Result<Vec<S3Object>, Box<dyn Error>> {
    let mut objects = Vec::new();
    let mut token = None::<String>;
    loop {
        let mut url = format!(
            "https://{bucket}.s3.amazonaws.com/?list-type=2&prefix={}&max-keys=1000",
            url_query_encode(prefix)
        );
        match (&token, start_after) {
            // continuation-token supersedes start-after on follow-up pages.
            (Some(token), _) => {
                url.push_str("&continuation-token=");
                url.push_str(&url_query_encode(token));
            }
            (None, Some(after)) => {
                url.push_str("&start-after=");
                url.push_str(&url_query_encode(after));
            }
            (None, None) => {}
        }
        let mut response = agent.get(&url).call()?;
        let xml = response.body_mut().read_to_string()?;
        let page = parse_s3_list_xml(&xml);
        objects.extend(page.objects);
        token = page.next_continuation_token;
        if token.is_none() {
            break;
        }
    }
    Ok(objects)
}

/// Download `object` from `bucket` to `dest` (atomic temp+rename), validating
/// the byte count against the listed size. Copied from rw-sat's download path
/// (minus the cache-hit shortcut: the follow engine downloads to a short-lived
/// temp file and deletes it after decode, so caching buys nothing here).
pub fn download_object_to(
    agent: &ureq::Agent,
    bucket: &str,
    object: &S3Object,
    dest: &Path,
) -> Result<(), Box<dyn Error>> {
    let url = object_url(bucket, &object.key);
    let mut response = agent.get(&url).call()?;
    let limit = object
        .size_bytes
        .saturating_add(16 * 1024 * 1024)
        .max(32 * 1024 * 1024);
    let bytes = response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()?;
    if object.size_bytes > 0 && bytes.len() as u64 != object.size_bytes {
        return Err(boxed_error(format!(
            "downloaded byte count mismatch for {}: expected {}, got {}",
            object.key,
            object.size_bytes,
            bytes.len()
        )));
    }
    atomic_write_bytes(dest, &bytes)?;
    Ok(())
}

struct S3ListPage {
    objects: Vec<S3Object>,
    next_continuation_token: Option<String>,
}

fn parse_s3_list_xml(xml: &str) -> S3ListPage {
    let mut objects = Vec::new();
    for contents in xml.split("<Contents>").skip(1) {
        let end = contents.find("</Contents>").unwrap_or(contents.len());
        let block = &contents[..end];
        let key = extract_xml_tag(block, "Key").unwrap_or_default();
        if key.is_empty() {
            continue;
        }
        let size_bytes = extract_xml_tag(block, "Size")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let last_modified = extract_xml_tag(block, "LastModified").unwrap_or_default();
        objects.push(S3Object {
            key,
            size_bytes,
            last_modified,
        });
    }
    S3ListPage {
        objects,
        next_continuation_token: extract_xml_tag(xml, "NextContinuationToken"),
    }
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml_unescape(&xml[start..end]))
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn url_query_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
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

/// Max duration for DNS resolution and for establishing the TCP+TLS
/// connection. A half-open socket must surface as an `Err` that feeds the
/// follow loop's holdback, never hang the 24/7 poller.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RECV_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
/// GLM granules are ~100-400 KB; a generous body timeout covers a slow link.
const RECV_BODY_TIMEOUT: Duration = Duration::from_secs(120);

/// Build the pure-Rust TLS agent. Copied from rw-sat: every IO phase gets an
/// explicit timeout (ureq 3 defaults them all to `None`, which would let one
/// stalled connection block the whole follow session forever).
pub fn build_agent() -> ureq::Agent {
    static CRYPTO_PROVIDER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    CRYPTO_PROVIDER.get_or_init(|| {
        rustls::crypto::CryptoProvider::install_default(rustls_rustcrypto::provider()).ok();
    });
    let crypto = std::sync::Arc::new(rustls_rustcrypto::provider());
    ureq::Agent::config_builder()
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_send_request(Some(SEND_REQUEST_TIMEOUT))
        .timeout_recv_response(Some(RECV_RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(RECV_BODY_TIMEOUT))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::WebPki)
                .unversioned_rustls_crypto_provider(crypto)
                .build(),
        )
        .build()
        .new_agent()
}

fn boxed_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

/// A temp file deleted on drop, used as the download target before decode.
pub(crate) struct ScratchFile {
    path: PathBuf,
}

impl ScratchFile {
    pub(crate) fn new(dir: &Path, key: &str) -> Self {
        let name = object_filename(key);
        Self {
            path: dir.join(format!("{}.{}.scratch", std::process::id(), name)),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_xml_parser_reads_continuation_token() {
        let xml = "<ListBucketResult><Contents><Key>a.nc</Key><Size>42</Size><LastModified>x</LastModified></Contents><NextContinuationToken>abc&amp;123</NextContinuationToken></ListBucketResult>";
        let page = parse_s3_list_xml(xml);
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].key, "a.nc");
        assert_eq!(page.objects[0].size_bytes, 42);
        assert_eq!(page.next_continuation_token.as_deref(), Some("abc&123"));
    }

    #[test]
    fn glm_hour_prefix_uses_day_of_year() {
        // 2026-06-11 is day-of-year 162.
        assert_eq!(glm_hour_prefix(2026, 162, 8), "GLM-L2-LCFA/2026/162/08/");
        assert_eq!(glm_hour_prefix(2026, 1, 0), "GLM-L2-LCFA/2026/001/00/");
    }

    #[test]
    fn bucket_mapping_accepts_aliases() {
        assert_eq!(bucket_for_satellite("goes19").unwrap(), "noaa-goes19");
        assert_eq!(bucket_for_satellite("G18").unwrap(), "noaa-goes18");
        assert_eq!(bucket_for_satellite("noaa-goes16").unwrap(), "noaa-goes16");
        assert!(bucket_for_satellite("himawari").is_err());
    }

    #[test]
    fn query_encoding_escapes_reserved_bytes() {
        assert_eq!(url_query_encode("a/b c+d"), "a%2Fb%20c%2Bd");
        assert_eq!(url_query_encode("GLM-L2-LCFA"), "GLM-L2-LCFA");
    }

    #[test]
    fn object_filename_strips_prefix() {
        assert_eq!(
            object_filename("GLM-L2-LCFA/2026/162/08/OR_GLM-L2-LCFA_G19_s1_e2_c3.nc"),
            "OR_GLM-L2-LCFA_G19_s1_e2_c3.nc"
        );
        assert_eq!(object_filename("bare.nc"), "bare.nc");
    }

    #[test]
    fn scratch_file_is_removed_on_drop() {
        let dir = std::env::temp_dir();
        let path = {
            let scratch = ScratchFile::new(&dir, "GLM-L2-LCFA/2026/162/08/x.nc");
            std::fs::write(scratch.path(), b"bytes").unwrap();
            assert!(scratch.path().is_file());
            scratch.path().to_path_buf()
        };
        assert!(!path.exists(), "scratch file deleted on drop");
    }

    #[test]
    fn agent_has_every_io_phase_timeout_set() {
        let timeouts = build_agent().config().timeouts();
        assert_eq!(timeouts.resolve, Some(CONNECT_TIMEOUT));
        assert_eq!(timeouts.connect, Some(CONNECT_TIMEOUT));
        assert_eq!(timeouts.send_request, Some(SEND_REQUEST_TIMEOUT));
        assert_eq!(timeouts.recv_response, Some(RECV_RESPONSE_TIMEOUT));
        assert_eq!(timeouts.recv_body, Some(RECV_BODY_TIMEOUT));
    }
}
