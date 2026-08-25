use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use http::HeaderValue;
use sha2::{Digest, Sha256};
use subtle::{Choice, ConstantTimeEq};
use thiserror::Error;

use crate::config::AuthConfig;

const MAX_TOKEN_FILE_BYTES: u64 = 1024 * 1024;
const MIN_TOKEN_BYTES: usize = 32;
const TOKEN_DIGEST_DOMAIN: &[u8] = b"rw-bearer-token-digest-v1\0";
const PRINCIPAL_HASH_DOMAIN: &[u8] = b"rw-authenticated-principal-v1\0";
const OPERATIONS_OWNER_HASH_DOMAIN: &[u8] = b"rw-operations-owner-id-v1\0";
const MAX_OPERATIONS_OWNER_ID_BYTES: usize = 128;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("failed to inspect API token file: {0}")]
    Inspect(#[source] std::io::Error),
    #[error("API token file must be a regular file and not a symbolic link")]
    UnsafeFileType,
    #[error("API token file exceeds the 1 MiB safety limit")]
    TooLarge,
    #[error("API token file permissions permit access by other users")]
    UnsafePermissions,
    #[error("failed to read API token file: {0}")]
    Read(#[source] std::io::Error),
    #[error("API tokens must contain at least 32 bytes")]
    TokenTooShort,
    #[error("API tokens may not contain control characters")]
    InvalidToken,
    #[error("operations write credential line {line} must contain <owner-id><TAB><bearer-token>")]
    InvalidWriteCredentialLine { line: usize },
    #[error(
        "operations write credential line {line} has an invalid owner ID; use 1-128 ASCII letters, digits, '.', '_', '-', ':', '@', or '+'"
    )]
    InvalidOperationsOwnerId { line: usize },
    #[error("one bearer token is mapped to different operations owners")]
    OperationsOwnerConflict,
    #[error("operations credential scopes overlap: {left} and {right}")]
    ScopeOverlap {
        left: &'static str,
        right: &'static str,
    },
    #[error("enabled operations APIs require at least one authenticated credential")]
    MissingOperationsCredential,
}

#[derive(Clone)]
struct Credential {
    token_digest: [u8; 32],
    principal_sha256: String,
}

#[derive(Clone, Default)]
pub struct TokenSet {
    credentials: Vec<Credential>,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("token_count", &self.credentials.len())
            .finish()
    }
}

impl TokenSet {
    pub fn load(config: &AuthConfig) -> Result<Self, AuthError> {
        let mut tokens = Vec::new();
        if let Ok(value) = std::env::var("RW_API_TOKENS") {
            tokens.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned),
            );
        }
        if let Some(path) = config.token_file.as_deref() {
            tokens.extend(read_token_file(path)?);
        }
        Self::from_tokens(tokens)
    }

    /// Load a dedicated, permission-restricted token file without consulting
    /// the ordinary BowEcho API-token environment variable.
    pub fn load_file(path: &Path) -> Result<Self, AuthError> {
        Self::from_tokens(read_token_file(path)?)
    }

    pub fn from_tokens<I, S>(tokens: I) -> Result<Self, AuthError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut unique = BTreeMap::new();
        for token in tokens {
            let token = token.as_ref();
            validate_token(token)?;
            unique.insert(
                hash_token(token.as_bytes()),
                token_principal_sha256(token.as_bytes()),
            );
        }
        Ok(Self {
            credentials: unique
                .into_iter()
                .map(|(token_digest, principal_sha256)| Credential {
                    token_digest,
                    principal_sha256,
                })
                .collect(),
        })
    }

    /// Load write credentials whose explicit owner IDs are independent of
    /// bearer material. Multiple tokens may deliberately map to one owner so
    /// rotation can overlap without changing access to durable owner state.
    pub(crate) fn load_write_file(path: &Path) -> Result<Self, AuthError> {
        let mappings = read_write_credential_file(path)?;
        Self::from_owner_tokens(mappings)
    }

    fn from_owner_tokens<I, O, T>(mappings: I) -> Result<Self, AuthError>
    where
        I: IntoIterator<Item = (O, T)>,
        O: AsRef<str>,
        T: AsRef<str>,
    {
        let mut unique = BTreeMap::new();
        for (owner_id, token) in mappings {
            let owner_id = owner_id.as_ref();
            let token = token.as_ref();
            validate_operations_owner_id(owner_id, 0)?;
            validate_token(token)?;
            let token_digest = hash_token(token.as_bytes());
            let principal_sha256 = operations_owner_sha256(owner_id.as_bytes());
            if unique
                .insert(token_digest, principal_sha256.clone())
                .is_some_and(|existing| existing != principal_sha256)
            {
                return Err(AuthError::OperationsOwnerConflict);
            }
        }
        Ok(Self {
            credentials: unique
                .into_iter()
                .map(|(token_digest, principal_sha256)| Credential {
                    token_digest,
                    principal_sha256,
                })
                .collect(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.credentials.is_empty()
    }

    pub fn len(&self) -> usize {
        self.credentials.len()
    }

    pub fn verify(&self, token: &str) -> bool {
        if token.len() < MIN_TOKEN_BYTES {
            return false;
        }
        let candidate = hash_token(token.as_bytes());
        let mut accepted = Choice::from(0);
        for credential in &self.credentials {
            accepted |= credential.token_digest.ct_eq(&candidate);
        }
        bool::from(accepted)
    }

    /// Compare two credential domains without retaining or exposing bearer
    /// values. Digests are domain-separated from raw SHA-256 and quota
    /// principal identities, and every pair is compared in constant time.
    pub(crate) fn overlaps(&self, other: &Self) -> bool {
        let mut overlap = Choice::from(0);
        for left in &self.credentials {
            for right in &other.credentials {
                overlap |= left.token_digest.ct_eq(&right.token_digest);
            }
        }
        bool::from(overlap)
    }

    pub fn verify_authorization_header(&self, value: Option<&HeaderValue>) -> bool {
        self.authorization_principal(value).is_some()
    }

    /// Return a stable, non-secret SHA-256 principal for quota accounting.
    /// The bearer token itself is never retained, logged, or exposed.
    pub fn authorization_principal(&self, value: Option<&HeaderValue>) -> Option<String> {
        let value = value.and_then(|value| value.to_str().ok())?;
        let token = value.strip_prefix("Bearer ")?;
        if token.is_empty() || token.trim() != token || !self.verify(token) {
            return None;
        }
        let candidate = hash_token(token.as_bytes());
        let mut accepted = Choice::from(0);
        let mut principal = None;
        for credential in &self.credentials {
            let matches = credential.token_digest.ct_eq(&candidate);
            accepted |= matches;
            if bool::from(matches) {
                principal = Some(credential.principal_sha256.clone());
            }
        }
        bool::from(accepted).then_some(principal).flatten()
    }

    /// Validate every configured operations credential domain and return the
    /// number of distinct credentials. This lets startup and `doctor` fail
    /// before opening durable state while keeping scope internals private.
    pub fn operations_credential_count(&self, config: &AuthConfig) -> Result<usize, AuthError> {
        let tokens = OperationsTokenSets::load(config, self)?;
        let count = tokens.len();
        if count == 0 {
            Err(AuthError::MissingOperationsCredential)
        } else {
            Ok(count)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperationsScope {
    Read,
    Write,
    Ingest,
    Admin,
}

impl OperationsScope {
    pub(crate) fn permits(self, required: Self) -> bool {
        matches!(
            (self, required),
            (Self::Admin, _)
                | (Self::Write, Self::Read | Self::Write)
                | (Self::Ingest, Self::Read | Self::Ingest)
                | (Self::Read, Self::Read)
        )
    }
}

/// Authenticated operations identity. Only a stable domain-separated digest
/// and its granted scope cross the middleware boundary; raw bearer material
/// is never retained in application state or request extensions.
#[derive(Clone)]
pub(crate) struct OperationsPrincipal {
    /// Retained for owner-scoped operations records; request handlers read it
    /// from the request extensions inserted by the operations middleware.
    #[allow(dead_code)]
    pub(crate) principal_sha256: String,
    pub(crate) scope: OperationsScope,
}

#[derive(Clone, Default)]
pub(crate) struct OperationsTokenSets {
    legacy_admin: TokenSet,
    read: TokenSet,
    write: TokenSet,
    ingest: TokenSet,
    admin: TokenSet,
}

impl std::fmt::Debug for OperationsTokenSets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperationsTokenSets")
            .field("legacy_admin_count", &self.legacy_admin.len())
            .field("read_count", &self.read.len())
            .field("write_count", &self.write.len())
            .field("ingest_count", &self.ingest.len())
            .field("admin_count", &self.admin.len())
            .finish()
    }
}

impl OperationsTokenSets {
    pub(crate) fn load(config: &AuthConfig, legacy_api: &TokenSet) -> Result<Self, AuthError> {
        let read = load_optional_file(config.ops_read_token_file.as_deref())?;
        let write = config
            .ops_write_token_file
            .as_deref()
            .map(TokenSet::load_write_file)
            .transpose()?
            .unwrap_or_default();
        let ingest = load_optional_file(config.ops_ingest_token_file.as_deref())?;
        let admin = load_optional_file(config.ops_admin_token_file.as_deref())?;
        for (left_name, left, right_name, right) in [
            ("general API", legacy_api, "read", &read),
            ("general API", legacy_api, "write", &write),
            ("general API", legacy_api, "ingest", &ingest),
            ("general API", legacy_api, "admin", &admin),
            ("read", &read, "write", &write),
            ("read", &read, "ingest", &ingest),
            ("read", &read, "admin", &admin),
            ("write", &write, "ingest", &ingest),
            ("write", &write, "admin", &admin),
            ("ingest", &ingest, "admin", &admin),
        ] {
            if left.overlaps(right) {
                return Err(AuthError::ScopeOverlap {
                    left: left_name,
                    right: right_name,
                });
            }
        }
        let legacy_admin = if config.legacy_api_tokens_are_operations_admins {
            legacy_api.clone()
        } else {
            TokenSet::default()
        };
        Ok(Self {
            legacy_admin,
            read,
            write,
            ingest,
            admin,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.legacy_admin.is_empty()
            && self.read.is_empty()
            && self.write.is_empty()
            && self.ingest.is_empty()
            && self.admin.is_empty()
    }

    fn len(&self) -> usize {
        self.legacy_admin.len()
            + self.read.len()
            + self.write.len()
            + self.ingest.len()
            + self.admin.len()
    }

    pub(crate) fn authorize(&self, value: Option<&HeaderValue>) -> Option<OperationsPrincipal> {
        for (tokens, scope) in [
            (&self.legacy_admin, OperationsScope::Admin),
            (&self.admin, OperationsScope::Admin),
            (&self.write, OperationsScope::Write),
            (&self.ingest, OperationsScope::Ingest),
            (&self.read, OperationsScope::Read),
        ] {
            if let Some(principal_sha256) = tokens.authorization_principal(value) {
                return Some(OperationsPrincipal {
                    principal_sha256,
                    scope,
                });
            }
        }
        None
    }
}

fn load_optional_file(path: Option<&Path>) -> Result<TokenSet, AuthError> {
    path.map(TokenSet::load_file)
        .transpose()
        .map(Option::unwrap_or_default)
}

fn read_token_file(path: &Path) -> Result<Vec<String>, AuthError> {
    let text = read_private_text_file(path)?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

fn read_write_credential_file(path: &Path) -> Result<Vec<(String, String)>, AuthError> {
    let text = read_private_text_file(path)?;
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            (!trimmed.is_empty() && !trimmed.starts_with('#')).then_some((index + 1, line))
        })
        .map(|(line_number, line)| {
            let (owner_id, token) = line
                .split_once('\t')
                .ok_or(AuthError::InvalidWriteCredentialLine { line: line_number })?;
            if token.contains('\t') || owner_id.trim() != owner_id || token.trim() != token {
                return Err(AuthError::InvalidWriteCredentialLine { line: line_number });
            }
            validate_operations_owner_id(owner_id, line_number)?;
            validate_token(token)?;
            Ok((owner_id.to_owned(), token.to_owned()))
        })
        .collect()
}

fn read_private_text_file(path: &Path) -> Result<String, AuthError> {
    let metadata = fs::symlink_metadata(path).map_err(AuthError::Inspect)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(AuthError::UnsafeFileType);
    }
    if metadata.len() > MAX_TOKEN_FILE_BYTES {
        return Err(AuthError::TooLarge);
    }
    validate_private_permissions(&metadata)?;
    fs::read_to_string(path).map_err(AuthError::Read)
}

#[cfg(unix)]
fn validate_private_permissions(metadata: &fs::Metadata) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(AuthError::UnsafePermissions);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_permissions(_metadata: &fs::Metadata) -> Result<(), AuthError> {
    // Windows deployments should grant the service identity and SYSTEM only.
    // A portable std API cannot evaluate a DACL, so operators must verify it
    // with `icacls`/`Get-Acl` as part of the documented deployment gate.
    Ok(())
}

fn validate_token(token: &str) -> Result<(), AuthError> {
    if token.len() < MIN_TOKEN_BYTES {
        return Err(AuthError::TokenTooShort);
    }
    if token.chars().any(char::is_control) {
        return Err(AuthError::InvalidToken);
    }
    Ok(())
}

fn validate_operations_owner_id(owner_id: &str, line: usize) -> Result<(), AuthError> {
    if owner_id.is_empty()
        || owner_id.len() > MAX_OPERATIONS_OWNER_ID_BYTES
        || !owner_id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'@' | b'+')
        })
    {
        return Err(AuthError::InvalidOperationsOwnerId { line });
    }
    Ok(())
}

fn hash_token(token: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(TOKEN_DIGEST_DOMAIN);
    digest.update(token);
    digest.finalize().into()
}

fn token_principal_sha256(token: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(PRINCIPAL_HASH_DOMAIN);
    digest.update(token);
    format!("{:x}", digest.finalize())
}

fn operations_owner_sha256(owner_id: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(OPERATIONS_OWNER_HASH_DOMAIN);
    digest.update(owner_id);
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TOKEN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn stores_only_digests_and_checks_bearer_tokens() {
        let tokens = TokenSet::from_tokens([TOKEN_A, TOKEN_A, TOKEN_B]).unwrap();
        assert_eq!(tokens.len(), 2);
        assert!(tokens.verify(TOKEN_A));
        assert!(!tokens.verify("cccccccccccccccccccccccccccccccc"));
        let header = HeaderValue::from_str(&format!("Bearer {TOKEN_B}")).unwrap();
        assert!(tokens.verify_authorization_header(Some(&header)));
        assert!(!format!("{tokens:?}").contains(TOKEN_A));
        let header = HeaderValue::from_str(&format!("Bearer {TOKEN_A}")).unwrap();
        let principal = tokens.authorization_principal(Some(&header)).unwrap();
        assert_eq!(principal.len(), 64);
        assert_ne!(
            principal,
            format!("{:x}", Sha256::digest(TOKEN_A.as_bytes()))
        );
    }

    #[test]
    fn rejects_short_control_or_malformed_bearer_values() {
        assert!(matches!(
            TokenSet::from_tokens(["short"]),
            Err(AuthError::TokenTooShort)
        ));
        assert!(matches!(
            TokenSet::from_tokens(["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"]),
            Err(AuthError::InvalidToken)
        ));
        let tokens = TokenSet::from_tokens([TOKEN_A]).unwrap();
        assert!(
            !tokens.verify_authorization_header(Some(&HeaderValue::from_static(
                "bearer aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )))
        );
    }

    #[test]
    fn detects_token_set_overlap_using_only_domain_separated_digests() {
        let normal = TokenSet::from_tokens([TOKEN_A]).unwrap();
        let disjoint = TokenSet::from_tokens([TOKEN_B]).unwrap();
        let overlapping = TokenSet::from_tokens([TOKEN_B, TOKEN_A]).unwrap();
        assert!(!normal.overlaps(&disjoint));
        assert!(normal.overlaps(&overlapping));
        assert!(!format!("{normal:?}{overlapping:?}").contains(TOKEN_A));
    }

    #[test]
    fn operations_scope_hierarchy_is_least_privilege() {
        assert!(OperationsScope::Read.permits(OperationsScope::Read));
        assert!(!OperationsScope::Read.permits(OperationsScope::Write));
        assert!(!OperationsScope::Read.permits(OperationsScope::Ingest));
        assert!(!OperationsScope::Read.permits(OperationsScope::Admin));
        assert!(OperationsScope::Write.permits(OperationsScope::Read));
        assert!(OperationsScope::Write.permits(OperationsScope::Write));
        assert!(!OperationsScope::Write.permits(OperationsScope::Ingest));
        assert!(!OperationsScope::Write.permits(OperationsScope::Admin));
        assert!(OperationsScope::Ingest.permits(OperationsScope::Read));
        assert!(!OperationsScope::Ingest.permits(OperationsScope::Write));
        assert!(OperationsScope::Ingest.permits(OperationsScope::Ingest));
        assert!(!OperationsScope::Ingest.permits(OperationsScope::Admin));
        assert!(OperationsScope::Admin.permits(OperationsScope::Read));
        assert!(OperationsScope::Admin.permits(OperationsScope::Write));
        assert!(OperationsScope::Admin.permits(OperationsScope::Ingest));
        assert!(OperationsScope::Admin.permits(OperationsScope::Admin));
    }

    #[test]
    fn explicit_operations_owner_survives_token_rotation() {
        let old = TokenSet::from_owner_tokens([("owner-a", TOKEN_A)]).unwrap();
        let rotated = TokenSet::from_owner_tokens([("owner-a", TOKEN_B)]).unwrap();
        let old_header = HeaderValue::from_str(&format!("Bearer {TOKEN_A}")).unwrap();
        let new_header = HeaderValue::from_str(&format!("Bearer {TOKEN_B}")).unwrap();
        assert_eq!(
            old.authorization_principal(Some(&old_header)),
            rotated.authorization_principal(Some(&new_header))
        );
        assert_ne!(
            old.authorization_principal(Some(&old_header)),
            TokenSet::from_tokens([TOKEN_A])
                .unwrap()
                .authorization_principal(Some(&old_header))
        );
    }

    #[test]
    fn explicit_operations_owner_rejects_ambiguous_or_invalid_mappings() {
        assert!(matches!(
            TokenSet::from_owner_tokens([("owner-a", TOKEN_A), ("owner-b", TOKEN_A)]),
            Err(AuthError::OperationsOwnerConflict)
        ));
        assert!(matches!(
            TokenSet::from_owner_tokens([("owner a", TOKEN_A)]),
            Err(AuthError::InvalidOperationsOwnerId { .. })
        ));
    }
}
