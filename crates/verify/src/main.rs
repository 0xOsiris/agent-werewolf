//! `werewolf-verify <transcript.json> [--enc-secret-file PATH]`: replays a staked game and prints
//! its `PublicValues` as JSON.

use std::{fs, path::PathBuf};

use anyhow::{Context, anyhow, ensure};
use clap::Parser;
use werewolf_verify::{
    Decrypt, Transcript, alloy_primitives::B256, check_result, encode::public_values_json, verify,
};

/// Transcripts are a few hundred KB; this bounds what an untrusted file can make us parse.
const MAX_TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "werewolf-verify",
    about = "Replay a staked Agent Werewolf transcript and print its PublicValues"
)]
struct Args {
    /// Transcript JSON, as served by `GET /v1/transcript`.
    transcript: PathBuf,
    /// File holding the operator's X25519 secret as 0x-hex; also decrypts and checks forced moves.
    #[arg(long)]
    enc_secret_file: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let path = &args.transcript;
    let size = fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    ensure!(
        size <= MAX_TRANSCRIPT_BYTES,
        "{} is {size} bytes, over the {MAX_TRANSCRIPT_BYTES}-byte limit",
        path.display()
    );
    let json = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let transcript: Transcript = serde_json::from_slice(&json)
        .with_context(|| format!("parsing transcript {}", path.display()))?;
    let decrypt = match &args.enc_secret_file {
        Some(path) => Decrypt::WithSecret(read_secret(path)?),
        None => Decrypt::Trust,
    };
    let values = verify(&transcript, decrypt).context("transcript does not replay")?;
    check_result(&transcript, &values).context("transcript result is wrong")?;
    println!("{:#}", public_values_json(&values));
    Ok(())
}

fn read_secret(path: &PathBuf) -> anyhow::Result<[u8; 32]> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // The parse error can quote secret characters, so it is replaced, not wrapped.
    let secret: B256 = text
        .trim()
        .parse()
        .map_err(|_| anyhow!("{} must hold 32 bytes of hex", path.display()))?;
    Ok(secret.0)
}
