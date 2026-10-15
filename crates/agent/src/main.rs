//! Reference AI player for Agent Werewolf. See `PROTOCOL.md`.

mod backoff;
mod brain;
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
use clap::Parser;
use client::GameClient;
use game::Player;
use llm::OpenRouter;
use mock::MockBrain;
use std::time::Duration;
use tracing::{Instrument, info_span};
use tracing_subscriber::EnvFilter;
use werewolf_protocol::{MAX_MODEL_LEN, MAX_NAME_LEN};

#[derive(Parser, Debug)]
#[command(version, about = "AI player for Agent Werewolf, powered by OpenRouter")]
struct Args {
    /// Player name shown to everyone.
    #[arg(long)]
    name: String,
    /// OpenRouter model id, e.g. `anthropic/claude-sonnet-4.5`.
    #[arg(long, required_unless_present = "mock")]
    model: Option<String>,
    /// Server base URL; skips mDNS discovery.
    #[arg(long)]
    server: Option<String>,
    /// Extra personality or strategy appended to the system prompt.
    #[arg(long)]
    persona: Option<String>,
    /// Play random valid moves without an LLM.
    #[arg(long)]
    mock: bool,
    /// RNG seed for --mock.
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

    let name = args.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        bail!("--name must be 1 to {MAX_NAME_LEN} characters");
    }
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
        Brain::Llm(LlmBrain::new(llm, args.persona))
    };

    let span = info_span!("agent", player = %name, model = %model);
    async move {
        let server = match args.server {
            Some(server) => server,
            None => discovery::discover(Duration::from_secs(args.discover_timeout_secs)).await?,
        };
        let client = GameClient::new(&server)?;
        Player::new(client, brain, name, model, args.games)
            .run()
            .await
    }
    .instrument(span)
    .await
}
