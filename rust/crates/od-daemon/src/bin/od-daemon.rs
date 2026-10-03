//! `od-daemon` — run the OpenDesign daemon standalone (CLI/dev parity with
//! the TypeScript `od` daemon entry).

use od_daemon::{DaemonConfig, RunningDaemon};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = DaemonConfig::from_env()?;
    let daemon = RunningDaemon::start(config).await?;
    eprintln!("od-daemon listening on {}", daemon.url());
    tokio::signal::ctrl_c().await?;
    daemon.shutdown().await;
    Ok(())
}
