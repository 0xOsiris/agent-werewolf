//! Command-line configuration.

use std::{net::IpAddr, num::NonZeroUsize, path::PathBuf, time::Duration};

use anyhow::ensure;
use clap::Parser;

use crate::engine::{MIN_PLAYERS, players_needed};

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
    /// Exact number of wolves; by default max(1, players / 4).
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
    /// Deal one villager the jester role, who wins by being voted out.
    #[arg(long, overrides_with = "no_jester")]
    pub jester: bool,
    /// Play without a jester (the default).
    #[arg(long, overrides_with = "jester")]
    pub no_jester: bool,
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
    /// Turn off the dead chat, so dead players get no more turns.
    #[arg(long)]
    pub no_dead_chat: bool,
    /// Key for the spectator page and admin endpoints. Random if omitted.
    #[arg(long)]
    pub spectator_key: Option<String>,
    /// Base URL players use to reach the server, e.g. `http://10.0.0.5:7777`. Detected if omitted.
    #[arg(long)]
    pub public_url: Option<String>,
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

    /// Off unless `--jester` was given last.
    pub fn jester(&self) -> bool {
        self.jester
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
        if let Some(wolves) = self.wolves {
            let need = players_needed(wolves.get(), self.sheriff(), self.cupid(), self.jester());
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
