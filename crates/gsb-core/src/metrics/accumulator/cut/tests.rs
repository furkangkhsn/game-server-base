use std::time::Instant;

use super::*;
use crate::metrics::MetricsEvent;
use crate::metrics::tests::room_sample;
use crate::shard::sample_id;

/// An accumulator holding room `room`'s shard rows at these
/// `(steps, lagged_ticks)`.
fn with_rows(room: u64, rows: &[(u64, u64)]) -> MetricAccumulator {
    let mut acc = MetricAccumulator::default();
    for (index, &(steps, lagged)) in rows.iter().enumerate() {
        let mut s = room_sample(sample_id(RoomId(room), index), Instant::now(), steps);
        s.lagged_ticks = lagged;
        acc.apply(MetricsEvent::Room(s));
    }
    acc
}

/// The inverse of the shard's row id: a shard row names its room, a
/// single room's row names none.
#[test]
fn a_shard_row_names_its_room() {
    for (room, index) in [(1, 0), (1, 3), (7, 255), (u64::from(u16::MAX), 1)] {
        let row = sample_id(RoomId(room), index);
        assert_eq!(sharded_room(row), Some(RoomId(room)), "{row:?}");
    }
    assert_eq!(sharded_room(RoomId(1)), None);
    assert_eq!(sharded_room(RoomId((1 << 16) - 1)), None);
}

/// Steps apart on the same `lagged_ticks` are a round in flight; equal
/// rows, and rows apart on `lagged_ticks`, are not.
#[test]
fn a_round_in_flight_is_steps_apart_on_equal_lag() {
    assert!(with_rows(1, &[(60, 0), (60, 0), (30, 0), (30, 0)]).round_in_flight());
    assert!(with_rows(1, &[(60, 0), (57, 3), (60, 0), (30, 0)]).round_in_flight());
    assert!(with_rows(1, &[(60, 0), (57, 3), (27, 3)]).round_in_flight());
    assert!(!with_rows(1, &[(60, 0); 4]).round_in_flight());
    assert!(!with_rows(1, &[(60, 0), (57, 3), (58, 2)]).round_in_flight());
    assert!(!MetricAccumulator::default().round_in_flight());
}

/// Each room is asked on its own: two rooms at different rounds, each
/// whole, are no round in flight.
#[test]
fn rooms_are_asked_apart() {
    let mut acc = with_rows(1, &[(60, 0), (60, 0)]);
    for index in 0..2 {
        let s = room_sample(sample_id(RoomId(2), index), Instant::now(), 90);
        acc.apply(MetricsEvent::Room(s));
    }
    assert!(!acc.round_in_flight());
}

/// Lingering rows are not asked: a stopped room's (its shards' final
/// samples — each froze at its own step), and a dead shard's (its task
/// ended without one): the live rows alone decide.
#[test]
fn lingering_rows_are_not_asked() {
    let mut acc = with_rows(1, &[(60, 0), (60, 0), (30, 0)]);
    acc.apply(MetricsEvent::RoomEndedUncounted(sample_id(RoomId(1), 2)));
    assert!(!acc.round_in_flight());
    let mut acc = with_rows(2, &[]);
    for (index, steps) in [(0, 61), (1, 59)] {
        let s = room_sample(sample_id(RoomId(2), index), Instant::now(), steps);
        acc.apply(MetricsEvent::RoomFinal(s));
    }
    assert!(!acc.round_in_flight());
}
