use std::net::SocketAddr;

use anyhow::Context;
use clap::Parser;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing_subscriber::EnvFilter;
use werewolf_server::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::parse();
    let addr = SocketAddr::new(config.bind, config.port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    let shutdown = CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => shutdown.cancel(),
                Err(err) => error!(%err, "cannot listen for Ctrl-C; stop the server with a signal"),
            }
        }
    });
    werewolf_server::serve(config, listener, shutdown).await
}
