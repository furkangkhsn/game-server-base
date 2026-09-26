//! The RPC mode's numbers (`--rpc-rate`, BACKLOG B23): the clients'
//! ledgers summed (`client/rpc/ledger.rs`), as one human line and the
//! RESULT line's `rpc_*` keys.
//!
//! The keys appear only on a run in the mode, right before `game=` like
//! the other optional segments: a run without it prints exactly the line
//! it always did, and every existing key keeps its place and format. The
//! room's own request counters (`req_*`) are on every line already — the
//! two sides of one path, side by side: what the clients saw
//! (`rpc_rej_conn`) against what the room counted (`req_rej_conn`).

use crate::client::{ClientReport, RpcTally};

/// The clients' ledgers summed.
pub(crate) fn rpc_total(reports: &[ClientReport]) -> RpcTally {
    let mut total = RpcTally::default();
    for r in reports {
        total.add(&r.rpc);
    }
    total
}

/// The `ok` latencies' percentiles in ms: (p50, p99, max); zeros with no
/// `ok` answer.
pub(crate) fn ok_latency_ms(t: &RpcTally) -> (f64, f64, f64) {
    let mut v = t.ok_lat_us.clone();
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_unstable();
    let at = |p: f64| {
        let i = ((v.len() as f64 * p) as usize).min(v.len() - 1);
        f64::from(v[i]) / 1000.0
    };
    (at(0.50), at(0.99), f64::from(v[v.len() - 1]) / 1000.0)
}

/// The RESULT keys of a run at `rate` requests/s in bursts of `burst`
/// (see the module docs for where they go).
pub(crate) fn rpc_segment(rate: f64, burst: u32, t: &RpcTally) -> String {
    let (p50, p99, max) = ok_latency_ms(t);
    format!(
        " rpc_rate={rate} rpc_burst={burst} rpc_sent={} rpc_ok={} rpc_to={} rpc_rej_conn={} \
         rpc_rej_room={} rpc_rej_dup={} rpc_rej_no_handler={} rpc_rej_malformed={} \
         rpc_rej_logic={} rpc_client_to={} rpc_late={} rpc_open={} rpc_dup_answers={} \
         rpc_unmatched={} rpc_ok_p50_ms={p50:.1} rpc_ok_p99_ms={p99:.1} rpc_ok_max_ms={max:.1}",
        t.sent,
        t.ok,
        t.timeout,
        t.conn_cap,
        t.room_cap,
        t.dup,
        t.no_handler,
        t.malformed,
        t.logic,
        t.client_timeouts(),
        t.late,
        t.open,
        t.dup_answers,
        t.unmatched,
    )
}

/// The human report's RPC line (plus a warning when the exactly-once
/// numbers are not zero).
pub(crate) fn rpc_lines(rate: f64, burst: u32, t: &RpcTally) -> String {
    let (p50, p99, max) = ok_latency_ms(t);
    let mut s = format!(
        "rpc (clients): rate={rate}/s burst={burst} sent={} answered={} ok={} \
         rejected[to={} conn={} room={} dup={} no_handler={} malformed={} logic={}] \
         client_timeouts={} (answered late {}, never {}) open_at_end={} \
         dup_answers={} unmatched={} ok_latency p50={p50:.1}ms p99={p99:.1}ms max={max:.1}ms",
        t.sent,
        t.answered(),
        t.ok,
        t.timeout,
        t.conn_cap,
        t.room_cap,
        t.dup,
        t.no_handler,
        t.malformed,
        t.logic,
        t.client_timeouts(),
        t.late,
        t.unanswered,
        t.open,
        t.dup_answers,
        t.unmatched,
    );
    if t.dup_answers > 0 || t.unmatched > 0 {
        s.push_str(&format!(
            "\nWARNING: {} duplicate and {} unmatched RPC answer(s): the engine promises \
             exactly one answer per accepted request",
            t.dup_answers, t.unmatched
        ));
    }
    s
}

#[cfg(test)]
mod tests;
