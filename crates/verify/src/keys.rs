//! Ed25519 session keys, which sign joins and every action.

use alloy_primitives::{B256, B512};
use ed25519_dalek::{Signature, Signer, VerifyingKey};

pub use ed25519_dalek::SigningKey;

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    #[error("session key is not a valid Ed25519 public key")]
    InvalidKey,
    #[error("signature does not verify under the session key")]
    BadSignature,
}

/// The public half, as registered on-chain.
pub fn session_key(key: &SigningKey) -> B256 {
    B256::from(key.verifying_key().to_bytes())
}

pub fn sign(key: &SigningKey, message: &[u8]) -> B512 {
    B512::from(key.sign(message).to_bytes())
}

/// Strict RFC 8032 verification, which rejects malleable and small-order inputs.
pub fn verify(session_key: &B256, message: &[u8], signature: &B512) -> Result<(), KeyError> {
    let key = parse(session_key)?;
    verify_with(&key, message, signature)
}

pub(crate) fn parse(session_key: &B256) -> Result<VerifyingKey, KeyError> {
    VerifyingKey::from_bytes(&session_key.0).map_err(|_| KeyError::InvalidKey)
}

pub(crate) fn verify_with(
    key: &VerifyingKey,
    message: &[u8],
    signature: &B512,
) -> Result<(), KeyError> {
    key.verify_strict(message, &Signature::from_bytes(&signature.0))
        .map_err(|_| KeyError::BadSignature)
}
