//! The load generator over the WebSocket door (BACKLOG B29):
//! `gsb-loadgen --transport ws` drives every game end to end through a
//! real `"ws"` listener — the in-process server, and an orchestrated run
//! whose served server and client children are both told `ws` — and the
//! line the WS door cannot serve is refused. Kept small like the other
//! loadgen smokes: a handful of clients for a few seconds.

use std::collections::HashMap;

use loadgen_rate::Run;

mod loadgen_rate;

/// Run the real `gsb-loadgen` with `args`.
fn loadgen(args: &[&str]) -> Run {
    loadgen_rate::run(args)
}

/// The RESULT line of a successful run, and its `key=value` pairs; the
/// run's tick-rate claims are checked on the way (`loadgen_rate`).
fn result(out: &Run) -> (String, HashMap<String, String>) {
    assert!(
        out.status.success(),
        "gsb-loadgen exited with {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.starts_with("RESULT "))
        .expect("RESULT line in output")
        .to_string();
    let kv = line
        .split_whitespace()
        .skip(1)
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    loadgen_rate::assert_tick_rate(out, &kv, &line);
    (line, kv)
}

/// A clean WebSocket run of `game` with `n` clients: every client through
/// the whole session on the `ws` transport, numbered inputs acked,
/// nothing refused or closed by the server, and no delta dropped beyond
/// `late_joins` (a join into a group already streaming deltas —
/// GAME-MODULE G3-2); the snapshot stream and the tick rate: [`result`].
fn assert_clean_ws(line: &str, kv: &HashMap<String, String>, n: u64, game: &str, late_joins: u64) {
    let get = |k: &str| -> u64 {
        kv.get(k)
            .unwrap_or_else(|| panic!("missing {k} in: {line}"))
            .parse()
            .unwrap_or_else(|_| panic!("{k} is not a number in: {line}"))
    };
    assert_eq!(kv["transport"], "ws", "{line}");
    assert!(line.ends_with(&format!(" game={game}")), "{line}");
    for k in ["connected", "joined", "left"] {
        assert_eq!(get(k), n, "{k}: {line}");
    }
    for k in ["errors", "server_closes", "dropped", "cap_rejected"] {
        assert_eq!(get(k), 0, "{k}: {line}");
    }
    assert!(get("gap_drops") <= late_joins, "{line}");
    assert!(get("moves") > 0 && get("acks") > 0, "inputs acked: {line}");
    assert!(get("ack_processed_max") > 1, "numbered inputs: {line}");
    assert!(
        get("client_in_bps") > 0 && get("client_out_bps") > 0,
        "{line}"
    );
}

/// Each game over the in-process server's WebSocket door.
#[test]
fn loadgen_drives_every_game_over_websocket() {
    // (game, clients, duration, late joiners' dropped deltas allowed)
    let runs: [(&str, u64, &str, u64); 4] = [
        ("demo", 6, "3", 0),
        ("arena", 6, "3", 3),
        ("mmo", 8, "4", 0),
        ("war", 12, "4", 12),
    ];
    let spawned: Vec<_> = runs
        .iter()
        .map(|&(game, n, secs, late)| {
            let n_s = n.to_string();
            let args: Vec<String> = [
                n_s.as_str(),
                "--game",
                game,
                "--transport",
                "ws",
                "--duration",
                secs,
                "--move-ms",
                "100",
            ]
            .map(String::from)
            .to_vec();
            (
                game,
                n,
                late,
                std::thread::spawn(move || {
                    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                    loadgen(&argv)
                }),
            )
        })
        .collect();
    for (game, n, late, run) in spawned {
        let out = run.join().expect("the run's thread");
        let (line, kv) = result(&out);
        assert_eq!(kv["mode"], "in-proc", "{line}");
        assert_clean_ws(&line, &kv, n, game, late);
    }
}

/// An orchestrated run over WebSocket: the served server child opens a
/// `ws` door (the orchestrator's readiness probe is a TCP connect, which
/// the door takes as a failed upgrade — no session, no server close) and
/// both client children connect through it.
#[test]
fn loadgen_orchestrates_over_websocket() {
    let out = loadgen(&[
        "--orchestrate",
        "4",
        "--procs",
        "2",
        "--transport",
        "ws",
        "--duration",
        "3",
        "--move-ms",
        "100",
    ]);
    let (line, kv) = result(&out);
    assert_clean_ws(&line, &kv, 4, "demo", 0);
    assert_eq!((kv["mode"].as_str(), kv["procs"].as_str()), ("sep", "2"));
}

/// The one combination the WS door cannot serve — TLS — is a usage
/// error (status 2, one line, no panic), in every mode.
#[test]
fn loadgen_refuses_tls_over_websocket() {
    for argv in [
        &[
            "1",
            "--transport",
            "ws",
            "--addr",
            "127.0.0.1:1",
            "--tls-ca",
            "ca.pem",
        ][..],
        &[
            "--orchestrate",
            "2",
            "--transport",
            "ws",
            "--tls-ca",
            "ca.pem",
        ],
    ] {
        let out = loadgen(argv);
        let e = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(2), "{argv:?}: {e}");
        assert!(
            e.starts_with("gsb-loadgen: --tls-ca with --transport ws"),
            "{e}"
        );
        assert!(!e.contains("panicked"), "{e}");
        assert_eq!(e.lines().count(), 1, "one line: {e}");
    }
}
