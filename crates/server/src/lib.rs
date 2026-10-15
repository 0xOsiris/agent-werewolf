//! Agent Werewolf game server: player HTTP API, game driver, and spectator stream.

mod assets;
mod chat;
mod config;
mod driver;
mod engine;
mod error;
mod game_log;
mod http;
mod mdns;
mod sanitize;
mod site;
mod spectator;
mod state;

use anyhow::Context;
use rand::{TryRng, rngs::SysRng};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use config::Config;

/// Serves on `listener` until `shutdown` fires. Prints the spectator URL first.
pub async fn serve(
    config: Config,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    config.validate()?;
    let addr = listener.local_addr().context("reading listen address")?;
    let key = match &config.spectator_key {
        Some(key) => key.clone(),
        None => state::random_hex(8).context("generating spectator key")?,
    };
    let seed = match config.seed {
        Some(seed) => seed,
        None => SysRng.try_next_u64().context("generating seed")?,
    };
    let site = site::Site::new(&config, addr)?;
    println!("spectate: {}/?key={key}  (seed {seed})", site.base_url);

    let mdns = if config.no_mdns {
        None
    } else {
        mdns::advertise(addr.port())
    };
    let app = state::AppState::new(config, key, seed, site, shutdown.clone());
    let served = axum::serve(listener, http::router(app))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("HTTP server failed");
    if let Some(mdns) = mdns {
        mdns.withdraw().await;
    }
    served
}
