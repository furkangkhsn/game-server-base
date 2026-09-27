//! The load generator's RPC mode (`--rpc-rate`, BACKLOG B23) against the
//! real in-process demo server: the clients' ledgers and the room's own
//! request counters are two views of one path, and they must agree.
//! Kept small like the other loadgen smokes (a few clients, a few
//! seconds); the load runs are manual (docs/RPC-CONTROL-PLANE.md §8.2).

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

/// The RESULT line of a successful run, and a getter for its numbers.
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

fn num(line: &str, kv: &HashMap<String, String>, k: &str) -> u64 {
    kv.get(k)
        .unwrap_or_else(|| panic!("missing {k} in: {line}"))
        .parse()
        .unwrap_or_else(|_| panic!("{k} is not a count in: {line}"))
}

/// The RPC ledger (docs/RPC-CONTROL-PLANE.md §8.3): every request a
/// client sent lands in exactly one of these terms — the room's buckets
/// and the connection-side drops. Every term must be on the line.
const LEDGER: [&str; 12] = [
    "req_local",
    "req_ext",
    "req_rej_malformed",
    "req_rej_dup",
    "req_rej_no_handler",
    "req_rej_logic",
    "req_rej_conn",
    "req_rej_room",
    "req_refused",
    "req_unread",
    "req_unbound",
    "requests_dropped_closed",
];

/// The ledger's sum on a RESULT line.
fn ledger(line: &str, kv: &HashMap<String, String>) -> u64 {
    LEDGER.iter().map(|k| num(line, kv, k)).sum()
}

/// What must hold on any run in the mode: every client through its
/// session with no error, the exactly-once numbers at zero, `game=`
/// still last.
fn assert_clean(line: &str, kv: &HashMap<String, String>, n: u64) {
    let get = |k| num(line, kv, k);
    for k in ["connected", "joined", "left"] {
        assert_eq!(get(k), n, "{k}: {line}");
    }
    // An answer-only private frame is no error in the mode.
    assert_eq!(get("errors"), 0, "{line}");
    assert_eq!(get("rpc_dup_answers"), 0, "exactly once: {line}");
    assert_eq!(get("rpc_unmatched"), 0, "every answer is ours: {line}");
    // The whole ledger closes: each request sent is in exactly one term.
    assert_eq!(ledger(line, kv), get("rpc_sent"), "the RPC ledger: {line}");
    assert!(line.ends_with(" game=demo"), "{line}");
}

/// A sane rate: every request is sent after the join, answered `ok`
/// through the external (worker) path well inside the client's limit,
/// once; the room registered each as an external request and rejected
/// none.
#[test]
fn every_request_is_answered_once_at_a_sane_rate() {
    let out = loadgen(&["6", "--duration", "4", "--rpc-rate", "5"]);
    let (line, kv) = result(&out);
    assert_clean(&line, &kv, 6);
    let get = |k| num(&line, &kv, k);
    let sent = get("rpc_sent");
    assert!(sent >= 6 * 10, "6 clients × 5/s × ~3 s joined: {line}");
    assert_eq!(get("rpc_client_to"), 0, "{line}");
    // Answered ok, or still in flight when the session ended.
    assert_eq!(get("rpc_ok") + get("rpc_open"), sent, "{line}");
    assert!(get("rpc_open") <= 6, "at most one in flight each: {line}");
    // The room's side: each one registered external, nothing rejected,
    // refused or timed out.
    assert!(get("req_ext") >= get("rpc_ok"), "{line}");
    // The room's ledger closes (B36): every request the clients sent was
    // either registered or still unread when its session left.
    assert_eq!(get("req_ext") + get("req_unread"), sent, "{line}");
    for k in [
        "rpc_to",
        "rpc_rej_conn",
        "rpc_rej_room",
        "rpc_rej_dup",
        "rpc_rej_no_handler",
        "rpc_rej_malformed",
        "rpc_rej_logic",
        "req_rej_conn",
        "req_rej_room",
        "req_rej_dup",
        "req_rej_malformed",
        "req_rej_no_handler",
        "req_rej_logic",
        "req_refused",
        "req_to",
        // The connection-side term of the ledger (B51): 0 here — every
        // membership is ended by its client, never by the room.
        "requests_dropped_closed",
    ] {
        assert_eq!(get(k), 0, "{k}: {line}");
    }
    let p50: f64 = kv["rpc_ok_p50_ms"].parse().expect("p50");
    let p99: f64 = kv["rpc_ok_p99_ms"].parse().expect("p99");
    assert!(0.0 < p50 && p50 <= p99 && p99 < 5_000.0, "{line}");
}

/// A burst above the per-connection pending cap (4): the requests past
/// it are answered with the cap's rejection, and the clients count
/// exactly as many as the room did — the reason text and the bucket are
/// one decision seen from both ends.
#[test]
fn a_burst_past_the_cap_is_counted_alike_on_both_sides() {
    let out = loadgen(&[
        "4",
        "--duration",
        "4",
        "--rpc-rate",
        "8",
        "--rpc-burst",
        "8",
    ]);
    let (line, kv) = result(&out);
    assert_clean(&line, &kv, 4);
    let get = |k| num(&line, &kv, k);
    let cap = get("rpc_rej_conn");
    assert!(cap > 0, "a burst of 8 meets the cap of 4: {line}");
    assert_eq!(cap, get("req_rej_conn"), "{line}");
    assert_eq!(get("rpc_rej_room"), get("req_rej_room"), "{line}");
    assert_eq!(get("req_refused"), 0, "no connection is congested: {line}");
    assert_eq!(get("rpc_client_to"), 0, "{line}");
    assert_eq!(
        get("rpc_ok") + cap + get("rpc_open"),
        get("rpc_sent"),
        "{line}"
    );
    // The room's ledger closes too (B36): registered, cap-rejected, or
    // unread when its session left.
    assert_eq!(
        get("req_ext") + get("req_rej_conn") + get("req_unread"),
        get("rpc_sent"),
        "{line}"
    );
}

/// The room's counters on the RESULT line cover the WHOLE run, the
/// leaves included (B36): the room samples once per metrics period, so a
/// run whose end falls mid-period must still report the requests read
/// after the period's last sample. A fractional duration puts the end
/// half a period past a sample.
#[test]
fn the_rooms_ledger_covers_the_end_of_the_run() {
    let out = loadgen(&["4", "--duration", "2.5", "--rpc-rate", "10"]);
    let (line, kv) = result(&out);
    assert_clean(&line, &kv, 4);
    let get = |k| num(&line, &kv, k);
    assert_eq!(
        get("req_ext") + get("req_unread"),
        get("rpc_sent"),
        "every request sent is in the room's final counters: {line}"
    );
}

/// A run without the mode carries no `rpc_*` key (its line is the one
/// it always was).
#[test]
fn a_plain_run_has_no_rpc_keys() {
    let out = loadgen(&["2", "--duration", "2"]);
    let (line, _) = result(&out);
    assert!(!line.contains(" rpc_"), "{line}");
}
