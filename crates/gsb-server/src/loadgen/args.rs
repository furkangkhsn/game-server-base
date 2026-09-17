//! The command line: every knob the generator takes, and the usage
//! text that documents them.

use crate::*;
use std::time::Duration;

pub(crate) fn parse_args() -> Args {
    let mut args = Args {
        clients: 100,
        offset: 0,
        duration: Duration::from_secs(10),
        move_ms: Duration::from_millis(150),
        room: 1,
        stagger_ms: 0.0,
        addr: None,
        profile: Profile::Ring,
        still_frac: 0.9,
        spawn_half: 50.0,
        visibility: gsb_server::Visibility::default(),
        topology: None,
        shard_count: 4,
        cell_size: 20.0,
        vision_radius: gsb_game::team::DEFAULT_VISION_RADIUS,
        max_snapshot_bytes: 1400,
        server_spawn_half: 50.0,
        serve: false,
        bind: "127.0.0.1:7777".into(),
        metrics_listen: None,
        orchestrate: false,
        transport: gsb_server::TransportKind::Tcp,
        tls_ca: None,
        tls_server_name: "localhost".into(),
        procs: 1,
        pin: false,
        pin_server_cores: 8,
        workers: 0,
        max_players: None,
        max_connections: None,
        idle_timeout_secs: None,
        flood_id: None,
        churn_secs: None,
        churn_cycles: 0,
        disconnect_grace_secs: None,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].clone();
        i += 1;
        let mut v = || {
            if i < argv.len() {
                let s = argv[i].clone();
                i += 1;
                s
            } else {
                panic!("{a} needs a value (try --help)")
            }
        };
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--duration" => args.duration = Duration::from_secs_f64(v().parse().expect("number")),
            "--move-ms" => args.move_ms = Duration::from_millis(v().parse().expect("number")),
            "--room" => args.room = v().parse().expect("number"),
            "--offset" => args.offset = v().parse().expect("number"),
            "--stagger-ms" => args.stagger_ms = v().parse().expect("number"),
            "--addr" => args.addr = Some(v()),
            "--profile" => {
                args.profile = Profile::parse(&v())
                    .unwrap_or_else(|| panic!("--profile: expected ring|spread|still (try --help)"))
            }
            "--still-frac" => {
                let f: f64 = v().parse().expect("number");
                assert!((0.0..=1.0).contains(&f), "--still-frac must be in 0..=1");
                args.still_frac = f;
            }
            "--spawn-half-size" => {
                let s = v().parse().expect("number");
                args.spawn_half = s;
                args.server_spawn_half = s; // one knob: spawn map = home map
            }
            "--visibility" => {
                let s = v();
                args.visibility = match s.as_str() {
                    "all" => gsb_server::Visibility::All,
                    "spatial" => gsb_server::Visibility::Spatial,
                    "team" => gsb_server::Visibility::Team,
                    "pvs" => gsb_server::Visibility::Pvs,
                    "sharded" => gsb_server::Visibility::Sharded,
                    other => {
                        panic!("--visibility: expected all|spatial|team|pvs|sharded, got {other}")
                    }
                };
            }
            "--topology" => {
                let s = v();
                args.topology = match s.as_str() {
                    "single" => Some(gsb_server::Topology::Single),
                    "sharded" => Some(gsb_server::Topology::Sharded),
                    other => panic!("--topology: expected single|sharded, got {other}"),
                };
            }
            "--shard-count" => {
                args.shard_count = v().parse().expect("number");
            }
            "--cell-size" => args.cell_size = v().parse().expect("number"),
            "--vision-radius" => args.vision_radius = v().parse().expect("number"),
            "--max-snapshot-bytes" => args.max_snapshot_bytes = v().parse().expect("number"),
            "--serve" => args.serve = true,
            "--bind" => args.bind = v(),
            "--metrics-listen" => args.metrics_listen = Some(v()),
            "--orchestrate" => args.orchestrate = true,
            "--procs" => args.procs = v().parse().expect("number"),
            "--pin" => args.pin = true,
            "--pin-server-cores" => args.pin_server_cores = v().parse().expect("number"),
            "--workers" => args.workers = v().parse().expect("number"),
            "--max-players" => {
                let n: u32 = v().parse().expect("number");
                args.max_players = Some(n);
            }
            "--max-connections" => {
                let n: u64 = v().parse().expect("number");
                args.max_connections = Some(n);
            }
            "--idle-timeout-secs" => {
                args.idle_timeout_secs = Some(v().parse().expect("number"));
            }
            "--flood-id" => args.flood_id = Some(v().parse().expect("number")),
            "--churn-secs" => {
                let f: f64 = v().parse().expect("number");
                assert!(f > 0.0, "--churn-secs must be > 0");
                args.churn_secs = Some(f);
            }
            "--disconnect-grace-secs" => {
                args.disconnect_grace_secs = Some(v().parse().expect("number"));
            }
            "--churn-cycles" => {
                args.churn_cycles = v().parse().expect("number");
            }
            "--transport" => {
                let s = v();
                args.transport = match s.as_str() {
                    "tcp" => gsb_server::TransportKind::Tcp,
                    "udp" => gsb_server::TransportKind::Udp,
                    other => panic!("--transport: expected tcp|udp, got {other}"),
                };
            }
            "--tls-ca" => args.tls_ca = Some(v()),
            "--tls-server-name" => args.tls_server_name = v(),
            s if s.starts_with("--") => panic!("unknown flag {s} (try --help)"),
            s => args.clients = s.parse().expect("N must be a number"),
        }
    }
    // The spread profile's default map (a "wide map": 2000×2000) — the
    // ring profile stays on the historical 100×100 arena, so the default
    // run is bit-identical to the historical one.
    if args.spawn_half == 50.0 && args.profile == Profile::Spread {
        args.spawn_half = 1000.0;
        args.server_spawn_half = 1000.0;
    }
    if args.orchestrate && args.serve {
        panic!("--orchestrate and --serve are mutually exclusive (try --help)");
    }
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
        panic!(
            "--stagger-ms {} × {} clients = {:.1}s of connect spread exceeds the \
             {}s run window: the late partitions would sleep past the deadline \
             and join nothing. Use --stagger-ms <= {:.1} (window/clients), or a \
             longer --duration",
            args.stagger_ms,
            args.clients,
            last_connect.as_secs_f64(),
            window.as_secs(),
            window.as_secs_f64() / args.clients as f64 * 1000.0,
        );
    }
    // TLS is an external-client feature this round: there is no way to hand
    // the in-process/served server its cert/key here, so a CA without an
    // external target would silently test plaintext against a plaintext
    // server — refuse instead of lying. And rUDP takes no TLS anywhere
    // (docs/SECURITY.md §2 decision 7).
    if args.tls_ca.is_some() && args.addr.is_none() {
        panic!(
            "--tls-ca requires --addr HOST:PORT: the in-process server has no \
             TLS config this round (it stays plaintext)"
        );
    }
    if args.tls_ca.is_some() && args.transport == gsb_server::TransportKind::Udp {
        panic!("--tls-ca with --transport udp is contradictory: rUDP takes no TLS");
    }
    if args.serve && args.clients != 100 && args.addr.is_none() {
        // `gsb-loadgen --serve` takes no client count; a bare number
        // before --serve is the N of a client run, so this is a mistake.
        eprintln!("note: --serve ignores the client count (it runs no clients)");
    }
    if !args.serve && !args.orchestrate && args.clients == 0 {
        panic!("N must be > 0");
    }
    args
}
