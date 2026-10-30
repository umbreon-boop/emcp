use std::net::SocketAddr;

use anyhow::Context;
use emcp::{bridge::Bridge, server, ws};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("emcp=info,rmcp=warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let address = std::env::var("EMCP_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".to_string());
    let address: SocketAddr = address
        .parse()
        .with_context(|| format!("invalid EMCP_ADDR: {address}"))?;

    let bridge = Bridge::new(tokio::runtime::Handle::current());
    let local_address = ws::spawn(address, bridge.clone())?;

    tracing::info!(%local_address, "emcp websocket bridge listening");
    tracing::info!("inject runtime.lua into the executor to connect");

    server::serve(bridge).await
}
