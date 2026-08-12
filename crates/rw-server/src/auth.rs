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
        if token.as_bytes().len() < MIN_TOKEN_BYTES {
            return false;
        }
        let candidate = hash_token(token.as_bytes());
        let mut accepted = Choice::from(0);
        for expected in &self.digests {
            accepted |= expected.ct_eq(&candidate);
        }
        bool::from(accepted)
    }

    pub fn verify_authorization_header(&self, value: Option<&HeaderValue>) -> bool {
        let Some(value) = value.and_then(|value| value.to_str().ok()) else {
            return false;
        };
        let Some(token) = value.strip_prefix("Bearer ") else {
            return false;
        };
        !token.is_empty() && token.trim() == token && self.verify(token)
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
    // A portable std API cannot evaluate an ACL; the installer/doctor command
    // performs the platform-specific check before public-bind service startup.
    Ok(())
}

fn validate_token(token: &str) -> Result<(), AuthError> {
    if token.as_bytes().len() < MIN_TOKEN_BYTES {
        return Err(AuthError::TokenTooShort);
    }
    if token.chars().any(char::is_control) {
        return Err(AuthError::InvalidToken);
    }
    Ok(())
}

fn hash_token(token: &[u8]) -> [u8; 32] {
    Sha256::digest(token).into()
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
}
