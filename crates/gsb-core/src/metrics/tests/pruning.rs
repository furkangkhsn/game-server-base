//! Table pruning: a destroyed room's accumulator is retired after its
//! grace windows (stragglers suppressed), and a closing connection
//! retires its per-connection entry into the monotonic total.

use super::*;

/// Table-prune lock 3a — a destroyed room's accumulator is dropped
/// when its grace windows run down instead of living forever; its
/// final measured window still reaches the reports (the destroy can
/// land between the room's last sample and the next report), the
/// dying producers' stragglers do NOT refresh it, and once the
/// windows burn down a re-created id reports normally again.
#[test]
fn destroyed_room_accumulator_is_pruned_and_stragglers_suppressed() {
    let mut acc = MetricAccumulator::default();
    let t = Instant::now();

    // Live room → reported.
    acc.apply(MetricsEvent::Room(room_sample(RoomId(3), t, 30)));
    assert_eq!(acc.report(t).rooms.len(), 1);

    // Destroyed → lingers: the notice lands after the room's last
    // sample, so this report must still carry its final window.
    acc.apply(MetricsEvent::RoomGone(RoomId(3)));
    let r = acc.report(t); // burns linger window 2 → 1
    assert_eq!(r.rooms.len(), 1, "the final measured window survives");
    assert_eq!(r.rooms[0].steps, 30);

    // The dead incarnation's shutdown-tick straggler (it lost the
    // race against the notice on the shared FIFO) neither refreshes
    // nor resurrects the entry.
    acc.apply(MetricsEvent::Room(room_sample(RoomId(3), t, 31)));
    let r = acc.report(t); // burns the last window 1 → 0: dropped
    assert_eq!(r.rooms.len(), 1, "still inside the linger window");
    assert_eq!(r.rooms[0].steps, 30, "frozen: straggler not applied");

    // Windows ran down: the accumulator is gone, and a further
    // sample is treated as a fresh incarnation's first (destroy →
    // re-create works; counters restart from zero by contract).
    acc.apply(MetricsEvent::Room(room_sample(RoomId(3), t, 32)));
    let r = acc.report(t);
    assert_eq!(r.rooms.len(), 1);
    assert_eq!(r.rooms[0].steps, 32, "the id is reusable after the linger");
}

/// Table-prune lock 3b — a closed connection's `conn_actions_dropped`
/// entry retires at its final flush (`last: true`), keeping the map
/// live-connections-only, while the cumulative net total stays
/// monotonic (retired drops fold into it).
#[test]
fn closing_connection_retires_its_actions_dropped_entry() {
    let mut acc = MetricAccumulator::default();
    let c1 = ConnectionId(1);
    let c2 = ConnectionId(2);

    // Two live-flush deltas accumulate under c1 and show in top.
    for delta in [3u64, 2] {
        acc.apply(MetricsEvent::Conn(ConnSample {
            conn: c1,
            bytes_in: 0,
            bytes_out: 0,
            frames_in: 0,
            frames_out: 0,
            actions_dropped: delta,
            metrics_dropped: 0,
            violations: 0,
            last: false,
        }));
    }
    let r = acc.report(Instant::now());
    assert_eq!(r.net.actions_dropped, 5);
    assert_eq!(r.actions_dropped_top, vec![(c1, 5)]);

    // The final flush may itself carry a last delta; everything folds
    // into the cumulative total and the entry retires.
    acc.apply(MetricsEvent::Conn(ConnSample {
        conn: c1,
        bytes_in: 0,
        bytes_out: 0,
        frames_in: 0,
        frames_out: 0,
        actions_dropped: 1,
        metrics_dropped: 0,
        violations: 0,
        last: true,
    }));
    // A different connection keeps dropping afterwards.
    acc.apply(MetricsEvent::Conn(ConnSample {
        conn: c2,
        bytes_in: 0,
        bytes_out: 0,
        frames_in: 0,
        frames_out: 0,
        actions_dropped: 4,
        metrics_dropped: 0,
        violations: 0,
        last: false,
    }));
    let r = acc.report(Instant::now());
    assert_eq!(
        r.net.actions_dropped, 10,
        "cumulative total includes retired drops (5 + 1 + 4)"
    );
    assert_eq!(
        r.actions_dropped_top,
        vec![(c2, 4)],
        "only the live dropper is attributed"
    );

    // Idempotent close (no double-retire): a stray second `last`
    // sample changes nothing.
    acc.apply(MetricsEvent::Conn(ConnSample {
        conn: c1,
        bytes_in: 0,
        bytes_out: 0,
        frames_in: 0,
        frames_out: 0,
        actions_dropped: 0,
        metrics_dropped: 0,
        violations: 0,
        last: true,
    }));
    let r = acc.report(Instant::now());
    assert_eq!(r.net.actions_dropped, 10);
    assert_eq!(r.actions_dropped_top, vec![(c2, 4)]);
}
