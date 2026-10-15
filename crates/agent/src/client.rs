//! Client for the game server's HTTP API.

use crate::http::{HttpError, send_json};
use alloy_primitives::U256;
use anyhow::{Context, bail};
use reqwest::{Client, Url};
use std::time::Duration;
use werewolf_protocol::{ActRequest, ActResponse, Info, JoinRequest, JoinResponse, Poll};
use werewolf_verify::Transcript;

/// The server caps long-polls at 30s.
const POLL_WAIT_SECS: u64 = 25;
/// Longer than the long-poll window so an idle poll isn't mistaken for an outage.
const POLL_TIMEOUT: Duration = Duration::from_secs(35);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct GameClient {
    http: Client,
    base: String,
}

impl GameClient {
    pub fn new(base: &str) -> anyhow::Result<Self> {
        let url = Url::parse(base).with_context(|| format!("invalid server URL {base:?}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("server URL must start with http:// or https://, got {base:?}");
        }
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build game server HTTP client")?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn info(&self) -> Result<Info, HttpError> {
        send_json(self.http.get(format!("{}/v1/info", self.base))).await
    }

    pub async fn join(&self, request: &JoinRequest) -> Result<JoinResponse, HttpError> {
        send_json(
            self.http
                .post(format!("{}/v1/join", self.base))
                .json(request),
        )
        .await
    }

    pub async fn poll(&self, token: &str) -> Result<Poll, HttpError> {
        let url = format!("{}/v1/poll?wait={POLL_WAIT_SECS}", self.base);
        send_json(self.http.get(url).bearer_auth(token).timeout(POLL_TIMEOUT)).await
    }

    pub async fn act(&self, token: &str, request: &ActRequest) -> Result<ActResponse, HttpError> {
        let url = format!("{}/v1/act", self.base);
        send_json(self.http.post(url).bearer_auth(token).json(request)).await
    }

    /// A staked game's transcript; 404 until the game has ended.
    pub async fn transcript(&self, game_id: U256) -> Result<Transcript, HttpError> {
        let url = format!("{}/v1/transcript?game_id={game_id}", self.base);
        send_json(self.http.get(url)).await
    }
}
