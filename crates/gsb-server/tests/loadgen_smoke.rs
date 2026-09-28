//! Smoke test for the load generator: spawns the real `gsb-loadgen`
//! binary (in-process server mode, tiny N) and asserts the tool still
//! works end-to-end — real TCP, real protocol handshake, real snapshot
//! stream, real server-side metric reports.
//!
//! Kept small on purpose (3 clients, 3 s): it must not slow the normal
//! suite noticeably. The heavyweight runs (100/500/1000 clients) are
//! manual: `cargo run -p gsb-server --release --bin gsb-loadgen 1000`.
//! They are also where "the room achieves 30 Hz" is measured: a 3 s
//! wall-clock window proves only what a stall cannot change
//! (`loadgen_rate`, BACKLOG F25).

mod loadgen_rate;

#[test]
fn loadgen_smoke() {
    let run = loadgen_rate::run(&["3", "--duration", "3", "--move-ms", "100"]);
    let out = &run.out;
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
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    let get = |k: &str| -> String {
        kv.get(k)
            .cloned()
            .unwrap_or_else(|| panic!("missing {k} in: {result_line}"))
    };

    assert_eq!(get("mode"), "in-proc");
    assert_eq!(get("clients"), "3");
    // The hosted game closes the line (GAME-MODULE §4.5).
    assert_eq!(get("game"), "demo");
    assert!(result_line.ends_with(" game=demo"), "{result_line}");
    // The full client path must work: connect, auth, join, leave.
    assert_eq!(get("connected"), "3", "all clients connect over real TCP");
    assert_eq!(
        get("joined"),
        "3",
        "all clients complete the join handshake"
    );
    assert_eq!(get("left"), "3", "all clients leave cleanly");
    assert_eq!(get("errors"), "0");

    // The room runs at the configured 30 Hz period, steps, never faster,
    // and both sides' rates reach the report (the metrics path in
    // action) — what a 3 s wall-clock run proves on any machine.
    loadgen_rate::assert_tick_rate(&run, &kv, result_line);

    // The registry must have registered all 3 connections at some point.
    let peak: u32 = get("peak_conns").parse().expect("number");
    assert!(
        peak >= 3,
        "registry never saw all 3 connections: peak={peak}"
    );

    // Server-side bytes must flow (snapshots out, MOVE_TOs in).
    let out_bps: u64 = get("server_out_bps").parse().expect("number");
    assert!(out_bps > 0, "no server-side outbound bytes counted");
    let in_bps: u64 = get("server_in_bps").parse().expect("number");
    assert!(in_bps > 0, "no server-side inbound bytes counted");

    // The metric queue (the room report fields that ride the metrics
    // wire / binary socket): presence + sanity, so a shifted queue
    // (fields reordered or dropped in the report codec) is caught here
    // even though a fully broken codec would already fail the tick-rate
    // claims.
    assert_metric_queue(&kv, result_line);
}

/// Assert the room-report "queue" fields on a RESULT line: the fine
/// step-duration percentiles (the measurement spec's metric) and the
/// RPC counters (cumulative + the in-flight gauge). The smoke scenarios
/// carry NO RPC traffic by construction (connect/auth/join/move/leave
/// only), so every RPC counter must be present and exactly 0 — garbage
/// values (a shifted queue) fail the zero check, missing keys fail the
/// `get`.
fn assert_metric_queue(kv: &std::collections::HashMap<String, String>, result_line: &str) {
    let get = |k: &str| -> String {
        kv.get(k)
            .cloned()
            .unwrap_or_else(|| panic!("missing {k} in: {result_line}"))
    };

    // Fine step percentiles: present, positive, ordered, and far below
    // a broken-tick regime (a 3–4 client room ticks in hundreds of
    // microseconds even on a loaded machine; >20 ms means the
    // histogram or the room itself is broken, not just slow).
    let p50: u64 = get("step_p50_fine_us").parse().expect("number");
    let p90: u64 = get("step_p90_fine_us").parse().expect("number");
    assert!(
        p50 > 0,
        "step_p50_fine_us must be positive for a ticking room"
    );
    assert!(
        p90 >= p50,
        "fine histogram invariant violated: p90 {p90} < p50 {p50}"
    );
    assert!(
        (p50..=20_000).contains(&p90) && p50 <= 20_000,
        "fine percentiles p50={p50} p90={p90} outside (0, 20 000] µs"
    );

    // RPC queue: all present and exactly 0 (no RPC traffic in smoke).
    for k in [
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
        "req_to",
        "req_late",
        "req_undelivered",
        "req_abandoned",
        "req_pending",
    ] {
        let v: u64 = get(k).parse().expect("number");
        assert_eq!(v, 0, "smoke runs no RPC traffic; {k} must be 0");
    }

    // The connection-side forwards into a closed action channel (B51):
    // every smoke membership is ended by its client, never by the room,
    // so none can meet a closed channel.
    // Nor can a smoke client send an RPC request (B55's two terms).
    for k in [
        "actions_dropped_closed",
        "requests_dropped_closed",
        "requests_dropped_full",
        "requests_no_room",
    ] {
        let v: u64 = get(k).parse().expect("number");
        assert_eq!(v, 0, "the room ended no membership; {k} must be 0");
    }

    // Server-initiated closes: the total and one key per reason, all
    // present (a shifted net-scope queue fails the parse or the zero) and
    // all 0 — a smoke run's clients leave on their own, so any non-zero
    // here is the server shedding a healthy client.
    let mut keys = vec!["server_closes".to_string()];
    keys.extend(
        gsb_core::conn::ServerClose::ALL
            .iter()
            .map(|r| format!("server_close_{}", r.label())),
    );
    for k in &keys {
        let v: u64 = get(k).parse().expect("number");
        assert_eq!(v, 0, "a smoke run sheds no client; {k} must be 0");
    }

    // The fan-out's two send failures (B32): present and numeric. Not
    // pinned to 0 — a client that closes its socket right after its LEAVE
    // result can meet one batch on its closed channel (`sends_closed`),
    // and that is timing, not a fault.
    // The plain game actions the room dropped unprocessed (B54): present
    // and numeric. Not pinned to 0 — a move sent right before the
    // client's LEAVE can still be unread when the leave lands. The
    // heartbeat throttle's surplus (B56) likewise: a client's heartbeat
    // cadence is its own business.
    for k in [
        "dropped",
        "sends_closed",
        "actions_unread",
        "actions_unbound",
        "hb_throttled_preauth",
        "hb_throttled_authed",
        // The connection actors' outbound losses (B57): a client that
        // closes right after its LEAVE result can leave a control frame
        // to a gone writer.
        "frames_out_closed",
        "close_notices_dropped",
        // What a server-decided end left unprocessed (B60).
        "requests_unprocessed",
        "actions_unprocessed",
        "control_frames_unprocessed",
        // The transport's own losses (B58), every counter.
        "transport_udp_requests_dropped_full",
        "transport_udp_actions_dropped_full",
        "transport_udp_control_frames_dropped_full",
        "transport_udp_acks_not_forwarded",
        "transport_udp_datagrams_oversized",
        "transport_udp_datagrams_malformed",
        "transport_udp_bad_cookies",
        "transport_udp_frags_refused",
        "transport_udp_sessions_dropped_accept_full",
        "transport_udp_frames_dropped_oversized",
        "transport_udp_control_frames_abandoned",
        "transport_udp_frames_drained",
        "transport_ws_close_frames_dropped",
        "transport_ws_pongs_dropped",
        "transport_handshakes_refused",
        "transport_handshakes_timed_out",
        "transport_handshakes_failed",
        "transport_metrics_dropped",
        // The stream pumps' losses (B66).
        "transport_stream_frames_unwritten",
        "transport_stream_batches_unwritten",
        "transport_stream_requests_dropped_closed",
        "transport_stream_actions_dropped_closed",
        "transport_stream_control_frames_dropped_closed",
        "transport_ws_control_frames_unwritten",
        "transport_ws_frames_dropped_after_close",
        // The rUDP tasks' remaining losses (B66).
        "transport_udp_game_datagrams_send_failed",
        "transport_udp_control_datagrams_send_failed",
        "transport_udp_acks_send_failed",
        "transport_udp_challenges_send_failed",
        "transport_udp_requests_dropped_closed",
        "transport_udp_actions_dropped_closed",
        "transport_udp_control_frames_dropped_closed",
        "transport_udp_datagrams_no_session",
        "transport_udp_frames_unsent",
        "transport_writer_verdicts_deferred",
        // What a closing door and rUDP's accept side drop (B74).
        "transport_handshakes_cut_closed",
        "transport_handshakes_unaccepted_closed",
        "transport_udp_sessions_dropped_accept_gone",
        "transport_udp_sessions_unaccepted_closed",
        // The WebSocket teardown close that could not be delivered (B80).
        "transport_ws_going_away_unsent_closed",
        "transport_ws_going_away_unsent_stalled",
        // The WebSocket control replies a closed queue refused (B83).
        "transport_ws_close_frames_dropped_closed",
        "transport_ws_pongs_dropped_closed",
        // What the stopping rooms/shards still held (B68).
        "joins_unprocessed",
        "resumes_unprocessed",
        "leaves_unprocessed",
        "detaches_unprocessed",
        "migrations_in_dropped",
        "effects_unsent",
        "effects_unapplied",
        "team_imports_unapplied",
        "border_updates_unapplied",
    ] {
        let _: u64 = get(k)
            .parse()
            .unwrap_or_else(|_| panic!("{k} is not a number in: {result_line}"));
    }

    // The collector kept up (0 samples dropped on the metrics channel).
    let dropped: u64 = get("metrics_dropped").parse().expect("number");
    assert_eq!(dropped, 0, "metrics channel dropped samples: {dropped}");
}

/// Smoke test for the separate-process mode (item A): the orchestrator
/// spawns a `--serve` server process and two client processes; the
/// server's metric reports must arrive over the binary metrics socket
/// (the channel data, not stdout parsing) and the merged RESULT line
/// must carry the process topology.
#[test]
fn loadgen_smoke_separate_processes() {
    let run = loadgen_rate::run(&[
        "--orchestrate",
        "4",
        "--procs",
        "2",
        "--duration",
        "3",
        "--move-ms",
        "100",
    ]);
    let out = &run.out;
    assert!(
        out.status.success(),
        "orchestrator exited with {:?}\nstdout:\n{}\nstderr:\n{}",
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
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    let get = |k: &str| -> String {
        kv.get(k)
            .cloned()
            .unwrap_or_else(|| panic!("missing {k} in: {result_line}"))
    };

    assert_eq!(get("mode"), "sep");
    assert_eq!(get("clients"), "4");
    assert_eq!(get("connected"), "4", "all clients connect over real TCP");
    assert_eq!(get("joined"), "4");
    assert_eq!(get("procs"), "2");
    // The process topology must be reported (the isolation story).
    let server_pid: u32 = get("server_pid").parse().expect("number");
    assert!(server_pid > 0, "server process pid must be reported");
    let client_pids = get("client_pids");
    assert!(
        client_pids.split(',').count() == 2,
        "two client processes: {client_pids}"
    );
    // The server's own metric reports must have crossed the binary socket:
    // the room's configured period, a sane rate and >0 steps (the
    // in-process mode's numbers come from the same report series); the
    // client-measured rate too (merged from the children's records).
    loadgen_rate::assert_tick_rate(&run, &kv, result_line);

    // The metric queue crossed the BINARY socket (the report data, not
    // stdout): presence + zero-RPC-traffic invariants.
    assert_metric_queue(&kv, result_line);
}

/// Smoke test for the churn profile (`--churn-secs`, RECONNECT §14.5):
/// N real clients cycle connect→join→DROP-without-leave→reconnect against
/// an in-process server whose disconnect-park grace comfortably exceeds
/// the cycle, so every reconnect after the first lands INSIDE the hold —
/// a server-accepted resume with the SAME wire id. Small N, short
/// window, same shape as `loadgen_smoke`.
#[test]
fn loadgen_churn_smoke() {
    let bin = env!("CARGO_BIN_EXE_gsb-loadgen");
    let out = std::process::Command::new(bin)
        .args([
            "4",
            "--duration",
            "7",
            "--move-ms",
            "100",
            "--churn-secs",
            "1.5",
            // One DROP→resume transition per identity: the exact §14.5
            // thundering-herd shape (N simultaneous resumes, nothing else
            // in flight); after it each client keeps playing until the
            // deadline (a resumed hero under sustained load).
            "--churn-cycles",
            "1",
            // grace 30 s >> run 7 s ⇒ the drop always parks and the next
            // join resumes (no expiry races in the window).
            "--disconnect-grace-secs",
            "30",
        ])
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
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    let get = |k: &str| -> String {
        kv.get(k)
            .cloned()
            .unwrap_or_else(|| panic!("missing {k} in: {result_line}"))
    };

    // Every client connected and joined at least one session.
    assert_eq!(get("connected"), "4", "all clients connect over real TCP");
    let joined: u64 = get("joined").parse().expect("number");
    assert!(joined >= 4, "every client completed at least one join");

    // The churn machinery actually ran: every client completed its drop
    // session plus the lingering resumed one.
    let cycles: u64 = get("churn_cycles").parse().expect("number");
    assert!(
        cycles >= 8,
        "4 clients × (drop session + linger session) expected, got {cycles}"
    );
    // EVERY reconnect was a SERVER-ACCEPTED RESUME: the same wire id on
    // the wire, mirrored exactly by the server's own counter.
    let resumed: u64 = get("resumed").parse().expect("number");
    assert_eq!(
        resumed, 4,
        "one resume per client (the herd), no more, no less"
    );
    let room_resumes: u64 = get("room_resumes").parse().expect("number");
    assert_eq!(
        room_resumes, resumed,
        "the server's accepted-resume counter must mirror the clients' \
         same-wire-id joins exactly"
    );
    let stale: u64 = get("resume_rejected_stale").parse().expect("number");
    assert_eq!(
        stale, 0,
        "no stale rejects in the one-drop-per-identity herd"
    );
    // No expiry ran (grace ≫ run) and nothing fell back to a fresh join.
    let fresh: u64 = get("fresh_joins").parse().expect("number");
    assert_eq!(fresh, 0, "no identity lost its park inside the grace");
    let ai: u64 = get("detach_expired_ai").parse().expect("number");
    let despawn: u64 = get("detach_expired_despawn").parse().expect("number");
    assert_eq!(ai + despawn, 0, "no park expired during the smoke window");
    assert_eq!(get("errors"), "0", "churn must be clean at this scale");
}

/// An orchestrated run whose server child dies at start (here: a shard
/// count the server refuses; in the wild: its port taken between the
/// orchestrator's allocation and the child's bind) ends, reporting the
/// dead child — it does not wait forever for a metric stream that child
/// never served. It used to: the reader retried its connect every 100 ms
/// for as long as anyone let it (a 51-minute hang under load, F25).
#[test]
fn an_orchestrated_run_whose_server_child_dies_still_ends() {
    use std::time::{Duration, Instant};
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_gsb-loadgen"))
        .args(["--orchestrate", "2", "--procs", "1", "--duration", "1"])
        .args(["--visibility", "sharded", "--shard-count", "0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning gsb-loadgen orchestrator");
    // Past the orchestrator's own bounds: the readiness probe (10 s),
    // the children's margins, the metric reader's grace.
    let deadline = Instant::now() + Duration::from_secs(90);
    while child.try_wait().expect("try_wait").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the orchestrator is still waiting for its dead server child");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = child.wait_with_output().expect("the orchestrator's output");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("server child exited non-success"),
        "the dead child is reported:\n{stderr}"
    );
    assert!(
        stderr.contains("the server child served no metric stream"),
        "and so is the missing metric stream:\n{stderr}"
    );
}
