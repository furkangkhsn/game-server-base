//! Smoke tests for the load generator's per-game bots (GAME-MODULE G3):
//! `gsb-loadgen --game arena|mmo` drives the real server hosting that
//! game end to end — real TCP, the game's own inputs and frames, acks
//! back — and `--game` reaches both sides of an orchestrated run. The
//! demo's smoke (`loadgen_smoke.rs`) is untouched. Kept small like it: a
//! handful of clients for a few seconds.

use std::collections::HashMap;
use std::process::{Command, Output};

/// Run the real `gsb-loadgen` with `args`.
fn loadgen(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gsb-loadgen"))
        .args(args)
        .output()
        .expect("spawning gsb-loadgen")
}

/// The RESULT line of a successful run, and its `key=value` pairs.
fn result(out: &Output) -> (String, HashMap<String, String>) {
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
    (line, kv)
}

/// What every game's clean run shows: all `n` clients through the whole
/// session, a snapshot stream, numbered inputs acked, nothing shed, the
/// hosted game closing the line.
fn assert_clean(line: &str, kv: &HashMap<String, String>, n: u64, game: &str) {
    let get = |k: &str| -> u64 {
        kv.get(k)
            .unwrap_or_else(|| panic!("missing {k} in: {line}"))
            .parse()
            .unwrap_or_else(|_| panic!("{k} is not a number in: {line}"))
    };
    assert!(line.ends_with(&format!(" game={game}")), "{line}");
    for k in ["connected", "joined", "left"] {
        assert_eq!(get(k), n, "{k}: {line}");
    }
    assert_eq!(get("errors"), 0, "{line}");
    assert_eq!(get("server_closes"), 0, "{line}");
    assert_eq!(get("gap_drops"), 0, "{line}");
    assert!(get("snap_total") >= 30 * n, "a stream per client: {line}");
    assert!(get("moves") > 0 && get("acks") > 0, "inputs acked: {line}");
    assert!(get("ack_processed_max") > 1, "numbered inputs: {line}");
    let hz: f64 = kv["server_hz"].parse().expect("number");
    assert!((20.0..=40.0).contains(&hz), "server_hz {hz}: {line}");
}

/// The arena: team fog, full snapshots only (no deltas), its own labels.
#[test]
fn loadgen_drives_the_arena() {
    let out = loadgen(&[
        "6",
        "--game",
        "arena",
        "--duration",
        "3",
        "--move-ms",
        "100",
    ]);
    let (line, kv) = result(&out);
    assert_clean(&line, &kv, 6, "arena");
    assert_eq!(kv["mode"], "in-proc");
    assert_eq!(
        (kv["visibility"].as_str(), kv["shards"].as_str()),
        ("team", "1")
    );
    assert_eq!(kv["profile"], "base-centre");
    assert_eq!(kv["deltas"], "0", "the team room sends fulls: {line}");
    assert_ne!(kv["fulls"], "0", "{line}");
}

/// The MMO: cell deltas over the shard grid, and the bots spread over
/// the shards (K4: without their first `Travel` every one would stand on
/// shard 0).
#[test]
fn loadgen_drives_the_mmo() {
    let out = loadgen(&["8", "--game", "mmo", "--duration", "4", "--move-ms", "100"]);
    let (line, kv) = result(&out);
    assert_clean(&line, &kv, 8, "mmo");
    assert_eq!(
        (kv["visibility"].as_str(), kv["shards"].as_str()),
        ("spatial", "4")
    );
    assert_eq!(kv["profile"], "roam");
    assert_ne!(kv["deltas"], "0", "the MMO sends cell deltas: {line}");
    let members: Vec<u32> = kv["shard_members"]
        .split(',')
        .map(|m| m.parse().expect("a member count"))
        .collect();
    assert_eq!(members.len(), 4, "{line}");
    assert_eq!(members.iter().sum::<u32>(), 8, "{line}");
    // Every session starts on shard 0 (K4); the bots' first `Travel`
    // sends three in four elsewhere (2 per shard here), and the later
    // occasional travels move only a few. Without the dispersal shard 0
    // keeps 6 of the 8.
    assert!(
        members[0] <= 4 && members.iter().filter(|&&m| m > 0).count() >= 3,
        "the population spread across the shards: {line}"
    );
}

/// An orchestrated MMO run: `--game` reaches the server child AND the
/// client children (a demo server would send none of the MMO's frames
/// and demo clients would read none of them — `snap_total` would be 0).
#[test]
fn loadgen_orchestrates_the_mmo() {
    let out = loadgen(&[
        "--orchestrate",
        "4",
        "--procs",
        "2",
        "--game",
        "mmo",
        "--duration",
        "3",
        "--move-ms",
        "100",
    ]);
    let (line, kv) = result(&out);
    assert_clean(&line, &kv, 4, "mmo");
    assert_eq!(kv["mode"], "sep");
    assert_eq!(kv["procs"], "2");
}

/// The command line refuses what cannot run: an unknown game (naming
/// the compiled-in ones) and a demo flag written for another game (in
/// either order) — the server's "explicitly written fixed key" rule.
#[test]
fn loadgen_refuses_a_wrong_game_line() {
    let stderr = |out: Output| {
        assert!(!out.status.success(), "must refuse");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let e = stderr(loadgen(&["1", "--game", "chess"]));
    assert!(e.contains("unknown game `chess`"), "{e}");
    assert!(e.contains("compiled in: demo, arena, mmo"), "{e}");
    let e = stderr(loadgen(&[
        "1",
        "--game",
        "arena",
        "--visibility",
        "spatial",
    ]));
    assert!(
        e.contains("--visibility does not apply to --game arena"),
        "{e}"
    );
    let e = stderr(loadgen(&["1", "--cell-size", "30", "--game", "mmo"]));
    assert!(
        e.contains("--cell-size does not apply to --game mmo"),
        "{e}"
    );
}
