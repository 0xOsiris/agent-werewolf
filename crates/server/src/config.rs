//! Command-line configuration.

use std::{net::IpAddr, path::PathBuf};

use anyhow::ensure;
use clap::Parser;

use crate::engine::MIN_PLAYERS;

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
    /// Start automatically once this many players have joined.
    #[arg(long)]
    pub auto_start: Option<usize>,
    #[arg(long, default_value_t = 45)]
    pub turn_timeout_secs: u64,
    /// Turn timeout for humans playing in the browser, who read and type slower than agents.
    #[arg(long, default_value_t = 60)]
    pub human_turn_timeout_secs: u64,
    #[arg(long, default_value_t = 2)]
    pub speak_rounds: u32,
    /// Pause after each feed event so the audience can read it.
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
}

impl Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.min_players >= MIN_PLAYERS,
            "--min-players must be at least {MIN_PLAYERS}"
        );
        ensure!(
            self.max_players >= self.min_players,
            "--max-players must be >= --min-players"
        );
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
