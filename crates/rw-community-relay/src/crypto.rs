//! End-to-end relay chunk encryption and key agreement.

use std::collections::BTreeSet;
use std::fmt;

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use hkdf::Hkdf;
use rand_core::{CryptoRng, OsRng, RngCore};
use rw_community_protocol::{
    EncryptedRelayEnvelope, EndToEndCipher, ProtocolLimits, RelayCredentialClaims, RelayDirection,
    SignatureAlgorithm, SignatureBlock, SignedRelayCredential, TrustedSigningKeys,
    canonical_relay_credential_bytes, object_sha256, verify_signed_relay_credential,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::{RelayError, valid_opaque_id, valid_sha256};

pub const KEY_OFFER_SCHEMA: &str = "rw.community.relay-key-offer.v1";
pub const SESSION_BINDING_SCHEMA: &str = "rw.community.relay-session-binding.v1";
const SESSION_BINDING_DOMAIN: &[u8] = b"rw-community-relay-session-binding-v1\0";
const SESSION_KEY_INFO: &[u8] = b"rw-community-relay-x25519-xchacha20poly1305-v1";
const ENVELOPE_AAD_DOMAIN: &[u8] = b"rw-community-relay-envelope-aad-v1\0";
const ACK_KEY_INFO: &[u8] = b"rw-community-relay-ack-xchacha20poly1305-v1";
const ACK_AAD_DOMAIN: &[u8] = b"rw-community-relay-ack-aad-v1\0";
pub const RELAY_ACK_SCHEMA: &str = "rw.community.relay-ack.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayAckKind {
    Chunk,
    TransferComplete,
    TransferReceipt,
    /// Downloader-authored, session/object-bound readiness marker. Sending it
    /// through the bound TURN allocation establishes the downloader's lazy
    /// receive permission before the uploader's first encrypted chunk.
    ReceiverReady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayRole {
    Uploader,
    Downloader,
}

/// One participant's ephemeral X25519 offer. It carries only an opaque
/// session, exact signed object identity, signed-credential fingerprint, and
/// ephemeral public key. There is no address, candidate, hostname, or account
/// identity field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EphemeralPublicOffer {
    pub schema: String,
    pub role: RelayRole,
    pub session_id: String,
    pub object_sha256: String,
    pub credential_fingerprint: String,
    pub public_key_base64: String,
}

impl EphemeralPublicOffer {
    pub fn validate(&self) -> Result<[u8; 32], RelayError> {
        if self.schema != KEY_OFFER_SCHEMA
            || !valid_opaque_id(&self.session_id)
            || !valid_sha256(&self.object_sha256)
            || !valid_sha256(&self.credential_fingerprint)
        {
            return Err(RelayError::KeyAgreementRejected);
        }
        if self.public_key_base64.len() != 44 {
            return Err(RelayError::KeyAgreementRejected);
        }
        decode_32(&self.public_key_base64)
    }
}

/// Origin/backend-signed binding for the two ephemeral public keys and their
/// already signed, object-scoped relay credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBinding {
    pub schema: String,
    pub session_id: String,
    pub object_sha256: String,
    pub cipher: EndToEndCipher,
    pub uploader_credential_fingerprint: String,
    pub downloader_credential_fingerprint: String,
    pub uploader_public_key_base64: String,
    pub downloader_public_key_base64: String,
    pub expires_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedSessionBinding {
    pub binding: SessionBinding,
    pub signature: SignatureBlock,
}

/// Marker returned only after the session signature and both object-scoped
/// relay credentials have been verified.
pub struct VerifiedSessionBinding(SessionBinding);

impl fmt::Debug for VerifiedSessionBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VerifiedSessionBinding([opaque session])")
    }
}

impl VerifiedSessionBinding {
    pub fn binding(&self) -> &SessionBinding {
        &self.0
    }
}

/// An ephemeral X25519 private key. Debug deliberately reveals nothing and
/// `StaticSecret` zeroizes its bytes on drop.
pub struct EphemeralKeyPair {
    secret: StaticSecret,
    public: PublicKey,
}

impl fmt::Debug for EphemeralKeyPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EphemeralKeyPair([redacted])")
    }
}

impl EphemeralKeyPair {
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    #[cfg(test)]
    fn from_seed(seed: [u8; 32]) -> Self {
        let secret = StaticSecret::from(seed);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    pub fn offer(
        &self,
        credential: &SignedRelayCredential,
        role: RelayRole,
        now_unix: i64,
        limits: &ProtocolLimits,
    ) -> Result<EphemeralPublicOffer, RelayError> {
        let expected_direction = match role {
            RelayRole::Uploader => RelayDirection::Upload,
            RelayRole::Downloader => RelayDirection::Download,
        };
        if credential.claims.direction != expected_direction {
            return Err(RelayError::CredentialInvalid);
        }
        Ok(EphemeralPublicOffer {
            schema: KEY_OFFER_SCHEMA.into(),
            role,
            session_id: credential.claims.session_id.clone(),
            object_sha256: credential.claims.object_sha256.clone(),
            credential_fingerprint: credential_fingerprint(credential, now_unix, limits)?,
            public_key_base64: base64::engine::general_purpose::STANDARD
                .encode(self.public.as_bytes()),
        })
    }

    pub fn derive_session_key(
        &self,
        verified: &VerifiedSessionBinding,
        role: RelayRole,
    ) -> Result<SessionKey, RelayError> {
        let binding = verified.binding();
        let (own_encoded, peer_encoded) = match role {
            RelayRole::Uploader => (
                &binding.uploader_public_key_base64,
                &binding.downloader_public_key_base64,
            ),
            RelayRole::Downloader => (
                &binding.downloader_public_key_base64,
                &binding.uploader_public_key_base64,
            ),
        };
        let own = decode_32(own_encoded)?;
        if own != *self.public.as_bytes() {
            return Err(RelayError::KeyAgreementRejected);
        }
        let peer = PublicKey::from(decode_32(peer_encoded)?);
        let shared = self.secret.diffie_hellman(&peer);
        if shared.as_bytes().iter().all(|byte| *byte == 0) {
            return Err(RelayError::KeyAgreementRejected);
        }
        let binding_bytes = canonical_session_binding_bytes(binding, "transcript")?;
        let salt = Sha256::digest(&binding_bytes);
        let hkdf = Hkdf::<Sha256>::new(Some(salt.as_slice()), shared.as_bytes());
        let mut key = [0_u8; 32];
        hkdf.expand(SESSION_KEY_INFO, &mut key)
            .map_err(|_| RelayError::KeyAgreementRejected)?;
        Ok(SessionKey(Zeroizing::new(key)))
    }
}

/// Per-object payload key. It is never serializable or debug-visible.
pub struct SessionKey(Zeroizing<[u8; 32]>);

impl fmt::Debug for SessionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionKey([redacted])")
    }
}

/// End-to-end authenticated relay control datagram. It contains only opaque
/// session/object identities and is never accepted as authorization by itself.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedRelayAck {
    pub schema: String,
    pub session_id: String,
    pub object_sha256: String,
    pub kind: RelayAckKind,
    pub chunk_index: u32,
    pub authenticator_base64: String,
}

impl fmt::Debug for AuthenticatedRelayAck {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthenticatedRelayAck([opaque session])")
    }
}

struct AckAuthenticator {
    cipher: XChaCha20Poly1305,
}

impl AckAuthenticator {
    fn from_session_key(key: &SessionKey) -> Result<Self, RelayError> {
        let hkdf = Hkdf::<Sha256>::new(None, key.0.as_slice());
        let mut ack_key = Zeroizing::new([0_u8; 32]);
        hkdf.expand(ACK_KEY_INFO, ack_key.as_mut())
            .map_err(|_| RelayError::KeyAgreementRejected)?;
        let cipher = XChaCha20Poly1305::new_from_slice(ack_key.as_slice())
            .map_err(|_| RelayError::KeyAgreementRejected)?;
        Ok(Self { cipher })
    }

    fn authenticate(
        &self,
        session_id: &str,
        object_sha256: &str,
        kind: RelayAckKind,
        chunk_index: u32,
    ) -> Result<AuthenticatedRelayAck, RelayError> {
        let (nonce, aad) = ack_material(session_id, object_sha256, kind, chunk_index)?;
        let authenticator = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &[],
                    aad: &aad,
                },
            )
            .map_err(|_| RelayError::AuthenticationFailed)?;
        Ok(AuthenticatedRelayAck {
            schema: RELAY_ACK_SCHEMA.into(),
            session_id: session_id.into(),
            object_sha256: object_sha256.into(),
            kind,
            chunk_index,
            authenticator_base64: base64::engine::general_purpose::STANDARD.encode(authenticator),
        })
    }

    fn verify(
        &self,
        ack: &AuthenticatedRelayAck,
        session_id: &str,
        object_sha256: &str,
        kind: RelayAckKind,
        chunk_index: u32,
    ) -> Result<(), RelayError> {
        if ack.schema != RELAY_ACK_SCHEMA
            || ack.session_id != session_id
            || ack.object_sha256 != object_sha256
            || ack.kind != kind
            || ack.chunk_index != chunk_index
            || ack.authenticator_base64.len() != 24
        {
            return Err(RelayError::AuthenticationFailed);
        }
        let authenticator = base64::engine::general_purpose::STANDARD
            .decode(&ack.authenticator_base64)
            .map_err(|_| RelayError::AuthenticationFailed)?;
        if authenticator.len() != 16 {
            return Err(RelayError::AuthenticationFailed);
        }
        let (nonce, aad) = ack_material(session_id, object_sha256, kind, chunk_index)?;
        self.cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &authenticator,
                    aad: &aad,
                },
            )
            .map_err(|_| RelayError::AuthenticationFailed)?;
        Ok(())
    }
}

pub fn credential_fingerprint(
    signed: &SignedRelayCredential,
    now_unix: i64,
    limits: &ProtocolLimits,
) -> Result<String, RelayError> {
    if signed.signature.signature_base64.len() > 128 {
        return Err(RelayError::CredentialInvalid);
    }
    let mut bytes = canonical_relay_credential_bytes(
        &signed.claims,
        &signed.signature.signing_key_id,
        now_unix,
        limits,
    )
    .map_err(|_| RelayError::CredentialInvalid)?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&signed.signature.signature_base64)
        .map_err(|_| RelayError::CredentialInvalid)?;
    if signature.len() != 64 {
        return Err(RelayError::CredentialInvalid);
    }
    put_bytes(&mut bytes, &signature);
    Ok(hex_sha256(&bytes))
}

pub fn build_session_binding(
    uploader: &EphemeralPublicOffer,
    downloader: &EphemeralPublicOffer,
    expires_unix: i64,
) -> Result<SessionBinding, RelayError> {
    uploader.validate()?;
    downloader.validate()?;
    if uploader.role != RelayRole::Uploader
        || downloader.role != RelayRole::Downloader
        || uploader.session_id != downloader.session_id
        || uploader.object_sha256 != downloader.object_sha256
        || uploader.credential_fingerprint == downloader.credential_fingerprint
        || uploader.public_key_base64 == downloader.public_key_base64
        || expires_unix <= 0
    {
        return Err(RelayError::KeyAgreementRejected);
    }
    Ok(SessionBinding {
        schema: SESSION_BINDING_SCHEMA.into(),
        session_id: uploader.session_id.clone(),
        object_sha256: uploader.object_sha256.clone(),
        cipher: EndToEndCipher::XChaCha20Poly1305,
        uploader_credential_fingerprint: uploader.credential_fingerprint.clone(),
        downloader_credential_fingerprint: downloader.credential_fingerprint.clone(),
        uploader_public_key_base64: uploader.public_key_base64.clone(),
        downloader_public_key_base64: downloader.public_key_base64.clone(),
        expires_unix,
    })
}

pub fn sign_session_binding(
    binding: SessionBinding,
    signing_key_id: impl Into<String>,
    signing_key: &SigningKey,
) -> Result<SignedSessionBinding, RelayError> {
    let signing_key_id = signing_key_id.into();
    if !valid_opaque_id(&signing_key_id) {
        return Err(RelayError::KeyAgreementRejected);
    }
    let bytes = canonical_session_binding_bytes(&binding, &signing_key_id)?;
    let signature = signing_key.sign(&bytes);
    Ok(SignedSessionBinding {
        binding,
        signature: SignatureBlock {
            algorithm: SignatureAlgorithm::Ed25519,
            signing_key_id,
            signature_base64: base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes()),
        },
    })
}

pub fn verify_signed_session_binding(
    signed: &SignedSessionBinding,
    uploader_credential: &SignedRelayCredential,
    downloader_credential: &SignedRelayCredential,
    now_unix: i64,
    trusted_keys: &TrustedSigningKeys,
    limits: &ProtocolLimits,
) -> Result<VerifiedSessionBinding, RelayError> {
    verify_signed_relay_credential(uploader_credential, now_unix, trusted_keys, limits)
        .map_err(|_| RelayError::CredentialInvalid)?;
    verify_signed_relay_credential(downloader_credential, now_unix, trusted_keys, limits)
        .map_err(|_| RelayError::CredentialInvalid)?;
    let upload = &uploader_credential.claims;
    let download = &downloader_credential.claims;
    let binding = &signed.binding;
    validate_session_binding(binding, now_unix)?;
    if upload.direction != RelayDirection::Upload
        || download.direction != RelayDirection::Download
        || upload.session_id != binding.session_id
        || download.session_id != binding.session_id
        || upload.object_sha256 != binding.object_sha256
        || download.object_sha256 != binding.object_sha256
        || binding.expires_unix > upload.expires_unix
        || binding.expires_unix > download.expires_unix
        || credential_fingerprint(uploader_credential, now_unix, limits)?
            != binding.uploader_credential_fingerprint
        || credential_fingerprint(downloader_credential, now_unix, limits)?
            != binding.downloader_credential_fingerprint
    {
        return Err(RelayError::KeyAgreementRejected);
    }
    let verifying_key = trusted_keys
        .get(&signed.signature.signing_key_id)
        .ok_or(RelayError::KeyAgreementRejected)?;
    if signed.signature.signature_base64.len() > 128 {
        return Err(RelayError::KeyAgreementRejected);
    }
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&signed.signature.signature_base64)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or(RelayError::KeyAgreementRejected)?;
    let bytes = canonical_session_binding_bytes(binding, &signed.signature.signing_key_id)?;
    verifying_key
        .verify(&bytes, &signature)
        .map_err(|_| RelayError::KeyAgreementRejected)?;
    Ok(VerifiedSessionBinding(binding.clone()))
}

fn validate_session_binding(binding: &SessionBinding, now_unix: i64) -> Result<(), RelayError> {
    if binding.schema != SESSION_BINDING_SCHEMA
        || !valid_opaque_id(&binding.session_id)
        || !valid_sha256(&binding.object_sha256)
        || !valid_sha256(&binding.uploader_credential_fingerprint)
        || !valid_sha256(&binding.downloader_credential_fingerprint)
        || binding.expires_unix <= now_unix
        || binding.expires_unix.saturating_sub(now_unix) > 15 * 60
    {
        return Err(RelayError::KeyAgreementRejected);
    }
    decode_32(&binding.uploader_public_key_base64)?;
    decode_32(&binding.downloader_public_key_base64)?;
    Ok(())
}

fn canonical_session_binding_bytes(
    binding: &SessionBinding,
    signing_key_id: &str,
) -> Result<Vec<u8>, RelayError> {
    if binding.schema != SESSION_BINDING_SCHEMA
        || !valid_opaque_id(&binding.session_id)
        || !valid_sha256(&binding.object_sha256)
        || !valid_sha256(&binding.uploader_credential_fingerprint)
        || !valid_sha256(&binding.downloader_credential_fingerprint)
        || !valid_opaque_id(signing_key_id)
    {
        return Err(RelayError::KeyAgreementRejected);
    }
    decode_32(&binding.uploader_public_key_base64)?;
    decode_32(&binding.downloader_public_key_base64)?;
    let mut bytes = Vec::with_capacity(512);
    bytes.extend_from_slice(SESSION_BINDING_DOMAIN);
    put_str(&mut bytes, signing_key_id);
    put_str(&mut bytes, &binding.schema);
    put_str(&mut bytes, &binding.session_id);
    put_str(&mut bytes, &binding.object_sha256);
    bytes.push(1); // XChaCha20-Poly1305 is the sole v1 suite.
    put_str(&mut bytes, &binding.uploader_credential_fingerprint);
    put_str(&mut bytes, &binding.downloader_credential_fingerprint);
    put_str(&mut bytes, &binding.uploader_public_key_base64);
    put_str(&mut bytes, &binding.downloader_public_key_base64);
    bytes.extend_from_slice(&binding.expires_unix.to_be_bytes());
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayChunkPolicy {
    pub max_plaintext_per_chunk: u32,
}

impl Default for RelayChunkPolicy {
    fn default() -> Self {
        Self {
            max_plaintext_per_chunk: crate::RELAY_PLAINTEXT_CHUNK_BYTES,
        }
    }
}

pub struct RelaySender {
    cipher: XChaCha20Poly1305,
    ack_authenticator: AckAuthenticator,
    credential: RelayCredentialClaims,
    limits: ProtocolLimits,
    chunk_size: u32,
    chunk_count: u32,
    next_index: u32,
    expected_bytes: u64,
    observed_bytes: u64,
    used_nonces: BTreeSet<[u8; 24]>,
}

impl fmt::Debug for RelaySender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelaySender([encrypted session])")
    }
}

impl RelaySender {
    pub fn new(
        key: SessionKey,
        binding: &VerifiedSessionBinding,
        credential: &RelayCredentialClaims,
        expected_bytes: u64,
        policy: RelayChunkPolicy,
        limits: ProtocolLimits,
    ) -> Result<Self, RelayError> {
        validate_cipher_construction(
            binding.binding(),
            credential,
            RelayDirection::Upload,
            expected_bytes,
            policy,
            &limits,
        )?;
        let chunk_count = expected_bytes.div_ceil(u64::from(policy.max_plaintext_per_chunk));
        let chunk_count = u32::try_from(chunk_count).map_err(|_| RelayError::EnvelopeRejected)?;
        Ok(Self {
            cipher: XChaCha20Poly1305::new_from_slice(key.0.as_slice())
                .map_err(|_| RelayError::KeyAgreementRejected)?,
            ack_authenticator: AckAuthenticator::from_session_key(&key)?,
            credential: credential.clone(),
            limits,
            chunk_size: policy.max_plaintext_per_chunk,
            chunk_count,
            next_index: 0,
            expected_bytes,
            observed_bytes: 0,
            used_nonces: BTreeSet::new(),
        })
    }

    pub fn encrypt_next<R: RngCore + CryptoRng>(
        &mut self,
        plaintext: &[u8],
        rng: &mut R,
    ) -> Result<EncryptedRelayEnvelope, RelayError> {
        let expected_chunk_size = self.expected_next_size()?;
        if plaintext.len() != expected_chunk_size as usize {
            return Err(RelayError::EnvelopeRejected);
        }
        let mut nonce = [0_u8; 24];
        let mut unique = false;
        for _ in 0..8 {
            rng.fill_bytes(&mut nonce);
            if self.used_nonces.insert(nonce) {
                unique = true;
                break;
            }
        }
        if !unique {
            return Err(RelayError::Replay);
        }
        let aad = envelope_aad(
            &self.credential.session_id,
            &self.credential.object_sha256,
            self.next_index,
            self.chunk_count,
            expected_chunk_size,
            &nonce,
        );
        let ciphertext = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| RelayError::AuthenticationFailed)?;
        let envelope = EncryptedRelayEnvelope {
            schema: rw_community_protocol::RELAY_ENVELOPE_SCHEMA.into(),
            session_id: self.credential.session_id.clone(),
            object_sha256: self.credential.object_sha256.clone(),
            cipher: EndToEndCipher::XChaCha20Poly1305,
            chunk_index: self.next_index,
            chunk_count: self.chunk_count,
            plaintext_size: expected_chunk_size,
            nonce_base64: base64::engine::general_purpose::STANDARD.encode(nonce),
            ciphertext_base64: base64::engine::general_purpose::STANDARD.encode(ciphertext),
        };
        envelope
            .validate(&self.credential, &self.limits)
            .map_err(|_| RelayError::EnvelopeRejected)?;
        self.next_index += 1;
        self.observed_bytes += u64::from(expected_chunk_size);
        Ok(envelope)
    }

    pub fn verify_ack(
        &self,
        ack: &AuthenticatedRelayAck,
        chunk_index: u32,
    ) -> Result<(), RelayError> {
        if chunk_index >= self.next_index {
            return Err(RelayError::OutOfOrder);
        }
        self.ack_authenticator.verify(
            ack,
            &self.credential.session_id,
            &self.credential.object_sha256,
            RelayAckKind::Chunk,
            chunk_index,
        )
    }

    /// Verify a downloader-authored readiness marker. The marker is
    /// idempotent and remains valid if network scheduling delivers a duplicate
    /// while a chunk ACK or final receipt is awaited.
    pub fn verify_receiver_ready(&self, ready: &AuthenticatedRelayAck) -> Result<(), RelayError> {
        self.ack_authenticator.verify(
            ready,
            &self.credential.session_id,
            &self.credential.object_sha256,
            RelayAckKind::ReceiverReady,
            0,
        )
    }

    pub fn completion_confirmation(&self) -> Result<AuthenticatedRelayAck, RelayError> {
        if self.next_index != self.chunk_count || self.observed_bytes != self.expected_bytes {
            return Err(RelayError::ObjectMismatch);
        }
        self.ack_authenticator.authenticate(
            &self.credential.session_id,
            &self.credential.object_sha256,
            RelayAckKind::TransferComplete,
            self.chunk_count - 1,
        )
    }

    pub fn verify_transfer_receipt(
        &self,
        receipt: &AuthenticatedRelayAck,
    ) -> Result<(), RelayError> {
        if self.next_index != self.chunk_count || self.observed_bytes != self.expected_bytes {
            return Err(RelayError::ObjectMismatch);
        }
        self.ack_authenticator.verify(
            receipt,
            &self.credential.session_id,
            &self.credential.object_sha256,
            RelayAckKind::TransferReceipt,
            self.chunk_count - 1,
        )
    }

    pub fn next_plaintext_size(&self) -> Result<u32, RelayError> {
        self.expected_next_size()
    }

    pub(crate) fn object_sha256(&self) -> &str {
        &self.credential.object_sha256
    }

    pub(crate) const fn expected_chunk_count(&self) -> u32 {
        self.chunk_count
    }

    fn expected_next_size(&self) -> Result<u32, RelayError> {
        if self.next_index >= self.chunk_count || self.observed_bytes >= self.expected_bytes {
            return Err(RelayError::OutOfOrder);
        }
        let remaining = self.expected_bytes - self.observed_bytes;
        Ok(remaining.min(u64::from(self.chunk_size)) as u32)
    }

    pub fn finish(self) -> Result<(), RelayError> {
        if self.next_index != self.chunk_count || self.observed_bytes != self.expected_bytes {
            return Err(RelayError::ObjectMismatch);
        }
        Ok(())
    }
}

pub struct RelayReceiver {
    cipher: XChaCha20Poly1305,
    ack_authenticator: AckAuthenticator,
    credential: RelayCredentialClaims,
    limits: ProtocolLimits,
    chunk_size: u32,
    chunk_count: u32,
    next_index: u32,
    expected_bytes: u64,
    observed_bytes: u64,
    used_nonces: BTreeSet<[u8; 24]>,
    assembled: Vec<u8>,
    last_accepted: Option<EncryptedRelayEnvelope>,
}

impl fmt::Debug for RelayReceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayReceiver([encrypted session])")
    }
}

impl RelayReceiver {
    pub fn new(
        key: SessionKey,
        binding: &VerifiedSessionBinding,
        credential: &RelayCredentialClaims,
        expected_bytes: u64,
        policy: RelayChunkPolicy,
        limits: ProtocolLimits,
    ) -> Result<Self, RelayError> {
        validate_cipher_construction(
            binding.binding(),
            credential,
            RelayDirection::Download,
            expected_bytes,
            policy,
            &limits,
        )?;
        let chunk_count = expected_bytes.div_ceil(u64::from(policy.max_plaintext_per_chunk));
        let chunk_count = u32::try_from(chunk_count).map_err(|_| RelayError::EnvelopeRejected)?;
        let capacity = usize::try_from(expected_bytes).map_err(|_| RelayError::EnvelopeRejected)?;
        Ok(Self {
            cipher: XChaCha20Poly1305::new_from_slice(key.0.as_slice())
                .map_err(|_| RelayError::KeyAgreementRejected)?,
            ack_authenticator: AckAuthenticator::from_session_key(&key)?,
            credential: credential.clone(),
            limits,
            chunk_size: policy.max_plaintext_per_chunk,
            chunk_count,
            next_index: 0,
            expected_bytes,
            observed_bytes: 0,
            used_nonces: BTreeSet::new(),
            assembled: Vec::with_capacity(capacity),
            last_accepted: None,
        })
    }

    pub fn accept(&mut self, envelope: &EncryptedRelayEnvelope) -> Result<(), RelayError> {
        self.accept_inner(envelope, false).map(|_| ())
    }

    /// Accept the next chunk, or authenticate the immediately preceding exact
    /// ciphertext again so a lost ACK can be retransmitted without appending
    /// bytes twice. Older, altered, or future chunks still fail closed.
    pub fn accept_reliable(
        &mut self,
        envelope: &EncryptedRelayEnvelope,
    ) -> Result<RelayReceiveDisposition, RelayError> {
        self.accept_inner(envelope, true)
    }

    fn accept_inner(
        &mut self,
        envelope: &EncryptedRelayEnvelope,
        permit_last_duplicate: bool,
    ) -> Result<RelayReceiveDisposition, RelayError> {
        if permit_last_duplicate
            && self.next_index > 0
            && envelope.chunk_index == self.next_index - 1
        {
            if self.last_accepted.as_ref() != Some(envelope) {
                return Err(RelayError::Replay);
            }
            // Re-run AEAD authentication before authorizing a repeated ACK.
            let plaintext = self.decrypt_checked(envelope, envelope.plaintext_size)?;
            if plaintext.len() != envelope.plaintext_size as usize {
                return Err(RelayError::EnvelopeRejected);
            }
            return Ok(RelayReceiveDisposition::DuplicateAuthenticated);
        }
        let max_ciphertext_bytes = u64::from(self.chunk_size).saturating_add(16);
        let max_ciphertext_base64 = max_ciphertext_bytes.div_ceil(3).saturating_mul(4);
        if envelope.nonce_base64.len() != 32
            || envelope.ciphertext_base64.len() as u64 > max_ciphertext_base64
        {
            return Err(RelayError::EnvelopeRejected);
        }
        envelope
            .validate(&self.credential, &self.limits)
            .map_err(|_| RelayError::EnvelopeRejected)?;
        if envelope.chunk_count != self.chunk_count {
            return Err(RelayError::EnvelopeRejected);
        }
        let nonce = decode_24(&envelope.nonce_base64)?;
        if self.used_nonces.contains(&nonce) {
            return Err(RelayError::Replay);
        }
        if envelope.chunk_index != self.next_index {
            return Err(RelayError::OutOfOrder);
        }
        let remaining = self.expected_bytes.saturating_sub(self.observed_bytes);
        let expected_size = remaining.min(u64::from(self.chunk_size)) as u32;
        if envelope.plaintext_size != expected_size {
            return Err(RelayError::EnvelopeRejected);
        }
        let plaintext = self.decrypt_checked(envelope, expected_size)?;
        if plaintext.len() != expected_size as usize {
            return Err(RelayError::EnvelopeRejected);
        }
        self.used_nonces.insert(nonce);
        self.observed_bytes = self
            .observed_bytes
            .checked_add(plaintext.len() as u64)
            .ok_or(RelayError::EnvelopeRejected)?;
        if self.observed_bytes > self.expected_bytes
            || self.observed_bytes > self.credential.max_bytes
            || self.observed_bytes > self.limits.max_encoded_bytes
        {
            return Err(RelayError::EnvelopeRejected);
        }
        self.assembled.extend_from_slice(&plaintext);
        self.next_index += 1;
        self.last_accepted = Some(envelope.clone());
        Ok(RelayReceiveDisposition::Accepted)
    }

    fn decrypt_checked(
        &self,
        envelope: &EncryptedRelayEnvelope,
        expected_size: u32,
    ) -> Result<Vec<u8>, RelayError> {
        envelope
            .validate(&self.credential, &self.limits)
            .map_err(|_| RelayError::EnvelopeRejected)?;
        let nonce = decode_24(&envelope.nonce_base64)?;
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(&envelope.ciphertext_base64)
            .map_err(|_| RelayError::EnvelopeRejected)?;
        let aad = envelope_aad(
            &envelope.session_id,
            &envelope.object_sha256,
            envelope.chunk_index,
            envelope.chunk_count,
            envelope.plaintext_size,
            &nonce,
        );
        let plaintext = self
            .cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| RelayError::AuthenticationFailed)?;
        if plaintext.len() != expected_size as usize {
            return Err(RelayError::EnvelopeRejected);
        }
        Ok(plaintext)
    }

    pub fn acknowledgement(&self, chunk_index: u32) -> Result<AuthenticatedRelayAck, RelayError> {
        if chunk_index >= self.next_index {
            return Err(RelayError::OutOfOrder);
        }
        self.ack_authenticator.authenticate(
            &self.credential.session_id,
            &self.credential.object_sha256,
            RelayAckKind::Chunk,
            chunk_index,
        )
    }

    /// Build the downloader-only readiness marker used to establish the TURN
    /// allocation's receive permission. It must be emitted before any data is
    /// accepted; retransmitting the returned immutable marker is harmless.
    pub fn receiver_ready(&self) -> Result<AuthenticatedRelayAck, RelayError> {
        if self.next_index != 0 || self.observed_bytes != 0 {
            return Err(RelayError::OutOfOrder);
        }
        self.ack_authenticator.authenticate(
            &self.credential.session_id,
            &self.credential.object_sha256,
            RelayAckKind::ReceiverReady,
            0,
        )
    }

    pub fn verify_completion(&self, completion: &AuthenticatedRelayAck) -> Result<(), RelayError> {
        if !self.is_complete_and_hash_valid() {
            return Err(RelayError::ObjectMismatch);
        }
        self.ack_authenticator.verify(
            completion,
            &self.credential.session_id,
            &self.credential.object_sha256,
            RelayAckKind::TransferComplete,
            self.chunk_count - 1,
        )
    }

    pub fn transfer_receipt(&self) -> Result<AuthenticatedRelayAck, RelayError> {
        if !self.is_complete_and_hash_valid() {
            return Err(RelayError::ObjectMismatch);
        }
        self.ack_authenticator.authenticate(
            &self.credential.session_id,
            &self.credential.object_sha256,
            RelayAckKind::TransferReceipt,
            self.chunk_count - 1,
        )
    }

    pub fn is_complete_and_hash_valid(&self) -> bool {
        self.next_index == self.chunk_count
            && self.observed_bytes == self.expected_bytes
            && object_sha256(&self.assembled) == self.credential.object_sha256
    }

    pub(crate) fn verified_bytes(&self) -> Result<&[u8], RelayError> {
        self.is_complete_and_hash_valid()
            .then_some(self.assembled.as_slice())
            .ok_or(RelayError::ObjectMismatch)
    }

    pub(crate) const fn expected_chunk_count(&self) -> u32 {
        self.chunk_count
    }

    pub fn finish(self) -> Result<Vec<u8>, RelayError> {
        if self.next_index != self.chunk_count
            || self.observed_bytes != self.expected_bytes
            || object_sha256(&self.assembled) != self.credential.object_sha256
        {
            return Err(RelayError::ObjectMismatch);
        }
        Ok(self.assembled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayReceiveDisposition {
    Accepted,
    DuplicateAuthenticated,
}

fn validate_cipher_construction(
    binding: &SessionBinding,
    credential: &RelayCredentialClaims,
    direction: RelayDirection,
    expected_bytes: u64,
    policy: RelayChunkPolicy,
    limits: &ProtocolLimits,
) -> Result<(), RelayError> {
    let chunks = expected_bytes.div_ceil(u64::from(policy.max_plaintext_per_chunk.max(1)));
    if credential.direction != direction
        || credential.session_id != binding.session_id
        || credential.object_sha256 != binding.object_sha256
        || expected_bytes == 0
        || expected_bytes > credential.max_bytes
        || expected_bytes > limits.max_encoded_bytes
        || policy.max_plaintext_per_chunk == 0
        || policy.max_plaintext_per_chunk > crate::RELAY_PLAINTEXT_CHUNK_BYTES
        || u64::from(policy.max_plaintext_per_chunk) > limits.max_encoded_bytes
        || chunks == 0
        || chunks > u64::from(credential.max_chunks)
        || chunks > u64::from(limits.max_relay_chunks)
    {
        return Err(RelayError::EnvelopeRejected);
    }
    Ok(())
}

/// Exact allocation-free chunk arithmetic shared by credential issuance and
/// clients. V1 preserves the full 64 MiB encoded-object protocol ceiling at
/// 512 bytes per datagram while rejecting any byte or chunk overrun.
pub fn bounded_relay_chunk_count(
    expected_bytes: u64,
    limits: &ProtocolLimits,
) -> Result<u32, RelayError> {
    if expected_bytes == 0 || expected_bytes > limits.max_encoded_bytes {
        return Err(RelayError::EnvelopeRejected);
    }
    let chunks = expected_bytes.div_ceil(u64::from(crate::RELAY_PLAINTEXT_CHUNK_BYTES));
    let chunks = u32::try_from(chunks).map_err(|_| RelayError::EnvelopeRejected)?;
    if chunks == 0 || chunks > limits.max_relay_chunks {
        return Err(RelayError::EnvelopeRejected);
    }
    Ok(chunks)
}

fn envelope_aad(
    session_id: &str,
    object_sha256: &str,
    chunk_index: u32,
    chunk_count: u32,
    plaintext_size: u32,
    nonce: &[u8; 24],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(192);
    bytes.extend_from_slice(ENVELOPE_AAD_DOMAIN);
    put_str(&mut bytes, rw_community_protocol::RELAY_ENVELOPE_SCHEMA);
    put_str(&mut bytes, session_id);
    put_str(&mut bytes, object_sha256);
    bytes.push(1); // XChaCha20-Poly1305
    bytes.extend_from_slice(&chunk_index.to_be_bytes());
    bytes.extend_from_slice(&chunk_count.to_be_bytes());
    bytes.extend_from_slice(&plaintext_size.to_be_bytes());
    put_bytes(&mut bytes, nonce);
    bytes
}

fn ack_material(
    session_id: &str,
    object_sha256: &str,
    kind: RelayAckKind,
    chunk_index: u32,
) -> Result<([u8; 24], Vec<u8>), RelayError> {
    if !valid_opaque_id(session_id) || !valid_sha256(object_sha256) {
        return Err(RelayError::AuthenticationFailed);
    }
    let mut aad = Vec::with_capacity(256);
    aad.extend_from_slice(ACK_AAD_DOMAIN);
    put_str(&mut aad, RELAY_ACK_SCHEMA);
    put_str(&mut aad, session_id);
    put_str(&mut aad, object_sha256);
    aad.push(match kind {
        RelayAckKind::Chunk => 1,
        RelayAckKind::TransferComplete => 2,
        RelayAckKind::TransferReceipt => 3,
        // Appended instead of renumbering the existing v1 authenticated
        // transcript values.
        RelayAckKind::ReceiverReady => 4,
    });
    aad.extend_from_slice(&chunk_index.to_be_bytes());
    let digest = Sha256::digest(&aad);
    let mut nonce = [0_u8; 24];
    nonce.copy_from_slice(&digest[..24]);
    Ok((nonce, aad))
}

fn decode_32(encoded: &str) -> Result<[u8; 32], RelayError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| RelayError::KeyAgreementRejected)?;
    bytes
        .try_into()
        .map_err(|_| RelayError::KeyAgreementRejected)
}

fn decode_24(encoded: &str) -> Result<[u8; 24], RelayError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| RelayError::EnvelopeRejected)?;
    bytes.try_into().map_err(|_| RelayError::EnvelopeRejected)
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_bytes(out, value.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(value);
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;
    use rw_community_protocol::{
        RELAY_CREDENTIAL_SCHEMA, RelayCredentialClaims, sign_relay_credential,
    };

    use super::*;

    fn credentials(
        object_hash: &str,
        expected_bytes: u64,
    ) -> (
        SigningKey,
        TrustedSigningKeys,
        SignedRelayCredential,
        SignedRelayCredential,
    ) {
        let key = SigningKey::from_bytes(&[7; 32]);
        let keys = BTreeMap::from([("relay-signing".into(), key.verifying_key())]);
        let common = RelayCredentialClaims {
            schema: RELAY_CREDENTIAL_SCHEMA.into(),
            relay_id: "cf-relay".into(),
            session_id: "session-one".into(),
            subject_id: "subject-upload".into(),
            object_sha256: object_hash.into(),
            direction: RelayDirection::Upload,
            issued_unix: 100,
            not_before_unix: 100,
            expires_unix: 700,
            max_bytes: expected_bytes,
            max_chunks: 32,
        };
        let upload = sign_relay_credential(
            common.clone(),
            "relay-signing",
            &key,
            100,
            &ProtocolLimits::default(),
        )
        .unwrap();
        let download = sign_relay_credential(
            RelayCredentialClaims {
                subject_id: "subject-download".into(),
                direction: RelayDirection::Download,
                ..common
            },
            "relay-signing",
            &key,
            100,
            &ProtocolLimits::default(),
        )
        .unwrap();
        (key, keys, upload, download)
    }

    fn bound_session(
        object: &[u8],
    ) -> (
        VerifiedSessionBinding,
        SessionKey,
        SessionKey,
        SignedRelayCredential,
        SignedRelayCredential,
    ) {
        let hash = object_sha256(object);
        let (signing_key, keys, upload, download) = credentials(&hash, object.len() as u64);
        let uploader = EphemeralKeyPair::from_seed([1; 32]);
        let downloader = EphemeralKeyPair::from_seed([2; 32]);
        let upload_offer = uploader
            .offer(
                &upload,
                RelayRole::Uploader,
                100,
                &ProtocolLimits::default(),
            )
            .unwrap();
        let download_offer = downloader
            .offer(
                &download,
                RelayRole::Downloader,
                100,
                &ProtocolLimits::default(),
            )
            .unwrap();
        let binding = build_session_binding(&upload_offer, &download_offer, 700).unwrap();
        let signed = sign_session_binding(binding, "relay-signing", &signing_key).unwrap();
        let verified = verify_signed_session_binding(
            &signed,
            &upload,
            &download,
            100,
            &keys,
            &ProtocolLimits::default(),
        )
        .unwrap();
        let uploader_key = uploader
            .derive_session_key(&verified, RelayRole::Uploader)
            .unwrap();
        let downloader_key = downloader
            .derive_session_key(&verified, RelayRole::Downloader)
            .unwrap();
        (verified, uploader_key, downloader_key, upload, download)
    }

    #[test]
    fn x25519_xchacha_round_trip_binds_every_chunk_to_object_and_session() {
        let object = b"origin-signed immutable weather object";
        let (binding, upload_key, download_key, upload, download) = bound_session(object);
        let policy = RelayChunkPolicy {
            max_plaintext_per_chunk: 9,
        };
        let mut sender = RelaySender::new(
            upload_key,
            &binding,
            &upload.claims,
            object.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        let mut receiver = RelayReceiver::new(
            download_key,
            &binding,
            &download.claims,
            object.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        let mut rng = ChaCha20Rng::from_seed([8; 32]);
        let mut envelopes = Vec::new();
        for chunk in object.chunks(9) {
            envelopes.push(sender.encrypt_next(chunk, &mut rng).unwrap());
        }
        sender.finish().unwrap();
        for envelope in &envelopes {
            receiver.accept(envelope).unwrap();
        }
        assert_eq!(receiver.finish().unwrap(), object);
    }

    #[test]
    fn tamper_replay_reordering_wrong_hash_and_wrong_session_fail_closed() {
        let object = b"0123456789abcdef";
        let policy = RelayChunkPolicy {
            max_plaintext_per_chunk: 8,
        };

        let make_envelopes = || {
            let (binding, upload_key, download_key, upload, download) = bound_session(object);
            let mut sender = RelaySender::new(
                upload_key,
                &binding,
                &upload.claims,
                object.len() as u64,
                policy,
                ProtocolLimits::default(),
            )
            .unwrap();
            let mut rng = ChaCha20Rng::from_seed([4; 32]);
            let envelopes = object
                .chunks(8)
                .map(|chunk| sender.encrypt_next(chunk, &mut rng).unwrap())
                .collect::<Vec<_>>();
            (binding, download_key, download, envelopes)
        };

        let (binding, key, download, mut envelopes) = make_envelopes();
        let mut receiver = RelayReceiver::new(
            key,
            &binding,
            &download.claims,
            object.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        let mut ciphertext = base64::engine::general_purpose::STANDARD
            .decode(&envelopes[0].ciphertext_base64)
            .unwrap();
        ciphertext[0] ^= 0x80;
        envelopes[0].ciphertext_base64 =
            base64::engine::general_purpose::STANDARD.encode(ciphertext);
        assert_eq!(
            receiver.accept(&envelopes[0]),
            Err(RelayError::AuthenticationFailed)
        );

        let (binding, key, download, envelopes) = make_envelopes();
        let mut receiver = RelayReceiver::new(
            key,
            &binding,
            &download.claims,
            object.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        receiver.accept(&envelopes[0]).unwrap();
        assert_eq!(receiver.accept(&envelopes[0]), Err(RelayError::Replay));

        let (binding, key, download, envelopes) = make_envelopes();
        let mut receiver = RelayReceiver::new(
            key,
            &binding,
            &download.claims,
            object.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        assert_eq!(receiver.accept(&envelopes[1]), Err(RelayError::OutOfOrder));

        for mutate in ["session_id", "object_sha256"] {
            let (binding, key, download, mut envelopes) = make_envelopes();
            if mutate == "session_id" {
                envelopes[0].session_id = "different-session".into();
            } else {
                envelopes[0].object_sha256 = "a".repeat(64);
            }
            let mut receiver = RelayReceiver::new(
                key,
                &binding,
                &download.claims,
                object.len() as u64,
                policy,
                ProtocolLimits::default(),
            )
            .unwrap();
            assert_eq!(
                receiver.accept(&envelopes[0]),
                Err(RelayError::EnvelopeRejected)
            );
        }
    }

    #[test]
    fn binding_signature_and_credential_identity_cannot_be_substituted() {
        let object = b"signed bytes";
        let hash = object_sha256(object);
        let (key, keys, upload, download) = credentials(&hash, object.len() as u64);
        let uploader = EphemeralKeyPair::from_seed([1; 32]);
        let downloader = EphemeralKeyPair::from_seed([2; 32]);
        let up = uploader
            .offer(
                &upload,
                RelayRole::Uploader,
                100,
                &ProtocolLimits::default(),
            )
            .unwrap();
        let down = downloader
            .offer(
                &download,
                RelayRole::Downloader,
                100,
                &ProtocolLimits::default(),
            )
            .unwrap();
        let mut signed = sign_session_binding(
            build_session_binding(&up, &down, 700).unwrap(),
            "relay-signing",
            &key,
        )
        .unwrap();
        signed.binding.object_sha256 = "b".repeat(64);
        assert!(
            verify_signed_session_binding(
                &signed,
                &upload,
                &download,
                100,
                &keys,
                &ProtocolLimits::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn authenticated_but_wrong_object_bytes_fail_the_final_sha256_gate() {
        let expected = b"aaaaaaaa";
        let hostile = b"bbbbbbbb";
        let (binding, upload_key, download_key, upload, download) = bound_session(expected);
        let policy = RelayChunkPolicy {
            max_plaintext_per_chunk: 8,
        };
        let mut sender = RelaySender::new(
            upload_key,
            &binding,
            &upload.claims,
            expected.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        let mut receiver = RelayReceiver::new(
            download_key,
            &binding,
            &download.claims,
            expected.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        let envelope = sender
            .encrypt_next(hostile, &mut ChaCha20Rng::from_seed([9; 32]))
            .unwrap();
        sender.finish().unwrap();
        receiver.accept(&envelope).unwrap();
        assert_eq!(receiver.finish(), Err(RelayError::ObjectMismatch));
    }

    #[test]
    fn datagram_and_full_encoded_ceiling_are_exact_without_large_allocation() {
        let limits = ProtocolLimits::default();
        assert_eq!(
            bounded_relay_chunk_count(64 * 1024 * 1024, &limits).unwrap(),
            131_072
        );
        assert_eq!(
            bounded_relay_chunk_count(64 * 1024 * 1024 + 1, &limits),
            Err(RelayError::EnvelopeRejected)
        );

        let object = vec![0x5a; crate::RELAY_PLAINTEXT_CHUNK_BYTES as usize];
        let hash = object_sha256(&object);
        let (signing_key, keys, mut upload, mut download) = credentials(&hash, object.len() as u64);
        upload.claims.session_id = "s".repeat(128);
        download.claims.session_id = upload.claims.session_id.clone();
        upload.claims.max_chunks = limits.max_relay_chunks;
        download.claims.max_chunks = upload.claims.max_chunks;
        upload = sign_relay_credential(upload.claims, "relay-signing", &signing_key, 100, &limits)
            .unwrap();
        download =
            sign_relay_credential(download.claims, "relay-signing", &signing_key, 100, &limits)
                .unwrap();
        let uploader = EphemeralKeyPair::from_seed([7; 32]);
        let downloader = EphemeralKeyPair::from_seed([8; 32]);
        let up_offer = uploader
            .offer(&upload, RelayRole::Uploader, 100, &limits)
            .unwrap();
        let down_offer = downloader
            .offer(&download, RelayRole::Downloader, 100, &limits)
            .unwrap();
        let signed = sign_session_binding(
            build_session_binding(&up_offer, &down_offer, 700).unwrap(),
            "relay-signing",
            &signing_key,
        )
        .unwrap();
        let verified =
            verify_signed_session_binding(&signed, &upload, &download, 100, &keys, &limits)
                .unwrap();
        let mut sender = RelaySender::new(
            uploader
                .derive_session_key(&verified, RelayRole::Uploader)
                .unwrap(),
            &verified,
            &upload.claims,
            object.len() as u64,
            RelayChunkPolicy::default(),
            limits,
        )
        .unwrap();
        let mut envelope = sender
            .encrypt_next(&object, &mut ChaCha20Rng::from_seed([15; 32]))
            .unwrap();
        envelope.chunk_index = u32::MAX - 1;
        envelope.chunk_count = u32::MAX;
        let encoded = serde_json::to_vec(&envelope).unwrap();
        assert!(encoded.len() <= crate::MAX_RELAY_WIRE_DATAGRAM_BYTES);
        assert_eq!(encoded.len(), 1_150);
    }

    #[test]
    fn reliable_duplicate_reacks_only_exact_authenticated_last_chunk() {
        let object = b"0123456789abcdef";
        let policy = RelayChunkPolicy {
            max_plaintext_per_chunk: 8,
        };
        let (binding, upload_key, download_key, upload, download) = bound_session(object);
        let mut sender = RelaySender::new(
            upload_key,
            &binding,
            &upload.claims,
            object.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        let mut receiver = RelayReceiver::new(
            download_key,
            &binding,
            &download.claims,
            object.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        let mut rng = ChaCha20Rng::from_seed([21; 32]);
        let first = sender.encrypt_next(&object[..8], &mut rng).unwrap();
        let second = sender.encrypt_next(&object[8..], &mut rng).unwrap();
        assert_eq!(
            receiver.accept_reliable(&second),
            Err(RelayError::OutOfOrder)
        );
        assert_eq!(
            receiver.accept_reliable(&first).unwrap(),
            RelayReceiveDisposition::Accepted
        );
        let ack = receiver.acknowledgement(0).unwrap();
        sender.verify_ack(&ack, 0).unwrap();
        assert_eq!(
            receiver.accept_reliable(&first).unwrap(),
            RelayReceiveDisposition::DuplicateAuthenticated
        );
        let mut altered = first.clone();
        altered.ciphertext_base64.replace_range(0..1, "A");
        assert_eq!(receiver.accept_reliable(&altered), Err(RelayError::Replay));
        assert_eq!(
            receiver.accept_reliable(&second).unwrap(),
            RelayReceiveDisposition::Accepted
        );
        let completion = sender.completion_confirmation().unwrap();
        receiver.verify_completion(&completion).unwrap();
        assert_eq!(receiver.finish().unwrap(), object);
    }

    #[test]
    fn receiver_ready_is_authenticated_idempotent_and_strictly_bound() {
        let object = b"receiver permission readiness";
        let policy = RelayChunkPolicy {
            max_plaintext_per_chunk: 8,
        };
        let (binding, upload_key, download_key, upload, download) = bound_session(object);
        let mut sender = RelaySender::new(
            upload_key,
            &binding,
            &upload.claims,
            object.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();
        let mut receiver = RelayReceiver::new(
            download_key,
            &binding,
            &download.claims,
            object.len() as u64,
            policy,
            ProtocolLimits::default(),
        )
        .unwrap();

        let ready = receiver.receiver_ready().unwrap();
        assert_eq!(ready.kind, RelayAckKind::ReceiverReady);
        assert_eq!(ready.chunk_index, 0);
        sender.verify_receiver_ready(&ready).unwrap();
        sender.verify_receiver_ready(&ready).unwrap();

        let mut tampered = ready.clone();
        let mut authenticator = base64::engine::general_purpose::STANDARD
            .decode(&tampered.authenticator_base64)
            .unwrap();
        authenticator[0] ^= 0x80;
        tampered.authenticator_base64 =
            base64::engine::general_purpose::STANDARD.encode(authenticator);
        assert_eq!(
            sender.verify_receiver_ready(&tampered),
            Err(RelayError::AuthenticationFailed)
        );

        let mut wrong_session = ready.clone();
        wrong_session.session_id = "another-session".into();
        assert_eq!(
            sender.verify_receiver_ready(&wrong_session),
            Err(RelayError::AuthenticationFailed)
        );

        let mut wrong_object = ready.clone();
        wrong_object.object_sha256 = "a".repeat(64);
        assert_eq!(
            sender.verify_receiver_ready(&wrong_object),
            Err(RelayError::AuthenticationFailed)
        );

        let mut wrong_role_kind = ready.clone();
        wrong_role_kind.kind = RelayAckKind::Chunk;
        assert_eq!(
            sender.verify_receiver_ready(&wrong_role_kind),
            Err(RelayError::AuthenticationFailed)
        );

        let mut out_of_order = ready;
        out_of_order.chunk_index = 1;
        assert_eq!(
            sender.verify_receiver_ready(&out_of_order),
            Err(RelayError::AuthenticationFailed)
        );

        let first = sender
            .encrypt_next(&object[..8], &mut ChaCha20Rng::from_seed([33; 32]))
            .unwrap();
        receiver.accept(&first).unwrap();
        assert_eq!(receiver.receiver_ready(), Err(RelayError::OutOfOrder));
    }
}
