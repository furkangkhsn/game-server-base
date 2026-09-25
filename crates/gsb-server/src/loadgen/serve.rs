//! Server-only mode: run the server, export metrics, print nothing.

use std::net::SocketAddr;

use gsb_core::metrics::MetricReport;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use super::*;
use crate::codec::*;
use crate::server::*;
use tokio::sync::mpsc;

/// The server process (`--serve`): the same server the in-process mode
/// runs, as a standalone process. Without `--metrics-listen`, reports go
/// to the `gsb-metric` log (RUST_LOG=info) like `gsb-server`; with it,
/// the collector's channel sink feeds one TCP connection — the
/// orchestrator — in the binary format above (the channel path is
/// preserved end-to-end; nothing is parsed from stdout).
pub(crate) async fn serve(args: Args) {
    init_tracing();
    let mut cfg = gsb_server::Config {
        bind: args.bind.clone(),
        room_count: 1,
        visibility: args.visibility,
        topology: args.topology,
        shard_count: args.shard_count,
        aoi_cell_size: args.cell_size,
        team_vision_radius: args.vision_radius,
        max_snapshot_bytes: args.max_snapshot_bytes,
        spawn_half_size: args.server_spawn_half,
        transport: args.transport,
        game: args.game.into(),
        ..Default::default()
    };
    // Capacity / lifecycle overrides (same semantics as in-process: an
    // explicit 0 means unlimited / disabled).
    apply_overrides(
        &mut cfg,
        &ServerOverrides {
            max_players: args.max_players,
            max_connections: args.max_connections,
            idle_timeout_secs: args.idle_timeout_secs,
            write_stall_secs: args.write_stall_secs,
            disconnect_grace_secs: args.disconnect_grace_secs,
            mmo_crystallize: args.mmo_crystallize,
        },
    );
    match args.metrics_listen {
        Some(listen) => {
            let listen: SocketAddr = listen.parse().expect("valid --metrics-listen HOST:PORT");
            let (tx, rx) = mpsc::unbounded_channel::<MetricReport>();
            let handle = gsb_server::start_server_metrics(cfg, tx)
                .await
                .expect("server starts");
            eprintln!(
                "serve: ready at {} (game={}, visibility={}, spawn_half={}, duration={}s; metric reports → {})",
                handle.addr,
                args.game,
                args.visibility,
                args.server_spawn_half,
                args.duration.as_secs(),
                listen
            );
            // The export task owns the report receiver (its only awaited
            // sources: the accept, then the channel). The main task's
            // awaited sources: the duration sleep, then stop().
            let export = tokio::spawn(metrics_export(rx, listen));
            tokio::time::sleep(args.duration).await;
            // Clean stop: the collector emits its final report when the
            // ticker's broadcast closes, then drops the channel sender —
            // the export task drains the final report and exits.
            handle.stop().await;
            if let Err(e) = export.await {
                eprintln!("serve: metrics export task failed: {e}");
            }
        }
        None => {
            let handle = gsb_server::start_server(cfg).await.expect("server starts");
            eprintln!(
                "serve: ready at {} (game={}, visibility={}, duration={}s; metric reports → gsb-metric log, RUST_LOG=info)",
                handle.addr,
                args.game,
                args.visibility,
                args.duration.as_secs()
            );
            tokio::time::sleep(args.duration).await;
            handle.stop().await;
        }
    }
}

/// Stream the collector's reports (channel data) to the single
/// orchestrator connection, one framed report at a time.
pub(crate) async fn metrics_export(
    mut rx: mpsc::UnboundedReceiver<MetricReport>,
    listen: SocketAddr,
) {
    let listener = match TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("serve: metrics listen bind failed: {e}");
            return;
        }
    };
    eprintln!("serve: metrics listening at {listen} (one connection)");
    let (mut stream, _peer) = match listener.accept().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("serve: metrics accept failed (no orchestrator?): {e}");
            return;
        }
    };
    while let Some(report) = rx.recv().await {
        let frame = encode_report(&report);
        if stream.write_all(&frame).await.is_err() || stream.flush().await.is_err() {
            break; // orchestrator went away
        }
    }
}

/// Allocate a free loopback port (bind-port-0, take the number, close).
/// The race window is microseconds and the consumer (the spawned server)
/// binds immediately — fine for a load tool on loopback.
pub(crate) async fn alloc_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let p = l.local_addr().expect("local addr").port();
    drop(l);
    p
}
