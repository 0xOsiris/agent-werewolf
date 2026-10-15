//! `werewolf-prove`: proves a staked game's replay with SP1, for `IWerewolfEscrow.prove`.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use alloy_sol_types::SolValue;
use anyhow::{Context, anyhow, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use sp1_sdk::{
    CpuProver, Elf, HashableKey, MockProver, ProveRequest, Prover, ProverClient, ProvingKey,
    SP1_CIRCUIT_VERSION, SP1ProofMode, SP1ProofWithPublicValues, SP1Stdin, include_elf,
    network::signer::NetworkSigner,
};
use werewolf_verify::{
    Decrypt, Transcript,
    alloy_primitives::{B256, Bytes},
    verify,
};

const ELF: Elf = include_elf!("werewolf-program");

/// Transcripts are a few hundred KB; this bounds what an untrusted file can make us parse.
const MAX_TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024;

/// `docker info` hangs when the daemon is wedged.
const DOCKER_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser, Debug)]
#[command(
    name = "werewolf-prove",
    about = "Prove a staked Agent Werewolf transcript with SP1"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Replay the transcript in SP1 and write `{public_values, proof, vkey, cycles}` as JSON.
    Prove(ProveArgs),
    /// Print the program verification key as bytes32 hex, the escrow's `PROGRAM_VKEY`.
    Vkey,
}

#[derive(clap::Args, Debug)]
struct ProveArgs {
    /// Transcript JSON, as served by `GET /v1/transcript`.
    #[arg(long)]
    transcript: PathBuf,
    /// File holding the operator's X25519 secret as 0x-hex.
    #[arg(long)]
    enc_secret_file: PathBuf,
    #[arg(long, value_enum)]
    mode: Mode,
    /// Where to write the JSON result.
    #[arg(long)]
    out: PathBuf,
    /// Gives up on `cpu` or `network` proving after this many seconds.
    #[arg(long, env = "WEREWOLF_PROVE_TIMEOUT_SECS", default_value_t = 1800)]
    timeout_secs: u64,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum Mode {
    /// Run the program without proving; the proof is empty.
    Execute,
    /// An empty proof, which only `SP1MockVerifier` accepts.
    Mock,
    /// A local Groth16 proof; needs Docker for the final wrap.
    Cpu,
    /// A Groth16 proof from the Succinct prover network; needs `NETWORK_PRIVATE_KEY`.
    Network,
}

/// A mode whose prerequisites hold, checked before minutes of program setup.
enum Backend {
    Execute,
    Mock,
    Cpu,
    Network(NetworkSigner),
}

impl Backend {
    async fn new(mode: Mode) -> anyhow::Result<Self> {
        Ok(match mode {
            Mode::Execute => Self::Execute,
            Mode::Mock => Self::Mock,
            Mode::Cpu => {
                ensure_docker().await?;
                Self::Cpu
            }
            Mode::Network => Self::Network(network_signer()?),
        })
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run(Cli::parse()).await {
        eprintln!("error: {err:#}");
        // Skips the runtime's drop, which would wait on proving threads a timeout abandoned.
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Prove(args) => prove(args).await,
        Command::Vkey => {
            let pk = MockProver::new()
                .await
                .setup(ELF)
                .await
                .context("program setup failed")?;
            println!("{}", pk.verifying_key().bytes32());
            Ok(())
        }
    }
}

async fn prove(args: ProveArgs) -> anyhow::Result<()> {
    let json = read_transcript(&args.transcript)?;
    let secret = read_secret(&args.enc_secret_file)?;
    let transcript: Transcript = serde_json::from_slice(&json)
        .with_context(|| format!("parsing transcript {}", args.transcript.display()))?;
    // Fails fast with a readable error instead of a guest panic.
    let expected = verify(&transcript, Decrypt::WithSecret(secret))
        .context("transcript does not replay")?
        .abi_encode();

    let backend = Backend::new(args.mode).await?;

    let mut stdin = SP1Stdin::new();
    stdin.write_vec(json);
    stdin.write(&secret);

    let mock = MockProver::new().await;
    let pk = mock.setup(ELF).await.context("program setup failed")?;
    let (public_values, report) = mock
        .execute(ELF, stdin.clone())
        .await
        .context("executing the program failed")?;
    ensure!(
        report.exit_code == 0,
        "program exited with code {}",
        report.exit_code
    );
    ensure!(
        public_values.as_slice() == expected.as_slice(),
        "program output differs from the native replay"
    );

    let timeout = Duration::from_secs(args.timeout_secs);
    let proof = match backend {
        Backend::Execute => Vec::new(),
        Backend::Mock => SP1ProofWithPublicValues::create_mock_proof(
            pk.verifying_key(),
            public_values,
            SP1ProofMode::Groth16,
            SP1_CIRCUIT_VERSION,
        )
        .bytes(),
        Backend::Cpu => {
            let proof = with_timeout(timeout, "cpu", async {
                let cpu = CpuProver::new().await;
                let pk = cpu.setup(ELF).await.context("program setup failed")?;
                cpu.prove(&pk, stdin)
                    .groth16()
                    .await
                    .context("cpu proving failed")
            })
            .await?;
            onchain_bytes(&proof, &expected)?
        }
        Backend::Network(signer) => {
            let proof = with_timeout(timeout, "network", async {
                let network = ProverClient::builder()
                    .network()
                    .signer(signer)
                    .build()
                    .await;
                let pk = network.setup(ELF).await.context("program setup failed")?;
                network
                    .prove(&pk, stdin)
                    .groth16()
                    .timeout(timeout)
                    .await
                    .context("network proving failed")
            })
            .await?;
            onchain_bytes(&proof, &expected)?
        }
    };

    let output = serde_json::json!({
        "public_values": Bytes::from(expected),
        "proof": Bytes::from(proof),
        "vkey": pk.verifying_key().bytes32(),
        "cycles": report.total_instruction_count(),
    });
    fs::write(&args.out, format!("{output:#}\n"))
        .with_context(|| format!("writing {}", args.out.display()))
}

fn read_transcript(path: &Path) -> anyhow::Result<Vec<u8>> {
    let size = fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    ensure!(
        size <= MAX_TRANSCRIPT_BYTES,
        "{} is {size} bytes, over the {MAX_TRANSCRIPT_BYTES}-byte limit",
        path.display()
    );
    fs::read(path).with_context(|| format!("reading {}", path.display()))
}

fn read_secret(path: &Path) -> anyhow::Result<[u8; 32]> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // The parse error can quote secret characters, so it is replaced, not wrapped.
    let secret: B256 = text
        .trim()
        .parse()
        .map_err(|_| anyhow!("{} must hold 32 bytes of 0x-hex", path.display()))?;
    Ok(secret.0)
}

fn network_signer() -> anyhow::Result<NetworkSigner> {
    let key = std::env::var("NETWORK_PRIVATE_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .context("network mode needs NETWORK_PRIVATE_KEY, the prover network account's key")?;
    // The SDK panics on a bad key, and its error could quote it.
    NetworkSigner::local(&key)
        .map_err(|_| anyhow!("NETWORK_PRIVATE_KEY is not a valid secp256k1 private key"))
}

/// Groth16 wrapping runs gnark in Docker; checking first avoids a panic after minutes of proving.
async fn ensure_docker() -> anyhow::Result<()> {
    let status = tokio::time::timeout(
        DOCKER_CHECK_TIMEOUT,
        tokio::process::Command::new("docker")
            .arg("info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await;
    match status {
        Ok(Ok(status)) if status.success() => Ok(()),
        _ => bail!(
            "cpu mode needs a running Docker daemon for the Groth16 wrap (`docker info` failed); \
             start Docker or use --mode network"
        ),
    }
}

async fn with_timeout<T>(
    timeout: Duration,
    mode: &str,
    proving: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(timeout, proving).await.map_err(|_| {
        anyhow!(
            "{mode} proving timed out after {}s: the prover is slow or unreachable; \
             raise --timeout-secs (WEREWOLF_PROVE_TIMEOUT_SECS) to wait longer",
            timeout.as_secs()
        )
    })?
}

/// The bytes `ISP1Verifier.verifyProof` takes, after checking the proof commits `expected`.
fn onchain_bytes(proof: &SP1ProofWithPublicValues, expected: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(
        proof.public_values.as_slice() == expected,
        "proof commits different public values than the native replay"
    );
    Ok(proof.bytes())
}
