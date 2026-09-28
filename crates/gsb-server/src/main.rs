//! gsb demo server binary.
//!
//! Usage: `gsb-server [config.toml]` (falls back to built-in defaults when
//! the file is missing; see `config.example.toml`).
//!
//! A refused startup — a config file that does not load, a key no
//! server can run with, a door that does not bind — is one message on
//! stderr (`gsb-server: <the error>`, its `Display` and causes through
//! [`gsb_server::error_chain`]) and exit status 1 (BACKLOG F63).

use std::path::Path;
use std::process::ExitCode;

use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    // Not `main -> Result`: the standard library prints a returned error
    // with `Debug` (`Error: Parse { … }` — no line number, and a parse
    // error's `Debug` carries the whole file).
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gsb-server: {}", gsb_server::error_chain(&*e));
            ExitCode::FAILURE
        }
    }
}

/// Load the config, start the server, and serve until a shutdown signal.
async fn run() -> Result<(), Box<dyn std::error::Error>> {
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
    info!(
        addr = %handle.addr,
        http = ?handle.http_addr,
        tick_hz = cfg.tick_hz,
        rooms = cfg.room_count,
        "gsb server is up"
    );

    // Wait for a shutdown signal without multiplexing: one watcher task per
    // signal, each reporting through the bounded channel, and `main` awaits a
    // single receive (the project idiom for waiting on several sources).
    let (signal_tx, mut signal_rx) = tokio::sync::mpsc::channel::<&'static str>(1);
    tokio::spawn({
        let signal_tx = signal_tx.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = signal_tx.send("ctrl-c").await;
        }
    });
    #[cfg(unix)]
    tokio::spawn({
        let signal_tx = signal_tx.clone();
        async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
            term.recv().await;
            let _ = signal_tx.send("SIGTERM").await;
        }
    });
    drop(signal_tx);

    let sig = signal_rx
        .recv()
        .await
        .expect("shutdown signal watcher exited without reporting");
    info!(signal = %sig, "shutting down");
    handle.stop().await;
    Ok(())
}
