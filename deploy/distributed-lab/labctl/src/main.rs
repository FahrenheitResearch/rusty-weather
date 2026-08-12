use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use ed25519_dalek::SigningKey;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
};
use rustwx_core::{GridShape, LatLonGrid};
use rw_community_protocol::{
    AttributionNotice, BEGIN_RUN_GENERATION_SCHEMA, BeginRunGenerationRequest, DataOrigin,
    FINALIZE_RUN_GENERATION_SCHEMA, FederationCoverageArea, FederationLimits,
    FederationModelCapability, FederationPolicyLinks, FederationProductCapability,
    FederationPublicKey, FederationQueryCapability, FederationQuotaSummary,
    FederationReplicationPolicy, FederationRetentionSummary, FinalizeRunGenerationRequest,
    HotManifestPointer, MissingPolicy, PublicationGrant, REQUEST_SCHEMA, RESOLVE_SCHEMA,
    RUN_GENERATION_CHUNK_SCHEMA_V1, RUN_GENERATION_FILE_SCHEMA, RUN_GENERATION_REPLICATION_SCHEMA,
    RecipeIdentity, ResolveObjectRequest, ResolveObjectResponse, RunGenerationFile,
    RunGenerationFileChunk, RunGenerationFileKind, RunGenerationReplicationManifest, ShareQuery,
    ShareRequest, SignatureAlgorithm, SourceProvenance, TimeWindow, TrustedSigningKeys,
    generation_content_sha256, object_sha256, request_sha256, sign_public_origin_descriptor,
    verify_signed_object,
};
use rw_community_relay::{
    AddressFamily, AdvertisementReceipt, FallbackTarget, HistoricalRelayClient,
    HistoricalRelayOutcome, HistoricalRelayPolicy, HistoricalRelaySecurity, NeverCancelled,
    ProviderTurnAllocationFactory, RELAY_ADVERTISE_PATH, RELAY_HISTORICAL_LOOKUP_PATH,
    RELAY_NEXT_GRANT_PATH, RELAY_ROUTE_REGISTRATION_PATH, RELAY_SESSION_COMPLETE_PATH,
    RELAY_SESSION_FAIL_PATH, RELAY_SESSION_REVOKE_PATH, RELAY_TRANSPORT_GRANT_PATH,
    RelayAdvertiseRequest, RelayBrokerHttp, RelayError, RelayGrantPollRequest,
    RelayHistoricalLookupRequest, RelayRoutePolicy, RelayRouteRegistrationReceipt,
    RelayRouteRegistrationRequest, RelaySessionCompletionRequest, RelaySessionFailureRequest,
    RelayTerminalResponse, RelayTransportGrantRequest, VerifiedRelaySeedStore, VerifiedSeedObject,
};
use rw_federation_proxy::{FEDERATION_PROXY_SCHEMA, FederationProxyRequest};
use rw_query::{RunDescriptor, RunSnapshot};
use rw_store::{HourIngestWriter, RwsExactTime, RwsSourceProvenance};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const AUTHORITY_URL: &str = "https://authority.rw-lab.example.edu";
const ALPHA_URL: &str = "https://alpha.rw-lab.example.edu";
const BETA_URL: &str = "https://beta.rw-lab.example.edu";
const R2_URL: &str = "https://r2-hot.rw-lab.example.edu";
const R2_BUCKET: &str = "rusty-weather-hot";
const CLOUDFLARE_TURN_API_URL: &str = "https://rtc.live.cloudflare.com";
const R2_READINESS_REQUEST_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
const CLOUDFLARE_READINESS_KEY_ID: &str = "ffffffffffffffffffffffffffffffff";
// 24 probes at a two-second per-request bound plus the intervals below keep
// each readiness gate below one minute even when every request times out.
const READINESS_ATTEMPTS: usize = 24;
const READINESS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const READINESS_INTERVAL: Duration = Duration::from_millis(500);
const MODEL: &str = "golden";
const RUN: &str = "distributed_lab";
const PRODUCT: &str = "analysis";
const AUTHORITY_TOKEN: &str = "distributed-lab-authority-client-token-0001";
const UPLOADER_TOKEN: &str = "distributed-lab-uploader-client-token-0001";
const DOWNLOADER_TOKEN: &str = "distributed-lab-downloader-client-token-0001";
const ALPHA_DATA_TOKEN: &str = "distributed-lab-alpha-origin-data-token-0001";
const BETA_DATA_TOKEN: &str = "distributed-lab-beta-origin-data-token-0001";
const R2_WRITE_TOKEN: &str = "distributed-lab-r2-write-token-0001";
const CLOUDFLARE_API_TOKEN: &str = "distributed-lab-cloudflare-api-token-0001";
const TURN_KEY_ID: &str = "0123456789abcdef0123456789abcdef";
const MAX_HTTP_BODY: u64 = 72 * 1024 * 1024;

type LabResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Serialize, Deserialize)]
struct LabPublicState {
    schema: String,
    authority_object_key_id: String,
    authority_object_public_key_base64: String,
    authority_relay_key_id: String,
    authority_relay_public_key_base64: String,
    alpha_object_key_id: String,
    alpha_object_public_key_base64: String,
    beta_object_key_id: String,
    beta_object_public_key_base64: String,
    fixture_first_valid_unix: i64,
    fixture_last_valid_unix: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct RelayBundle {
    request: ShareRequest,
    manifest: rw_community_protocol::SignedObjectManifest,
    object_sha256: String,
    request_sha256: String,
}

#[derive(Clone)]
struct LabHttp {
    agent: ureq::Agent,
    base: Arc<str>,
    token: Arc<str>,
}

struct HttpReply {
    status: u16,
    bytes: Vec<u8>,
}

impl LabHttp {
    fn new(base: &str, token: &str, ca_path: &Path) -> LabResult<Self> {
        Self::new_with_timeout(base, token, ca_path, Duration::from_secs(90))
    }

    fn new_with_timeout(
        base: &str,
        token: &str,
        ca_path: &Path,
        timeout: Duration,
    ) -> LabResult<Self> {
        let ca = fs::read(ca_path)?;
        let certificate = ureq::tls::Certificate::from_pem(&ca)?;
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .https_only(true)
            .proxy(None)
            .max_redirects(0)
            .max_idle_connections(0)
            .timeout_global(Some(timeout))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::Rustls)
                    .root_certs(ureq::tls::RootCerts::new_with_certs(&[certificate]))
                    .unversioned_rustls_crypto_provider(Arc::new(
                        rustls::crypto::ring::default_provider(),
                    ))
                    .build(),
            )
            .build();
        Ok(Self {
            agent: config.new_agent(),
            base: Arc::from(base),
            token: Arc::from(token),
        })
    }

    fn url(&self, path: &str) -> LabResult<String> {
        if !path.starts_with('/') || path.contains("..") || path.contains('\\') {
            return Err("invalid fixed lab path".into());
        }
        Ok(format!("{}{}", self.base, path))
    }

    fn get(&self, path: &str) -> LabResult<HttpReply> {
        let mut response = self
            .agent
            .get(self.url(path)?)
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "application/json")
            .call()?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY)
            .read_to_vec()?;
        Ok(HttpReply { status, bytes })
    }

    fn post_json<T: Serialize>(&self, path: &str, value: &T) -> LabResult<HttpReply> {
        self.post_json_headers(path, value, &[])
    }

    fn post_json_headers<T: Serialize>(
        &self,
        path: &str,
        value: &T,
        headers: &[(&str, &str)],
    ) -> LabResult<HttpReply> {
        let body = serde_json::to_vec(value)?;
        let mut request = self
            .agent
            .post(self.url(path)?)
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .header("accept", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut response = request.send(&body)?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY)
            .read_to_vec()?;
        Ok(HttpReply { status, bytes })
    }

    fn post_octets(&self, path: &str, bytes: &[u8]) -> LabResult<HttpReply> {
        let mut response = self
            .agent
            .post(self.url(path)?)
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/octet-stream")
            .header("accept", "application/json")
            .send(bytes)?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY)
            .read_to_vec()?;
        Ok(HttpReply { status, bytes })
    }

    fn put_immutable_octets(&self, path: &str, bytes: &[u8]) -> LabResult<HttpReply> {
        let mut response = self
            .agent
            .put(self.url(path)?)
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/octet-stream")
            .header("if-none-match", "*")
            .header("accept", "application/json")
            .send(bytes)?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY)
            .read_to_vec()?;
        Ok(HttpReply { status, bytes })
    }

    fn public_get(&self, path: &str) -> LabResult<HttpReply> {
        let mut response = self.agent.get(self.url(path)?).call()?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY)
            .read_to_vec()?;
        Ok(HttpReply { status, bytes })
    }
}

fn require_status(reply: HttpReply, expected: u16, context: &str) -> LabResult<Vec<u8>> {
    if reply.status != expected {
        let detail = String::from_utf8_lossy(&reply.bytes);
        return Err(format!("{context}: HTTP {}: {detail}", reply.status).into());
    }
    Ok(reply.bytes)
}

fn decode_json<T: DeserializeOwned>(bytes: &[u8], context: &str) -> LabResult<T> {
    serde_json::from_slice(bytes).map_err(|error| format!("{context}: {error}").into())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| i64::try_from(value.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn auth_principal(token: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rw-authenticated-principal-v1\0");
    digest.update(token.as_bytes());
    format!("{:x}", digest.finalize())
}

fn replication_owner(token: &str) -> String {
    let principal = auth_principal(token);
    let mut digest = Sha256::new();
    digest.update(b"rw-server-generation-replication-owner-v1\0");
    digest.update(principal.as_bytes());
    format!("{:x}", digest.finalize())
}

fn key(byte: u8) -> SigningKey {
    SigningKey::from_bytes(&[byte; 32])
}

fn key_secret(key: &SigningKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.to_bytes())
}

fn key_public(key: &SigningKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes())
}

fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> LabResult<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

#[cfg(unix)]
fn private_file(path: &Path) -> LabResult<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn private_file(_path: &Path) -> LabResult<()> {
    Ok(())
}

#[cfg(unix)]
fn prepare_container_ownership(root: &Path) -> LabResult<()> {
    let common = Command::new("chown")
        .args(["-R", "65532:65532"])
        .arg(root)
        .status()?;
    if !common.success() {
        return Err("failed to assign isolated lab runtime ownership".into());
    }
    make_evidence_directory_host_writable(&root.join("results"))?;
    Ok(())
}

#[cfg(not(unix))]
fn prepare_container_ownership(_root: &Path) -> LabResult<()> {
    Ok(())
}

#[cfg(unix)]
fn make_evidence_directory_host_writable(results: &Path) -> LabResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    // This isolated directory contains only non-secret release evidence and
    // compose logs. The host runner must be able to append failure evidence
    // after provisioning assigns the rest of /lab to the unprivileged
    // container uid. Do not broaden this mode to /lab, configs, or secrets.
    fs::set_permissions(results, fs::Permissions::from_mode(0o777))?;
    Ok(())
}

fn provision(root: &Path) -> LabResult<()> {
    if root != Path::new("/lab") {
        return Err("provisioning is restricted to the container's /lab mount".into());
    }
    for child in [
        "configs",
        "control",
        "data",
        "replication",
        "results",
        "secrets",
    ] {
        let path = root.join(child);
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(path)?;
    }

    fs::create_dir_all(root.join("tls"))?;
    if !root.join("tls/ca.crt").is_file()
        || !root.join("tls/lab.crt").is_file()
        || !root.join("tls/lab.key").is_file()
    {
        generate_tls(root)?;
    }

    let authority_object = key(11);
    let authority_relay = key(12);
    let federation_catalog = key(13);
    let alpha_object = key(21);
    let alpha_descriptor = key(22);
    let alpha_replication = key(23);
    let beta_object = key(31);
    let beta_descriptor = key(32);
    let beta_replication = key(33);

    let secrets = [
        (
            "authority-api-tokens.txt",
            format!("{AUTHORITY_TOKEN}\n{UPLOADER_TOKEN}\n{DOWNLOADER_TOKEN}\n"),
        ),
        ("alpha-api-tokens.txt", format!("{UPLOADER_TOKEN}\n")),
        ("beta-api-tokens.txt", format!("{UPLOADER_TOKEN}\n")),
        ("authority-community.key", key_secret(&authority_object)),
        ("authority-relay.key", key_secret(&authority_relay)),
        ("federation-catalog.key", key_secret(&federation_catalog)),
        ("alpha-community.key", key_secret(&alpha_object)),
        ("alpha-replication.key", key_secret(&alpha_replication)),
        ("beta-community.key", key_secret(&beta_object)),
        ("beta-replication.key", key_secret(&beta_replication)),
        ("alpha-data.token", ALPHA_DATA_TOKEN.into()),
        ("beta-data.token", BETA_DATA_TOKEN.into()),
        ("r2-write.token", R2_WRITE_TOKEN.into()),
        ("cloudflare-api.token", CLOUDFLARE_API_TOKEN.into()),
    ];
    for (name, value) in secrets {
        let path = root.join("secrets").join(name);
        write(&path, value)?;
        private_file(&path)?;
    }
    let scoped = root.join("secrets/authority-scoped");
    fs::create_dir_all(&scoped)?;
    for name in ["alpha-data.token", "beta-data.token"] {
        let target = scoped.join(name);
        fs::copy(root.join("secrets").join(name), &target)?;
        private_file(&target)?;
    }

    let now = now_unix();
    let first_valid = now.saturating_sub(3_600);
    let last_valid = now;
    create_source_store(root, first_valid, last_valid)?;
    let begin = create_replication_request(root, now, &beta_replication)?;
    write(
        root.join("replication/begin.json"),
        serde_json::to_vec_pretty(&begin)?,
    )?;

    let alpha_signed = origin_descriptor(
        "alpha-university-lab",
        "Alpha University Lab",
        "https://alpha.rw-lab.example.edu",
        "alpha-descriptor-v1",
        &alpha_descriptor,
        "alpha-object-v1",
        &alpha_object,
        now,
    )?;
    let beta_signed = origin_descriptor(
        "beta-public-lab",
        "Beta Public Lab",
        "https://beta.rw-lab.example.edu",
        "beta-descriptor-v1",
        &beta_descriptor,
        "beta-object-v1",
        &beta_object,
        now,
    )?;
    write(
        root.join("configs/alpha-descriptor.json"),
        serde_json::to_vec_pretty(&alpha_signed)?,
    )?;
    write(
        root.join("configs/beta-descriptor.json"),
        serde_json::to_vec_pretty(&beta_signed)?,
    )?;

    let public = LabPublicState {
        schema: "rw.distributed-lab.public-state.v1".into(),
        authority_object_key_id: "authority-object-v1".into(),
        authority_object_public_key_base64: key_public(&authority_object),
        authority_relay_key_id: "authority-relay-v1".into(),
        authority_relay_public_key_base64: key_public(&authority_relay),
        alpha_object_key_id: "alpha-object-v1".into(),
        alpha_object_public_key_base64: key_public(&alpha_object),
        beta_object_key_id: "beta-object-v1".into(),
        beta_object_public_key_base64: key_public(&beta_object),
        fixture_first_valid_unix: first_valid,
        fixture_last_valid_unix: last_valid,
    };
    write(
        root.join("control/public.json"),
        serde_json::to_vec_pretty(&public)?,
    )?;

    write(
        root.join("configs/authority.toml"),
        authority_config(
            &public,
            &key_public(&alpha_descriptor),
            &key_public(&beta_descriptor),
        ),
    )?;
    write(
        root.join("configs/alpha.toml"),
        origin_config("alpha", ALPHA_DATA_TOKEN),
    )?;
    write(
        root.join("configs/beta.toml"),
        origin_config("beta", BETA_DATA_TOKEN),
    )?;

    for path in [
        "data/authority/store",
        "data/authority/artifacts",
        "data/authority/community",
        "data/authority/federation",
        "data/alpha/store",
        "data/alpha/artifacts",
        "data/alpha/community",
        "data/alpha/replication",
        "data/beta/store",
        "data/beta/artifacts",
        "data/beta/community",
        "data/beta/replication",
        "data/r2",
        "results/captures",
    ] {
        fs::create_dir_all(root.join(path))?;
    }
    write(root.join("control/provisioned"), format!("{now}\n"))?;
    prepare_container_ownership(root)?;
    Ok(())
}

fn generate_tls(root: &Path) -> LabResult<()> {
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let mut ca_name = DistinguishedName::new();
    ca_name.push(DnType::CommonName, "Rusty Weather distributed lab CA");
    ca_params.distinguished_name = ca_name;
    let ca_key = KeyPair::generate()?;
    let ca = ca_params.self_signed(&ca_key)?;
    let ca_issuer = Issuer::new(ca_params, ca_key);

    let names = vec![
        "authority.rw-lab.example.edu".to_string(),
        "alpha.rw-lab.example.edu".to_string(),
        "beta.rw-lab.example.edu".to_string(),
        "r2-hot.rw-lab.example.edu".to_string(),
        "rtc.live.cloudflare.com".to_string(),
    ];
    let mut leaf_params = CertificateParams::new(names)?;
    let mut leaf_name = DistinguishedName::new();
    leaf_name.push(
        DnType::CommonName,
        "Rusty Weather distributed lab endpoints",
    );
    leaf_params.distinguished_name = leaf_name;
    let leaf_key = KeyPair::generate()?;
    let leaf = leaf_params.signed_by(&leaf_key, &ca_issuer)?;

    write(root.join("tls/ca.crt"), ca.pem())?;
    write(root.join("tls/lab.crt"), leaf.pem())?;
    write(root.join("tls/lab.key"), leaf_key.serialize_pem())?;
    private_file(&root.join("tls/lab.key"))?;
    Ok(())
}

fn create_source_store(root: &Path, first_valid: i64, last_valid: i64) -> LabResult<()> {
    let store = root.join("data/source");
    let grid = LatLonGrid::new(
        GridShape::new(4, 4)?,
        vec![
            40.0, 40.0, 40.0, 40.0, 40.5, 40.5, 40.5, 40.5, 41.0, 41.0, 41.0, 41.0, 41.5, 41.5,
            41.5, 41.5,
        ],
        vec![
            -100.5, -100.0, -99.5, -99.0, -100.5, -100.0, -99.5, -99.0, -100.5, -100.0, -99.5,
            -99.0, -100.5, -100.0, -99.5, -99.0,
        ],
    )?;
    for (slot, valid) in [(0_u16, first_valid), (1_u16, last_valid)] {
        let mut writer = HourIngestWriter::begin_exact(
            &store,
            MODEL,
            RUN,
            slot,
            RwsExactTime::new(u64::from(slot) * 3_600, valid),
            &grid,
            None,
            "distributed-lab",
        )?;
        writer.set_source_provenance(vec![RwsSourceProvenance::new(
            "simulation-owner",
            vec!["generation".into()],
            vec!["rws".into()],
        )?])?;
        let offset = f32::from(slot);
        let surface = (0..16)
            .map(|index| 290.0 + index as f32 * 0.1 + offset)
            .collect::<Vec<_>>();
        writer.add_derived_2d("t2m", "K", &surface)?;
        let p1000 = surface.iter().map(|value| value - 1.0).collect::<Vec<_>>();
        let p850 = surface.iter().map(|value| value - 8.0).collect::<Vec<_>>();
        let p500 = surface.iter().map(|value| value - 35.0).collect::<Vec<_>>();
        writer.add_volume(
            "temperature",
            "K",
            serde_json::json!({"kind":"distributed_lab"}),
            &[(1000, &p1000), (850, &p850), (500, &p500)],
        )?;
        writer.finish(u64::try_from(now_unix()).unwrap_or(0))?;
    }
    Ok(())
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn generation_file(
    root: &Path,
    kind: RunGenerationFileKind,
    name: &str,
) -> LabResult<RunGenerationFile> {
    let bytes = fs::read(root.join(name))?;
    let hash = sha(&bytes);
    write(Path::new("/lab/replication/chunks").join(&hash), &bytes)?;
    Ok(RunGenerationFile {
        schema: RUN_GENERATION_FILE_SCHEMA.into(),
        kind,
        file_name: name.into(),
        byte_size: bytes.len() as u64,
        file_sha256: hash.clone(),
        chunks: vec![RunGenerationFileChunk {
            schema: RUN_GENERATION_CHUNK_SCHEMA_V1.into(),
            ordinal: 0,
            file_offset: 0,
            object_sha256: hash,
            byte_size: bytes.len() as u64,
        }],
    })
}

fn create_replication_request(
    root: &Path,
    now: i64,
    _replication_key: &SigningKey,
) -> LabResult<BeginRunGenerationRequest> {
    let store = root.join("data/source");
    let run_dir = store.join(MODEL).join(RUN);
    let snapshot = RunSnapshot::open(&store, MODEL, RUN)?;
    let axis = snapshot.time_axis();
    let files = vec![
        generation_file(&run_dir, RunGenerationFileKind::RunManifest, "run.json")?,
        generation_file(&run_dir, RunGenerationFileKind::Grid, "grid.rwg")?,
        generation_file(
            &run_dir,
            RunGenerationFileKind::Hour {
                storage_slot: axis[0].storage_slot,
                valid_unix: axis[0].valid_unix,
            },
            "f000.rws",
        )?,
        generation_file(
            &run_dir,
            RunGenerationFileKind::Hour {
                storage_slot: axis[1].storage_slot,
                valid_unix: axis[1].valid_unix,
            },
            "f001.rws",
        )?,
    ];
    let provenance = vec![SourceProvenance {
        provider: "simulation-owner".into(),
        roles: vec!["generation".into()],
        products: vec!["rws".into()],
    }];
    let mut manifest = RunGenerationReplicationManifest {
        schema: RUN_GENERATION_REPLICATION_SCHEMA.into(),
        generation_id: "beta-distributed-lab-generation".into(),
        model: MODEL.into(),
        run: RUN.into(),
        source_snapshot_id: snapshot.descriptor().snapshot_id.clone(),
        grid_hash: snapshot.descriptor().grid_hash.clone(),
        owner_principal_sha256: replication_owner(UPLOADER_TOKEN),
        publication: PublicationGrant {
            data_origin: DataOrigin::PrivateWrf,
            explicit_owner_publication: true,
            redistribution_rights_confirmed: true,
        },
        source_provenance: provenance,
        total_bytes: files.iter().map(|file| file.byte_size).sum(),
        files,
        generation_sha256: "00".repeat(32),
        published_unix: now,
        retain_until_unix: now + 86_400,
        attributions: vec![AttributionNotice {
            provider: "simulation-owner".into(),
            notice: "Explicit owner publication for the isolated release lab.".into(),
            source_url: "https://beta.rw-lab.example.edu/attribution".into(),
            license: "Owner-authorized redistribution".into(),
            license_url: "https://beta.rw-lab.example.edu/license".into(),
            terms_url: "https://beta.rw-lab.example.edu/terms".into(),
            disclaimer: "Synthetic test data only.".into(),
        }],
        modification_notices: vec!["Encoded as an immutable Rusty Weather lab generation.".into()],
    };
    manifest.generation_sha256 = generation_content_sha256(&manifest)?;
    Ok(BeginRunGenerationRequest {
        schema: BEGIN_RUN_GENERATION_SCHEMA.into(),
        manifest,
    })
}

#[allow(clippy::too_many_arguments)]
fn origin_descriptor(
    origin_id: &str,
    display_name: &str,
    https_base_url: &str,
    descriptor_key_id: &str,
    descriptor_key: &SigningKey,
    object_key_id: &str,
    object_key: &SigningKey,
    now: i64,
) -> LabResult<rw_community_protocol::SignedPublicOriginDescriptor> {
    let descriptor_public = FederationPublicKey {
        algorithm: SignatureAlgorithm::Ed25519,
        key_id: descriptor_key_id.into(),
        public_key_base64: key_public(descriptor_key),
        not_before_unix: now - 300,
        expires_unix: now + 6 * 24 * 60 * 60,
    };
    let object_public = FederationPublicKey {
        algorithm: SignatureAlgorithm::Ed25519,
        key_id: object_key_id.into(),
        public_key_base64: key_public(object_key),
        not_before_unix: now - 300,
        expires_unix: now + 6 * 24 * 60 * 60,
    };
    let descriptor = rw_community_protocol::PublicOriginDescriptor {
        schema: rw_community_protocol::FEDERATION_ORIGIN_SCHEMA.into(),
        origin_id: origin_id.into(),
        display_name: display_name.into(),
        https_base_url: https_base_url.into(),
        health_path: "/v1/health/ready".into(),
        descriptor_signing_keys: vec![descriptor_public],
        object_signing_keys: vec![object_public],
        models: vec![FederationModelCapability {
            model: MODEL.into(),
            products: vec![FederationProductCapability {
                product: PRODUCT.into(),
                queries: vec![
                    FederationQueryCapability::PointSeries,
                    FederationQueryCapability::Sounding,
                    FederationQueryCapability::NativeWindow,
                    FederationQueryCapability::ArbitraryDomainMap,
                    FederationQueryCapability::TemporalGrid,
                ],
                pressure_levels_hpa: vec![500, 850, 1000],
            }],
        }],
        geographic_coverage: vec![FederationCoverageArea {
            coverage_id: "synthetic-grid".into(),
            west_longitude_e7: -1_010_000_000,
            south_latitude_e7: 390_000_000,
            east_longitude_e7: -980_000_000,
            north_latitude_e7: 430_000_000,
        }],
        retention: FederationRetentionSummary {
            queryable_run_hours: 24,
            immutable_object_hours: 168,
            published_case_hours: 168,
            previous_generations: 1,
        },
        api_schema_version: "rw-api-v1".into(),
        build_version: "distributed-lab".into(),
        issued_unix: now - 30,
        expires_unix: now + 5 * 24 * 60 * 60,
        policy_links: FederationPolicyLinks {
            attribution_url: format!("{https_base_url}/attribution"),
            acceptable_use_url: format!("{https_base_url}/policy"),
            privacy_url: format!("{https_base_url}/privacy"),
        },
        replication: FederationReplicationPolicy {
            accepts_replication: true,
            maximum_object_bytes: 64 * 1024 * 1024,
            monthly_ingress_bytes: 1024 * 1024 * 1024,
            models: vec![MODEL.into()],
        },
        quotas: FederationQuotaSummary {
            maximum_request_bytes: 1024 * 1024,
            maximum_response_bytes: 64 * 1024 * 1024,
            requests_per_minute: 120,
            concurrent_requests: 8,
            monthly_egress_bytes: 10 * 1024 * 1024 * 1024,
        },
    };
    Ok(sign_public_origin_descriptor(
        descriptor,
        descriptor_key_id,
        descriptor_key,
        &FederationLimits::default(),
    )?)
}

fn common_server(prefix: &str) -> String {
    format!(
        r#"[server]
listen = "0.0.0.0:8788"
store_root = "/var/lib/rusty-weather/store"
artifact_root = "/var/lib/rusty-weather/artifacts"
allow_unauthenticated_public_bind = false

[auth]
token_file = "/tmp/rusty-weather-api-tokens"
protect_metrics = true

[logging]
filter = "rw_server=info,tower_http=info"
format = "json"

# Generated only for the isolated distributed lab: {prefix}.
"#
    )
}

fn origin_config(name: &str, _data_token: &str) -> String {
    let operator = auth_principal(UPLOADER_TOKEN);
    format!(
        r#"{}
[origin_catalog]
enabled = true
publication_sources = "replication"
refresh_seconds = 1
max_age_seconds = 7200

[generation_replication]
enabled = true
security_tests_passed = true
capacity_audit_completed = true
kill_switch = false
control_root = "/var/lib/rusty-weather/generation-replication"
signing_key_file = "/tmp/rusty-weather-generation-replication-signing.key"
signing_key_id = "{name}-replication-v1"
operator_principals = ["{operator}"]

[generation_replication.limits]
maximum_generation_bytes = 16777216
maximum_files = 64
maximum_chunks = 256
maximum_chunk_bytes = 8388608
maximum_manifest_bytes = 2097152
maximum_retention_seconds = 604800
maximum_provenance_entries = 8
maximum_attributions = 8

[generation_replication.quotas]
per_owner_storage_bytes = 67108864
total_storage_bytes = 268435456
per_owner_generations = 8
total_generations = 32
per_owner_concurrent_uploads = 2
total_concurrent_uploads = 8
per_owner_upload_bytes_per_month = 134217728
total_upload_bytes_per_month = 536870912
upload_ttl_seconds = 3600
maximum_state_bytes = 8388608
maximum_gc_entries = 100000
maximum_gc_deletions = 10000

[community]
enabled = true
capacity_audit_completed = true
kill_switch = false
root = "/var/lib/rusty-weather/community-cache"
signing_key_file = "/tmp/rusty-weather-community-signing.key"
signing_key_id = "{name}-object-v1"
object_manifest_retention_seconds = 604800
trusted_public_keys = []

[community.hot_store]
provider = "disabled"

[federation.proxy]
enabled = false
security_tests_passed = true
kill_switch = true
accept_local_resolve = true
local_resolve_token_file = "/tmp/rusty-weather-community-origin.token"
authority_origin_id = "hetzner-authority"
authority_https_root = "https://authority.rw-lab.example.edu"
maximum_attempts = 2
accounting_state_file = "/var/lib/rusty-weather/community-cache/federation-accounting.json"
monthly_download_bytes_per_principal = 1073741824
concurrent_requests_per_principal = 2
maximum_principals = 1000
resolve_timeout_seconds = 2
connect_timeout_seconds = 4
send_timeout_seconds = 5
receive_timeout_seconds = 20
global_timeout_seconds = 30
"#,
        common_server(name)
    )
}

fn authority_config(
    public: &LabPublicState,
    alpha_descriptor: &str,
    beta_descriptor: &str,
) -> String {
    let operator = auth_principal(AUTHORITY_TOKEN);
    format!(
        r#"{}
[origin_catalog]
enabled = false
publication_sources = "scheduler"
refresh_seconds = 5
max_age_seconds = 7200

[community]
enabled = true
capacity_audit_completed = true
kill_switch = false
root = "/var/lib/rusty-weather/community-cache"
signing_key_file = "/tmp/rusty-weather-community-signing.key"
signing_key_id = "{}"
object_manifest_retention_seconds = 604800
trusted_public_keys = [
  "{}:{}",
  "{}:{}",
]

[community.hot_store]
provider = "r2"
base_url = "https://r2-hot.rw-lab.example.edu"
bucket = "rusty-weather-hot"
token_file = "/tmp/rusty-weather-r2-gateway.token"

[community.promotion]
enabled = true
minimum_hits = 1
window_seconds = 300
maximum_object_bytes = 67108864

[community.relay]
enabled = true
security_tests_passed = true
capacity_audit_completed = true
provider_pricing_verified = true
kill_switch = false
state_file = "/var/lib/rusty-weather/community-cache/relay-state.json"
signing_key_file = "/tmp/rusty-weather-community-relay-signing.key"
signing_key_id = "{}"
relay_id = "cloudflare-turn"
credential_lifetime_seconds = 600
max_chunk_plaintext_bytes = 512
archival_origin_available = false
operator_principals = ["{operator}"]

[community.relay.cloudflare]
turn_key_id = "{TURN_KEY_ID}"
api_token_file = "/tmp/rusty-weather-cloudflare-turn-api.token"
allowed_turn_hosts = ["turn.cloudflare.com"]
audited_relay_cidrs = ["11.231.0.15/32"]
resolve_timeout_seconds = 3
connect_timeout_seconds = 5
send_timeout_seconds = 5
receive_timeout_seconds = 10
global_timeout_seconds = 15

[community.relay.quotas]
per_user_upload_bytes_per_month = 1073741824
per_user_download_bytes_per_month = 1073741824
per_user_advertised_storage_bytes = 1073741824
per_user_concurrency = 2
global_concurrency = 16
global_relay_bytes_per_month = 10737418240
cost_stop_after_bytes_per_month = 8589934592

[community.relay.promotion]
successful_recoveries = 1
relayed_bytes = 1

[federation]
enabled = true
health_monitor_enabled = false
catalog_id = "distributed-lab-origins"
catalog_signing_key_id = "distributed-lab-catalog-v1"
catalog_signing_key_file = "/tmp/rusty-weather-federation-signing.key"
descriptor_files = [
  "/etc/rusty-weather/alpha-descriptor.json",
  "/etc/rusty-weather/beta-descriptor.json",
]
revoked_origin_ids = []
revoked_key_ids = []
catalog_ttl_seconds = 300
health_failure_threshold = 3
health_quarantine_seconds = 60
maximum_selection_results = 8

[[federation.approved_origins]]
origin_id = "alpha-university-lab"
descriptor_signing_keys = [
  {{ key_id = "alpha-descriptor-v1", public_key_base64 = "{alpha_descriptor}" }},
]
data_bearer_token_file = "/tmp/rusty-weather-secrets/alpha-data.token"

[[federation.approved_origins]]
origin_id = "beta-public-lab"
descriptor_signing_keys = [
  {{ key_id = "beta-descriptor-v1", public_key_base64 = "{beta_descriptor}" }},
]
data_bearer_token_file = "/tmp/rusty-weather-secrets/beta-data.token"

[federation.proxy]
enabled = true
security_tests_passed = true
kill_switch = false
operator_principals = ["{operator}"]
accept_local_resolve = false
authority_origin_id = "hetzner-authority"
authority_https_root = "https://authority.rw-lab.example.edu"
maximum_attempts = 2
accounting_state_file = "/var/lib/rusty-weather/federation/accounting.json"
control_state_file = "/var/lib/rusty-weather/federation/control.json"
monthly_download_bytes_per_principal = 1073741824
concurrent_requests_per_principal = 2
maximum_principals = 1000
resolve_timeout_seconds = 2
connect_timeout_seconds = 4
send_timeout_seconds = 5
receive_timeout_seconds = 20
global_timeout_seconds = 30
"#,
        common_server("authority"),
        public.authority_object_key_id,
        public.alpha_object_key_id,
        public.alpha_object_public_key_base64,
        public.beta_object_key_id,
        public.beta_object_public_key_base64,
        public.authority_relay_key_id,
    )
}

fn wait_ready(http: &LabHttp, name: &str) -> LabResult<()> {
    for _ in 0..120 {
        match http.public_get("/v1/health/ready") {
            Ok(reply) if reply.status == 200 => return Ok(()),
            _ => thread::sleep(Duration::from_millis(500)),
        }
    }
    Err(format!("{name} did not become ready").into())
}

fn wait_for_exact_not_found<F>(name: &str, mut probe: F) -> LabResult<()>
where
    F: FnMut() -> LabResult<HttpReply>,
{
    wait_for_exact_not_found_with_policy(name, READINESS_ATTEMPTS, READINESS_INTERVAL, &mut probe)
}

fn wait_for_exact_not_found_with_policy<F>(
    name: &str,
    attempts: usize,
    interval: Duration,
    mut probe: F,
) -> LabResult<()>
where
    F: FnMut() -> LabResult<HttpReply>,
{
    let mut last_observation = "no response".to_string();
    for attempt in 0..attempts {
        match probe() {
            Ok(reply) if reply.status == 404 => return Ok(()),
            Ok(reply) => last_observation = format!("HTTP {}", reply.status),
            Err(_) => last_observation = "transport error".into(),
        }
        if attempt + 1 < attempts {
            thread::sleep(interval);
        }
    }
    Err(format!(
        "{name} did not return the required exact HTTP 404 readiness response ({last_observation})"
    )
    .into())
}

fn wait_r2_ready(http: &LabHttp) -> LabResult<()> {
    let absent_pointer = format!("/{R2_BUCKET}/v2/requests/{R2_READINESS_REQUEST_SHA256}.json");
    wait_for_exact_not_found("R2 hot store", || http.public_get(&absent_pointer))
}

fn wait_cloudflare_turn_api_ready(http: &LabHttp) -> LabResult<()> {
    let absent_key = format!("/v1/turn/keys/{CLOUDFLARE_READINESS_KEY_ID}/credentials/generate");
    let probe = serde_json::json!({
        "ttl": 1,
        "customIdentifier": "distributed-lab-readiness"
    });
    wait_for_exact_not_found("Cloudflare TURN credential API", || {
        http.post_json(&absent_key, &probe)
    })
}

fn request_for(descriptor: &RunDescriptor, public: &LabPublicState, recipe: &str) -> ShareRequest {
    ShareRequest {
        schema: REQUEST_SCHEMA.into(),
        model: MODEL.into(),
        run: RUN.into(),
        snapshot_id: descriptor.snapshot_id.clone(),
        grid_hash: descriptor.grid_hash.clone(),
        variables: vec!["t2m".into()],
        query: ShareQuery::PointSeries {
            latitude_e7: 407_500_000,
            longitude_e7: -997_500_000,
            window: TimeWindow::Utc {
                start_unix: public.fixture_first_valid_unix,
                end_unix: public.fixture_last_valid_unix + 1,
            },
            missing_policy: MissingPolicy::Strict,
        },
        recipe: RecipeIdentity {
            recipe_id: recipe.into(),
            recipe_version: "1".into(),
            parameters: BTreeMap::from([("federation_product".into(), PRODUCT.into())]),
        },
        source_provenance: vec![SourceProvenance {
            provider: "simulation-owner".into(),
            roles: vec!["generation".into()],
            products: vec!["rws".into()],
        }],
        publication: PublicationGrant {
            data_origin: DataOrigin::PrivateWrf,
            explicit_owner_publication: true,
            redistribution_rights_confirmed: true,
        },
    }
}

fn exercise(root: &Path) -> LabResult<()> {
    let ca = root.join("tls/ca.crt");
    let authority = LabHttp::new(AUTHORITY_URL, AUTHORITY_TOKEN, &ca)?;
    let alpha = LabHttp::new(ALPHA_URL, ALPHA_DATA_TOKEN, &ca)?;
    let beta = LabHttp::new(BETA_URL, UPLOADER_TOKEN, &ca)?;
    wait_ready(&authority, "authority")?;
    wait_ready(&alpha, "alpha")?;
    wait_ready(&beta, "beta")?;
    // Compose's dependency ordering starts containers but does not make
    // Wrangler/R2 or the credential edge request-ready. Probe valid, absent
    // identities over the same pinned HTTPS/DNS path before either service
    // can participate in the exercise; only an exact 404 proves readiness.
    let r2_readiness = LabHttp::new_with_timeout(
        R2_URL,
        "unused-public-get-token-000000000000",
        &ca,
        READINESS_PROBE_TIMEOUT,
    )?;
    wait_r2_ready(&r2_readiness)?;
    let cloudflare_turn_api = LabHttp::new_with_timeout(
        CLOUDFLARE_TURN_API_URL,
        CLOUDFLARE_API_TOKEN,
        &ca,
        READINESS_PROBE_TIMEOUT,
    )?;
    wait_cloudflare_turn_api_ready(&cloudflare_turn_api)?;
    let r2 = LabHttp::new(R2_URL, "unused-public-get-token-000000000000", &ca)?;

    let begin: BeginRunGenerationRequest = decode_json(
        &fs::read(root.join("replication/begin.json"))?,
        "replication begin",
    )?;
    let owner: serde_json::Value = decode_json(
        &require_status(
            beta.get("/v1/community/generation-replication/owner")?,
            200,
            "owner identity",
        )?,
        "owner identity",
    )?;
    if owner["owner_principal_sha256"] != begin.manifest.owner_principal_sha256 {
        return Err("server replication owner does not match the signed owner".into());
    }
    require_status(
        beta.post_json("/v1/community/generations", &begin)?,
        201,
        "begin generation",
    )?;
    let missing: rw_community_protocol::RunGenerationMissingPage = decode_json(
        &require_status(
            beta.get(&format!(
                "/v1/community/generations/{}/missing?limit=1024",
                begin.manifest.generation_id
            ))?,
            200,
            "missing chunks",
        )?,
        "missing chunks",
    )?;
    for chunk in &missing.chunks {
        let bytes = fs::read(root.join("replication/chunks").join(&chunk.object_sha256))?;
        require_status(
            beta.post_octets(
                &format!(
                    "/v1/community/generations/{}/chunks/{}",
                    begin.manifest.generation_id, chunk.object_sha256
                ),
                &bytes,
            )?,
            204,
            "upload chunk",
        )?;
    }
    let finalize = FinalizeRunGenerationRequest {
        schema: FINALIZE_RUN_GENERATION_SCHEMA.into(),
        generation_sha256: begin.manifest.generation_sha256.clone(),
    };
    let finalized = require_status(
        beta.post_json(
            &format!(
                "/v1/community/generations/{}/finalize",
                begin.manifest.generation_id
            ),
            &finalize,
        )?,
        201,
        "finalize generation",
    )?;
    write(root.join("results/replication-finalized.json"), &finalized)?;

    let descriptor: RunDescriptor = decode_json(
        &require_status(
            beta.get(&format!("/v1/models/{MODEL}/runs/{RUN}"))?,
            200,
            "replicated run detail",
        )?,
        "replicated run detail",
    )?;
    let public: LabPublicState =
        decode_json(&fs::read(root.join("control/public.json"))?, "public state")?;

    let operational_request = request_for(&descriptor, &public, "distributed-federation");
    let alpha_miss = LabHttp::new(ALPHA_URL, ALPHA_DATA_TOKEN, &ca)?.post_json_headers(
        "/v1/federation/objects/resolve-local",
        &ResolveObjectRequest {
            schema: RESOLVE_SCHEMA.into(),
            request: operational_request.clone(),
        },
        &[("x-rusty-federation-hop", "1")],
    )?;
    if alpha_miss.status == 200 {
        return Err("empty alpha origin unexpectedly resolved the replicated beta run".into());
    }
    let federated = FederationProxyRequest {
        schema: FEDERATION_PROXY_SCHEMA.into(),
        request: operational_request.clone(),
        preferred_origin_id: None,
    };
    let resolved: ResolveObjectResponse = decode_json(
        &require_status(
            authority.post_json("/v1/federation/objects/resolve", &federated)?,
            200,
            "authority federation resolve",
        )?,
        "authority federation resolve",
    )?;
    let signed = resolved
        .signed_manifest
        .as_ref()
        .ok_or("federation response omitted its signed manifest")?;
    let authority_object = require_status(
        authority.get(&format!(
            "/v1/community/objects/{}",
            signed.manifest.object_sha256
        ))?,
        200,
        "authority staged object",
    )?;
    let authority_keys = TrustedSigningKeys::from([(
        public.authority_object_key_id.clone(),
        rw_community_protocol::parse_verifying_key_base64(
            &public.authority_object_public_key_base64,
        )?,
    )]);
    verify_signed_object(
        signed,
        &operational_request,
        &authority_object,
        now_unix(),
        &authority_keys,
        &rw_community_protocol::ProtocolLimits::default(),
    )?;
    write(
        root.join("results/federation-resolve.json"),
        serde_json::to_vec_pretty(&resolved)?,
    )?;

    let hot_pointer_bytes = require_status(
        r2.public_get(&format!(
            "/{R2_BUCKET}/v2/requests/{}.json",
            resolved.request_sha256
        ))?,
        200,
        "R2 promoted request pointer",
    )?;
    let hot_pointer: HotManifestPointer =
        decode_json(&hot_pointer_bytes, "R2 promoted request pointer")?;
    hot_pointer.validate_for_request(&resolved.request_sha256)?;
    let hot_manifest = require_status(
        r2.public_get(&format!(
            "/{R2_BUCKET}/v2/manifests/{}.json",
            hot_pointer.manifest_sha256
        ))?,
        200,
        "R2 promoted manifest",
    )?;
    if hot_manifest != serde_json::to_vec(signed)? {
        return Err("R2 manifest bytes differ from the authority-signed manifest".into());
    }
    let hot_object = require_status(
        r2.public_get(&format!(
            "/{R2_BUCKET}/v1/objects/{}",
            signed.manifest.object_sha256
        ))?,
        200,
        "R2 promoted object",
    )?;
    if hot_object != authority_object {
        return Err("R2 object bytes differ from the authority object".into());
    }
    let r2_writer = LabHttp::new(R2_URL, R2_WRITE_TOKEN, &ca)?;
    let immutable_replay = r2_writer.put_immutable_octets(
        &format!("/{R2_BUCKET}/v1/objects/{}", signed.manifest.object_sha256),
        &hot_object,
    )?;
    if immutable_replay.status != 412 {
        return Err(format!(
            "R2 immutable replay was not rejected: HTTP {}",
            immutable_replay.status
        )
        .into());
    }
    write(
        root.join("results/r2-immutability.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "rw.distributed-lab.r2-immutability.v1",
            "object_sha256": signed.manifest.object_sha256,
            "second_if_none_match_status": 412,
            "original_bytes_preserved": true
        }))?,
    )?;

    let relay_request = request_for(&descriptor, &public, "distributed-cold-relay");
    let beta_data = LabHttp::new(BETA_URL, BETA_DATA_TOKEN, &ca)?;
    let beta_resolved: ResolveObjectResponse = decode_json(
        &require_status(
            beta_data.post_json_headers(
                "/v1/federation/objects/resolve-local",
                &ResolveObjectRequest {
                    schema: RESOLVE_SCHEMA.into(),
                    request: relay_request.clone(),
                },
                &[("x-rusty-federation-hop", "1")],
            )?,
            200,
            "beta cold object resolve",
        )?,
        "beta cold object resolve",
    )?;
    let beta_signed = beta_resolved
        .signed_manifest
        .ok_or("beta cold response omitted its manifest")?;
    let relay_object = require_status(
        beta_data.get(&format!(
            "/v1/federation/objects/{}",
            beta_signed.manifest.object_sha256
        ))?,
        200,
        "beta cold object",
    )?;
    let cold_identity = request_sha256(&relay_request)?;
    let cold_r2 = r2.public_get(&format!("/{R2_BUCKET}/v1/manifests/{cold_identity}.json"))?;
    if cold_r2.status != 404 {
        return Err("cold relay fixture unexpectedly existed in R2".into());
    }
    let bundle = RelayBundle {
        request: relay_request,
        object_sha256: object_sha256(&relay_object),
        request_sha256: cold_identity,
        manifest: beta_signed,
    };
    if bundle.object_sha256 != bundle.manifest.manifest.object_sha256 {
        return Err("cold object hash differs from the beta signed identity".into());
    }
    write(
        root.join("control/relay-bundle.json"),
        serde_json::to_vec_pretty(&bundle)?,
    )?;
    write(root.join("control/relay-object.bin"), &relay_object)?;
    write(root.join("control/prepared"), b"ready\n")?;
    Ok(())
}

#[async_trait]
impl RelayBrokerHttp for LabHttp {
    async fn historical_lookup(
        &self,
        request: RelayHistoricalLookupRequest,
    ) -> Result<Vec<u8>, RelayError> {
        relay_bytes(self.post_json(RELAY_HISTORICAL_LOOKUP_PATH, &request))
    }

    async fn advertise(
        &self,
        request: RelayAdvertiseRequest,
    ) -> Result<AdvertisementReceipt, RelayError> {
        relay_json(self.post_json(RELAY_ADVERTISE_PATH, &request))
    }

    async fn next_grant(&self, request: RelayGrantPollRequest) -> Result<Vec<u8>, RelayError> {
        let reply = self
            .post_json(RELAY_NEXT_GRANT_PATH, &request)
            .map_err(|_| RelayError::TransportUnavailable)?;
        if reply.status == 404 {
            return Err(RelayError::NotAvailable);
        }
        relay_bytes(Ok(reply))
    }

    async fn register_route(
        &self,
        request: RelayRouteRegistrationRequest,
    ) -> Result<RelayRouteRegistrationReceipt, RelayError> {
        relay_json(self.post_json(RELAY_ROUTE_REGISTRATION_PATH, &request))
    }

    async fn transport_grant(
        &self,
        request: RelayTransportGrantRequest,
    ) -> Result<Vec<u8>, RelayError> {
        let reply = self
            .post_json(RELAY_TRANSPORT_GRANT_PATH, &request)
            .map_err(|_| RelayError::TransportUnavailable)?;
        if reply.status == 404 || reply.status == 409 {
            return Err(RelayError::NotAvailable);
        }
        relay_bytes(Ok(reply))
    }

    async fn complete(
        &self,
        request: RelaySessionCompletionRequest,
    ) -> Result<RelayTerminalResponse, RelayError> {
        relay_json(self.post_json(RELAY_SESSION_COMPLETE_PATH, &request))
    }

    async fn fail(
        &self,
        request: RelaySessionFailureRequest,
    ) -> Result<RelayTerminalResponse, RelayError> {
        relay_json(self.post_json(RELAY_SESSION_FAIL_PATH, &request))
    }

    async fn revoke(
        &self,
        request: RelaySessionFailureRequest,
    ) -> Result<RelayTerminalResponse, RelayError> {
        relay_json(self.post_json(RELAY_SESSION_REVOKE_PATH, &request))
    }
}

fn relay_bytes(result: LabResult<HttpReply>) -> Result<Vec<u8>, RelayError> {
    let reply = result.map_err(|_| RelayError::TransportUnavailable)?;
    if reply.status == 200 || reply.status == 201 {
        Ok(reply.bytes)
    } else if reply.status == 404 {
        Err(RelayError::NotAvailable)
    } else {
        Err(RelayError::TransportUnavailable)
    }
}

fn relay_json<T: DeserializeOwned>(result: LabResult<HttpReply>) -> Result<T, RelayError> {
    let bytes = relay_bytes(result)?;
    serde_json::from_slice(&bytes).map_err(|_| RelayError::CredentialInvalid)
}

struct OneSeed {
    object: VerifiedSeedObject,
}

impl VerifiedRelaySeedStore for OneSeed {
    fn load_exact(&self, object_sha256: &str) -> Result<Option<VerifiedSeedObject>, RelayError> {
        if self.object.manifest.manifest.object_sha256 != object_sha256 {
            return Ok(None);
        }
        Ok(Some(self.object.clone()))
    }
}

fn relay_security(public: &LabPublicState) -> LabResult<HistoricalRelaySecurity> {
    let origin =
        rw_community_protocol::parse_verifying_key_base64(&public.beta_object_public_key_base64)?;
    let relay = rw_community_protocol::parse_verifying_key_base64(
        &public.authority_relay_public_key_base64,
    )?;
    Ok(HistoricalRelaySecurity {
        trusted_origin_keys: TrustedSigningKeys::from([(
            public.beta_object_key_id.clone(),
            origin,
        )]),
        trusted_relay_keys: TrustedSigningKeys::from([(
            public.authority_relay_key_id.clone(),
            relay,
        )]),
        route_policy: RelayRoutePolicy::from_audited_cidrs(["11.231.0.15/32"])
            .map_err(|error| error.to_string())?,
        limits: rw_community_protocol::ProtocolLimits::default(),
    })
}

fn relay_policy() -> HistoricalRelayPolicy {
    HistoricalRelayPolicy {
        opted_in: true,
        seeding_opted_in: true,
        metered_network: false,
        allow_metered_seeding: false,
        disk_allowance_bytes: 64 * 1024 * 1024,
        upload_allowance_bytes: 64 * 1024 * 1024,
        download_allowance_bytes: 64 * 1024 * 1024,
        route_poll_attempts: 120,
        route_poll_interval: Duration::from_millis(250),
        session_timeout: Duration::from_secs(90),
        reliability: rw_community_relay::RelayReliabilityPolicy::default(),
    }
}

fn wait_file(path: &Path, seconds: u64) -> LabResult<()> {
    for _ in 0..seconds.saturating_mul(10) {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("timed out waiting for {}", path.display()).into())
}

async fn uploader(root: &Path) -> LabResult<()> {
    wait_file(&root.join("control/traffic-start"), 60)?;
    let public: LabPublicState =
        decode_json(&fs::read(root.join("control/public.json"))?, "public state")?;
    let bundle: RelayBundle = decode_json(
        &fs::read(root.join("control/relay-bundle.json"))?,
        "relay bundle",
    )?;
    let object = fs::read(root.join("control/relay-object.bin"))?;
    let seed = VerifiedSeedObject {
        manifest: bundle.manifest,
        encoded: object,
    };
    let http = LabHttp::new(AUTHORITY_URL, UPLOADER_TOKEN, &root.join("tls/ca.crt"))?;
    let client = HistoricalRelayClient::new(
        http,
        ProviderTurnAllocationFactory {
            family: AddressFamily::Ipv4,
        },
        relay_security(&public)?,
        relay_policy(),
    )?;
    client.advertise_verified(&seed, now_unix()).await?;
    write(root.join("control/advertised"), b"ready\n")?;
    let store = OneSeed { object: seed };
    for _ in 0..240 {
        match client.serve_one(&store, now_unix(), &NeverCancelled).await {
            Ok(true) => {
                write(
                    root.join("results/uploader.json"),
                    serde_json::to_vec_pretty(&serde_json::json!({
                        "schema": "rw.distributed-lab.uploader-result.v1",
                        "object_sha256": bundle.object_sha256,
                        "completed": true
                    }))?,
                )?;
                return Ok(());
            }
            Ok(false) | Err(RelayError::NotAvailable) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err("uploader never received a relay grant".into())
}

async fn downloader(root: &Path) -> LabResult<()> {
    wait_file(&root.join("control/traffic-start"), 60)?;
    wait_file(&root.join("control/advertised"), 60)?;
    let public: LabPublicState =
        decode_json(&fs::read(root.join("control/public.json"))?, "public state")?;
    let bundle: RelayBundle = decode_json(
        &fs::read(root.join("control/relay-bundle.json"))?,
        "relay bundle",
    )?;
    let r2 = LabHttp::new(
        R2_URL,
        "unused-public-get-token-000000000000",
        &root.join("tls/ca.crt"),
    )?;
    let miss = r2.public_get(&format!(
        "/{R2_BUCKET}/v2/requests/{}.json",
        bundle.request_sha256
    ))?;
    if miss.status != 404 {
        return Err("cold recovery was attempted despite an R2 hit".into());
    }
    let http = LabHttp::new(AUTHORITY_URL, DOWNLOADER_TOKEN, &root.join("tls/ca.crt"))?;
    let client = HistoricalRelayClient::new(
        http,
        ProviderTurnAllocationFactory {
            family: AddressFamily::Ipv4,
        },
        relay_security(&public)?,
        relay_policy(),
    )?;
    let outcome = client
        .recover_historical(
            &bundle.request,
            &bundle.manifest,
            now_unix(),
            &NeverCancelled,
        )
        .await?;
    let bytes = match outcome {
        HistoricalRelayOutcome::Recovered(bytes) => bytes,
        HistoricalRelayOutcome::Fallback(FallbackTarget::ArchivalHttpsOrigin) => {
            return Err("relay unexpectedly selected archival HTTPS fallback".into());
        }
        HistoricalRelayOutcome::Fallback(FallbackTarget::Unavailable) => {
            return Err("relay returned unavailable instead of exact recovery".into());
        }
    };
    if object_sha256(&bytes) != bundle.object_sha256 {
        return Err("recovered relay object failed its final SHA-256".into());
    }
    write(root.join("results/recovered-object.bin"), &bytes)?;
    write(
        root.join("results/downloader.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "rw.distributed-lab.downloader-result.v1",
            "object_sha256": bundle.object_sha256,
            "completed": true,
            "fallback": null
        }))?,
    )?;
    Ok(())
}

fn verify_results(root: &Path) -> LabResult<()> {
    let expected = fs::read(root.join("control/relay-object.bin"))?;
    let actual = fs::read(root.join("results/recovered-object.bin"))?;
    if expected != actual {
        return Err("relay recovery did not preserve exact object bytes".into());
    }
    verify_relay_accounting(root, &expected)?;
    for file in [
        "results/replication-finalized.json",
        "results/federation-resolve.json",
        "results/r2-immutability.json",
        "results/relay-accounting.json",
        "results/uploader.json",
        "results/downloader.json",
        "results/packet-proof.json",
    ] {
        let path = root.join(file);
        if !path.is_file() || fs::metadata(&path)?.len() == 0 {
            return Err(format!("missing release-lab evidence: {}", path.display()).into());
        }
    }
    let mut scan = Vec::new();
    for file in [
        "results/uploader.json",
        "results/downloader.json",
        "results/compose.log",
    ] {
        let path = root.join(file);
        if path.exists() {
            scan.extend(fs::read(path)?);
        }
    }
    let scan = String::from_utf8_lossy(&scan).to_ascii_lowercase();
    for forbidden in [
        "11.231.0.21",
        "11.231.0.22",
        "host candidate",
        "server-reflexive",
        "srflx",
        "stun:",
    ] {
        if scan.contains(forbidden) {
            return Err(
                format!("app-visible evidence leaked forbidden relay state: {forbidden}").into(),
            );
        }
    }
    write(
        root.join("results/verified.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "rw.distributed-lab.verification.v1",
            "replication": "passed",
            "two_origin_failover": "passed",
            "r2_immutable_hot_store": "passed",
            "relay_exact_sha256": "passed",
            "broker_role_completions": "passed",
            "relay_quota_teardown": "passed",
            "hot_promotion_signal": "passed",
            "packet_policy": "passed",
            "peer_visible_addresses": "absent"
        }))?,
    )?;
    Ok(())
}

fn verify_relay_accounting(root: &Path, expected: &[u8]) -> LabResult<()> {
    let state: serde_json::Value = decode_json(
        &fs::read(root.join("data/authority/community/relay-state.json"))?,
        "relay accounting state",
    )?;
    if state.get("schema").and_then(serde_json::Value::as_str)
        != Some("rw.community.relay-state.v2")
    {
        return Err("relay accounting state has an unexpected schema".into());
    }
    let usage = state
        .get("usage")
        .and_then(serde_json::Value::as_object)
        .ok_or("relay accounting state omitted usage")?;
    if usage.len() != 2 {
        return Err("relay accounting did not contain two distinct principals".into());
    }
    let mut uploaded = 0_u64;
    let mut downloaded = 0_u64;
    for principal in usage.values() {
        uploaded = uploaded
            .checked_add(
                principal
                    .get("uploaded")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("relay principal accounting omitted uploaded bytes")?,
            )
            .ok_or("relay uploaded accounting overflow")?;
        downloaded = downloaded
            .checked_add(
                principal
                    .get("downloaded")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("relay principal accounting omitted downloaded bytes")?,
            )
            .ok_or("relay downloaded accounting overflow")?;
        for zero_field in [
            "reserved_upload",
            "reserved_download",
            "active_uploads",
            "active_downloads",
        ] {
            if principal
                .get(zero_field)
                .and_then(serde_json::Value::as_u64)
                != Some(0)
            {
                return Err(format!("relay principal did not release {zero_field}").into());
            }
        }
    }
    let expected_bytes = u64::try_from(expected.len())?;
    if uploaded != expected_bytes || downloaded != expected_bytes {
        return Err("both relay roles did not account the exact object size".into());
    }
    if state
        .get("global_relayed")
        .and_then(serde_json::Value::as_u64)
        != Some(expected_bytes)
        || state
            .get("global_reserved")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
    {
        return Err("global relay quota accounting was not settled".into());
    }
    if !state
        .get("pending_sessions")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        return Err("completed relay session remained pending".into());
    }
    let revoked_roles = state
        .get("revoked_credentials")
        .and_then(serde_json::Value::as_object)
        .map(serde_json::Map::len)
        .ok_or("relay state omitted revoked credentials")?;
    if revoked_roles != 2 {
        return Err("both role credentials were not revoked at teardown".into());
    }
    let object_hash = object_sha256(expected);
    let popularity = state
        .get("popularity")
        .and_then(serde_json::Value::as_object)
        .and_then(|entries| entries.get(&object_hash))
        .ok_or("completed recovery did not update popularity")?;
    if popularity
        .get("successful_recoveries")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
        || popularity
            .get("relayed_bytes")
            .and_then(serde_json::Value::as_u64)
            != Some(expected_bytes)
        || popularity
            .get("promotion_emitted")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
    {
        return Err("successful relay did not emit its hot-promotion signal".into());
    }
    write(
        root.join("results/relay-accounting.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "rw.distributed-lab.relay-accounting.v1",
            "distinct_principals": 2,
            "uploader_completed_bytes": uploaded,
            "downloader_completed_bytes": downloaded,
            "active_uploads": 0,
            "active_downloads": 0,
            "global_reserved_bytes": 0,
            "pending_sessions": 0,
            "revoked_role_credentials": revoked_roles,
            "successful_recoveries": 1,
            "promotion_emitted": true
        }))?,
    )?;
    Ok(())
}

#[tokio::main]
async fn main() -> LabResult<()> {
    let mut args = std::env::args().skip(1);
    let command = args
        .next()
        .ok_or("expected provision, exercise, uploader, downloader, or verify")?;
    let root = PathBuf::from(args.next().unwrap_or_else(|| "/lab".into()));
    match command.as_str() {
        "provision" => provision(&root),
        "exercise" => exercise(&root),
        "uploader" => uploader(&root).await,
        "downloader" => downloader(&root).await,
        "verify" => verify_results(&root),
        _ => Err(format!("unknown labctl command: {command}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_identities_are_valid_and_distinct_from_real_fixtures() {
        assert_eq!(R2_READINESS_REQUEST_SHA256.len(), 64);
        assert!(
            R2_READINESS_REQUEST_SHA256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_eq!(CLOUDFLARE_READINESS_KEY_ID.len(), 32);
        assert!(
            CLOUDFLARE_READINESS_KEY_ID
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_ne!(CLOUDFLARE_READINESS_KEY_ID, TURN_KEY_ID);
    }

    #[test]
    fn readiness_retries_transport_and_non_404_until_exact_not_found() {
        let mut calls = 0;
        wait_for_exact_not_found_with_policy("scripted endpoint", 3, Duration::ZERO, || {
            calls += 1;
            match calls {
                1 => Err("not listening yet".into()),
                2 => Ok(HttpReply {
                    status: 200,
                    bytes: Vec::new(),
                }),
                _ => Ok(HttpReply {
                    status: 404,
                    bytes: Vec::new(),
                }),
            }
        })
        .unwrap();
        assert_eq!(calls, 3);
    }

    #[test]
    fn readiness_is_bounded_and_rejects_every_non_404_status() {
        let mut calls = 0;
        let error =
            wait_for_exact_not_found_with_policy("scripted endpoint", 2, Duration::ZERO, || {
                calls += 1;
                Ok(HttpReply {
                    status: 503,
                    bytes: Vec::new(),
                })
            })
            .unwrap_err();
        assert_eq!(calls, 2);
        assert!(error.to_string().contains("exact HTTP 404"));
        assert!(error.to_string().contains("HTTP 503"));
    }

    #[cfg(unix)]
    #[test]
    fn only_non_secret_evidence_directory_becomes_host_writable() {
        use std::os::unix::fs::PermissionsExt as _;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rw-distributed-lab-permissions-{}-{nonce}",
            std::process::id()
        ));
        let results = root.join("results");
        let secret = root.join("secrets/credential");
        fs::create_dir_all(&results).unwrap();
        write(&secret, b"not-a-real-secret").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();

        make_evidence_directory_host_writable(&results).unwrap();

        assert_eq!(
            fs::metadata(&results).unwrap().permissions().mode() & 0o777,
            0o777
        );
        assert_eq!(
            fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }
}
