//! Bounded, content-addressed storage used by the opt-in Community Cache.
//!
//! This module is transport-neutral. In particular, it has no discovery,
//! signaling, ICE, STUN, peer-address, or direct-connectivity surface.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rw_community_protocol::SignedCaseRoomManifest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const INDEX_SCHEMA: &str = "rw.community.local-index.v1";
const MAX_DIRECTORY_ENTRIES: usize = 1_000_000;
const MAX_CASE_DIRECTORY_ENTRIES: usize = 100_000;
const ACCOUNTING_SCHEMA: &str = "rw.community.accounting.v1";
const MAX_ACCOUNTING_STATE_BYTES: u64 = 8 * 1024 * 1024;
type ManifestAndObject = (Vec<u8>, Vec<u8>);

#[derive(Debug, Error)]
pub enum CommunityStoreError {
    #[error("Community Cache is disabled by the server kill switch")]
    Killed,
    #[error("community object or manifest exceeds a configured size limit")]
    TooLarge,
    #[error("community storage or transfer quota is exhausted")]
    Quota,
    #[error("community object hash does not match its bytes")]
    HashMismatch,
    #[error("invalid community cache identity: {0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] rw_store::RwStoreError),
}

#[derive(Debug, Clone, Copy)]
pub struct CasLimits {
    pub maximum_object_bytes: u64,
    pub maximum_manifest_bytes: u64,
    pub storage_bytes: u64,
    pub maximum_objects: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct CaseLimits {
    pub maximum_manifest_bytes: u64,
    pub storage_bytes: u64,
    pub maximum_cases: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexEntry {
    schema: String,
    request_sha256: String,
    object_sha256: String,
    object_bytes: u64,
    accessed_unix: i64,
}

#[derive(Debug, Default)]
struct CasIndex {
    requests: BTreeMap<String, IndexEntry>,
    object_sizes: BTreeMap<String, u64>,
}

#[derive(Clone)]
pub struct CommunityCas {
    root: Arc<PathBuf>,
    limits: CasLimits,
    case_limits: CaseLimits,
    index: Arc<Mutex<CasIndex>>,
}

impl std::fmt::Debug for CommunityCas {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommunityCas")
            .field("root", &self.root)
            .field("limits", &self.limits)
            .field("case_limits", &self.case_limits)
            .finish_non_exhaustive()
    }
}

impl CommunityCas {
    pub fn open(
        root: impl Into<PathBuf>,
        limits: CasLimits,
        case_limits: CaseLimits,
    ) -> Result<Self, CommunityStoreError> {
        if limits.maximum_object_bytes == 0
            || limits.maximum_manifest_bytes == 0
            || limits.storage_bytes < limits.maximum_object_bytes
            || limits.maximum_objects == 0
        {
            return Err(CommunityStoreError::Invalid(
                "CAS limits are zero or internally inconsistent".into(),
            ));
        }
        if case_limits.maximum_manifest_bytes == 0
            || case_limits.storage_bytes < case_limits.maximum_manifest_bytes
            || case_limits.maximum_cases == 0
            || case_limits.maximum_cases > MAX_CASE_DIRECTORY_ENTRIES
        {
            return Err(CommunityStoreError::Invalid(
                "case storage limits are zero, excessive, or internally inconsistent".into(),
            ));
        }
        let root = root.into();
        create_real_directory(&root)?;
        create_real_directory(&root.join("objects"))?;
        create_real_directory(&root.join("manifests"))?;
        create_real_directory(&root.join("index"))?;
        create_real_directory(&root.join("cases"))?;
        let index = load_index(&root, limits)?;
        let store = Self {
            root: Arc::new(root),
            limits,
            case_limits,
            index: Arc::new(Mutex::new(index)),
        };
        store.inspect_cases(now_unix(), true)?;
        Ok(store)
    }

    /// Atomically publish an already-signed canonical manifest and its exact
    /// immutable payload. Cryptographic signature validation belongs to the
    /// protocol boundary and must occur before this method is called.
    pub fn put(
        &self,
        request_sha256: &str,
        object_sha256: &str,
        object: &[u8],
        signed_manifest: &[u8],
    ) -> Result<(), CommunityStoreError> {
        validate_hash(request_sha256)?;
        validate_hash(object_sha256)?;
        if object.len() as u64 > self.limits.maximum_object_bytes
            || signed_manifest.len() as u64 > self.limits.maximum_manifest_bytes
        {
            return Err(CommunityStoreError::TooLarge);
        }
        if sha256_hex(object) != object_sha256 {
            return Err(CommunityStoreError::HashMismatch);
        }

        let mut index = self.index.lock().expect("community CAS mutex poisoned");
        let existing_size = index.object_sizes.get(object_sha256).copied().unwrap_or(0);
        let added_bytes = (object.len() as u64).saturating_sub(existing_size);
        evict_for_admission(
            &self.root,
            self.limits,
            &mut index,
            added_bytes,
            object_sha256,
        )?;

        let object_path = self.object_path(object_sha256);
        if object_path.exists() {
            let metadata = regular_file_metadata(&object_path)?;
            if metadata.len() != object.len() as u64
                || sha256_hex(&read_bounded(
                    &object_path,
                    self.limits.maximum_object_bytes,
                )?) != object_sha256
            {
                return Err(CommunityStoreError::HashMismatch);
            }
        } else {
            rw_store::atomic::atomic_write_bytes(&object_path, object)?;
        }
        rw_store::atomic::atomic_write_bytes(&self.manifest_path(request_sha256), signed_manifest)?;
        let entry = IndexEntry {
            schema: INDEX_SCHEMA.into(),
            request_sha256: request_sha256.into(),
            object_sha256: object_sha256.into(),
            object_bytes: object.len() as u64,
            accessed_unix: now_unix(),
        };
        rw_store::atomic::atomic_write_bytes(
            &self.index_path(request_sha256),
            &serde_json::to_vec(&entry)?,
        )?;
        index
            .object_sizes
            .insert(object_sha256.into(), object.len() as u64);
        index.requests.insert(request_sha256.into(), entry);
        Ok(())
    }

    pub fn get(
        &self,
        request_sha256: &str,
    ) -> Result<Option<ManifestAndObject>, CommunityStoreError> {
        validate_hash(request_sha256)?;
        let mut index = self.index.lock().expect("community CAS mutex poisoned");
        let Some(entry) = index.requests.get(request_sha256).cloned() else {
            return Ok(None);
        };
        let manifest = read_bounded(
            &self.manifest_path(request_sha256),
            self.limits.maximum_manifest_bytes,
        )?;
        let object = read_bounded(
            &self.object_path(&entry.object_sha256),
            self.limits.maximum_object_bytes,
        )?;
        if object.len() as u64 != entry.object_bytes || sha256_hex(&object) != entry.object_sha256 {
            return Err(CommunityStoreError::HashMismatch);
        }
        let mut touched = entry;
        touched.accessed_unix = now_unix();
        rw_store::atomic::atomic_write_bytes(
            &self.index_path(request_sha256),
            &serde_json::to_vec(&touched)?,
        )?;
        index.requests.insert(request_sha256.into(), touched);
        Ok(Some((manifest, object)))
    }

    pub fn get_object(&self, object_sha256: &str) -> Result<Option<Vec<u8>>, CommunityStoreError> {
        validate_hash(object_sha256)?;
        let path = self.object_path(object_sha256);
        if !path.exists() {
            return Ok(None);
        }
        let object = read_bounded(&path, self.limits.maximum_object_bytes)?;
        if sha256_hex(&object) != object_sha256 {
            return Err(CommunityStoreError::HashMismatch);
        }
        Ok(Some(object))
    }

    /// Return one durable request-manifest reference for an object. Callers
    /// must verify the signed manifest before serving the returned bytes.
    pub fn get_object_reference(
        &self,
        object_sha256: &str,
    ) -> Result<Option<ManifestAndObject>, CommunityStoreError> {
        validate_hash(object_sha256)?;
        let request = self
            .index
            .lock()
            .expect("community CAS mutex poisoned")
            .requests
            .values()
            .find(|entry| entry.object_sha256 == object_sha256)
            .map(|entry| entry.request_sha256.clone());
        match request {
            Some(request) => self.get(&request),
            None => Ok(None),
        }
    }

    /// Drop one untrusted/stale request mapping and its now-unreferenced
    /// object. All targets are validated fixed-layout cache entries.
    pub fn invalidate_request(&self, request_sha256: &str) -> Result<(), CommunityStoreError> {
        validate_hash(request_sha256)?;
        let mut index = self.index.lock().expect("community CAS mutex poisoned");
        let Some(entry) = index.requests.remove(request_sha256) else {
            return Ok(());
        };
        remove_file_if_present(&self.index_path(request_sha256))?;
        remove_file_if_present(&self.manifest_path(request_sha256))?;
        if !index
            .requests
            .values()
            .any(|candidate| candidate.object_sha256 == entry.object_sha256)
        {
            remove_file_if_present(&self.object_path(&entry.object_sha256))?;
            index.object_sizes.remove(&entry.object_sha256);
        }
        Ok(())
    }

    pub fn put_case(&self, case_id: &str, bytes: &[u8]) -> Result<(), CommunityStoreError> {
        validate_case_id(case_id)?;
        if bytes.is_empty() || bytes.len() as u64 > self.case_limits.maximum_manifest_bytes {
            return Err(CommunityStoreError::TooLarge);
        }
        let signed: SignedCaseRoomManifest = serde_json::from_slice(bytes)?;
        if signed.manifest.case_id != case_id {
            return Err(CommunityStoreError::Invalid(
                "case filename identity does not match its signed manifest".into(),
            ));
        }
        let now = now_unix();
        if signed.manifest.retain_until_unix <= now {
            return Err(CommunityStoreError::Invalid(
                "expired case manifests cannot be stored".into(),
            ));
        }
        let cases = self.inspect_cases(now, true)?;
        let existing_bytes = cases.get(case_id).copied().unwrap_or(0);
        let count_after = cases.len() + usize::from(existing_bytes == 0);
        let bytes_after = cases
            .values()
            .copied()
            .sum::<u64>()
            .saturating_sub(existing_bytes)
            .checked_add(bytes.len() as u64)
            .ok_or(CommunityStoreError::Quota)?;
        if count_after > self.case_limits.maximum_cases
            || bytes_after > self.case_limits.storage_bytes
        {
            return Err(CommunityStoreError::Quota);
        }
        rw_store::atomic::atomic_write_bytes(&self.case_path(case_id), bytes)?;
        Ok(())
    }

    pub fn get_case(&self, case_id: &str) -> Result<Option<Vec<u8>>, CommunityStoreError> {
        validate_case_id(case_id)?;
        let path = self.case_path(case_id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = read_bounded(&path, self.case_limits.maximum_manifest_bytes)?;
        let signed: SignedCaseRoomManifest = serde_json::from_slice(&bytes)?;
        if signed.manifest.case_id != case_id {
            return Err(CommunityStoreError::Invalid(
                "stored case filename does not match its signed identity".into(),
            ));
        }
        if signed.manifest.retain_until_unix <= now_unix() {
            remove_file_if_present(&path)?;
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    pub fn case_count(&self) -> Result<usize, CommunityStoreError> {
        Ok(self.inspect_cases(now_unix(), true)?.len())
    }

    pub fn case_storage_bytes(&self) -> Result<u64, CommunityStoreError> {
        Ok(self
            .inspect_cases(now_unix(), true)?
            .values()
            .copied()
            .sum())
    }

    pub fn object_count(&self) -> usize {
        self.index
            .lock()
            .expect("community CAS mutex poisoned")
            .object_sizes
            .len()
    }

    pub fn storage_bytes(&self) -> u64 {
        self.index
            .lock()
            .expect("community CAS mutex poisoned")
            .object_sizes
            .values()
            .copied()
            .sum()
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.root.join("objects").join(hash)
    }

    fn manifest_path(&self, hash: &str) -> PathBuf {
        self.root.join("manifests").join(format!("{hash}.json"))
    }

    fn index_path(&self, hash: &str) -> PathBuf {
        self.root.join("index").join(format!("{hash}.json"))
    }

    fn case_path(&self, case_id: &str) -> PathBuf {
        self.root.join("cases").join(format!("{case_id}.json"))
    }

    fn inspect_cases(
        &self,
        now: i64,
        clean_expired: bool,
    ) -> Result<BTreeMap<String, u64>, CommunityStoreError> {
        let mut cases = BTreeMap::new();
        for (inspected, directory_entry) in fs::read_dir(self.root.join("cases"))?.enumerate() {
            if inspected >= MAX_CASE_DIRECTORY_ENTRIES {
                return Err(CommunityStoreError::Invalid(
                    "case storage contains too many directory entries".into(),
                ));
            }
            let directory_entry = directory_entry?;
            if !directory_entry.file_type()?.is_file() {
                return Err(CommunityStoreError::Invalid(
                    "case storage contains a non-file entry".into(),
                ));
            }
            let path = directory_entry.path();
            let case_id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    CommunityStoreError::Invalid("case filename is not valid UTF-8".into())
                })?;
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                return Err(CommunityStoreError::Invalid(
                    "case storage contains an unsupported filename".into(),
                ));
            }
            validate_case_id(case_id)?;
            let bytes = read_bounded(&path, self.case_limits.maximum_manifest_bytes)?;
            let signed: SignedCaseRoomManifest = serde_json::from_slice(&bytes)?;
            if signed.manifest.case_id != case_id {
                return Err(CommunityStoreError::Invalid(
                    "stored case filename does not match its signed identity".into(),
                ));
            }
            if signed.manifest.retain_until_unix <= now {
                if clean_expired {
                    remove_file_if_present(&path)?;
                }
                continue;
            }
            cases.insert(case_id.to_string(), bytes.len() as u64);
        }
        let total_bytes = cases.values().copied().sum::<u64>();
        if cases.len() > self.case_limits.maximum_cases
            || total_bytes > self.case_limits.storage_bytes
        {
            return Err(CommunityStoreError::Quota);
        }
        Ok(cases)
    }
}

fn load_index(root: &Path, limits: CasLimits) -> Result<CasIndex, CommunityStoreError> {
    let mut index = CasIndex::default();
    for (inspected, directory_entry) in fs::read_dir(root.join("index"))?.enumerate() {
        if inspected >= MAX_DIRECTORY_ENTRIES {
            return Err(CommunityStoreError::Invalid(
                "community index has too many entries".into(),
            ));
        }
        let directory_entry = directory_entry?;
        let path = directory_entry.path();
        if !directory_entry.file_type()?.is_file() {
            return Err(CommunityStoreError::Invalid(
                "community index contains a non-file entry".into(),
            ));
        }
        let bytes = read_bounded(&path, limits.maximum_manifest_bytes)?;
        let entry: IndexEntry = serde_json::from_slice(&bytes)?;
        validate_index_entry(&entry)?;
        let filename = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if filename != entry.request_sha256 {
            return Err(CommunityStoreError::Invalid(
                "community index filename does not match request identity".into(),
            ));
        }
        let object_path = root.join("objects").join(&entry.object_sha256);
        let metadata = regular_file_metadata(&object_path)?;
        if metadata.len() != entry.object_bytes || metadata.len() > limits.maximum_object_bytes {
            return Err(CommunityStoreError::Invalid(
                "community object metadata does not match its index".into(),
            ));
        }
        index
            .object_sizes
            .insert(entry.object_sha256.clone(), entry.object_bytes);
        index.requests.insert(entry.request_sha256.clone(), entry);
    }
    if index.object_sizes.len() > limits.maximum_objects
        || index.object_sizes.values().copied().sum::<u64>() > limits.storage_bytes
    {
        evict_for_admission(root, limits, &mut index, 0, "")?;
    }
    Ok(index)
}

fn evict_for_admission(
    root: &Path,
    limits: CasLimits,
    index: &mut CasIndex,
    added_bytes: u64,
    incoming_hash: &str,
) -> Result<(), CommunityStoreError> {
    let mut order = index
        .requests
        .values()
        .map(|entry| (entry.accessed_unix, entry.request_sha256.clone()))
        .collect::<Vec<_>>();
    order.sort();
    let mut cursor = 0usize;
    loop {
        let bytes = index.object_sizes.values().copied().sum::<u64>();
        let incoming_is_new =
            !incoming_hash.is_empty() && !index.object_sizes.contains_key(incoming_hash);
        let objects_after = index.object_sizes.len() + usize::from(incoming_is_new);
        if bytes.saturating_add(added_bytes) <= limits.storage_bytes
            && objects_after <= limits.maximum_objects
        {
            return Ok(());
        }
        let Some((_, request_hash)) = order.get(cursor) else {
            return Err(CommunityStoreError::Quota);
        };
        cursor += 1;
        let Some(entry) = index.requests.remove(request_hash) else {
            continue;
        };
        remove_file_if_present(&root.join("index").join(format!("{request_hash}.json")))?;
        remove_file_if_present(&root.join("manifests").join(format!("{request_hash}.json")))?;
        if !index
            .requests
            .values()
            .any(|candidate| candidate.object_sha256 == entry.object_sha256)
        {
            remove_file_if_present(&root.join("objects").join(&entry.object_sha256))?;
            index.object_sizes.remove(&entry.object_sha256);
        }
    }
}

fn validate_index_entry(entry: &IndexEntry) -> Result<(), CommunityStoreError> {
    if entry.schema != INDEX_SCHEMA || entry.object_bytes == 0 {
        return Err(CommunityStoreError::Invalid(
            "unsupported or malformed community index entry".into(),
        ));
    }
    validate_hash(&entry.request_sha256)?;
    validate_hash(&entry.object_sha256)
}

fn validate_hash(hash: &str) -> Result<(), CommunityStoreError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(CommunityStoreError::Invalid(
            "invalid SHA-256 identity".into(),
        ));
    }
    Ok(())
}

fn validate_case_id(case_id: &str) -> Result<(), CommunityStoreError> {
    if case_id.is_empty()
        || case_id.len() > 128
        || !case_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CommunityStoreError::Invalid("invalid case-room id".into()));
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, CommunityStoreError> {
    let metadata = regular_file_metadata(path)?;
    if metadata.len() > limit {
        return Err(CommunityStoreError::TooLarge);
    }
    Ok(fs::read(path)?)
}

fn regular_file_metadata(path: &Path) -> Result<fs::Metadata, CommunityStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(CommunityStoreError::Invalid(
            "community cache entry is not a regular file".into(),
        ));
    }
    Ok(metadata)
}

fn create_real_directory(path: &Path) -> Result<(), CommunityStoreError> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(CommunityStoreError::Invalid(
            "community cache root must be a real directory".into(),
        ));
    }
    Ok(())
}

fn remove_file_if_present(path: &Path) -> Result<(), CommunityStoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
pub struct AccountingLimits {
    pub upload_bytes_per_month: u64,
    pub download_bytes_per_month: u64,
    pub promoted_bytes_per_month: u64,
    pub concurrent_transfers: usize,
    pub maximum_principals: usize,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrincipalUsage {
    uploaded: u64,
    downloaded: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableAccountingState {
    schema: String,
    month: u32,
    promoted_bytes: u64,
    principals: BTreeMap<String, PrincipalUsage>,
}

impl DurableAccountingState {
    fn empty(month: u32) -> Self {
        Self {
            schema: ACCOUNTING_SCHEMA.into(),
            month,
            promoted_bytes: 0,
            principals: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct AccountingInner {
    durable: DurableAccountingState,
    active: BTreeMap<String, usize>,
}

/// Durable per-principal monthly transfer accounting. HTTP auth supplies a
/// stable digest as the principal, so bearer tokens are never persisted.
#[derive(Debug, Clone)]
pub struct QuotaLedger {
    path: Option<Arc<PathBuf>>,
    limits: AccountingLimits,
    inner: Arc<Mutex<AccountingInner>>,
}

pub struct TransferPermit {
    principal: String,
    ledger: QuotaLedger,
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.ledger.inner.lock() {
            let remove = if let Some(active) = inner.active.get_mut(&self.principal) {
                *active = active.saturating_sub(1);
                *active == 0
            } else {
                false
            };
            if remove {
                inner.active.remove(&self.principal);
            }
        }
    }
}

impl QuotaLedger {
    pub fn open(
        path: impl Into<PathBuf>,
        limits: AccountingLimits,
        month: u32,
    ) -> Result<Self, CommunityStoreError> {
        validate_accounting_limits(limits)?;
        validate_month(month)?;
        let path = path.into();
        let parent = path.parent().ok_or_else(|| {
            CommunityStoreError::Invalid("accounting state path has no parent".into())
        })?;
        create_real_directory(parent)?;
        let durable = if path.exists() {
            let bytes = read_bounded(&path, MAX_ACCOUNTING_STATE_BYTES)?;
            let state: DurableAccountingState = serde_json::from_slice(&bytes)?;
            validate_accounting_state(&state, limits)?;
            if state.month == month {
                state
            } else {
                DurableAccountingState::empty(month)
            }
        } else {
            DurableAccountingState::empty(month)
        };
        let ledger = Self {
            path: Some(Arc::new(path)),
            limits,
            inner: Arc::new(Mutex::new(AccountingInner {
                durable,
                active: BTreeMap::new(),
            })),
        };
        ledger.persist_current()?;
        Ok(ledger)
    }

    pub fn memory(limits: AccountingLimits, month: u32) -> Result<Self, CommunityStoreError> {
        validate_accounting_limits(limits)?;
        validate_month(month)?;
        Ok(Self {
            path: None,
            limits,
            inner: Arc::new(Mutex::new(AccountingInner {
                durable: DurableAccountingState::empty(month),
                active: BTreeMap::new(),
            })),
        })
    }

    pub fn begin(
        &self,
        principal: &str,
        month: u32,
    ) -> Result<TransferPermit, CommunityStoreError> {
        validate_quota_principal(principal)?;
        validate_month(month)?;
        let mut inner = self.inner.lock().expect("community quota mutex poisoned");
        if inner.durable.month != month {
            let candidate = DurableAccountingState::empty(month);
            self.persist(&candidate)?;
            inner.durable = candidate;
        }
        if !inner.durable.principals.contains_key(principal) {
            if inner.durable.principals.len() >= self.limits.maximum_principals {
                return Err(CommunityStoreError::Quota);
            }
            let mut candidate = inner.durable.clone();
            candidate
                .principals
                .insert(principal.into(), PrincipalUsage::default());
            self.persist(&candidate)?;
            inner.durable = candidate;
        }
        let active = inner.active.get(principal).copied().unwrap_or(0);
        if active >= self.limits.concurrent_transfers {
            return Err(CommunityStoreError::Quota);
        }
        inner.active.insert(principal.into(), active + 1);
        Ok(TransferPermit {
            principal: principal.into(),
            ledger: self.clone(),
        })
    }

    pub fn charge_upload(&self, principal: &str, bytes: u64) -> Result<(), CommunityStoreError> {
        self.charge(principal, bytes, true)
    }

    pub fn charge_download(&self, principal: &str, bytes: u64) -> Result<(), CommunityStoreError> {
        self.charge(principal, bytes, false)
    }

    fn charge(&self, principal: &str, bytes: u64, upload: bool) -> Result<(), CommunityStoreError> {
        validate_quota_principal(principal)?;
        let mut inner = self.inner.lock().expect("community quota mutex poisoned");
        let mut candidate = inner.durable.clone();
        let usage = candidate
            .principals
            .get_mut(principal)
            .ok_or(CommunityStoreError::Quota)?;
        let (used, limit) = if upload {
            (&mut usage.uploaded, self.limits.upload_bytes_per_month)
        } else {
            (&mut usage.downloaded, self.limits.download_bytes_per_month)
        };
        let next = used.checked_add(bytes).ok_or(CommunityStoreError::Quota)?;
        if next > limit {
            return Err(CommunityStoreError::Quota);
        }
        *used = next;
        self.persist(&candidate)?;
        inner.durable = candidate;
        Ok(())
    }

    pub fn reserve_promotion(&self, month: u32, bytes: u64) -> Result<(), CommunityStoreError> {
        validate_month(month)?;
        let mut inner = self.inner.lock().expect("community quota mutex poisoned");
        if inner.durable.month != month {
            let candidate = DurableAccountingState::empty(month);
            self.persist(&candidate)?;
            inner.durable = candidate;
        }
        let next = inner
            .durable
            .promoted_bytes
            .checked_add(bytes)
            .ok_or(CommunityStoreError::Quota)?;
        if next > self.limits.promoted_bytes_per_month {
            return Err(CommunityStoreError::Quota);
        }
        let mut candidate = inner.durable.clone();
        candidate.promoted_bytes = next;
        self.persist(&candidate)?;
        inner.durable = candidate;
        Ok(())
    }

    fn persist_current(&self) -> Result<(), CommunityStoreError> {
        let inner = self.inner.lock().expect("community quota mutex poisoned");
        self.persist(&inner.durable)
    }

    fn persist(&self, state: &DurableAccountingState) -> Result<(), CommunityStoreError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(state)?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_ACCOUNTING_STATE_BYTES {
            return Err(CommunityStoreError::TooLarge);
        }
        rw_store::atomic::atomic_write_bytes(path, &bytes)?;
        Ok(())
    }
}

fn validate_accounting_limits(limits: AccountingLimits) -> Result<(), CommunityStoreError> {
    if limits.upload_bytes_per_month == 0
        || limits.download_bytes_per_month == 0
        || limits.promoted_bytes_per_month == 0
        || limits.concurrent_transfers == 0
        || limits.maximum_principals == 0
    {
        return Err(CommunityStoreError::Invalid(
            "accounting limits must all be greater than zero".into(),
        ));
    }
    Ok(())
}

fn validate_accounting_state(
    state: &DurableAccountingState,
    limits: AccountingLimits,
) -> Result<(), CommunityStoreError> {
    if state.schema != ACCOUNTING_SCHEMA || state.principals.len() > limits.maximum_principals {
        return Err(CommunityStoreError::Invalid(
            "unsupported or oversized accounting state".into(),
        ));
    }
    validate_month(state.month)?;
    if state.promoted_bytes > limits.promoted_bytes_per_month {
        return Err(CommunityStoreError::Quota);
    }
    for (principal, usage) in &state.principals {
        validate_quota_principal(principal)?;
        if usage.uploaded > limits.upload_bytes_per_month
            || usage.downloaded > limits.download_bytes_per_month
        {
            return Err(CommunityStoreError::Quota);
        }
    }
    Ok(())
}

fn validate_month(month: u32) -> Result<(), CommunityStoreError> {
    if month / 100 < 1970 || !(1..=12).contains(&(month % 100)) {
        return Err(CommunityStoreError::Invalid(
            "invalid accounting month".into(),
        ));
    }
    Ok(())
}

fn validate_quota_principal(principal: &str) -> Result<(), CommunityStoreError> {
    if principal.is_empty()
        || principal.len() > 128
        || !principal
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CommunityStoreError::Invalid(
            "invalid quota principal".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rw_community_protocol::{
        CASE_SCHEMA, CaseArtifactRef, CaseArtifactType, CaseModelSource, CaseRoomManifest,
        DataOrigin, PublicationGrant, SourceProvenance, sign_case_manifest,
    };

    fn limits(bytes: u64, objects: usize) -> CasLimits {
        CasLimits {
            maximum_object_bytes: bytes,
            maximum_manifest_bytes: 1024,
            storage_bytes: bytes,
            maximum_objects: objects,
        }
    }

    fn case_limits(bytes: u64, cases: usize) -> CaseLimits {
        CaseLimits {
            maximum_manifest_bytes: bytes,
            storage_bytes: bytes.saturating_mul(cases as u64),
            maximum_cases: cases,
        }
    }

    fn accounting_limits(upload: u64, download: u64, promoted: u64) -> AccountingLimits {
        AccountingLimits {
            upload_bytes_per_month: upload,
            download_bytes_per_month: download,
            promoted_bytes_per_month: promoted,
            concurrent_transfers: 1,
            maximum_principals: 2,
        }
    }

    fn signed_case(case_id: &str, retain_until_unix: i64) -> Vec<u8> {
        let manifest = CaseRoomManifest {
            schema: CASE_SCHEMA.into(),
            case_id: case_id.into(),
            title: "Bounded case".into(),
            event_start_unix: 1,
            event_end_unix: 2,
            published_unix: retain_until_unix.saturating_sub(60),
            retain_until_unix,
            publication: PublicationGrant {
                data_origin: DataOrigin::PublicProvider,
                explicit_owner_publication: true,
                redistribution_rights_confirmed: true,
            },
            sources: vec![CaseModelSource {
                model: "hrrr".into(),
                run: "20260812T00Z".into(),
                snapshot_id: "a".repeat(64),
                grid_hash: "b".repeat(64),
                source_provenance: vec![SourceProvenance {
                    provider: "noaa-aws-public-data".into(),
                    roles: vec!["surface".into()],
                    products: vec!["wrfsfc".into()],
                }],
            }],
            artifacts: vec![CaseArtifactRef {
                artifact_id: "artifact-a".into(),
                artifact_type: CaseArtifactType::DerivedTable,
                request_sha256: "c".repeat(64),
                object_sha256: "d".repeat(64),
            }],
            attributions: vec![],
            modification_notices: vec!["Derived by Rusty Weather.".into()],
        };
        serde_json::to_vec(
            &sign_case_manifest(manifest, "origin-a", &SigningKey::from_bytes(&[7u8; 32])).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn cas_rejects_tampered_hashes_and_oversized_objects() {
        let directory = tempfile::tempdir().unwrap();
        let cas = CommunityCas::open(directory.path(), limits(8, 2), case_limits(1024, 2)).unwrap();
        let request = sha256_hex(b"request");
        let object = sha256_hex(b"hello");
        assert!(matches!(
            cas.put(&request, &object, b"tamper", b"{}"),
            Err(CommunityStoreError::HashMismatch)
        ));
        assert!(matches!(
            cas.put(&request, &sha256_hex(b"123456789"), b"123456789", b"{}"),
            Err(CommunityStoreError::TooLarge)
        ));
    }

    #[test]
    fn cache_identity_and_lru_eviction_do_not_mix_requests() {
        let directory = tempfile::tempdir().unwrap();
        let cas = CommunityCas::open(directory.path(), limits(8, 1), case_limits(1024, 2)).unwrap();
        let request_a = sha256_hex(b"model=a;run=1;grid=x;vars=t;recipe=raw");
        let request_b = sha256_hex(b"model=a;run=2;grid=x;vars=t;recipe=raw");
        let hash_a = sha256_hex(b"aaaa");
        let hash_b = sha256_hex(b"bbbb");
        cas.put(&request_a, &hash_a, b"aaaa", b"{\"a\":1}").unwrap();
        cas.put(&request_b, &hash_b, b"bbbb", b"{\"b\":1}").unwrap();
        assert!(cas.get(&request_a).unwrap().is_none());
        let (_, object) = cas.get(&request_b).unwrap().unwrap();
        assert_eq!(object, b"bbbb");
        assert_eq!(cas.object_count(), 1);
    }

    #[test]
    fn per_principal_quotas_and_concurrency_fail_closed() {
        let ledger = QuotaLedger::memory(accounting_limits(10, 20, 30), 202608).unwrap();
        let permit = ledger.begin("user-a", 202608).unwrap();
        assert!(matches!(
            ledger.begin("user-a", 202608),
            Err(CommunityStoreError::Quota)
        ));
        ledger.charge_upload("user-a", 10).unwrap();
        assert!(matches!(
            ledger.charge_upload("user-a", 1),
            Err(CommunityStoreError::Quota)
        ));
        ledger.charge_download("user-a", 20).unwrap();
        drop(permit);
        assert!(ledger.begin("user-a", 202608).is_ok());
        assert!(ledger.begin("user-b", 202608).is_ok());
    }

    #[test]
    fn monthly_transfer_and_promotion_accounting_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("accounting.json");
        let limits = accounting_limits(10, 20, 30);
        {
            let ledger = QuotaLedger::open(&path, limits, 202608).unwrap();
            let permit = ledger.begin("user-a", 202608).unwrap();
            ledger.charge_upload("user-a", 10).unwrap();
            ledger.charge_download("user-a", 20).unwrap();
            ledger.reserve_promotion(202608, 30).unwrap();
            drop(permit);
        }
        let ledger = QuotaLedger::open(&path, limits, 202608).unwrap();
        let _permit = ledger.begin("user-a", 202608).unwrap();
        assert!(matches!(
            ledger.charge_upload("user-a", 1),
            Err(CommunityStoreError::Quota)
        ));
        assert!(matches!(
            ledger.charge_download("user-a", 1),
            Err(CommunityStoreError::Quota)
        ));
        assert!(matches!(
            ledger.reserve_promotion(202608, 1),
            Err(CommunityStoreError::Quota)
        ));

        let next_month = QuotaLedger::open(&path, limits, 202609).unwrap();
        let _permit = next_month.begin("user-a", 202609).unwrap();
        next_month.charge_upload("user-a", 10).unwrap();
        next_month.reserve_promotion(202609, 30).unwrap();
    }

    #[test]
    fn case_storage_is_bounded_and_expired_cases_are_cleaned_on_restart() {
        let directory = tempfile::tempdir().unwrap();
        let first = signed_case("case-a", now_unix() + 3600);
        let second = signed_case("case-b", now_unix() + 3600);
        let cases = CaseLimits {
            maximum_manifest_bytes: 4096,
            storage_bytes: 4096,
            maximum_cases: 1,
        };
        {
            let cas = CommunityCas::open(directory.path(), limits(8, 1), cases).unwrap();
            cas.put_case("case-a", &first).unwrap();
            assert!(matches!(
                cas.put_case("case-b", &second),
                Err(CommunityStoreError::Quota)
            ));
            assert_eq!(cas.case_count().unwrap(), 1);
        }

        let expired = signed_case("case-a", now_unix() - 1);
        rw_store::atomic::atomic_write_bytes(
            &directory.path().join("cases").join("case-a.json"),
            &expired,
        )
        .unwrap();
        let restarted = CommunityCas::open(directory.path(), limits(8, 1), cases).unwrap();
        assert_eq!(restarted.case_count().unwrap(), 0);
        assert!(restarted.get_case("case-a").unwrap().is_none());
        restarted.put_case("case-b", &second).unwrap();
        assert_eq!(restarted.case_storage_bytes().unwrap(), second.len() as u64);
    }
}
