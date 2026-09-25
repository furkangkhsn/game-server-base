//! Each client rule of `kit.proto`, one test each (both encodings of
//! every group frame — see `feed`).

use super::*;

/// A full replaces the whole view (nothing of the old one survives).
#[test]
fn a_full_replaces_the_view() {
    let (view, outcomes) = feed(&[
        Frame::full(1, &[(1, 0, 0), (2, 5, 5)]),
        Frame::full(2, &[(3, 45, 0)]),
    ]);
    assert_eq!(outcomes, [Apply::Full, Apply::Full]);
    assert_eq!(sorted(&view), [(3, 45, 0)]);
    assert_eq!(view.last_sequence(), Some(2));
}

/// A sequence `<=` the last accepted is discarded — a full and a delta
/// alike, equal or lower — and changes nothing.
#[test]
fn a_duplicate_or_older_sequence_is_discarded() {
    let (view, outcomes) = feed(&[
        Frame::full(5, &[(1, 0, 0)]),
        Frame::full(5, &[(2, 0, 0)]),
        Frame::full(4, &[(3, 0, 0)]),
        Frame::delta(5, &[(4, 0, 0)]).removing(&[1]),
        Frame::delta(3, &[]).exiting(&[Cell(0, 0)]),
    ]);
    assert_eq!(
        outcomes,
        [
            Apply::Full,
            Apply::Stale,
            Apply::Stale,
            Apply::Stale,
            Apply::Stale
        ]
    );
    assert_eq!(sorted(&view), [(1, 0, 0)]);
    assert_eq!(view.last_sequence(), Some(5));
    assert_eq!(view.counters().stale, 4);
    assert_eq!(view.counters().fulls, 1);
}

/// Before the first accepted frame there is no last accepted sequence,
/// so nothing is a duplicate: a full with the proto3-default sequence 0
/// (the kit's terminal `match_result` snapshot shape) is applied by a
/// fresh view. Once accepted, a second 0 is a duplicate.
#[test]
fn nothing_is_stale_before_the_first_accepted_frame() {
    let (view, outcomes) = feed(&[Frame::full(0, &[(7, 1, 1)]), Frame::full(0, &[])]);
    assert_eq!(outcomes, [Apply::Full, Apply::Stale]);
    assert_eq!(sorted(&view), [(7, 1, 1)]);
    assert!(view.has_baseline());
}

/// A delta without a baseline is dropped (counted), however many; the
/// next full establishes the baseline and deltas apply from there.
#[test]
fn a_delta_without_a_baseline_is_dropped_until_a_full() {
    let (view, outcomes) = feed(&[
        Frame::delta(3, &[(1, 0, 0)]),
        Frame::delta(4, &[(2, 0, 0)]),
        Frame::full(5, &[(3, 0, 0)]),
        Frame::delta(6, &[(4, 1, 1)]),
    ]);
    assert_eq!(
        outcomes,
        [
            Apply::NoBaseline,
            Apply::NoBaseline,
            Apply::Full,
            Apply::Delta
        ]
    );
    assert_eq!(sorted(&view), [(3, 0, 0), (4, 1, 1)]);
    assert_eq!(view.counters().gap_drops, 2);
    assert_eq!(view.counters().deltas, 1);
}

/// A delta with a baseline applies across a sequence gap (the stream is
/// event-driven: a gap is normal).
#[test]
fn a_delta_applies_across_a_sequence_gap() {
    let (view, outcomes) = feed(&[
        Frame::full(10, &[(1, 0, 0)]),
        Frame::delta(40, &[(1, 3, 3)]),
    ]);
    assert_eq!(outcomes, [Apply::Full, Apply::Delta]);
    assert_eq!(sorted(&view), [(1, 3, 3)]);
    assert_eq!(view.last_sequence(), Some(40));
}

/// `cell_exits` forgets every held record in the named cell — and only
/// those (the neighbour cell's record and the far one stay).
#[test]
fn a_cell_exit_forgets_exactly_that_cells_records() {
    let (view, _) = feed(&[
        Frame::full(1, &[(1, 21, 0), (2, 39, 19), (3, 5, 5), (4, -45, 0)]),
        Frame::delta(2, &[]).exiting(&[Cell(1, 0)]),
    ]);
    assert_eq!(sorted(&view), [(3, 5, 5), (4, -45, 0)]);
    let (view, _) = feed(&[
        Frame::full(1, &[(1, -45, 0), (2, 5, 5)]),
        Frame::delta(2, &[]).exiting(&[Cell(-3, 0), Cell(9, 9)]),
    ]);
    assert_eq!(sorted(&view), [(2, 5, 5)]);
}

/// The order: a record whose cell is exited AND which is upserted into
/// that same cell in the same delta (it left and came back) is present
/// with its new value — `cell_exits` before `entities`.
#[test]
fn a_record_that_exits_a_cell_and_reenters_it_in_one_delta_is_present() {
    let (view, _) = feed(&[
        Frame::full(1, &[(1, 21, 0), (2, 22, 1)]),
        Frame::delta(2, &[(1, 30, 5)]).exiting(&[Cell(1, 0)]),
    ]);
    assert_eq!(sorted(&view), [(1, 30, 5)], "1 came back, 2 is gone");
}

/// The order: an id both removed and upserted in the same delta is
/// present — `removed` before `entities`.
#[test]
fn a_removal_plus_a_readd_in_one_delta_is_present() {
    let (view, _) = feed(&[
        Frame::full(1, &[(1, 0, 0), (2, 1, 1)]),
        Frame::delta(2, &[(1, 2, 2)]).removing(&[1, 2]),
    ]);
    assert_eq!(sorted(&view), [(1, 2, 2)]);
}

/// The one-shot private full is applied UNCONDITIONALLY — over a newer
/// group sequence — and adopts its sequence: the group's next delta
/// applies on top of it.
#[test]
fn the_private_full_is_applied_unconditionally_and_adopts_its_sequence() {
    let mut view = View::default();
    view.apply_snapshot(&Frame::full(10, &[(1, 0, 0)]).kit())
        .expect("decodes");
    let got = view.apply_private(&Frame::full(7, &[(2, 5, 5)]).private());
    assert_eq!(got, Ok(PrivateEvent::Full { sequence: 7 }));
    assert_eq!(sorted(&view), [(2, 5, 5)]);
    assert_eq!(view.last_sequence(), Some(7));
    let next = view.apply_snapshot(&Frame::delta(8, &[(3, 1, 1)]).kit());
    assert_eq!(next.map(|s| s.apply), Ok(Apply::Delta));
    assert_eq!(sorted(&view), [(2, 5, 5), (3, 1, 1)]);
    let c = view.counters();
    assert_eq!((c.fulls, c.private_fulls, c.deltas), (2, 1, 1));
}

/// A fresh connection's crossing batch: the new group's delta (dropped,
/// no baseline) then the one-shot full (the baseline, same batch).
#[test]
fn the_private_full_is_the_baseline_of_a_fresh_view() {
    let mut view = View::default();
    let first = view.apply_snapshot(&Frame::delta(12, &[(1, 0, 0)]).kit());
    assert_eq!(first.map(|s| s.apply), Ok(Apply::NoBaseline));
    let full = view.apply_private(&Frame::full(12, &[(1, 0, 0), (2, 3, 3)]).private());
    assert_eq!(full, Ok(PrivateEvent::Full { sequence: 12 }));
    assert!(view.has_baseline());
    let next = view.apply_snapshot(&Frame::delta(13, &[]).removing(&[2]).kit());
    assert_eq!(next.map(|s| s.apply), Ok(Apply::Delta));
    assert_eq!(sorted(&view), [(1, 0, 0)]);
}

/// A private snapshot flagged `delta` is a protocol error: never
/// applied, counted as an error, not as a full.
#[test]
fn a_private_delta_is_an_error_and_changes_nothing() {
    let mut view = View::default();
    view.apply_snapshot(&Frame::full(1, &[(1, 0, 0)]).kit())
        .expect("decodes");
    let bad = Frame::delta(2, &[(2, 0, 0)]).removing(&[1]).private();
    assert_eq!(view.apply_private(&bad), Err(ClientError::PrivateDelta));
    assert_eq!(sorted(&view), [(1, 0, 0)]);
    assert_eq!(view.last_sequence(), Some(1));
    let c = view.counters();
    assert_eq!((c.fulls, c.private_fulls, c.errors), (1, 0, 1));
}

/// The private frame's other shapes: an ack, and no payload arm at all
/// (responses only) — neither touches the view.
#[test]
fn a_private_ack_or_empty_frame_leaves_the_view_alone() {
    let mut view = View::default();
    let ack = proto::Private {
        payload: Some(proto::private::Payload::Ack(proto::InputAck {
            processed_up_to: 42,
        })),
        ..Default::default()
    };
    assert_eq!(
        view.apply_private(&ack.encode_to_vec()),
        Ok(PrivateEvent::Ack(42))
    );
    let responses_only = proto::Private {
        responses: vec![gsb_protocol::base::RpcResponse::default()],
        ..Default::default()
    };
    assert_eq!(
        view.apply_private(&responses_only.encode_to_vec()),
        Ok(PrivateEvent::Empty)
    );
    // A session payload the decoder ignores (the default) is still
    // reported as one.
    let session = proto::Private {
        game: vec![1, 2, 3],
        ..responses_only
    };
    assert_eq!(
        view.apply_private(&session.encode_to_vec()),
        Ok(PrivateEvent::Session)
    );
    assert!(!view.has_baseline() && view.is_empty());
    assert_eq!(*view.counters(), Counters::default());
}
