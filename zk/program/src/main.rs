//! SP1 guest: replays a staked game's transcript and commits `abi.encode(PublicValues)`.
//!
//! Stdin: the transcript JSON (`write_vec`), then the operator's 32-byte X25519 secret (`write`).
//! Any replay error panics, so no proof exists for a transcript that `werewolf-verify` rejects.

#![no_main]
sp1_zkvm::entrypoint!(main);

use alloy_sol_types::SolValue;
use werewolf_verify::{Decrypt, Transcript, verify};

pub fn main() {
    let json = sp1_zkvm::io::read_vec();
    let secret = sp1_zkvm::io::read::<[u8; 32]>();
    let transcript: Transcript = serde_json::from_slice(&json)
        .unwrap_or_else(|err| panic!("transcript does not parse: {err}"));
    let values = verify(&transcript, Decrypt::WithSecret(secret))
        .unwrap_or_else(|err| panic!("transcript does not replay: {err}"));
    sp1_zkvm::io::commit_slice(&values.abi_encode());
}
