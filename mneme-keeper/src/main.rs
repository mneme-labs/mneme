mod keeper;

use clap::Parser;
use mneme_common::MnemeConfig;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "mneme-keeper", about = "MnemeCache keeper node (Hypnos)")]
struct Cli {
    #[arg(short, long, default_value = "/etc/mneme/mneme.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Must be called before any rustls usage when multiple providers are compiled in.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mneme_keeper=info,mneme_common=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = MnemeConfig::from_file(&cli.config).unwrap_or_else(|e| {
        tracing::warn!("Cannot load config from {}: {e} — using defaults", cli.config);
        MnemeConfig::default()
    });

    info!(
        node_id = %config.node.node_id,
        replication_addr = %config.replication_addr(),
        "Starting Hypnos (mneme-keeper)"
    );

    keeper::hypnos::Hypnos::start(config).await
}
