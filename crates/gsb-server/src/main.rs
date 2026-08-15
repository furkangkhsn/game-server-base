//! gsb demo server binary.
//!
//! Usage: `gsb-server [config.toml]` (falls back to built-in defaults when
//! the file is missing; see `config.example.toml`).

use std::path::Path;

use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".into());
    let cfg = if Path::new(&config_path).exists() {
        info!(path = %config_path, "loading config");
        gsb_server::Config::from_file(&config_path)?
    } else {
        info!(path = %config_path, "config file not found; using defaults");
        gsb_server::Config::default()
    };

    let handle = gsb_server::start_server(cfg.clone()).await?;
    info!(addr = %handle.addr, tick_hz = cfg.tick_hz, rooms = cfg.room_count, "gsb server is up");

    tokio::signal::ctrl_c().await?;
    info!("ctrl-c received; shutting down");
    handle.stop().await;
    Ok(())
}
