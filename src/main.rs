mod ari_control;
mod ari_external_media;
mod bridge;
mod config;
mod dial_string;
mod session;
mod slmodem;

use anyhow::Result;
use ari_external_media::ExternalMediaRequest;
use clap::Parser;
use config::{Cli, Config};
use dial_string::DialString;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let cli = Cli::parse();
    let mut cfg: Config = cli.into();
    let dial = DialString::parse(&cfg.dial_string)?;
    cfg.dial_string = dial.as_str().to_string();
    let external_media = ExternalMediaRequest::from_env_defaults();
    let external_media_pairs = external_media.query_pairs();

    tracing::info!(
        dial = %cfg.dial_string,
        media = %cfg.media_addr,
        fd = cfg.socket_fd,
        ari_endpoint = ExternalMediaRequest::endpoint_path(),
        ari_transport = external_media.transport.as_str(),
        ari_encapsulation = external_media.encapsulation.as_str(),
        ari_query_pairs = ?external_media_pairs,
        "event=startup"
    );

    bridge::run(cfg, external_media).await
}

fn init_logging() {
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "slmodem_asterisk_bridge=info,info".to_string());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
