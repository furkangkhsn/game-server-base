//! What each orchestrated child is told: the knobs both sides must agree
//! on reach both (the cell size, the game), and a game's children get
//! none of another game's flags.

use super::*;

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
    let argv = client_args(&args, 10, 0, 7777, 1);
    assert_eq!(value_of(&argv, "--cell-size"), Some("50"));
}

/// The default is forwarded too (explicitly, not by omission), so a
/// child never depends on the two binaries sharing one default.
#[test]
fn client_children_get_the_default_cell_size_explicitly() {
    let argv = client_args(&Args::defaults(), 10, 0, 7777, 1);
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
        assert_eq!(
            value_of(&server_args(&args, 7777, 7778, 1), "--game"),
            Some(game)
        );
        assert_eq!(
            value_of(&client_args(&args, 10, 0, 7777, 1), "--game"),
            Some(game)
        );
    }
}

/// The demo's own flags go to a demo run's children (the served server
/// keeps its strategy knobs, the clients their profile and cell size) —
/// and to no other game's: those children would refuse to start.
#[test]
fn only_a_demo_run_forwards_the_demo_flags() {
    let args = Args::defaults();
    let server = server_args(&args, 7777, 7778, 1);
    for flag in [
        "--visibility",
        "--shard-count",
        "--cell-size",
        "--vision-radius",
    ] {
        assert!(server.iter().any(|a| a == flag), "{flag}: {server:?}");
    }
    assert!(server.iter().any(|a| a == "--spawn-half-size"));
    let client = client_args(&args, 10, 0, 7777, 1);
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
            server_args(&other, 7777, 7778, 1),
            client_args(&other, 10, 0, 7777, 1),
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
    assert_eq!(
        value_of(&server_args(&args, 7777, 7778, 1), "--conn-out"),
        None
    );
    args.conn_out = Some(2);
    assert_eq!(
        value_of(&server_args(&args, 7777, 7778, 1), "--conn-out"),
        Some("2")
    );
    assert_eq!(
        value_of(&client_args(&args, 10, 0, 7777, 1), "--conn-out"),
        None
    );
}

/// The slow reader is a client knob: every client child stalls the same
/// way, the server never hears of it.
#[test]
fn the_slow_reader_goes_to_the_clients_only() {
    let mut args = Args::defaults();
    assert_eq!(
        value_of(&client_args(&args, 10, 0, 7777, 1), "--stall-ms"),
        None
    );
    args.stall_ms = Some(900);
    let argv = client_args(&args, 10, 0, 7777, 1);
    assert_eq!(value_of(&argv, "--stall-ms"), Some("900"));
    assert_eq!(value_of(&argv, "--stall-every-ms"), Some("5000"));
    assert_eq!(
        value_of(&server_args(&args, 7777, 7778, 1), "--stall-ms"),
        None
    );
}
