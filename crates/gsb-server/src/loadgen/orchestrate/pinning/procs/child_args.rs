//! The children's command lines — the server child's and each client
//! child's — built as plain data so what each child is told can be
//! checked without spawning one.

use super::*;

/// Whether the run drives the 2D demo: its own flags (`bot::is_demo_only`)
/// are forwarded only then — the other games' children refuse them.
fn is_demo(args: &Args) -> bool {
    args.game == gsb_server::games::demo::DemoModule::NAME
}

/// The `--workers` a child is told, if any (B37): under `--pin` the size
/// of its core set; unpinned, the operator's explicit `--workers N`;
/// otherwise none at all, so the child runs on its own runtime default
/// (`available_parallelism`, `main.rs`) — the one default the plain and
/// `--serve` modes have too.
fn child_workers(args: &Args, pinned_cores: Option<usize>) -> Option<usize> {
    // `workers = 0` is "not asked" (the child's `main` reads it the same
    // way); forwarding `max(1)` of it pinned every unpinned child to ONE
    // worker while this said "runtime default".
    pinned_cores.or((args.workers > 0).then_some(args.workers))
}

/// Push `--workers N` when the child is told a worker count
/// ([`child_workers`]); nothing otherwise.
fn push_workers(argv: &mut Vec<String>, args: &Args, pinned_cores: Option<usize>) {
    if let Some(n) = child_workers(args, pinned_cores) {
        argv.push("--workers".into());
        argv.push(n.to_string());
    }
}

/// The argument vector for the server child: the served server with its
/// game door and its metric stream both on port 0 — it binds them and
/// reports the ports it got on its `SERVING` line (BACKLOG F31; this
/// process picks no port) — and the runtime workers [`child_workers`]
/// picks (`pinned_cores`: the size of its core set under `--pin`). It outlives the clients by 3 s (the
/// clean stop happens after the clients left, so the final report
/// windows cover the leave flushes).
pub(super) fn server_args(args: &Args, pinned_cores: Option<usize>) -> Vec<String> {
    let mut sargs: Vec<String> = vec![
        "--serve".into(),
        "--game".into(),
        args.game.into(),
        "--bind".into(),
        "127.0.0.1:0".into(),
        "--metrics-listen".into(),
        "127.0.0.1:0".into(),
    ];
    if is_demo(args) {
        sargs.extend([
            "--visibility".into(),
            args.visibility.to_string(),
            "--shard-count".into(),
            args.shard_count.to_string(),
            "--cell-size".into(),
            args.cell_size.to_string(),
            "--vision-radius".into(),
            args.vision_radius.to_string(),
        ]);
    }
    sargs.extend([
        "--max-snapshot-bytes".into(),
        args.max_snapshot_bytes.to_string(),
    ]);
    if is_demo(args) {
        sargs.extend([
            "--spawn-half-size".into(),
            args.server_spawn_half.to_string(),
        ]);
    }
    sargs.extend([
        "--transport".into(),
        args.transport.to_string(),
        "--duration".into(),
        (args.duration + Duration::from_secs(3))
            .as_secs()
            .to_string(),
    ]);
    push_workers(&mut sargs, args, pinned_cores);
    // The explicit topology axis is forwarded ONLY when the operator set
    // it: an explicit key wins over the legacy derivation at resolve time,
    // so unconditionally forwarding "single" would silently flatten a
    // legacy `--visibility sharded` run into one whole-world room.
    if let Some(t) = args.topology {
        sargs.push("--topology".into());
        sargs.push(t.to_string());
    }
    // Capacity / lifecycle guards (forwarded only when the operator
    // chose them; the served server keeps its config defaults otherwise).
    if let Some(n) = args.max_players {
        sargs.push("--max-players".into());
        sargs.push(n.to_string());
    }
    if let Some(n) = args.max_connections {
        sargs.push("--max-connections".into());
        sargs.push(n.to_string());
    }
    if let Some(s) = args.idle_timeout_secs {
        sargs.push("--idle-timeout-secs".into());
        sargs.push(s.to_string());
    }
    if let Some(s) = args.write_stall_secs {
        sargs.push("--write-stall-secs".into());
        sargs.push(s.to_string());
    }
    if let Some(n) = args.conn_out {
        sargs.push("--conn-out".into());
        sargs.push(n.to_string());
    }
    if let Some(n) = args.listen_backlog {
        sargs.push("--listen-backlog".into());
        sargs.push(n.to_string());
    }
    if let Some(n) = args.udp_recv_buffer {
        sargs.push("--udp-recv-buffer".into());
        sargs.push(n.to_string());
    }
    if let Some(f) = args.disconnect_grace_secs {
        sargs.push("--disconnect-grace-secs".into());
        sargs.push(f.to_string());
    }
    if let Some(on) = args.mmo_crystallize {
        sargs.push("--mmo-crystallize".into());
        sargs.push(if on { "on" } else { "off" }.into());
    }
    sargs
}

/// The argument vector for one client child: `count` clients starting at
/// global id `offset`, aimed at the served server's game door `server`
/// (the address its `SERVING` line reported), with the runtime workers
/// [`child_workers`] picks. Every knob the CLIENT side reads must be
/// forwarded here — a knob the orchestrator forwards only to the server
/// leaves the two processes disagreeing about the run.
pub(super) fn client_args(
    args: &Args,
    count: u64,
    offset: u64,
    server: SocketAddr,
    pinned_cores: Option<usize>,
) -> Vec<String> {
    let mut cargs = vec![
        count.to_string(),
        "--game".into(),
        args.game.into(),
        "--addr".into(),
        server.to_string(),
        "--offset".into(),
        offset.to_string(),
        "--duration".into(),
        args.duration.as_secs().to_string(),
        "--move-ms".into(),
        args.move_ms.as_millis().to_string(),
        "--room".into(),
        args.room.to_string(),
        "--stagger-ms".into(),
        args.stagger_ms.to_string(),
    ];
    if is_demo(args) {
        cargs.extend([
            "--profile".into(),
            match args.profile {
                Profile::Ring => "ring".into(),
                Profile::Spread => "spread".into(),
                Profile::Still => "still".into(),
            },
            "--still-frac".into(),
            args.still_frac.to_string(),
            "--spawn-half-size".into(),
            args.spawn_half.to_string(),
            // The client view recomputes spatial cells from wire
            // coordinates (`CellExit` eviction), so it needs the server's
            // cell size.
            "--cell-size".into(),
            args.cell_size.to_string(),
        ]);
    }
    cargs.extend(["--transport".into(), args.transport.to_string()]);
    push_workers(&mut cargs, args, pinned_cores);
    // The flood client (by global id) belongs to exactly one child:
    // forward the flag only to the child whose id range contains it.
    if let Some(k) = args.flood_id
        && offset <= k
        && k < offset + count
    {
        cargs.push("--flood-id".into());
        cargs.push(k.to_string());
    }
    if let Some(c) = args.churn_secs {
        cargs.push("--churn-secs".into());
        cargs.push(c.to_string());
    }
    if let Some(ms) = args.stall_ms {
        cargs.push("--stall-ms".into());
        cargs.push(ms.to_string());
        cargs.push("--stall-every-ms".into());
        cargs.push(args.stall_every_ms.to_string());
    }
    if args.churn_cycles != 0 {
        cargs.push("--churn-cycles".into());
        cargs.push(args.churn_cycles.to_string());
    }
    if let Some(f) = args.disconnect_grace_secs {
        cargs.push("--disconnect-grace-secs".into());
        cargs.push(f.to_string());
    }
    if args.mmo_duel_frac > 0.0 {
        cargs.push("--mmo-duel-frac".into());
        cargs.push(args.mmo_duel_frac.to_string());
    }
    cargs
}

#[cfg(test)]
mod tests;
