//! Reference AI player for Agent Werewolf. See `PROTOCOL.md`.

mod backoff;
mod brain;
mod cast;
mod chat;
mod client;
mod discovery;
mod game;
mod http;
mod llm;
mod mock;
mod prompt;
mod reply;

use anyhow::{Context, bail};
use brain::{Brain, LlmBrain};
use cast::Character;
use clap::Parser;
use client::GameClient;
use game::Player;
use llm::OpenRouter;
use mock::MockBrain;
use rand::{SeedableRng, rngs::StdRng};
use std::{path::PathBuf, time::Duration};
use tracing::{Instrument, info_span};
use tracing_subscriber::EnvFilter;
use werewolf_protocol::MAX_MODEL_LEN;

#[derive(Parser, Debug)]
#[command(version, about = "AI player for Agent Werewolf, powered by OpenRouter")]
struct Args {
    /// Player name shown to everyone.
    #[arg(long, required_unless_present = "cast")]
    name: Option<String>,
    /// OpenRouter model id, e.g. `anthropic/claude-sonnet-4.5`.
    #[arg(long, required_unless_present = "mock")]
    model: Option<String>,
    /// Server base URL; skips mDNS discovery.
    #[arg(long)]
    server: Option<String>,
    /// Extra personality or strategy appended to the system prompt.
    #[arg(long)]
    persona: Option<String>,
    /// File of `Name|persona` lines, re-read every game; each game plays a random one. Replaces --name and --persona.
    #[arg(long, conflicts_with_all = ["name", "persona"])]
    cast: Option<PathBuf>,
    /// Play random valid moves without an LLM.
    #[arg(long)]
    mock: bool,
    /// RNG seed for --mock and the --cast draw.
    #[arg(long)]
    seed: Option<u64>,
    /// Games to play before exiting; 0 plays forever.
    #[arg(long, default_value_t = 1)]
    games: u32,
    #[arg(long, default_value_t = 10)]
    discover_timeout_secs: u64,
    /// Upper bound on a single LLM request.
    #[arg(long, default_value_t = 60)]
    llm_timeout_secs: u64,
    #[arg(long, default_value = "https://openrouter.ai/api/v1")]
    openrouter_base_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,werewolf_agent=info")),
        )
        .init();
    let args = Args::parse();

    let cast_file = args.cast.clone();
    let cast = match (&args.cast, &args.name) {
        (Some(path), _) => cast::load(path)?,
        (None, Some(name)) => {
            vec![Character::new(name, args.persona.as_deref()).context("--name")?]
        }
        (None, None) => bail!("--name is required unless --cast is set"),
    };
    let model = match (&args.model, args.mock) {
        (_, true) => "mock".to_owned(),
        (Some(model), false) => model.trim().to_owned(),
        (None, false) => bail!("--model is required unless --mock is set"),
    };
    if model.is_empty() || model.chars().count() > MAX_MODEL_LEN {
        bail!("--model must be 1 to {MAX_MODEL_LEN} characters");
    }

    let brain = if args.mock {
        Brain::Mock(MockBrain::new(args.seed))
    } else {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .context(
                "OPENROUTER_API_KEY is not set; export it, or pass --mock to play without an LLM",
            )?;
        let llm = OpenRouter::new(
            &args.openrouter_base_url,
            api_key,
            model.clone(),
            Duration::from_secs(args.llm_timeout_secs),
        )?;
        Brain::Llm(LlmBrain::new(llm))
    };
    let rng = args.seed.map_or_else(rand::make_rng, StdRng::seed_from_u64);

    let span = info_span!("agent", model = %model);
    async move {
        let server = match args.server {
            Some(server) => server,
            None => discovery::discover(Duration::from_secs(args.discover_timeout_secs)).await?,
        };
        let client = GameClient::new(&server)?;
        Player::new(client, brain, cast, cast_file, model, args.games, rng)
            .run()
            .await
    }
    .instrument(span)
    .await
}
