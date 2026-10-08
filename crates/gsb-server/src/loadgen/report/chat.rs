//! The chat/presence scenario's numbers (`--chat-rate`, `--friends`;
//! `crate::chat`, `client/chat.rs`): the clients' event tables summed,
//! the chat's push → receive latency, the joins' latency, the registry
//! probe and the router's throughput — one human block and the RESULT
//! line's `ev_*`, `chat_*`, `join_*`, `reg_*` keys.
//!
//! The keys appear only on a run in the scenario, right before `game=`
//! like the other optional segments: a run without it prints exactly
//! the line it always did. The server's own event counters (`events_*`,
//! `pushes_*`, `pub_<name>_*`, `sub_<name>_*`) are on every line already.

use gsb_core::metrics::MetricReport;

use crate::chat::{ChatPlan, ProbeTally};
use crate::client::{ClientReport, EventTally};

/// The scenario's numbers, folded.
pub(crate) struct ChatTotals {
    pub(crate) plan: ChatPlan,
    pub(crate) rooms: u32,
    /// Clients whose table reported at least one gap.
    pub(crate) gapped_clients: u64,
    pub(crate) tally: EventTally,
    /// The joins' latencies, µs.
    pub(crate) join_us: Vec<u64>,
    pub(crate) probe: Option<ProbeTally>,
    /// Pushes the router took a second, over the run's window.
    pub(crate) routed_per_s: f64,
    /// CLIENT legs the router handed to connections a second.
    pub(crate) client_legs_per_s: f64,
}

/// Fold the clients' and the server's numbers (`dur`: the run's window,
/// seconds).
pub(crate) fn chat_totals(
    plan: ChatPlan,
    rooms: u32,
    reports: &[ClientReport],
    server: &[MetricReport],
    probe: Option<&ProbeTally>,
    dur: f64,
) -> ChatTotals {
    let mut tally = EventTally::default();
    let mut gapped_clients = 0;
    for t in reports.iter().filter_map(|r| r.events.as_ref()) {
        gapped_clients += u64::from(t.gapped);
        tally.add(t);
    }
    let router = server
        .iter()
        .filter_map(|r| r.registry.map(|g| g.events))
        .max_by_key(|e| e.events_routed)
        .unwrap_or_default();
    ChatTotals {
        plan,
        rooms,
        gapped_clients,
        tally,
        join_us: reports.iter().filter_map(|r| r.join_us).collect(),
        probe: probe.cloned(),
        routed_per_s: router.events_routed as f64 / dur,
        client_legs_per_s: router.events_client_sent as f64 / dur,
    }
}

/// (p50, p99, max) of `v` (zeros when empty).
fn spread<T: Copy + Ord + Default>(v: &[T]) -> (T, T, T) {
    if v.is_empty() {
        return Default::default();
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let at = |p: f64| s[((s.len() as f64 * p) as usize).min(s.len() - 1)];
    (at(0.50), at(0.99), s[s.len() - 1])
}

/// The human block.
pub(crate) fn chat_lines(t: &ChatTotals) -> String {
    let c = &t.tally.counters;
    let (l50, l99, lmax) = spread(&t.tally.chat_lat_us);
    let (j50, j99, jmax) = spread(&t.join_us);
    let probe = t.probe.as_ref().map_or_else(
        || "probe: none".to_string(),
        |p| {
            let (r50, r99, rmax) = spread(&p.rtt_us);
            format!(
                "registry probe: answered={} busy={} gone={} rtt p50={r50}us p99={r99}us max={rmax}us",
                p.answered, p.busy, p.gone
            )
        },
    );
    format!(
        "chat/presence: rate={}/s channels={} friends={} rooms={} | router: routed={:.0}/s client_legs={:.0}/s\n\
         client events: received={} gaps={} missed={} gapped_clients={} restarted={} stale={} \
         dropped_unread={} undecodable={} | chat_lines={} friend_online={} friend_offline={} item_granted={}\n\
         chat push->receive: p50={l50}us p99={l99}us max={lmax}us | join: p50={j50}us p99={j99}us max={jmax}us\n\
         {probe}",
        t.plan.rate,
        t.plan.channels,
        t.plan.friends,
        t.rooms,
        t.routed_per_s,
        t.client_legs_per_s,
        c.received,
        c.gaps,
        c.missed,
        t.gapped_clients,
        c.restarted,
        c.stale,
        c.dropped_unread,
        c.undecodable + c.body_undecodable,
        t.tally.chat_lines,
        t.tally.friend_online,
        t.tally.friend_offline,
        t.tally.item_granted,
    )
}

/// The RESULT keys (see the module docs for where they go).
pub(crate) fn chat_segment(t: &ChatTotals) -> String {
    let mut seg = format!(
        " chat_rate={} chat_channels={} friends={} rooms={}",
        t.plan.rate, t.plan.channels, t.plan.friends, t.rooms
    );
    for (k, v) in t.tally.counters.fields() {
        seg.push_str(&format!(" ev_{k}={v}"));
    }
    let (l50, l99, lmax) = spread(&t.tally.chat_lat_us);
    let (j50, j99, jmax) = spread(&t.join_us);
    let p = t.probe.clone().unwrap_or_default();
    let (r50, r99, rmax) = spread(&p.rtt_us);
    seg.push_str(&format!(
        " ev_gapped_clients={} chat_lines_rx={} friend_online_rx={} friend_offline_rx={} item_granted_rx={} \
         chat_lat_p50_us={l50} chat_lat_p99_us={l99} chat_lat_max_us={lmax} \
         join_p50_us={j50} join_p99_us={j99} join_max_us={jmax} \
         reg_probes={} reg_busy={} reg_gone={} reg_rtt_p50_us={r50} reg_rtt_p99_us={r99} \
         reg_rtt_max_us={rmax} routed_per_s={:.0} client_legs_per_s={:.0}",
        t.gapped_clients,
        t.tally.chat_lines,
        t.tally.friend_online,
        t.tally.friend_offline,
        t.tally.item_granted,
        p.answered,
        p.busy,
        p.gone,
        t.routed_per_s,
        t.client_legs_per_s,
    ));
    seg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_segment_names_every_key_once() {
        let mut r = ClientReport::new(1);
        r.join_us = Some(1500);
        r.events = Some(EventTally {
            chat_lines: 2,
            gapped: true,
            chat_lat_us: vec![10, 30, 20],
            ..Default::default()
        });
        let plan = ChatPlan {
            rate: 5.0,
            channels: 2,
            friends: 3,
        };
        let probe = ProbeTally {
            answered: 2,
            rtt_us: vec![7, 9],
            ..Default::default()
        };
        let t = chat_totals(plan, 4, &[r, ClientReport::new(2)], &[], Some(&probe), 10.0);
        let seg = chat_segment(&t);
        for key in [
            " chat_rate=5 ",
            " rooms=4 ",
            " ev_received=0 ",
            " ev_body_undecodable=0 ",
            " ev_gapped_clients=1 ",
            " chat_lines_rx=2 ",
            " chat_lat_p50_us=20 ",
            " chat_lat_max_us=30 ",
            " join_p50_us=1500 ",
            " reg_probes=2 ",
            " reg_rtt_max_us=9 ",
        ] {
            assert_eq!(seg.matches(key).count(), 1, "{key} in {seg}");
        }
        assert!(seg.ends_with("client_legs_per_s=0"), "{seg}");
        assert!(chat_lines(&t).contains("gapped_clients=1"));
    }
}
