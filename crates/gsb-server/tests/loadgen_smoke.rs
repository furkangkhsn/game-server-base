//! Smoke test for the load generator: spawns the real `gsb-loadgen`
//! binary (in-process server mode, tiny N) and asserts the tool still
//! works end-to-end — real TCP, real protocol handshake, real snapshot
//! stream, real server-side metric reports.
//!
//! Kept small on purpose (3 clients, 3 s): it must not slow the normal
//! suite noticeably. The heavyweight runs (100/500/1000 clients) are
//! manual: `cargo run -p gsb-server --release --bin gsb-loadgen 1000`.

#[test]
fn loadgen_smoke() {
    let bin = env!("CARGO_BIN_EXE_gsb-loadgen");
    let out = std::process::Command::new(bin)
        .args(["3", "--duration", "3", "--move-ms", "100"])
        .output()
        .expect("spawning gsb-loadgen");
    assert!(
        out.status.success(),
        "gsb-loadgen exited with {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result_line = stdout
        .lines()
        .find(|l| l.starts_with("RESULT "))
        .expect("RESULT line in output");

    let kv: std::collections::HashMap<String, String> = result_line
        .split_whitespace()
        .skip(1)
        .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    let get = |k: &str| -> String { kv.get(k).cloned().unwrap_or_else(|| panic!("missing {k} in: {result_line}")) };

    assert_eq!(get("mode"), "in-proc");
    assert_eq!(get("clients"), "3");
    // The full client path must work: connect, auth, join, leave.
    assert_eq!(get("connected"), "3", "all clients connect over real TCP");
    assert_eq!(get("joined"), "3", "all clients complete the join handshake");
    assert_eq!(get("left"), "3", "all clients leave cleanly");
    assert_eq!(get("errors"), "0");

    // The room must actually be ticking at ~the configured 30 Hz, as seen
    // from both sides: the client's snapshot-sequence rate and the
    // server's own metric reports (the metrics path in action).
    let client_hz: f64 = get("tick_hz_med").parse().expect("number");
    assert!(
        (20.0..=40.0).contains(&client_hz),
        "client-measured tick rate {client_hz} Hz far from the configured 30 Hz"
    );
    let server_hz: f64 = get("server_hz").parse().expect("number");
    assert!(
        (20.0..=40.0).contains(&server_hz),
        "server-reported step rate {server_hz} Hz far from the configured 30 Hz"
    );
    let steps: u64 = get("steps").parse().expect("number");
    assert!(
        steps >= 60,
        "3 s at ~30 Hz should yield ~90 room steps, got {steps}"
    );

    // Every client must actually receive the snapshot stream (the world
    // changes every tick while all clients move).
    let snaps: u64 = get("snap_total").parse().expect("number");
    assert!(
        snaps >= 30,
        "3 clients over 3 s should see well over 30 snapshots, got {snaps}"
    );

    // The registry must have registered all 3 connections at some point.
    let peak: u32 = get("peak_conns").parse().expect("number");
    assert!(peak >= 3, "registry never saw all 3 connections: peak={peak}");

    // Server-side bytes must flow (snapshots out, MOVE_TOs in).
    let out_bps: u64 = get("server_out_bps").parse().expect("number");
    assert!(out_bps > 0, "no server-side outbound bytes counted");
    let in_bps: u64 = get("server_in_bps").parse().expect("number");
    assert!(in_bps > 0, "no server-side inbound bytes counted");
}
