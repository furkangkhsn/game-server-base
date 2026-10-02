//! The whole-line checks of the command line: what no single flag
//! decides (a game's flags, mode conflicts, the connect schedule, TLS).
//! Also where a flag's default depends on another flag.

use super::*;

/// Finish `args` (defaults that depend on other flags) and refuse a line
/// that cannot run. `game_flags`: the game-specific flags written.
pub(super) fn check(args: &mut Args, game_flags: &[String]) -> Result<(), CliError> {
    // The spread profile's default map (a "wide map": 2000×2000) — the
    // ring profile stays on the historical 100×100 arena, so the default
    // run is bit-identical to the historical one.
    if args.spawn_half == 50.0 && args.profile == Profile::Spread {
        args.spawn_half = 1000.0;
        args.server_spawn_half = 1000.0;
    }
    // The socket addresses the run parses later (`run`, `serve`): a bad
    // one is refused here, naming its flag.
    for (flag, value) in [
        ("--addr", &args.addr),
        ("--metrics-listen", &args.metrics_listen),
    ] {
        if let Some(v) = value
            && v.parse::<std::net::SocketAddr>().is_err()
        {
            return Err(CliError(format!(
                "{flag}: expected HOST:PORT (an IP address and a port), got `{v}`"
            )));
        }
    }
    // The pinned key of an EXTERNAL sealed rUDP server: the in-process
    // and served servers draw their own (their clients learn it there).
    refuse_unless(
        args.udp_server_key.is_none()
            || (args.addr.is_some()
                && args.transport == crate::Transport::Udp
                && args.udp_security == gsb_server::UdpSecurityKind::Sealed),
        "--udp-server-key pins an external sealed rUDP server: with --addr and \
         --transport udp, not with --udp-security plaintext",
    )?;
    refuse_unless(
        !(args.addr.is_some()
            && args.transport == crate::Transport::Udp
            && args.udp_security == gsb_server::UdpSecurityKind::Sealed
            && args.udp_server_key.is_none()),
        "--transport udp against an external server is sealed by default: give its \
         public key (--udp-server-key HEX, logged at its bind as public_key=), or \
         --udp-security plaintext for a plaintext door",
    )?;
    let written: Vec<&str> = game_flags.iter().map(String::as_str).collect();
    crate::bot::check_game_flags(args.game, &written).map_err(CliError)?;
    // The capture records what THIS process's plain clients receive: an
    // orchestrated run's clients live in child processes, a served run
    // has none, and a churn client's sessions are not one stream.
    refuse_unless(
        args.capture.is_none() || !(args.orchestrate || args.serve || args.churn_secs.is_some()),
        "--capture records a plain client run: not with --orchestrate, --serve or --churn-secs",
    )?;
    // The RPC mode's ledger lives in this process's plain clients: an
    // orchestrated run's `CLIENT` lines do not carry it, a served run has
    // no clients, and a churn client's sessions are not one ledger.
    refuse_unless(
        args.rpc_rate.is_none() || !(args.orchestrate || args.serve || args.churn_secs.is_some()),
        "--rpc-rate drives a plain client run: not with --orchestrate, --serve or --churn-secs",
    )?;
    refuse_unless(
        args.rpc_burst.is_none() || args.rpc_rate.is_some(),
        "--rpc-burst shapes --rpc-rate: give the rate too",
    )?;
    refuse_unless(
        !(args.orchestrate && args.serve),
        "--orchestrate and --serve are mutually exclusive (try --help)",
    )?;
    // The connect stagger sleeps `GLOBAL id × stagger_ms` (partition
    // invariance: a `--procs P` split connects each id at the same
    // instant as a single-process run), so the LAST client's delay is
    // `N × stagger_ms` against ONE shared window of `--duration`. A
    // schedule where that overruns the window silently starves the late
    // partitions — they sleep past their own deadline and report
    // joined=0 — which reads as a server problem. Refuse to start
    // instead: shrink `--stagger-ms`, grow `--duration`, or drop the
    // stagger (the herd then lands on admission, not on connect).
    let window = args.duration;
    let last_connect = Duration::from_secs_f64(args.clients as f64 * args.stagger_ms / 1000.0);
    if last_connect > window {
        return Err(CliError(format!(
            "--stagger-ms {} × {} clients = {:.1}s of connect spread exceeds the \
             {}s run window: the late partitions would sleep past the deadline \
             and join nothing. Use --stagger-ms <= {:.1} (window/clients), or a \
             longer --duration",
            args.stagger_ms,
            args.clients,
            last_connect.as_secs_f64(),
            window.as_secs(),
            window.as_secs_f64() / args.clients as f64 * 1000.0,
        )));
    }
    // The client half could speak `wss://` (a WS handshake over a TLS
    // stream), but the server's WebSocket door is plain only: a TLS run
    // against it would fail every handshake, or measure some other door.
    // Checked first, in every mode: it is the precise reason.
    refuse_unless(
        !(args.tls_ca.is_some() && args.transport == crate::Transport::Ws),
        "--tls-ca with --transport ws: the gsb WebSocket door has no TLS \
         form (a \"ws\" listener refuses TLS files)",
    )?;
    // TLS is an external-client feature this round: there is no way to hand
    // the in-process/served server its cert/key here, so a CA without an
    // external target would silently test plaintext against a plaintext
    // server — refuse instead of lying. And rUDP takes no TLS anywhere
    // (docs/SECURITY.md §2 decision 7).
    refuse_unless(
        !(args.tls_ca.is_some() && args.addr.is_none()),
        "--tls-ca requires --addr HOST:PORT: the in-process server has no \
         TLS config this round (it stays plaintext)",
    )?;
    refuse_unless(
        !(args.tls_ca.is_some() && args.transport == crate::Transport::Udp),
        "--tls-ca with --transport udp is contradictory: rUDP takes no TLS",
    )?;
    if args.serve && args.clients != 100 && args.addr.is_none() {
        // `gsb-loadgen --serve` takes no client count; a bare number
        // before --serve is the N of a client run, so this is a mistake.
        eprintln!("note: --serve ignores the client count (it runs no clients)");
    }
    refuse_unless(
        args.serve || args.orchestrate || args.clients != 0,
        "N must be > 0",
    )?;
    Ok(())
}
