//! The RPC segment's keys, their order, and the latency percentiles.

use super::*;

fn tally() -> RpcTally {
    RpcTally {
        sent: 20,
        ok: 10,
        timeout: 1,
        conn_cap: 2,
        room_cap: 3,
        dup: 4,
        no_handler: 5,
        malformed: 6,
        logic: 7,
        dup_answers: 8,
        unmatched: 9,
        late: 11,
        unanswered: 12,
        open: 13,
        ok_lat_us: vec![40_000, 10_000, 30_000, 20_000],
    }
}

/// Every key once, in its order, with its own field's value (distinct
/// values: a crossed mapping shows).
#[test]
fn the_segment_names_each_number_once() {
    let seg = rpc_segment(2.5, 8, &tally());
    assert_eq!(
        seg,
        " rpc_rate=2.5 rpc_burst=8 rpc_sent=20 rpc_ok=10 rpc_to=1 rpc_rej_conn=2 \
         rpc_rej_room=3 rpc_rej_dup=4 rpc_rej_no_handler=5 rpc_rej_malformed=6 \
         rpc_rej_logic=7 rpc_client_to=23 rpc_late=11 rpc_open=13 rpc_dup_answers=8 \
         rpc_unmatched=9 rpc_ok_p50_ms=30.0 rpc_ok_p99_ms=40.0 rpc_ok_max_ms=40.0"
    );
}

/// The percentiles are over the raw latencies (sorted), in ms; none
/// reads as zeros.
#[test]
fn the_latency_percentiles_are_over_the_raw_values() {
    let mut t = RpcTally {
        ok_lat_us: (1..=1000).rev().map(|ms| ms * 1000).collect(),
        ..Default::default()
    };
    assert_eq!(ok_latency_ms(&t), (501.0, 991.0, 1000.0));
    t.ok_lat_us = vec![1_500];
    assert_eq!(ok_latency_ms(&t), (1.5, 1.5, 1.5));
    assert_eq!(ok_latency_ms(&RpcTally::default()), (0.0, 0.0, 0.0));
}

/// The human line warns only when an exactly-once number is not zero.
#[test]
fn the_human_line_warns_on_a_second_answer() {
    let mut t = tally();
    assert!(rpc_lines(1.0, 1, &t).contains("WARNING"));
    t.dup_answers = 0;
    assert!(rpc_lines(1.0, 1, &t).contains("WARNING"), "unmatched too");
    t.unmatched = 0;
    let line = rpc_lines(1.0, 1, &t);
    assert!(!line.contains("WARNING"), "{line}");
    assert!(line.contains("client_timeouts=23 (answered late 11, never 12)"));
}
