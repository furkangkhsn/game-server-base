//! Server-only mode: run the server, export metrics, and print one
//! line — the addresses it bound (`SERVING`, [`announce`]).

use std::net::SocketAddr;
use std::time::Duration;

use gsb_core::metrics::MetricReport;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use super::*;
use crate::codec::*;
use crate::server::*;
use tokio::sync::mpsc;

mod announce;
pub(crate) use announce::*;

/// How long the served server waits, once stopped, for its metric
/// export to finish (BACKLOG F40). A reader that took the stream gets the
/// final report within milliseconds (it is in the channel when `stop`
/// returns; a local write of a few KiB). What the bound ends is an export
/// nobody took — a hand-run `--serve`, an orchestrator that died — still
/// waiting on `accept`, or one whose reader stopped reading: they held
/// the process forever.
const EXPORT_STOP_GRACE: Duration = Duration::from_secs(2);

/// The server process (`--serve`): the same server the in-process mode
/// runs, as a standalone process. Without `--metrics-listen`, reports go
/// to the `gsb-metric` log (RUST_LOG=info) like `gsb-server`; with it,
/// the collector's channel sink feeds one TCP connection — the
/// orchestrator — in the binary format above (the channel path is
/// preserved end-to-end; no report is parsed from stdout). Stdout
/// carries one line of its own, once every door is bound: the `SERVING`
/// line with the bound addresses ([`Serving`]).
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
        game: args.game.into(),
        ..Default::default()
    };
    args.transport.open_door(&mut cfg);
    // Capacity / lifecycle overrides (same semantics as in-process: an
    // explicit 0 means unlimited / disabled).
    apply_overrides(
        &mut cfg,
        &ServerOverrides {
            max_players: args.max_players,
            max_connections: args.max_connections,
            idle_timeout_secs: args.idle_timeout_secs,
            write_stall_secs: args.write_stall_secs,
            conn_out: args.conn_out,
            disconnect_grace_secs: args.disconnect_grace_secs,
            mmo_crystallize: args.mmo_crystallize,
            listen_backlog: args.listen_backlog,
            udp_recv_buffer: args.udp_recv_buffer,
        },
    );
    // Every door is bound before the SERVING line goes out (BACKLOG
    // F31): the metric listener first (a refused bind ends the child
    // before a server exists), then the server's own listeners. The line
    // names the bound addresses — port 0 included — so whoever started
    // this child (the orchestrator) never has to guess a free port.
    let metrics = match &args.metrics_listen {
        Some(listen) => {
            let listen: SocketAddr = listen.parse().expect("valid --metrics-listen HOST:PORT");
            match TcpListener::bind(listen).await {
                Ok(l) => Some(l),
                Err(e) => {
                    eprintln!("serve: metrics listen bind {listen} failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => None,
    };
    let (report_tx, report_rx) = match metrics {
        Some(_) => {
            let (tx, rx) = mpsc::unbounded_channel::<MetricReport>();
            (Some(tx), Some(rx))
        }
        None => (None, None),
    };
    let handle = match start_hosted(cfg, report_tx).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!(
                "serve: the server did not start: {}",
                gsb_server::error_chain(&e)
            );
            std::process::exit(1);
        }
    };
    let serving = Serving {
        addr: handle.addr,
        metrics: metrics
            .as_ref()
            .map(|l| l.local_addr().expect("a bound listener has an address")),
    };
    println!("{}", serving.line());
    let _ = std::io::Write::flush(&mut std::io::stdout());
    eprintln!(
        "serve: ready at {} (game={}, visibility={}, spawn_half={}, duration={}s; metric reports → {})",
        handle.addr,
        args.game,
        args.visibility,
        args.server_spawn_half,
        args.duration.as_secs(),
        serving.metrics.map_or_else(
            || "gsb-metric log, RUST_LOG=info".to_string(),
            |m| m.to_string()
        )
    );
    // The export task owns the report receiver and the metric listener
    // (its only awaited sources: the accept, then the channel). The main
    // task's awaited sources: the duration sleep, then stop().
    let export = match (metrics, report_rx) {
        (Some(listener), Some(rx)) => Some(tokio::spawn(metrics_export(rx, listener))),
        _ => None,
    };
    tokio::time::sleep(args.duration).await;
    // Clean stop: the collector emits its final report once every room
    // and connection has ended (F35), then drops the channel sender — the
    // export task drains the final report and exits, IF someone took the
    // stream. One awaited join under a deadline (the pump idiom), then
    // abort: nobody on the stream must not keep this process alive (F40).
    handle.stop().await;
    if let Some(mut export) = export {
        match tokio::time::timeout(EXPORT_STOP_GRACE, &mut export).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("serve: metrics export task failed: {e}"),
            Err(_) => {
                export.abort();
                eprintln!(
                    "serve: the metric stream was not taken (or not read) within \
                     {EXPORT_STOP_GRACE:?} of the stop; its reports are dropped"
                );
            }
        }
    }
}

/// Stream the collector's reports (channel data) to the single
/// orchestrator connection on `listener` (bound before the `SERVING`
/// line), one framed report at a time. Ends when the collector drops the
/// channel (after its final report) or the reader goes away; the caller
/// bounds how long it waits for that after the stop (`EXPORT_STOP_GRACE`
/// — this task alone would wait on `accept` forever if nobody came).
pub(crate) async fn metrics_export(
    mut rx: mpsc::UnboundedReceiver<MetricReport>,
    listener: TcpListener,
) {
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
