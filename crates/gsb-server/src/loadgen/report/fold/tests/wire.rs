//! The orchestrator's metrics wire (`codec.rs`) carries every shard's
//! detach-ceiling, remote-effect and migration counters: a separate-
//! process run folds the same numbers an in-process run does.

use super::*;
use crate::codec::{decode_report, encode_report};

/// The nine counters, as one comparable tuple.
fn counters(r: &RoomReport) -> [u64; 9] {
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
