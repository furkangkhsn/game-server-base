//! The parser's refusals are errors carrying the reason (the binary
//! prints them and exits 2 — `tests/loadgen_games.rs`), and a good line
//! still parses.

use super::*;

fn line(argv: &[&str]) -> Result<Cli, CliError> {
    parse(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn refused(argv: &[&str]) -> String {
    match line(argv) {
        Err(CliError(why)) => why,
        Ok(_) => panic!("{argv:?} parsed"),
    }
}

/// Each kind of refusal, with its message.
#[test]
fn a_bad_line_is_an_error_with_its_reason() {
    let cases: [(&[&str], &str); 18] = [
        (&["--duration"], "--duration needs a value (try --help)"),
        (
            &["--duration", "x"],
            "--duration: expected a number, got `x`",
        ),
        (&["many"], "N: expected a number, got `many`"),
        (&["--frobnicate"], "unknown flag --frobnicate (try --help)"),
        (
            &["--profile", "zigzag"],
            "--profile: expected ring|spread|still (try --help)",
        ),
        (
            &["--transport", "carrier-pigeon"],
            "--transport: expected tcp|udp|ws, got carrier-pigeon",
        ),
        (&["--still-frac", "2"], "--still-frac must be in 0..=1"),
        (
            &["--orchestrate", "--serve"],
            "--orchestrate and --serve are mutually exclusive (try --help)",
        ),
        (&["0"], "N must be > 0"),
        (&["--capture-clients", "0"], "--capture-clients must be > 0"),
        (&["--conn-out", "0"], "--conn-out must be at least 1"),
        (
            &["--listen-backlog", "0"],
            "--listen-backlog must be 1..=2147483647 (the kernel caps it at somaxconn)",
        ),
        (
            &["--listen-backlog", "2147483648"],
            "--listen-backlog must be 1..=2147483647 (the kernel caps it at somaxconn)",
        ),
        (
            &["--udp-recv-buffer", "4095"],
            "--udp-recv-buffer must be 4096..=2147483647 (Linux caps it at net.core.rmem_max)",
        ),
        (
            &["--udp-recv-buffer", "2147483648"],
            "--udp-recv-buffer must be 4096..=2147483647 (Linux caps it at net.core.rmem_max)",
        ),
        (
            &["--udp-congestion", "fast"],
            "--udp-congestion: expected off|pace, got fast",
        ),
        (&["--stall-ms", "0"], "--stall-ms must be > 0"),
        (
            &[
                "--transport",
                "ws",
                "--addr",
                "127.0.0.1:1",
                "--tls-ca",
                "ca.pem",
            ],
            "--tls-ca with --transport ws: the gsb WebSocket door has no TLS \
             form (a \"ws\" listener refuses TLS files)",
        ),
    ];
    for (argv, why) in cases {
        assert_eq!(refused(argv), why, "{argv:?}");
    }
    assert!(refused(&["--game", "chess"]).contains("unknown game `chess`"));
    assert_eq!(
        refused(&["--addr", "localhost"]),
        "--addr: expected HOST:PORT (an IP address and a port), got `localhost`"
    );
    assert!(refused(&["--serve", "--metrics-listen", "x:1"]).starts_with("--metrics-listen"));
    assert!(line(&["--addr", "127.0.0.1:7777"]).is_ok());
    // WebSocket in every mode: in-process, against --addr, served and
    // orchestrated (the children are told `--transport ws`).
    for argv in [
        &["--transport", "ws"][..],
        &["--transport", "ws", "--addr", "127.0.0.1:7777"],
        &["--transport", "ws", "--serve"],
        &["--transport", "ws", "--orchestrate"],
    ] {
        let Ok(Cli::Run(args)) = line(argv) else {
            panic!("{argv:?} is a run");
        };
        assert_eq!(args.transport, crate::Transport::Ws, "{argv:?}");
    }
}

/// A good line is a run with its knobs; `--help` is a help request.
#[test]
fn a_good_line_parses() {
    let Ok(Cli::Run(args)) = line(&["7", "--duration", "2.5", "--game", "demo"]) else {
        panic!("a run");
    };
    assert_eq!(args.clients, 7);
    assert_eq!(args.duration, Duration::from_millis(2_500));
    let Ok(Cli::Run(args)) = line(&["--conn-out", "2"]) else {
        panic!("a run");
    };
    assert_eq!(args.conn_out, Some(2));
    assert_eq!(args.listen_backlog, None);
    let Ok(Cli::Run(args)) = line(&["--listen-backlog", "4096"]) else {
        panic!("a run");
    };
    assert_eq!(args.listen_backlog, Some(4096));
    assert_eq!(args.udp_recv_buffer, None);
    let Ok(Cli::Run(args)) = line(&["--udp-recv-buffer", "8388608"]) else {
        panic!("a run");
    };
    assert_eq!(args.udp_recv_buffer, Some(8 << 20));
    assert_eq!(args.udp_congestion, None, "unset: the config's default");
    for (v, want) in [
        ("off", gsb_server::UdpCongestionKind::Off),
        ("pace", gsb_server::UdpCongestionKind::Pace),
    ] {
        let Ok(Cli::Run(args)) = line(&["--udp-congestion", v]) else {
            panic!("a run");
        };
        assert_eq!(args.udp_congestion, Some(want), "{v}");
    }
    let Ok(Cli::Run(args)) = line(&["--stall-ms", "900", "--stall-every-ms", "3000"]) else {
        panic!("a run");
    };
    assert_eq!((args.stall_ms, args.stall_every_ms), (Some(900), 3000));
    assert!(matches!(line(&["5", "--help"]), Ok(Cli::Help)));
}

/// The RPC mode (B23): a rate and a burst parse; a bad value, a burst
/// without a rate, a mode that has no ledger to keep (orchestrated,
/// served, churn), and another game are refused with their reason.
#[test]
fn the_rpc_mode_parses_and_refuses() {
    let Ok(Cli::Run(args)) = line(&["--rpc-rate", "2.5", "--rpc-burst", "8"]) else {
        panic!("a run");
    };
    assert_eq!((args.rpc_rate, args.rpc_burst), (Some(2.5), Some(8)));
    let Ok(Cli::Run(args)) = line(&["--rpc-rate", "1", "--addr", "127.0.0.1:7777"]) else {
        panic!("a run against --addr");
    };
    assert_eq!((args.rpc_rate, args.rpc_burst), (Some(1.0), None));
    let plain = "--rpc-rate drives a plain client run: not with --orchestrate, --serve or \
                 --churn-secs";
    for (argv, why) in [
        (
            &["--rpc-rate", "0"][..],
            "--rpc-rate must be > 0 (requests/s per client)",
        ),
        (
            &["--rpc-rate", "inf"],
            "--rpc-rate must be > 0 (requests/s per client)",
        ),
        (
            &["--rpc-rate", "1", "--rpc-burst", "0"],
            "--rpc-burst must be at least 1",
        ),
        (
            &["--rpc-burst", "4"],
            "--rpc-burst shapes --rpc-rate: give the rate too",
        ),
        (&["--rpc-rate", "1", "--orchestrate"], plain),
        (&["--rpc-rate", "1", "--serve"], plain),
        (&["--rpc-rate", "1", "--churn-secs", "2"], plain),
    ] {
        assert_eq!(refused(argv), why, "{argv:?}");
    }
    #[cfg(feature = "game-arena")]
    assert!(
        refused(&["--game", "arena", "--rpc-rate", "1"])
            .starts_with("--rpc-rate does not apply to --game arena")
    );
}
