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

/// The logic counters (GSMC) cross the wire name by name, with their
/// fold rules and the overflow count; an empty set stays empty. The
/// help line does not travel.
#[test]
fn the_logic_counters_survive_the_wire() {
    let mut sent = three_shards();
    sent.rooms[1].logic = LogicCounters::new();
    let frame = encode_report(&sent);
    assert_eq!(&frame[..4], b"GMSG", "the magic, little-endian GSMG");
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
