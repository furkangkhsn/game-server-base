//! The border protocol: FIFO/drop semantics of the in-process
//! link, and the full/delta exchange with its sequence gaps,
//! resyncs and periodic fulls.

use super::*;

mod resync;

/// Drive an [`InProcLink`] directly: FIFO order on drain; a send onto
/// a full link returns `LinkFull::Full` carrying the rejected message
/// without disturbing what is already queued; a drained link accepts
/// again; a link whose receive end is gone reports `Closed`.
#[test]
fn inproc_link_preserves_fifo_and_drop_semantics() {
    let (tx, rx) = channel::<ShardMsg<TState, TStrip>>(2);
    let mut link = InProcLink {
        tx: Some(tx),
        rx: Some(rx),
        mode: ExchangeMode::Delta,
    };
    // Fill to capacity, then one more: the third send must be refused
    // whole — nothing of it queued.
    for from in 0..2usize {
        assert!(
            link.send(ShardMsg::ResyncRequest { from }).is_ok(),
            "send {from} onto an empty/capacity-2 link"
        );
    }
    match link.send(ShardMsg::ResyncRequest { from: 2 }).unwrap_err() {
        LinkFull::Full {
            msg: ShardMsg::ResyncRequest { from },
        } => assert_eq!(from, 2, "the EXACT refused message comes back"),
        other => panic!("expected Full carrying the message, got {other:?}"),
    }
    // FIFO preserved and no partial state: exactly the two accepted
    // sends, in order; the rejected third did not squeeze in.
    let drained = link.drain();
    assert_eq!(drained.len(), 2, "only the accepted sends deliver");
    for (i, m) in drained.iter().enumerate() {
        match m {
            ShardMsg::ResyncRequest { from } => assert_eq!(*from, i),
            other => panic!("unexpected message in drain: {other:?}"),
        }
    }
    // An emptied link accepts again (the channel semantics, not a
    // poisoned wrapper).
    assert!(link.send(ShardMsg::ResyncRequest { from: 7 }).is_ok());
    assert!(link.send(ShardMsg::ResyncRequest { from: 8 }).is_ok());
    assert!(link.send(ShardMsg::ResyncRequest { from: 9 }).is_err());
    let drained = link.drain();
    assert_eq!(drained.len(), 2);
    assert!(matches!(drained[0], ShardMsg::ResyncRequest { from: 7 }));
    assert!(matches!(drained[1], ShardMsg::ResyncRequest { from: 8 }));
    assert!(link.drain().is_empty(), "drain empties fully");

    // Closed: a link whose receive end is gone refuses with `Closed`
    // (not Full), still handing the message back; a send-only link's
    // drain is simply empty.
    let (tx, rx) = channel::<ShardMsg<TState, TStrip>>(1);
    drop(rx);
    let mut dead = InProcLink {
        tx: Some(tx),
        rx: None,
        mode: ExchangeMode::AlwaysFull,
    };
    match dead.send(ShardMsg::ResyncRequest { from: 5 }).unwrap_err() {
        LinkFull::Closed {
            msg: ShardMsg::ResyncRequest { from },
        } => assert_eq!(from, 5),
        other => panic!("expected Closed carrying the message, got {other:?}"),
    }
    assert!(dead.drain().is_empty());
}

/// Delta lock 1 — an entity entering the strip appears in the
/// neighbor's view; moving updates it in place; leaving removes it
/// (no ghost). The bootstrap is an explicit Full; every later step is
/// a minimal delta.
#[tokio::test]
async fn delta_exchange_applies_upserts_and_exits_correctly() {
    let mut r = BorderRig::new();

    // Enter: first contact ships the whole strip as a Full...
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    let (seq, entities) = expect_full(&msgs);
    assert_eq!(
        entities,
        [rec(100, -1, 0)],
        "bootstrap Full carries the strip"
    );
    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs.len(), 1, "view established");
    assert_eq!(
        r.s1.border[&0].expected_seq,
        seq + 1,
        "the receiver expects the next sequence"
    );

    // Move: only the changed record ships, as an upsert delta.
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    let msgs = r.drain01();
    let (_seq2, upserts, exits) = expect_delta(&msgs);
    assert_eq!(upserts, [rec(100, -1, 1)]);
    assert!(exits.is_empty(), "a move is not an exit");
    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs[&100].state.y, 1, "position updated");

    // A second entity enters: only IT is new.
    put(&mut r.s0, 101, -1.0, 5.0);
    r.step0(3);
    let msgs = r.drain01();
    let (_seq3, upserts, _exits3) = expect_delta(&msgs);
    assert_eq!(upserts, [rec(101, -1, 5)]);
    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs.len(), 2);

    // Leave: an explicit exit record — the borrowed view must drop
    // the entity (a full-era wholesale replace never had this failure
    // mode; a delta without exits would ghost forever).
    let _ = r.s0.world.ents.remove(&101);
    r.step0(4);
    let msgs = r.drain01();
    let (_seq4, upserts, exits) = expect_delta(&msgs);
    assert!(upserts.is_empty(), "a leave is not an upsert");
    assert_eq!(exits, [101]);
    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs.len(), 1, "no ghost after the exit");
    assert!(!r.s1.border[&0].recs.contains_key(&101));
}
