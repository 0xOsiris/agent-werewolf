//! Command-line configuration.

use std::{net::IpAddr, num::NonZeroUsize, path::PathBuf, time::Duration};

use anyhow::{Context, ensure};
use clap::{Parser, ValueEnum};
use url::Url;
use werewolf_engine::{MIN_PLAYERS, players_needed};
use werewolf_verify::alloy_primitives::{Address, aliases::U96};

/// Wolves in a staked game when `--wolves` is not given; staked rules must fix the count.
pub const STAKED_WOLVES: usize = 2;
/// The escrow's `MAX_SEATS`.
const MAX_STAKED_SEATS: usize = 12;

#[derive(Parser, Debug, Clone)]
#[command(name = "werewolf-server", about = "Agent Werewolf game server")]
pub struct Config {
    #[arg(long, default_value = "0.0.0.0")]
    pub bind: IpAddr,
    #[arg(long, default_value_t = 7777)]
    pub port: u16,
    #[arg(long, default_value_t = MIN_PLAYERS)]
    pub min_players: usize,
    #[arg(long, default_value_t = 12)]
    pub max_players: usize,
    /// Exact number of wolves; by default max(1, players / 4), or 2 in staked games.
    #[arg(long)]
    pub wolves: Option<NonZeroUsize>,
    /// Deal one villager the sheriff role (the default).
    #[arg(long, overrides_with = "no_sheriff")]
    pub sheriff: bool,
    /// Play without a sheriff.
    #[arg(long, overrides_with = "sheriff")]
    pub no_sheriff: bool,
    /// Deal one villager the cupid role (the default).
    #[arg(long, overrides_with = "no_cupid")]
    pub cupid: bool,
    /// Play without a cupid.
    #[arg(long, overrides_with = "cupid")]
    pub no_cupid: bool,
    /// Start automatically once this many players have joined.
    #[arg(long)]
    pub auto_start: Option<usize>,
    #[arg(long, default_value_t = 45)]
    pub turn_timeout_secs: u64,
    /// Turn timeout for humans playing in the browser, who read and type slower than agents.
    #[arg(long, default_value_t = 60)]
    pub human_turn_timeout_secs: u64,
    /// Length of the day's open discussion. Seconds; fractions allowed.
    #[arg(long, default_value = "120", value_parser = parse_secs)]
    pub discussion_secs: Duration,
    /// Length of the night's wolf chat, held only with 2+ living wolves.
    #[arg(long, default_value = "40", value_parser = parse_secs)]
    pub wolf_chat_secs: Duration,
    /// Minimum gap between a human's chat answers.
    #[arg(long, default_value = "8", value_parser = parse_secs)]
    pub chat_cooldown_secs: Duration,
    /// Non-empty messages each human may post per chat window.
    #[arg(long, default_value_t = 6)]
    pub max_chat_messages: u32,
    /// Minimum gap between an agent's chat answers.
    #[arg(long, default_value = "15", value_parser = parse_secs)]
    pub ai_chat_cooldown_secs: Duration,
    /// Agents wait an extra random 0..this before each chat turn, so replies spread out.
    #[arg(long, default_value = "10", value_parser = parse_secs)]
    pub ai_chat_jitter_secs: Duration,
    /// Non-empty messages each agent may post per chat window.
    #[arg(long, default_value_t = 3)]
    pub ai_max_chat_messages: u32,
    /// Close a chat early once nobody has posted for this long and everyone has answered.
    #[arg(long, default_value = "20", value_parser = parse_secs)]
    pub quiet_secs: Duration,
    /// Pause after each non-chat feed event so the audience can read it.
    #[arg(long, default_value_t = 1500)]
    pub pace_ms: u64,
    /// RNG seed for the first game; later games use seed+1, seed+2, ... Random if omitted.
    #[arg(long)]
    pub seed: Option<u64>,
    /// Directory for per-game JSONL logs and summaries.
    #[arg(long, default_value = "./games")]
    pub log_dir: PathBuf,
    /// Don't advertise the server over mDNS.
    #[arg(long)]
    pub no_mdns: bool,
    /// Key for the spectator page and admin endpoints. Random if omitted.
    #[arg(long)]
    pub spectator_key: Option<String>,
    /// Base URL players use to reach the server, e.g. `http://10.0.0.5:7777`. Detected if omitted.
    #[arg(long)]
    pub public_url: Option<String>,
    /// Run staked games with this buy-in, in the token's base units. See docs/SETTLEMENT.md.
    #[arg(long)]
    pub stake: Option<U96>,
    /// Chain id the RPC must serve; staked games only.
    #[arg(long)]
    pub chain_id: Option<u64>,
    /// JSON-RPC endpoint the operator uses; staked games only. Never logged.
    #[arg(long)]
    pub rpc_url: Option<Url>,
    /// RPC endpoint offered to wallets and agents in `/v1/info`; none if omitted, since
    /// `--rpc-url` may carry a provider key.
    #[arg(long)]
    pub public_rpc_url: Option<Url>,
    /// Escrow contract address; staked games only.
    #[arg(long)]
    pub escrow: Option<Address>,
    /// ERC-20 token of the buy-in; staked games only.
    #[arg(long)]
    pub token: Option<Address>,
    /// File holding the operator's hex secp256k1 key, which signs receipts and transactions.
    #[arg(long)]
    pub operator_key_file: Option<PathBuf>,
    /// Seconds from `create` until joins close.
    #[arg(long, default_value_t = 900)]
    pub join_window_secs: u64,
    /// Seconds after joins close by which the result must be proposed.
    #[arg(long, default_value_t = 5400)]
    pub play_window_secs: u64,
    /// The `werewolf-prove` binary run when a result is challenged.
    #[arg(long, default_value = "werewolf-prove")]
    pub prover_bin: PathBuf,
    #[arg(long, value_enum, default_value_t = ProverMode::Mock)]
    pub prover_mode: ProverMode,
    /// How often settlement checks deadlines and retries. Seconds; fractions allowed.
    #[arg(long, default_value = "15", value_parser = parse_secs)]
    pub settle_poll_secs: Duration,
}

/// `werewolf-prove --mode`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProverMode {
    Execute,
    Mock,
    Cpu,
    Network,
}

impl ProverMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Mock => "mock",
            Self::Cpu => "cpu",
            Self::Network => "network",
        }
    }
}

/// The staked-mode flags, all present.
#[derive(Clone, Debug)]
pub struct StakeConfig {
    pub stake: U96,
    pub chain_id: u64,
    pub rpc_url: Url,
    pub public_rpc_url: Option<Url>,
    pub escrow: Address,
    pub token: Address,
    pub operator_key_file: PathBuf,
    pub join_window_secs: u64,
    pub play_window_secs: u64,
    pub prover_bin: PathBuf,
    pub prover_mode: ProverMode,
    pub settle_poll: Duration,
    /// Secrets and transcripts, under `--log-dir`.
    pub dir: PathBuf,
}

impl Config {
    /// On unless `--no-sheriff` was given last.
    pub fn sheriff(&self) -> bool {
        !self.no_sheriff
    }

    /// On unless `--no-cupid` was given last.
    pub fn cupid(&self) -> bool {
        !self.no_cupid
    }

    /// Whether games are staked (`--stake`); the one switch for staked-only behaviour.
    pub fn is_staked(&self) -> bool {
        self.stake.is_some()
    }

    /// The wolf count for new games: `--wolves`, or the staked default.
    pub fn wolves(&self) -> Option<NonZeroUsize> {
        self.wolves.or_else(|| {
            self.is_staked()
                .then(|| NonZeroUsize::new(STAKED_WOLVES))
                .flatten()
        })
    }

    /// The staked-mode settings, or `None` for free games.
    pub fn stake_config(&self) -> anyhow::Result<Option<StakeConfig>> {
        let Some(stake) = self.stake else {
            return Ok(None);
        };
        let need = |flag: &str| format!("--stake needs {flag}");
        Ok(Some(StakeConfig {
            stake,
            chain_id: self.chain_id.with_context(|| need("--chain-id"))?,
            rpc_url: self.rpc_url.clone().with_context(|| need("--rpc-url"))?,
            public_rpc_url: self.public_rpc_url.clone(),
            escrow: self.escrow.with_context(|| need("--escrow"))?,
            token: self.token.with_context(|| need("--token"))?,
            operator_key_file: self
                .operator_key_file
                .clone()
                .with_context(|| need("--operator-key-file"))?,
            join_window_secs: self.join_window_secs,
            play_window_secs: self.play_window_secs,
            prover_bin: self.prover_bin.clone(),
            prover_mode: self.prover_mode,
            settle_poll: self.settle_poll_secs,
            dir: self.log_dir.join("staked"),
        }))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.min_players >= MIN_PLAYERS,
            "--min-players must be at least {MIN_PLAYERS}"
        );
        ensure!(
            self.max_players >= self.min_players,
            "--max-players must be >= --min-players"
        );
        if let Some(wolves) = self.wolves() {
            let need = players_needed(wolves.get(), self.sheriff(), self.cupid());
            ensure!(
                need <= self.max_players,
                "--wolves {wolves} needs at least {need} players, but --max-players is {}",
                self.max_players
            );
        }
        if let Some(n) = self.auto_start {
            ensure!(
                (self.min_players..=self.max_players).contains(&n),
                "--auto-start must be between --min-players and --max-players"
            );
        }
        ensure!(
            self.turn_timeout_secs > 0 && self.human_turn_timeout_secs > 0,
            "turn timeouts must be positive"
        );
        ensure!(
            !self.discussion_secs.is_zero()
                && !self.wolf_chat_secs.is_zero()
                && !self.quiet_secs.is_zero(),
            "--discussion-secs, --wolf-chat-secs, and --quiet-secs must be positive"
        );
        ensure!(
            self.max_chat_messages > 0 && self.ai_max_chat_messages > 0,
            "--max-chat-messages and --ai-max-chat-messages must be positive"
        );
        if let Some(url) = &self.public_url {
            // The URL ends up in JSON and a QR code, so keep it a plain absolute http(s) URL.
            ensure!(
                (url.starts_with("http://") || url.starts_with("https://"))
                    && !url.chars().any(|c| c.is_whitespace() || c.is_control()),
                "--public-url must be an http:// or https:// URL"
            );
        }
        if let Some(stake) = self.stake_config()? {
            ensure!(!stake.stake.is_zero(), "--stake must be positive");
            ensure!(
                self.max_players <= MAX_STAKED_SEATS,
                "staked games have at most {MAX_STAKED_SEATS} seats; lower --max-players"
            );
            ensure!(
                stake.join_window_secs > 0 && stake.play_window_secs > 0,
                "--join-window-secs and --play-window-secs must be positive"
            );
            ensure!(
                !stake.settle_poll.is_zero(),
                "--settle-poll-secs must be positive"
            );
        }
        if let Some(key) = &self.spectator_key {
            // The key travels in a URL query and an HTTP header.
            ensure!(
                !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "--spectator-key must be non-empty and use only [A-Za-z0-9_-]"
            );
        }
        Ok(())
    }
}

/// Seconds as a decimal, so tests can use sub-second chat windows.
fn parse_secs(value: &str) -> Result<Duration, String> {
    let secs: f64 = value.parse().map_err(|err| format!("{err}"))?;
    Duration::try_from_secs_f64(secs).map_err(|err| err.to_string())
}
