//! The record run (`WorldSnapshot.records`, field 6) through the view:
//! every frame of a sequence in both framings — the entities form
//! through a decoder that did not opt in, the run form through one that
//! did — gives the same outcomes and the same view; the run keeps the
//! application order (`removed` → `cell_exits` → records); and a frame
//! the decoder cannot take is an error that changes nothing.

use prost::encoding::encode_varint;

use super::*;
use crate::testing::{Dec, packed_body};

type RunView = ClientView<Dec<true>>;

impl Frame {
    /// The run form of this frame: what a kit room of a game whose codec
    /// opted into the run writes (header, `removed`, `cell_exits`, then
    /// ONE `records` field — absent when there are no records).
    fn run(&self) -> Vec<u8> {
        let mut out = BytesMut::new();
        write_snapshot_header(&mut out, self.sequence, self.delta);
        out.extend_from_slice(&encode_entity_exits(&self.removed));
        for &cell in &self.exits {
            let grid = Grid2::new(CELL);
            out.extend_from_slice(&encode_cell_exit::<WirePos, _>(&grid, cell));
        }
        let run = self.run_body();
        if !run.is_empty() {
            out.extend_from_slice(&[0x32]);
            encode_varint(run.len() as u64, &mut out);
            out.extend_from_slice(&run);
        }
        out.to_vec()
    }

    /// The records as a run: `id varint + packed body` each.
    fn run_body(&self) -> Vec<u8> {
        let mut run = BytesMut::new();
        for &(id, x, y) in &self.records {
            encode_varint(id, &mut run);
            packed_body(&WirePos { x, y }, &mut run);
        }
        run.to_vec()
    }

    /// The run form through a generated encoder (field order by number,
    /// packed `removed`).
    fn run_generated(&self) -> Vec<u8> {
        proto::WorldSnapshot {
            entities: Vec::new(),
            records: self.run_body(),
            ..self.message()
        }
        .encode_to_vec()
    }
}

fn run_sorted(view: &RunView) -> Vec<(u64, i32, i32)> {
    let mut v: Vec<_> = view.iter().map(|(id, &(x, y))| (id, x, y)).collect();
    v.sort_unstable();
    v
}

/// A long sequence — fulls, deltas across gaps, removals, cell exits, an
/// exit and a re-entry in one delta, a duplicate, a baseline-less delta,
/// runs longer than 127 bytes — gives the same outcome and view in both
/// framings, frame by frame, the kit's form and a generated encoder's.
#[test]
fn a_run_applies_exactly_like_entities() {
    let many: Vec<(u64, i32, i32)> = (1..=60)
        .map(|i| (i * 300, i as i32 * 7, -(i as i32)))
        .collect();
    let frames = [
        Frame::delta(1, &[(1, 0, 0)]),
        Frame::full(2, &many),
        Frame::delta(3, &[(300, 21, 0), (5, 5, 5)]).removing(&[600, 900]),
        Frame::delta(9, &[(1200, 30, 5)]).exiting(&[Cell(1, 0), Cell(0, -1)]),
        Frame::delta(9, &[(7, 7, 7)]),
        Frame::delta(10, &[(5, -45, 3)]).removing(&[5]),
        Frame::delta(11, &[]).removing(&[1500]),
        Frame::full(12, &[]),
        Frame::delta(13, &many[..20]),
    ];
    let (mut ent, mut run, mut gen_run) = (
        View::default(),
        RunView::new(Dec::new(CELL)),
        RunView::new(Dec::new(CELL)),
    );
    for f in &frames {
        let a = ent.apply_snapshot(&f.kit()).expect("entities");
        let b = run.apply_snapshot(&f.run()).expect("run");
        let c = gen_run
            .apply_snapshot(&f.run_generated())
            .expect("generated run");
        assert_eq!((a, a), (b, c), "{f:?}");
        assert_eq!(sorted(&ent), run_sorted(&run), "{f:?}");
        assert_eq!(run_sorted(&run), run_sorted(&gen_run), "{f:?}");
    }
    assert_eq!(ent.counters(), run.counters());
    assert!(
        frames[1].run_body().len() >= 0x80,
        "a multi-byte run length"
    );
    let c = run.counters();
    assert_eq!((c.fulls, c.deltas, c.gap_drops, c.stale), (2, 5, 1, 1));
}

/// The order in a run frame: `removed`, then `cell_exits`, then the
/// run — a record removed and re-added, or exited with its cell and
/// back in it, in the same delta is present with its new value.
#[test]
fn a_run_is_applied_after_removed_and_cell_exits() {
    let mut view = RunView::new(Dec::new(CELL));
    let full = Frame::full(1, &[(1, 21, 0), (2, 22, 1), (3, 0, 0)]);
    view.apply_snapshot(&full.run()).expect("full");
    let delta = Frame::delta(2, &[(1, 30, 5), (3, 1, 1)])
        .removing(&[3])
        .exiting(&[Cell(1, 0)]);
    view.apply_snapshot(&delta.run()).expect("delta");
    assert_eq!(run_sorted(&view), [(1, 30, 5), (3, 1, 1)], "2 is gone");
}

/// A frame with a record run through a decoder that did not opt in
/// (`ClientDecoder::RUN` false) is `UnexpectedRun`: rejected before
/// the view changes (the view, its baseline and its sequence stay), as
/// a group frame and as the one-shot private full.
#[test]
fn a_run_to_a_decoder_that_did_not_opt_in_is_an_error() {
    let mut view = View::default();
    view.apply_snapshot(&Frame::full(1, &[(1, 0, 0)]).kit())
        .expect("entities");
    let delta = Frame::delta(2, &[(2, 5, 5)]).removing(&[1]);
    assert_eq!(
        view.apply_snapshot(&delta.run()),
        Err(ClientError::UnexpectedRun)
    );
    let one_shot = proto::Private {
        payload: Some(proto::private::Payload::Snapshot(proto::WorldSnapshot {
            sequence: 3,
            records: Frame::full(3, &[(9, 9, 9)]).run_body(),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(
        view.apply_private(&one_shot.encode_to_vec()),
        Err(ClientError::UnexpectedRun)
    );
    assert_eq!(sorted(&view), [(1, 0, 0)]);
    assert_eq!(view.last_sequence(), Some(1));
    assert_eq!(view.counters().errors, 2);
}

/// A frame's records ride ONE field: records in both `entities` and a
/// run, two runs, or a field 6 that is not length-delimited are
/// malformed — rejected before the view changes.
#[test]
fn records_in_two_places_are_malformed() {
    let base = Frame::full(1, &[(1, 0, 0)]);
    let mut both = Frame::delta(2, &[(2, 1, 1)]).run();
    both.extend_from_slice(&[0x12, 0x02, 0x08, 0x05]); // an entities entry
    let mut run_after = Frame::delta(2, &[(2, 1, 1)]).kit();
    run_after.extend_from_slice(&[0x32, 0x03, 0x03, 0x02, 0x02]); // then a run
    let mut twice = Frame::delta(2, &[(2, 1, 1)]).run();
    twice.extend_from_slice(&[0x32, 0x03, 0x03, 0x02, 0x02]); // a second run
    let mut varint = Frame::delta(2, &[]).run();
    varint.extend_from_slice(&[0x30, 0x07]); // field 6 as a varint
    for (what, frame) in [
        ("run, then entities", both),
        ("entities, then a run", run_after),
        ("two runs", twice),
        ("not bytes", varint),
    ] {
        let mut view = RunView::new(Dec::new(CELL));
        view.apply_snapshot(&base.run()).expect("full");
        let got = view.apply_snapshot(&frame);
        assert!(
            matches!(got, Err(ClientError::Malformed(_))),
            "{what}: {got:?}"
        );
        assert_eq!(run_sorted(&view), [(1, 0, 0)], "{what}");
        assert_eq!(view.last_sequence(), Some(1), "{what}");
    }
}

/// A run record the decoder cannot read (a truncated body) is found
/// while the view is changing: the view is left empty and without a
/// baseline — never half a frame — like an undecodable `entities` body.
#[test]
fn a_truncated_run_leaves_the_view_without_a_baseline() {
    let mut view = RunView::new(Dec::new(CELL));
    view.apply_snapshot(&Frame::full(1, &[(1, 0, 0)]).run())
        .expect("full");
    let mut bad = BytesMut::new();
    write_snapshot_header(&mut bad, 2, true);
    bad.extend_from_slice(&encode_entity_exits(&[1]));
    bad.extend_from_slice(&[0x32, 0x04, 0x02, 0x02, 0x02, 0x03]); // id 2 at (1, 1), then id 3 alone
    let got = view.apply_snapshot(&bad);
    assert!(matches!(got, Err(ClientError::Malformed(_))), "{got:?}");
    assert!(view.is_empty() && !view.has_baseline());
}
