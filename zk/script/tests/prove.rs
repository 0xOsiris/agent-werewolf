//! Runs `werewolf-prove` on the honest game from `crates/verify/tests`.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use alloy_sol_types::SolValue;
use serde_json::Value;
use werewolf_verify::{
    Decrypt, Transcript,
    alloy_primitives::{B256, Bytes},
    verify,
};

/// The operator secret `crates/verify/tests/common` builds the transcript with.
const ENC_SECRET: [u8; 32] = [0x33; 32];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/verify/tests")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn write_secret(name: &str, secret: [u8; 32]) -> PathBuf {
    let path = tmp(name);
    fs::write(&path, format!("{}\n", B256::from(secret))).unwrap();
    path
}

fn run(transcript: &Path, secret: &Path, mode: &str, out: &Path) -> Output {
    let _ = fs::remove_file(out);
    Command::new(env!("CARGO_BIN_EXE_werewolf-prove"))
        .args(["prove", "--mode", mode, "--transcript"])
        .arg(transcript)
        .arg("--enc-secret-file")
        .arg(secret)
        .arg("--out")
        .arg(out)
        .output()
        .unwrap()
}

fn hex_field(output: &Value, field: &str) -> String {
    output[field].as_str().unwrap().to_owned()
}

#[test]
fn execute_matches_native_replay_and_vectors() {
    let transcript = fixture("transcript.json");
    let secret = write_secret("secret-execute", ENC_SECRET);
    let out = tmp("execute.json");
    let result = run(&transcript, &secret, "execute", &out);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output: Value = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();

    let parsed: Transcript = serde_json::from_slice(&fs::read(&transcript).unwrap()).unwrap();
    let native = verify(&parsed, Decrypt::WithSecret(ENC_SECRET)).unwrap();
    let vectors: Value =
        serde_json::from_slice(&fs::read(fixture("vectors.json")).unwrap()).unwrap();

    let public_values = hex_field(&output, "public_values");
    assert_eq!(public_values, Bytes::from(native.abi_encode()).to_string());
    assert_eq!(
        public_values,
        hex_field(&vectors["transcript"], "public_values")
    );
    assert_eq!(hex_field(&output, "proof"), "0x");
    assert_eq!(hex_field(&output, "vkey").len(), 66);
    let cycles = output["cycles"].as_u64().unwrap();
    assert!(cycles > 0);
    eprintln!("cycles: {cycles}");
}

#[test]
fn mock_proof_is_empty() {
    let secret = write_secret("secret-mock", ENC_SECRET);
    let out = tmp("mock.json");
    let result = run(&fixture("transcript.json"), &secret, "mock", &out);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output: Value = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();
    assert_eq!(hex_field(&output, "proof"), "0x");
}

#[test]
fn tampered_transcript_fails_native_check() {
    let mut transcript: Value =
        serde_json::from_slice(&fs::read(fixture("transcript.json")).unwrap()).unwrap();
    let message = transcript["steps"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|step| step["type"] == "chat")
        .unwrap()["messages"][0]
        .as_object_mut()
        .unwrap();
    message.insert("text".into(), "I am the seer.".into());
    let tampered = tmp("tampered.json");
    fs::write(&tampered, transcript.to_string()).unwrap();

    let secret = write_secret("secret-tampered", ENC_SECRET);
    let out = tmp("tampered-out.json");
    let result = run(&tampered, &secret, "execute", &out);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(
        stderr.contains("transcript does not replay: step"),
        "{stderr}"
    );
    assert!(!out.exists());
}

#[test]
fn wrong_secret_fails_without_echoing_it() {
    let secret = write_secret("secret-wrong", [0x44; 32]);
    let out = tmp("wrong-secret.json");
    let result = run(&fixture("transcript.json"), &secret, "execute", &out);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(stderr.contains("enc_key is not the public key"), "{stderr}");
    assert!(!stderr.contains("4444"), "{stderr}");
    assert!(!out.exists());
}
