//! What each orchestrated child is told: the knobs both sides must agree
//! on reach both (the cell size, the game), and a game's children get
//! none of another game's flags.

use super::*;

/// A reported game door to aim the client children at.
fn door() -> SocketAddr {
    "127.0.0.1:7777".parse().unwrap()
}

/// The value that follows `flag` in an argument vector, if present.
fn value_of<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .map(String::as_str)
}

/// GAME-MODULE §6 decision 10: the client view's cell size must be the
/// one the server runs with. The client decodes a spatial room's
/// `CellExit` records by recomputing cells from wire coordinates
/// (`client/view.rs`), so a child left at the default 20 while the
/// server runs `--cell-size 50` evicts the wrong entities.
#[test]
fn client_children_get_the_orchestrated_cell_size() {
    let mut args = Args::defaults();
    args.cell_size = 50.0;
    let argv = client_args(&args, 10, 0, door(), None, None);
    assert_eq!(value_of(&argv, "--cell-size"), Some("50"));
}

/// The default is forwarded too (explicitly, not by omission), so a
/// child never depends on the two binaries sharing one default.
#[test]
fn client_children_get_the_default_cell_size_explicitly() {
    let argv = client_args(&Args::defaults(), 10, 0, door(), None, None);
    assert_eq!(value_of(&argv, "--cell-size"), Some("20"));
}

/// GAME-MODULE G3: `--game` reaches the server child AND every client
/// child, for every game this build drives — a game forwarded to one side
/// only would run one game's bots against another game's server.
#[test]
fn both_children_get_the_game() {
    for game in crate::bot::games() {
        let mut args = Args::defaults();
        args.game = game;
        assert_eq!(value_of(&server_args(&args, None), "--game"), Some(game));
        assert_eq!(
            value_of(&client_args(&args, 10, 0, door(), None, None), "--game"),
            Some(game)
        );
    }
}

/// Both children speak the run's transport: the served server opens
/// that door, the client children connect through it (B29: `ws` too).
#[test]
fn both_children_get_the_transport() {
    for t in [
        crate::Transport::Tcp,
        crate::Transport::Udp,
        crate::Transport::Ws,
    ] {
        let mut args = Args::defaults();
        args.transport = t;
        let want = Some(t.to_string());
        let server = server_args(&args, None);
        let client = client_args(&args, 10, 0, door(), None, None);
        assert_eq!(value_of(&server, "--transport").map(str::to_string), want);
        assert_eq!(value_of(&client, "--transport").map(str::to_string), want);
    }
}

/// The demo's own flags go to a demo run's children (the served server
/// keeps its strategy knobs, the clients their profile and cell size) —
/// and to no other game's: those children would refuse to start.
#[test]
fn only_a_demo_run_forwards_the_demo_flags() {
    let args = Args::defaults();
    let server = server_args(&args, None);
    for flag in [
        "--visibility",
        "--shard-count",
        "--cell-size",
        "--vision-radius",
    ] {
        assert!(server.iter().any(|a| a == flag), "{flag}: {server:?}");
    }
    assert!(server.iter().any(|a| a == "--spawn-half-size"));
    let client = client_args(&args, 10, 0, door(), None, None);
    for flag in [
        "--profile",
        "--still-frac",
        "--spawn-half-size",
        "--cell-size",
    ] {
        assert!(client.iter().any(|a| a == flag), "{flag}: {client:?}");
    }
    for game in crate::bot::games() {
        if game == args.game {
            continue;
        }
        let mut other = Args::defaults();
        other.game = game;
        for argv in [
            server_args(&other, None),
            client_args(&other, 10, 0, door(), None, None),
        ] {
            let written: Vec<&str> = argv.iter().map(String::as_str).collect();
            assert!(
                crate::bot::check_game_flags(game, &written).is_ok(),
                "{game}: {argv:?}"
            );
            assert!(
                argv.iter()
                    .any(|a| a == "--max-snapshot-bytes" || a == "--transport")
            );
        }
    }
}

/// `--conn-out` is a server knob: the served server gets it, the clients
/// never do (their own outbound half is not what it sizes).
#[test]
fn the_outbound_capacity_goes_to_the_server_only() {
    let mut args = Args::defaults();
    assert_eq!(value_of(&server_args(&args, None), "--conn-out"), None);
    args.conn_out = Some(2);
    assert_eq!(value_of(&server_args(&args, None), "--conn-out"), Some("2"));
    assert_eq!(
        value_of(&client_args(&args, 10, 0, door(), None, None), "--conn-out"),
        None
    );
}

/// `--listen-backlog` is a server knob (B84): the served server's doors
/// get it; unset, the served server keeps its config default.
#[test]
fn the_listen_backlog_goes_to_the_server_only() {
    let mut args = Args::defaults();
    let flag = "--listen-backlog";
    assert_eq!(value_of(&server_args(&args, None), flag), None);
    args.listen_backlog = Some(4096);
    assert_eq!(value_of(&server_args(&args, None), flag), Some("4096"));
    assert_eq!(
        value_of(&client_args(&args, 10, 0, door(), None, None), flag),
        None
    );
}

/// `--udp-recv-buffer` is a server knob (B4): the served server's UDP
/// door gets it; unset, the served server keeps its config default.
#[test]
fn the_udp_recv_buffer_goes_to_the_server_only() {
    let mut args = Args::defaults();
    let flag = "--udp-recv-buffer";
    assert_eq!(value_of(&server_args(&args, None), flag), None);
    args.udp_recv_buffer = Some(1 << 22);
    assert_eq!(value_of(&server_args(&args, None), flag), Some("4194304"));
    assert_eq!(
        value_of(&client_args(&args, 10, 0, door(), None, None), flag),
        None
    );
}

/// The slow reader is a client knob: every client child stalls the same
/// way, the server never hears of it.
#[test]
fn the_slow_reader_goes_to_the_clients_only() {
    let mut args = Args::defaults();
    assert_eq!(
        value_of(&client_args(&args, 10, 0, door(), None, None), "--stall-ms"),
        None
    );
    args.stall_ms = Some(900);
    let argv = client_args(&args, 10, 0, door(), None, None);
    assert_eq!(value_of(&argv, "--stall-ms"), Some("900"));
    assert_eq!(value_of(&argv, "--stall-every-ms"), Some("5000"));
    assert_eq!(value_of(&server_args(&args, None), "--stall-ms"), None);
}

/// B37: an unpinned run whose operator named no worker count tells
/// neither child one — each runs on its own runtime default
/// (`available_parallelism`), not on a single worker.
#[test]
fn unpinned_children_keep_their_runtime_default() {
    let args = Args::defaults();
    assert_eq!(args.workers, 0, "the operator named no worker count");
    let server = server_args(&args, None);
    let client = client_args(&args, 10, 0, door(), None, None);
    assert_eq!(value_of(&server, "--workers"), None, "{server:?}");
    assert_eq!(value_of(&client, "--workers"), None, "{client:?}");
}

/// An explicit `--workers N` still reaches both unpinned children.
#[test]
fn an_explicit_worker_count_reaches_both_children() {
    let mut args = Args::defaults();
    args.workers = 3;
    let server = server_args(&args, None);
    let client = client_args(&args, 10, 0, door(), None, None);
    assert_eq!(value_of(&server, "--workers"), Some("3"));
    assert_eq!(value_of(&client, "--workers"), Some("3"));
}

/// Under `--pin` a child's workers are its core set's size, whatever
/// `--workers` says (unchanged by B37).
#[test]
fn a_pinned_child_gets_its_core_count() {
    for explicit in [0, 3] {
        let mut args = Args::defaults();
        args.workers = explicit;
        let server = server_args(&args, Some(8));
        let client = client_args(&args, 10, 0, door(), None, Some(2));
        assert_eq!(value_of(&server, "--workers"), Some("8"));
        assert_eq!(value_of(&client, "--workers"), Some("2"));
    }
}

/// BACKLOG F31: the orchestrator picks no port. The server child binds
/// port 0 for its game door and its metric stream (and reports what it
/// got — `serve::announce`); every client child is aimed at the address
/// that report named.
#[test]
fn no_child_is_told_a_port_picked_in_advance() {
    let args = Args::defaults();
    let server = server_args(&args, None);
    assert_eq!(value_of(&server, "--bind"), Some("127.0.0.1:0"));
    assert_eq!(value_of(&server, "--metrics-listen"), Some("127.0.0.1:0"));
    let reported: SocketAddr = "127.0.0.1:41873".parse().unwrap();
    let client = client_args(&args, 10, 0, reported, None, None);
    assert_eq!(value_of(&client, "--addr"), Some("127.0.0.1:41873"));
}
