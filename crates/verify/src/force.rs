//! Forced-move payloads: an action sealed to the operator's X25519 key, posted on-chain.

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use alloy_primitives::B256;
use sha2::{Digest, Sha256};
use werewolf_protocol::Action;
use x25519_dalek::{PublicKey, SharedSecret, StaticSecret};

use crate::encode::Domain;

const FORCE_TAG: &[u8] = b"werewolf/force/v1";
/// Safe because every payload uses a fresh ephemeral key, hence a fresh AES key.
const NONCE: [u8; 12] = [0; 12];
const TAG_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum ForceError {
    #[error("payload is {0} bytes, shorter than an ephemeral key and a tag")]
    TooShort(usize),
    #[error("shared secret is non-contributory")]
    NonContributory,
    #[error("AES-GCM rejected the payload")]
    Aead,
    #[error("plaintext is not an action: {0}")]
    Json(#[from] serde_json::Error),
    #[cfg(feature = "getrandom")]
    #[error("drawing an ephemeral key: {0}")]
    Rng(getrandom::Error),
}

/// The X25519 public key for `secret`; the operator posts it as `encKey`.
pub fn public_key(secret: &[u8; 32]) -> B256 {
    B256::from(PublicKey::from(&StaticSecret::from(*secret)).to_bytes())
}

/// Seals `action` for `round` and `seat` to `enc_key`, using `ephemeral` as `e`.
pub fn encrypt(
    enc_key: &B256,
    ephemeral: [u8; 32],
    domain: &Domain,
    round: u32,
    seat: u32,
    action: &Action,
) -> Result<Vec<u8>, ForceError> {
    let ephemeral = StaticSecret::from(ephemeral);
    let e_pub = PublicKey::from(&ephemeral).to_bytes();
    let shared = ephemeral.diffie_hellman(&PublicKey::from(enc_key.0));
    let cipher = cipher(&shared, &e_pub, &enc_key.0)?;
    let plaintext = serde_json::to_vec(action)?;
    let aad = aad(domain, round, seat);
    let sealed = cipher
        .encrypt(
            &Nonce::from(NONCE),
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| ForceError::Aead)?;
    Ok([e_pub.as_slice(), &sealed].concat())
}

/// [`encrypt`] with a fresh ephemeral key from the OS.
#[cfg(feature = "getrandom")]
pub fn encrypt_random(
    enc_key: &B256,
    domain: &Domain,
    round: u32,
    seat: u32,
    action: &Action,
) -> Result<Vec<u8>, ForceError> {
    let mut ephemeral = [0; 32];
    getrandom::fill(&mut ephemeral).map_err(ForceError::Rng)?;
    encrypt(enc_key, ephemeral, domain, round, seat, action)
}

/// Opens a payload posted for `round` and `seat` with the operator's `secret`.
pub fn decrypt(
    secret: &[u8; 32],
    domain: &Domain,
    round: u32,
    seat: u32,
    payload: &[u8],
) -> Result<Action, ForceError> {
    let Some((e_pub, sealed)) = payload.split_first_chunk::<32>() else {
        return Err(ForceError::TooShort(payload.len()));
    };
    if sealed.len() < TAG_LEN {
        return Err(ForceError::TooShort(payload.len()));
    }
    let secret = StaticSecret::from(*secret);
    let enc_key = PublicKey::from(&secret).to_bytes();
    let shared = secret.diffie_hellman(&PublicKey::from(*e_pub));
    let cipher = cipher(&shared, e_pub, &enc_key)?;
    let aad = aad(domain, round, seat);
    let plaintext = cipher
        .decrypt(
            &Nonce::from(NONCE),
            Payload {
                msg: sealed,
                aad: &aad,
            },
        )
        .map_err(|_| ForceError::Aead)?;
    Ok(serde_json::from_slice(&plaintext)?)
}

/// `k = sha256("werewolf/force/v1" ‖ X25519(e, encKey) ‖ ePub ‖ encKey)`.
fn cipher(
    shared: &SharedSecret,
    e_pub: &[u8; 32],
    enc_key: &[u8; 32],
) -> Result<Aes256Gcm, ForceError> {
    if !shared.was_contributory() {
        return Err(ForceError::NonContributory);
    }
    let key = Sha256::new()
        .chain_update(FORCE_TAG)
        .chain_update(shared.as_bytes())
        .chain_update(e_pub)
        .chain_update(enc_key)
        .finalize();
    Ok(Aes256Gcm::new(&key))
}

/// `be32(round) ‖ be32(seat) ‖ D`.
pub fn aad(domain: &Domain, round: u32, seat: u32) -> Vec<u8> {
    [
        &round.to_be_bytes()[..],
        &seat.to_be_bytes(),
        &domain.to_bytes(),
    ]
    .concat()
}
