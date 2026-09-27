//! A stopping room's final sample (BACKLOG B62, `MetricsEvent::RoomFinal`)
//! in the accumulator: taken inside the destroyed-room linger, where a
//! periodic straggler is refused; and starting that linger itself when
//! no `RoomGone` did, so a final sample never leaves a ghost row.

use super::*;

/// The registry's notice first (the destroy's order), then the room's
/// final sample: the lingering row takes it and reports it, a later
/// straggler still does not refresh it, and the row goes when the
/// windows run down.
#[test]
fn the_final_sample_lands_in_the_lingering_row() {
    let mut acc = MetricAccumulator::default();
    let t = Instant::now();
    acc.apply(MetricsEvent::Room(room_sample(RoomId(3), t, 30)));
    acc.apply(MetricsEvent::RoomGone(RoomId(3)));
    let mut last = room_sample(RoomId(3), t, 31);
    last.requests_dropped_unread = 2;
    acc.apply(MetricsEvent::RoomFinal(last));
    acc.apply(MetricsEvent::Room(room_sample(RoomId(3), t, 32)));
    let r = acc.report(t);
    assert_eq!(r.rooms.len(), 1);
    assert_eq!(r.rooms[0].steps, 31, "the final sample, not the straggler");
    assert_eq!(r.rooms[0].requests_dropped_unread, 2);
    assert_eq!(acc.report(t).rooms.len(), 1, "the second linger window");
    assert!(acc.report(t).rooms.is_empty(), "then the row is gone");
}

/// No notice (lost to a full channel, or never sent): the final sample
/// starts the linger itself — reported, then dropped, never a ghost. A
/// room that never sampled before its stop is reported too.
#[test]
fn a_final_sample_without_a_notice_starts_the_linger() {
    let mut acc = MetricAccumulator::default();
    let t = Instant::now();
    acc.apply(MetricsEvent::RoomFinal(room_sample(RoomId(4), t, 9)));
    let r = acc.report(t);
    assert_eq!(r.rooms.len(), 1, "a room that never sampled is reported");
    assert_eq!(r.rooms[0].steps, 9);
    acc.apply(MetricsEvent::Room(room_sample(RoomId(4), t, 10)));
    assert_eq!(acc.report(t).rooms[0].steps, 9, "stragglers refused");
    assert!(acc.report(t).rooms.is_empty(), "no ghost row");
}
