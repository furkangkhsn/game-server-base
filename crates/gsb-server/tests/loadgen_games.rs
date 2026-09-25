//! Smoke tests for the load generator's per-game bots (GAME-MODULE G3):
//! `gsb-loadgen --game arena|mmo|war` drives the real server hosting that
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
        .env("RUST_BACKTRACE", "1")
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
/// hosted game closing the line. `late_joins`: how many clients may drop
/// one delta at their join (a join into a group already streaming
/// deltas: the group's delta precedes the one-shot private full in the
/// same batch — GAME-MODULE G3-2).
fn assert_clean(line: &str, kv: &HashMap<String, String>, n: u64, game: &str, late_joins: u64) {
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
    assert!(get("gap_drops") <= late_joins, "{line}");
    assert!(get("snap_total") >= 30 * n, "a stream per client: {line}");
    assert!(get("moves") > 0 && get("acks") > 0, "inputs acked: {line}");
    assert!(get("ack_processed_max") > 1, "numbered inputs: {line}");
    let hz: f64 = kv["server_hz"].parse().expect("number");
    assert!((20.0..=40.0).contains(&hz), "server_hz {hz}: {line}");
}

/// The arena: team fog in the team room's delta mode (fulls for fresh
/// teams, one-shot to late joiners and on the keep-alive), its own
/// labels.
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
    // Three teams: every joiner after its team's first is a late one.
    assert_clean(&line, &kv, 6, "arena", 3);
    assert_eq!(kv["mode"], "in-proc");
    assert_eq!(
        (kv["visibility"].as_str(), kv["shards"].as_str()),
        ("team", "1")
    );
    assert_eq!(kv["profile"], "base-centre");
    assert_ne!(kv["deltas"], "0", "the team room sends deltas: {line}");
    assert_ne!(kv["fulls"], "0", "{line}");
}

/// The MMO: cell deltas over the shard grid, and the bots spread over
/// the shards (K4: each bot logs in as its saved character on waystone
/// `id mod 4` — the loadgen hosts the MMO over its bots' roster; an
/// unsaved bot would start on shard 0).
#[test]
fn loadgen_drives_the_mmo() {
    let out = loadgen(&["8", "--game", "mmo", "--duration", "4", "--move-ms", "100"]);
    let (line, kv) = result(&out);
    assert_clean(&line, &kv, 8, "mmo", 0);
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
    // Every bot starts on its home shard (2 per shard here), and the
    // occasional travels move only a few. Unsaved (no roster), shard 0
    // keeps 6 of the 8.
    assert!(
        members[0] <= 4 && members.iter().filter(|&&m| m > 0).count() >= 3,
        "the population spread across the shards: {line}"
    );
}

/// An orchestrated MMO run: `--game` reaches the server child AND the
/// client children (a demo server would send none of the MMO's frames
/// and demo clients would read none of them — `snap_total` would be 0),
/// and the server child hosts the bots' roster.
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
    assert_clean(&line, &kv, 4, "mmo", 0);
    assert_eq!(kv["mode"], "sep");
    assert_eq!(kv["procs"], "2");
    // The server child hosts the bots' roster too: one bot per shard at
    // the start (unsaved, all four would start on shard 0).
    let members: Vec<u32> = kv["shard_members"]
        .split(',')
        .map(|m| m.parse().expect("a member count"))
        .collect();
    assert_eq!(members.iter().sum::<u32>(), 4, "{line}");
    assert!(members[0] <= 2, "the server child's roster: {line}");
}

/// The per-shard members of a sharded game's RESULT line.
fn shard_members(kv: &HashMap<String, String>) -> Vec<u32> {
    kv["shard_members"]
        .split(',')
        .map(|m| m.parse().expect("a member count"))
        .collect()
}

/// The war (W2): team fog over the sharded map in delta mode, the bots
/// spread over the four shards from their saved characters (the loadgen
/// hosts the war over its bots' roster), and the team exchange in the
/// RESULT line — every shard exports every tick and the hub's relays
/// arrive.
#[test]
fn loadgen_drives_the_war() {
    // 8 s: the team rates are read over the steady window, which opens at
    // the first report past 100 steps and closes at the last one with
    // everyone in — a 5 s run can leave the two on the same report (rates
    // of 0, a flaky assertion).
    let out = loadgen(&["12", "--game", "war", "--duration", "8", "--move-ms", "100"]);
    let (line, kv) = result(&out);
    // Players join factions that are already playing on their shards:
    // up to one late joiner's dropped delta each (G3-2).
    assert_clean(&line, &kv, 12, "war", 12);
    assert_eq!(
        (kv["visibility"].as_str(), kv["shards"].as_str()),
        ("team", "4")
    );
    assert_eq!(kv["profile"], "posts");
    assert_ne!(kv["deltas"], "0", "the war sends team deltas: {line}");
    let members = shard_members(&kv);
    assert_eq!(members.len(), 4, "{line}");
    assert_eq!(members.iter().sum::<u32>(), 12, "{line}");
    assert!(
        members.iter().all(|&m| m > 0),
        "the roster spreads the bots over every shard: {line}"
    );
    let rate = |k: &str| -> f64 { kv[k].parse().expect("a number") };
    // Four shards, every one with team traffic (towers everywhere):
    // about 4 × 30 exports a second, each relayed on.
    assert!(rate("team_exports_s") > 60.0, "{line}");
    assert!(rate("team_imports_s") > 0.0, "{line}");
    assert!(rate("team_records_per_export") >= 3.0, "{line}");
    assert_eq!(kv["team_export_drops"], "0", "{line}");
    assert_eq!(kv["team_over_cap"], "0", "{line}");
}

/// An orchestrated war: `--game war` reaches the server child and the
/// client children, the server child hosts the bots' roster, and the
/// team counters cross the metrics wire.
#[test]
fn loadgen_orchestrates_the_war() {
    let out = loadgen(&[
        "--orchestrate",
        "8",
        "--procs",
        "2",
        "--game",
        "war",
        "--duration",
        "8", // a steady window for the rates (see above)
        "--move-ms",
        "100",
    ]);
    let (line, kv) = result(&out);
    assert_clean(&line, &kv, 8, "war", 8);
    assert_eq!(kv["mode"], "sep");
    let members = shard_members(&kv);
    assert_eq!(members.iter().sum::<u32>(), 8, "{line}");
    assert!(members[0] < 8, "the server child's roster: {line}");
    let exports: f64 = kv["team_exports_s"].parse().expect("a number");
    assert!(exports > 60.0, "the team counters crossed the wire: {line}");
}

/// The command line refuses what cannot run: an unknown game (naming
/// the compiled-in ones) and a demo flag written for another game (in
/// either order) — the server's "explicitly written fixed key" rule. A
/// refusal is a message on stderr and exit status 2: no panic, no
/// backtrace (even with `RUST_BACKTRACE` set).
#[test]
fn loadgen_refuses_a_wrong_game_line() {
    let stderr = |out: Output| {
        let e = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(2), "a usage error: {e}");
        assert!(e.starts_with("gsb-loadgen: "), "{e}");
        assert!(!e.contains("panicked") && !e.contains("backtrace"), "{e}");
        assert_eq!(e.lines().count(), 1, "one line: {e}");
        e
    };
    let e = stderr(loadgen(&["1", "--game", "chess"]));
    assert!(e.contains("unknown game `chess`"), "{e}");
    assert!(e.contains("compiled in: demo, arena, mmo, war"), "{e}");
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
    let e = stderr(loadgen(&["--duration", "soon"]));
    assert!(
        e.contains("--duration: expected a number, got `soon`"),
        "{e}"
    );
    let e = stderr(loadgen(&["1", "--bogus"]));
    assert!(e.contains("unknown flag --bogus (try --help)"), "{e}");
}

/// `--capture` (KIT-ARCHITECTURE §10 "A22"): the sampled clients' files
/// hold exactly the game-band stream each client applied — replaying a
/// file through the kit's reference client reproduces that client's
/// counters — and a line that cannot capture is refused.
#[test]
fn loadgen_captures_the_frames_its_clients_applied() {
    use gsb_kit::client::wire::{Fields, Value};
    use gsb_kit::client::{ClientDecoder, ClientError, ClientView, PrivateEvent};

    /// Stores a record's body under its wire id (field 1 — every game's
    /// record starts with it); no cells (the arena has none).
    struct Bodies;
    impl ClientDecoder for Bodies {
        type Record = Vec<u8>;
        type Cell = ();
        fn record(&self, body: &[u8]) -> Result<(u64, Vec<u8>), ClientError> {
            let mut id = 0;
            for field in Fields::new(body) {
                if let (1, Value::Varint(v)) = field? {
                    id = v;
                }
            }
            Ok((id, body.to_vec()))
        }
        fn cell_of(&self, _: &Vec<u8>) {}
        fn cell_exit(&self, _: &[u8]) -> Result<(), ClientError> {
            Ok(())
        }
    }

    let dir = std::env::temp_dir().join(format!("gsb-capture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let dir_s = dir.to_str().expect("a UTF-8 temp dir");
    let out = Command::new(env!("CARGO_BIN_EXE_gsb-loadgen"))
        .args(["6", "--game", "arena", "--duration", "3"])
        .args(["--capture", dir_s, "--capture-clients", "2"])
        .env("GSB_LOADGEN_CLIENT_LINES", "1")
        .output()
        .expect("spawning gsb-loadgen");
    let (line, kv) = result(&out);
    assert_clean(&line, &kv, 6, "arena", 6);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let client = |id: u64| -> HashMap<String, u64> {
        let prefix = format!("CLIENT id={id} ");
        stdout
            .lines()
            .find(|l| l.starts_with(&prefix))
            .expect("the client's line")
            .split_whitespace()
            .filter_map(|kv| kv.split_once('='))
            .filter_map(|(k, v)| Some((k.to_string(), v.parse().ok()?)))
            .collect()
    };
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .expect("the capture dir")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .into_string()
                .expect("UTF-8")
        })
        .collect();
    files.sort();
    assert_eq!(files, ["client-0.gsbcap", "client-3.gsbcap"]);
    for id in [0u64, 3] {
        let bytes = std::fs::read(dir.join(format!("client-{id}.gsbcap"))).expect("read");
        assert_eq!(&bytes[..8], b"GSBCAP1\n");
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), id);
        let name_len = usize::from(u16::from_le_bytes(bytes[16..18].try_into().unwrap()));
        assert_eq!(&bytes[18..18 + name_len], b"arena");
        let mut rest = &bytes[18 + name_len..];
        let mut view = ClientView::new(Bodies);
        let (mut snapshots, mut acks, mut joins) = (0u64, 0u64, 0u64);
        while !rest.is_empty() {
            let kind = rest[0];
            let len = u32::from_le_bytes(rest[5..9].try_into().unwrap()) as usize;
            let payload = &rest[9..9 + len];
            match kind {
                0 => {
                    view.apply_snapshot(payload).expect("a captured snapshot");
                    snapshots += 1;
                }
                1 => {
                    let event = view.apply_private(payload);
                    if matches!(
                        event.expect("a captured private frame"),
                        PrivateEvent::Ack(_)
                    ) {
                        acks += 1;
                    }
                }
                2 => joins += 1,
                other => panic!("unknown frame kind {other}"),
            }
            rest = &rest[9 + len..];
        }
        let seen = client(id);
        let c = view.counters();
        assert_eq!(snapshots, seen["snapshots"], "client {id}");
        assert!(snapshots > 30, "a stream: client {id}");
        assert_eq!(acks, seen["acks"], "client {id}");
        assert!(acks > 0, "private frames: client {id}");
        assert_eq!(joins, 1, "the join result: client {id}");
        assert_eq!(
            (c.fulls, c.private_fulls, c.deltas, c.gap_drops),
            (
                seen["fulls"],
                seen["private_fulls"],
                seen["deltas"],
                seen["gap_drops"]
            ),
            "client {id}"
        );
        assert_eq!(view.len() as u64, seen["view_size"], "client {id}");
    }
    let _ = std::fs::remove_dir_all(&dir);

    let refused = loadgen(&["--orchestrate", "4", "--capture", dir_s]);
    let e = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "{e}");
    assert!(e.contains("--capture records a plain client run"), "{e}");
    assert!(!dir.exists(), "a refused line creates nothing");
}
