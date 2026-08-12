use std::collections::HashSet;
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
}

#[derive(Clone, Default)]
pub struct TokenSet {
    digests: Vec<[u8; 32]>,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("token_count", &self.digests.len())
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
        let mut unique = HashSet::new();
        for token in tokens {
            let token = token.as_ref();
            validate_token(token)?;
            unique.insert(hash_token(token.as_bytes()));
        }
        let mut digests: Vec<_> = unique.into_iter().collect();
        digests.sort_unstable();
        Ok(Self { digests })
    }

    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    pub fn len(&self) -> usize {
        self.digests.len()
    }

    pub fn verify(&self, token: &str) -> bool {
        if token.len() < MIN_TOKEN_BYTES {
            return false;
        }
        let candidate = hash_token(token.as_bytes());
        let mut accepted = Choice::from(0);
        for expected in &self.digests {
            accepted |= expected.ct_eq(&candidate);
        }
        bool::from(accepted)
    }

    /// Compare two credential domains without retaining or exposing bearer
    /// values. Digests are domain-separated from raw SHA-256 and quota
    /// principal identities, and every pair is compared in constant time.
    pub(crate) fn overlaps(&self, other: &Self) -> bool {
        let mut overlap = Choice::from(0);
        for left in &self.digests {
            for right in &other.digests {
                overlap |= left.ct_eq(right);
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
        let mut digest = Sha256::new();
        digest.update(PRINCIPAL_HASH_DOMAIN);
        digest.update(token.as_bytes());
        Some(format!("{:x}", digest.finalize()))
    }
}

fn read_token_file(path: &Path) -> Result<Vec<String>, AuthError> {
    let metadata = fs::symlink_metadata(path).map_err(AuthError::Inspect)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(AuthError::UnsafeFileType);
    }
    if metadata.len() > MAX_TOKEN_FILE_BYTES {
        return Err(AuthError::TooLarge);
    }
    validate_private_permissions(&metadata)?;
    let text = fs::read_to_string(path).map_err(AuthError::Read)?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
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

fn hash_token(token: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(TOKEN_DIGEST_DOMAIN);
    digest.update(token);
    digest.finalize().into()
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
}
