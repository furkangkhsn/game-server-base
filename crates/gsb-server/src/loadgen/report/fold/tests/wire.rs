//! The orchestrator's metrics wire (`codec.rs`) carries every shard's
//! detach-ceiling, remote-effect, migration and team-exchange counters:
//! a separate-process run folds the same numbers an in-process run does.

use super::*;
use crate::codec::{decode_report, encode_report};
use gsb_core::metrics::{LOGIC_COUNTERS_MAX, LogicCounter, LogicFold};

/// The seventeen counters, as one comparable tuple.
fn counters(r: &RoomReport) -> [u64; 17] {
    [
        r.detach_forced,
        r.effects_applied,
        r.effects_forwarded,
        r.effects_orphaned,
        r.effects_dropped,
        r.effects_refused,
        r.migrations_out,
        r.migrations_in,
        r.migrations_failed,
        r.team_exports,
        r.team_export_drops,
        r.team_export_records,
        r.team_over_cap,
        r.team_imports,
        r.team_import_records,
        r.team_expired,
        r.team_over_budget,
    ]
}

#[test]
fn the_new_counters_survive_the_wire() {
    let sent = three_shards();
    let frame = encode_report(&sent);
    // The frame is `[magic][body_len][body]`; the decoder takes the body.
    let got = decode_report(&frame[8..]).expect("decodes");
    assert_eq!(got.rooms.len(), 3);
    for (a, b) in sent.rooms.iter().zip(&got.rooms) {
        assert_eq!(counters(a), counters(b), "shard {:?}", a.room);
        // The fields on either side of them still line up.
        assert_eq!(a.detach_expired_ai, b.detach_expired_ai);
        assert_eq!(a.requests_local, b.requests_local);
        assert_eq!(a.metrics_dropped, b.metrics_dropped);
    }
}

/// The congested connections' RPC refusals (GSMD, F15) cross the wire
/// as their own field, between the room-cap rejections and the timeouts.
#[test]
fn the_congested_refusals_survive_the_wire() {
    let sent = three_shards();
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    for (a, b) in sent.rooms.iter().zip(&got.rooms) {
        assert_eq!(
            (
                a.requests_rejected_conn_cap,
                a.requests_rejected_room_cap,
                a.requests_refused_congested,
                a.requests_timed_out
            ),
            (
                b.requests_rejected_conn_cap,
                b.requests_rejected_room_cap,
                b.requests_refused_congested,
                b.requests_timed_out
            ),
            "shard {:?}",
            a.room
        );
    }
}

/// The requests a session left unread (GSMH, B36) cross the wire as
/// their own field, between the congested refusals and the timeouts.
#[test]
fn the_unread_requests_survive_the_wire() {
    let sent = three_shards();
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    for (a, b) in sent.rooms.iter().zip(&got.rooms) {
        assert_eq!(
            (
                a.requests_refused_congested,
                a.requests_dropped_unread,
                a.requests_timed_out
            ),
            (
                b.requests_refused_congested,
                b.requests_dropped_unread,
                b.requests_timed_out
            ),
            "shard {:?}",
            a.room
        );
    }
}

/// The input the room dropped unprocessed (GSML, B54) crosses the wire
/// in three fields of its own: the unbound requests after the unread
/// ones, the two action counters after the abandoned requests.
#[test]
fn the_unprocessed_input_survives_the_wire() {
    let sent = three_shards();
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    for (a, b) in sent.rooms.iter().zip(&got.rooms) {
        assert_eq!(
            (
                a.requests_dropped_unread,
                a.requests_dropped_unbound,
                a.requests_timed_out,
                a.requests_abandoned,
                a.actions_dropped_unread,
                a.actions_dropped_unbound,
                a.pending_requests
            ),
            (
                b.requests_dropped_unread,
                b.requests_dropped_unbound,
                b.requests_timed_out,
                b.requests_abandoned,
                b.actions_dropped_unread,
                b.actions_dropped_unbound,
                b.pending_requests
            ),
            "shard {:?}",
            a.room
        );
    }
}

/// What a session that ended took with it (GSMK, B53) crosses the wire
/// as two fields of its own, between the late reports and the pending
/// gauge.
#[test]
fn the_undelivered_answers_survive_the_wire() {
    let sent = three_shards();
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    for (a, b) in sent.rooms.iter().zip(&got.rooms) {
        assert_eq!(
            (
                a.requests_late,
                a.requests_undelivered,
                a.requests_abandoned,
                a.pending_requests
            ),
            (
                b.requests_late,
                b.requests_undelivered,
                b.requests_abandoned,
                b.pending_requests
            ),
            "shard {:?}",
            a.room
        );
    }
}

/// The fan-out's closed-channel sends (GSMI, B32) cross the wire as
/// their own field, between the drop rate and the keep-alive re-sends.
#[test]
fn the_closed_sends_survive_the_wire() {
    let sent = three_shards();
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    for (a, b) in sent.rooms.iter().zip(&got.rooms) {
        assert_eq!(
            (a.dropped, a.sends_closed, a.keepalive_resends),
            (b.dropped, b.sends_closed, b.keepalive_resends),
            "shard {:?}",
            a.room
        );
    }
}

/// The logic counters (GSMC) cross the wire name by name, with their
/// fold rules and the overflow count; an empty set stays empty. The
/// help line does not travel.
#[test]
fn the_logic_counters_survive_the_wire() {
    let mut sent = three_shards();
    sent.rooms[1].logic = LogicCounters::new();
    let frame = encode_report(&sent);
    assert_eq!(&frame[..4], b"YMSG", "the magic, little-endian GSMY");
    let got = decode_report(&frame[8..]).expect("decodes");
    for (a, b) in sent.rooms.iter().zip(&got.rooms) {
        let names = |r: &RoomReport| -> Vec<(String, LogicFold, u64)> {
            r.logic
                .slots()
                .iter()
                .map(|s| (s.counter.name().to_owned(), s.counter.fold(), s.value))
                .collect()
        };
        assert_eq!(names(a), names(b), "shard {:?}", a.room);
        assert_eq!(a.logic.dropped(), b.logic.dropped());
        assert_eq!(a.metrics_dropped, b.metrics_dropped);
    }
    assert!(got.rooms[1].logic.is_empty());
    assert_eq!(got.rooms[0].logic.slots()[0].counter.help(), "");
    assert_eq!(
        got.net.bytes_in, sent.net.bytes_in,
        "the tail still lines up"
    );
}

/// A frame whose logic record the encoder cannot have written is
/// refused, not half-read: a bad name, an unknown rule, too many.
#[test]
fn a_malformed_logic_record_is_refused() {
    let sent = report(vec![shard(0)]);
    let frame = encode_report(&sent);
    let body = &frame[8..];
    // shard(0)'s set: kills, fights_peak — find the first name.
    let at = body
        .windows(5)
        .position(|w| w == b"kills")
        .expect("the first name");
    let mut bad_name = body.to_vec();
    bad_name[at] = b'K';
    assert!(decode_report(&bad_name).is_none(), "an invalid name");
    let mut bad_fold = body.to_vec();
    bad_fold[at + 5] = 7;
    assert!(decode_report(&bad_fold).is_none(), "an unknown fold rule");
    assert!(decode_report(body).is_some(), "the untouched frame decodes");
}

/// A record claiming one counter more than a set holds is refused even
/// when every one of them is well formed (the decoder would otherwise
/// fold the extra one into the set's overflow and accept the frame).
#[test]
fn a_logic_record_over_the_bound_is_refused() {
    let mut room = shard(0);
    room.logic = LogicCounters::new();
    for i in 0..LOGIC_COUNTERS_MAX {
        room.logic
            .put(&LogicCounter::sum(&format!("c{i:02}"), ""), 1);
    }
    let frame = encode_report(&report(vec![room]));
    let mut body = frame[8..].to_vec();
    assert!(decode_report(&body).is_some(), "sixteen decode");
    let first = body.windows(3).position(|w| w == b"c00").expect("c00");
    let last = body.windows(3).position(|w| w == b"c15").expect("c15");
    // One more well-formed record after the sixteenth, and the count
    // (before the u32 overflow count and the first length byte) bumped.
    let mut extra = vec![3u8];
    extra.extend_from_slice(b"c16");
    extra.push(0);
    extra.extend_from_slice(&1u64.to_le_bytes());
    let end = last + 3 + 1 + 8;
    body.splice(end..end, extra);
    body[first - 1 - 4 - 1] = (LOGIC_COUNTERS_MAX + 1) as u8;
    assert!(decode_report(&body).is_none(), "seventeen are refused");
}

/// The net-scope rate-limited input count (GSME, E1) crosses the wire as
/// its own field, between the violations and the server closes.
#[test]
fn the_rate_limited_input_survives_the_wire() {
    let mut sent = three_shards();
    sent.net.violations = 3;
    sent.net.input_rate_limited = 4_242;
    sent.net
        .server_closes
        .add(gsb_core::conn::ServerClose::IdleTimeout);
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    assert_eq!(got.net.violations, 3);
    assert_eq!(got.net.input_rate_limited, 4_242);
    assert_eq!(got.net.server_closes.total(), 1);
}

/// The forwards dropped into a closed action channel (GSMJ, B51) cross
/// the wire as their own two fields, between the rate-limited input and
/// the server closes.
#[test]
fn the_closed_channel_forwards_survive_the_wire() {
    let mut sent = three_shards();
    sent.net.input_rate_limited = 5;
    sent.net.actions_dropped_closed = 7;
    sent.net.requests_dropped_closed = 11;
    sent.net
        .server_closes
        .add(gsb_core::conn::ServerClose::Kicked);
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    assert_eq!(got.net.input_rate_limited, 5);
    assert_eq!(got.net.actions_dropped_closed, 7);
    assert_eq!(got.net.requests_dropped_closed, 11);
    assert_eq!(got.net.server_closes.total(), 1);
}

/// The RPC ledger's two connection-side edges (GSMM, B55) cross the wire
/// as their own fields, after the closed-channel forwards.
#[test]
fn the_full_channel_and_no_room_requests_survive_the_wire() {
    let mut sent = three_shards();
    sent.net.actions_dropped = 3;
    sent.net.requests_dropped_closed = 11;
    sent.net.requests_dropped_full = 13;
    sent.net.requests_no_room = 17;
    sent.net
        .server_closes
        .add(gsb_core::conn::ServerClose::Kicked);
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    assert_eq!(got.net.actions_dropped, 3);
    assert_eq!(got.net.requests_dropped_closed, 11);
    assert_eq!(got.net.requests_dropped_full, 13);
    assert_eq!(got.net.requests_no_room, 17);
    assert_eq!(got.net.server_closes.total(), 1);
}

/// The heartbeat throttle's surplus (GSMN, B56) crosses the wire as two
/// fields of its own, after the no-room requests.
#[test]
fn the_throttled_heartbeats_survive_the_wire() {
    let mut sent = three_shards();
    sent.net.requests_no_room = 17;
    sent.net.heartbeats_throttled_preauth = 19;
    sent.net.heartbeats_throttled_authed = 23;
    sent.net
        .server_closes
        .add(gsb_core::conn::ServerClose::Kicked);
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    assert_eq!(got.net.requests_no_room, 17);
    assert_eq!(got.net.heartbeats_throttled_preauth, 19);
    assert_eq!(got.net.heartbeats_throttled_authed, 23);
    assert_eq!(got.net.server_closes.total(), 1);
}

/// The connection actors' outbound losses (GSMO, B57) cross the wire as
/// two fields of their own, after the throttled heartbeats.
#[test]
fn the_outbound_losses_survive_the_wire() {
    let mut sent = three_shards();
    sent.net.heartbeats_throttled_authed = 23;
    sent.net.frames_out_closed = 29;
    sent.net.close_notices_dropped = 31;
    sent.net
        .server_closes
        .add(gsb_core::conn::ServerClose::Kicked);
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    assert_eq!(got.net.heartbeats_throttled_authed, 23);
    assert_eq!(got.net.frames_out_closed, 29);
    assert_eq!(got.net.close_notices_dropped, 31);
    assert_eq!(got.net.server_closes.total(), 1);
}

/// The registry section's control-plane losses (GSMP, B57; GSMU added
/// `rooms_ended_uncounted`, B67; GSMW the team hubs' refused relays,
/// B72; GSMY the joins a stopped room refused, B75) cross the wire after
/// its `closes`, and the net section still decodes after them.
#[test]
fn the_control_plane_losses_survive_the_wire() {
    let mut sent = three_shards();
    sent.registry = Some(gsb_core::metrics::RegistryReport {
        rooms: 1,
        conns: 2,
        rooms_created: 3,
        rooms_destroyed: 4,
        rooms_died: 5,
        joins: 6,
        leaves: 7,
        opens: 8,
        closes: 9,
        join_ops_dropped: 10,
        close_ops_dropped: 11,
        match_results_dropped_full: 12,
        match_results_dropped_closed: 13,
        rooms_ended_uncounted: 14,
        team_relays_dropped_full: 15,
        team_relays_dropped_closed: 16,
        joins_refused_closed: 17,
    });
    sent.net.close_notices_dropped = 31;
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    let g = got.registry.expect("the registry section");
    assert_eq!(
        (
            g.closes,
            g.join_ops_dropped,
            g.close_ops_dropped,
            g.match_results_dropped_full,
            g.match_results_dropped_closed,
            g.rooms_ended_uncounted,
            g.team_relays_dropped_full,
            g.team_relays_dropped_closed,
            g.joins_refused_closed
        ),
        (9, 10, 11, 12, 13, 14, 15, 16, 17)
    );
    assert_eq!(got.net.close_notices_dropped, 31);
}

/// What a server-decided end left unprocessed (GSMQ, B60) crosses the
/// wire as three fields of their own, after the outbound losses.
#[test]
fn the_unprocessed_frames_survive_the_wire() {
    let mut sent = three_shards();
    sent.net.close_notices_dropped = 31;
    sent.net.requests_unprocessed = 37;
    sent.net.actions_unprocessed = 41;
    sent.net.control_frames_unprocessed = 43;
    sent.net
        .server_closes
        .add(gsb_core::conn::ServerClose::RoomGone);
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    assert_eq!(got.net.close_notices_dropped, 31);
    assert_eq!(got.net.requests_unprocessed, 37);
    assert_eq!(got.net.actions_unprocessed, 41);
    assert_eq!(got.net.control_frames_unprocessed, 43);
    assert_eq!(got.net.server_closes.total(), 1);
}

/// The transport's own losses (GSMR, B58; GSMS and GSMT added the
/// stream pumps' and the rUDP tasks' remaining ones, B66) cross the wire as a section of their own after the attribution
/// list: every counter in its slot.
#[test]
fn the_transport_losses_survive_the_wire() {
    let mut sent = three_shards();
    sent.actions_dropped_top = vec![(gsb_core::id::ConnectionId(4), 9)];
    let mut values = [0u64; gsb_core::metrics::TRANSPORT_COUNT];
    for (i, v) in values.iter_mut().enumerate() {
        *v = 50 + i as u64;
    }
    sent.transport = gsb_core::metrics::TransportCounters::from_values(values);
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    assert_eq!(got.transport, sent.transport);
    assert_eq!(got.actions_dropped_top, sent.actions_dropped_top);
}

/// Every server-close reason crosses the wire in its own slot (GSMF
/// added `idle_input`, E6; GSMG `kicked`, E8): each counted once, each read back at its
/// own reason — none merged into a neighbour, none lost at the end.
#[test]
fn every_server_close_reason_survives_the_wire() {
    use gsb_core::conn::ServerClose;
    let mut sent = three_shards();
    for (i, reason) in ServerClose::ALL.iter().enumerate() {
        for _ in 0..=i {
            sent.net.server_closes.add(*reason);
        }
    }
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    for (i, reason) in ServerClose::ALL.iter().enumerate() {
        assert_eq!(
            got.net.server_closes.get(*reason),
            i as u64 + 1,
            "{reason:?}"
        );
    }
    assert_eq!(got.net.server_closes.get(ServerClose::IdleInput), 12);
    assert_eq!(got.net.server_closes.get(ServerClose::Kicked), 13);
}

/// What the stopping rooms still held (GSMV, B68) crosses the wire per
/// room, every counter in its slot, and folds as a SUM.
#[test]
fn the_stop_counts_survive_the_wire_and_fold_as_sums() {
    let mut sent = three_shards();
    let mut values = [0u64; gsb_core::metrics::STOP_COUNT];
    for (i, v) in values.iter_mut().enumerate() {
        *v = 70 + i as u64;
    }
    sent.rooms[2].stop = gsb_core::metrics::StopCounts::from_values(values);
    let got = decode_report(&encode_report(&sent)[8..]).expect("decodes");
    for (a, b) in sent.rooms.iter().zip(&got.rooms) {
        assert_eq!(a.stop, b.stop);
    }
    let folded = fold_rooms(&sent).expect("three rows");
    assert_eq!(
        folded.stop.joins_unprocessed,
        1 + 2 + 70,
        "summed over rows"
    );
    assert_eq!(folded.stop.effects_unsent, 3 + 75);
}
