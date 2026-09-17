//! The delta stream's recovery paths: a sequence gap, a rebuilt
//! shard's first exchange, the periodic full, and a refused send.

use super::*;

/// Delta lock 2 (§6.4 pin 3a) — a lost delta is DETECTED, not silently
/// diverged: the receiver rejects the next delta on its sequence
/// mismatch, quarantines the view, sends a ResyncRequest upstream, and
/// the serving Full restores a correct complete view.
#[tokio::test]
async fn seq_gap_triggers_resync_full() {
    let mut r = BorderRig::new();

    // Bootstrap: Full(seq=1) delivered → expected becomes 2.
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    r.deliver_to_s1(msgs);

    // THE LOSS: the next delta (seq=2, y→1) never arrives.
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    let lost = r.drain01();
    assert_eq!(lost.len(), 1, "the delta was sent — then dropped by us");
    // ...and discarded. Nothing delivered.

    // The NEXT delta (seq=3) carries the wrong sequence number.
    put(&mut r.s0, 100, -1.0, 2.0);
    r.step0(3);
    let msgs = r.drain01();
    let (seq, _, _) = expect_delta(&msgs);
    assert_eq!(seq, 3, "the sender stamped consecutively");
    r.deliver_to_s1(msgs); // rejected INSIDE handle_msg

    assert!(
        r.s1.border[&0].stale_until_full,
        "the mismatch quarantines the view"
    );
    assert_eq!(
        r.s1.border[&0].recs[&100].state.y, 0,
        "nothing after the last GOOD exchange was applied (no \
         half-applied state)"
    );
    assert_eq!(
        r.s1.bstats.resync_requests_sent, 1,
        "exactly one resync request went upstream"
    );
    // The request crossed back over the controlled channel:
    let requests: Vec<_> = {
        let mut out = Vec::new();
        while let Ok(m) = r.rx10.try_recv() {
            out.push(m);
        }
        out
    };
    assert!(
        requests
            .iter()
            .any(|m| matches!(m, ShardMsg::ResyncRequest { from: 1 })),
        "ResyncRequest flowed to the neighbor: {requests:?}"
    );
    r.deliver_to_s0(requests);

    // The healing Full: even with NO further changes the flagged
    // neighbor gets a Full next tick, and it restores the COMPLETE
    // current truth (including what the lost delta carried).
    r.step0(4);
    let msgs = r.drain01();
    let (_, entities) = expect_full(&msgs);
    assert_eq!(
        entities,
        [rec(100, -1, 2)],
        "the healing Full re-baselines everything"
    );
    r.deliver_to_s1(msgs);
    assert!(!r.s1.border[&0].stale_until_full, "quarantine lifted");
    assert_eq!(r.s1.border[&0].recs[&100].state.y, 2, "view correct again");
    assert!(
        !r.s1.border[&0].stale_until_full && r.s1.border[&0].expected_seq == 5,
        "sequence re-baselined past the healing Full"
    );
}

/// Delta lock 3 (pin 3b) — a rebuilt shard's fresh incarnation leads
/// with a FULL (its sender state starts empty), and the receiver
/// resets cleanly: the dead incarnation's records cannot ghost.
#[tokio::test]
async fn rebuilt_shard_first_exchange_is_full_and_resets_receiver() {
    let mut r = BorderRig::new();

    // Incarnation A establishes a populated view on shard 1.
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs.len(), 1);

    // REBUILD: a brand-new actor for shard 0 — fresh world (the new
    // incarnation respawned different entities), fresh export state,
    // its own channel to the SAME receiver.
    let (tx01p, mut rx01p) = mpsc::channel(16);
    let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
    std::mem::forget(_drx);
    let mut s0p = rig_actor(0, vec![d, tx01p]);
    put(&mut s0p, 200, -1.0, 7.0);
    assert!(s0p.step_phases(&tinfo(50)), "rebuilt shard runs");

    let mut first = Vec::new();
    while let Ok(m) = rx01p.try_recv() {
        first.push(m);
    }
    let (seq, entities) = expect_full(&first);
    assert_eq!(
        entities,
        [rec(200, -1, 7)],
        "the FRESH incarnation's first exchange is a Full of ITS strip"
    );
    r.deliver_to_s1(first);

    // The receiver reset cleanly: exactly the new incarnation's
    // records, old-incarnation ghost gone, sequence re-baselined.
    let view = &r.s1.border[&0];
    assert_eq!(view.recs.len(), 1, "whole-view replacement: {view:?}");
    assert!(view.recs.contains_key(&200), "new entity present");
    assert!(
        !view.recs.contains_key(&100),
        "the dead incarnation's record must not survive as a ghost"
    );
    assert_eq!(
        view.expected_seq,
        seq + 1,
        "expected sequence re-baselined from the new stream"
    );
    assert!(!view.stale_until_full);
}

/// Delta lock 4 (pin 3c) — the periodic sigorta: a quiet neighbor is
/// shipped NOTHING on ordinary ticks (the byte win), but the 256-tick
/// cadence forces a Full even with zero changes.
#[tokio::test]
async fn periodic_full_fires_on_cadence() {
    let mut r = BorderRig::new();

    // Bootstrap + one change establish a non-empty ledger.
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }

    // Quiet tick: no changes ⇒ NOTHING ships (this skip is the point
    // of the whole exercise).
    r.step0(3);
    assert!(r.drain01().is_empty(), "an unchanged strip ships nothing");

    // ...but the cadence tick forces a Full regardless of quietness.
    assert!(r.s0.world.ents.len() == 1, "still just the one entity");
    r.step0(BORDER_FULL_EVERY_TICKS);
    let msgs = r.drain01();
    let (_, entities) = expect_full(&msgs);
    assert_eq!(entities.len(), 1, "the Full carries the whole strip");

    // And quietness resumes right after.
    r.step0(BORDER_FULL_EVERY_TICKS + 1);
    assert!(
        r.drain01().is_empty(),
        "no change after the cadence ⇒ silent"
    );

    // Counter cross-check within this run: two Fulls (bootstrap +
    // periodic), one delta, zero drops.
    assert_eq!(r.s0.bstats.full_exchanges, 2);
    assert_eq!(r.s0.bstats.delta_exchanges, 1);
    assert_eq!(r.s0.bstats.export_drops, 0);
}

/// Delta lock 5 (backpressure correctness) — a try_send failure on a
/// DELTA marks that neighbor for a Full, which arrives on the very
/// next tick carrying the data the dropped delta would have brought:
/// divergence heals within ONE tick instead of the 256-tick cadence.
#[tokio::test]
async fn send_failure_marks_neighbor_for_full_resync() {
    let mut r = BorderRig::new();

    // Bootstrap normally.
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }

    // Saturate the neighbor mailbox: nothing else fits.
    while r
        .tx01
        .try_send(ShardMsg::ResyncRequest { from: 999 })
        .is_ok()
    {}

    // A strip change now ships a delta — which MUST fail.
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    assert_eq!(r.s0.bstats.delta_drops, 1, "the failed delta is counted");
    assert!(
        r.s0.export[&1].needs_full,
        "the failure flags the neighbor for a Full"
    );

    // Unblock the channel (drain the dummies AND anything else).
    while r.rx01.try_recv().is_ok() {}

    // Next tick, NO further changes: the flag alone forces a Full —
    // and it carries the position update the dropped delta had.
    r.step0(3);
    let msgs = r.drain01();
    let (_, entities) = expect_full(&msgs);
    assert_eq!(
        entities,
        [rec(100, -1, 1)],
        "the healing Full contains what the dropped delta carried"
    );
    r.deliver_to_s1(msgs);
    assert_eq!(
        r.s1.border[&0].recs[&100].state.y, 1,
        "the receiver converged despite the loss"
    );
    assert!(!r.s0.export[&1].needs_full, "flag consumed");
}

/// Delta lock 6 (pin 4) — the own-wins filter applies IDENTICALLY to
/// records that entered the view via a delta: an entity that just
/// migrated INTO this shard wins over the neighbor's stale borrowed
/// copy, so the snapshot lists it once, at the OWN position.
#[tokio::test]
async fn own_wins_filter_applies_to_delta_applied_records() {
    let mut r = BorderRig::new();

    // Shard 1 gains its own member at x = 0 (conn 10 ⇒ x = 0 per the
    // test logic's spawn rule; region 1, so nothing migrates).
    let (out_tx, mut out_rx) = mpsc::channel::<FrameBatch>(16);
    let (reply_tx, reply_rx) = oneshot::channel();
    assert!(r.s1.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(10),
            epoch: 1,
            out: out_tx,
            reply: reply_tx,
        },
        &tctx(1),
    ));
    let w_own = reply_rx.await.expect("join reply").expect("join ok").0;

    // Bootstrap an EMPTY strip from shard 0 (Full, first contact),
    // then apply a DELTA that inserts the stale borrowed copy of the
    // just-crossed own entity — the exact crossing-tick shape of pin
    // 4. The copy enters the view THROUGH the delta path.
    r.step0(1); // empty strip, first contact ⇒ Full{entities: []}
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }
    r.s1.handle_msg(
        ShardMsg::Border {
            from: 0,
            exchange: BorderExchange::Delta {
                seq: 2, // matches the expected sequence after the Full
                tick: 2,
                upserts: vec![rec(w_own, -9, 0)],
                exits: vec![],
            },
        },
        &tctx(2),
    );
    assert_eq!(
        r.s1.border[&0].recs.get(&w_own).map(|b| b.state.x),
        Some(-9),
        "the stale copy IS in the borrowed view (delta applied)"
    );

    // Broadcast: the snapshot must contain the entity EXACTLY ONCE,
    // at the OWN (fresh) position — the borrowed copy filtered.
    assert!(r.s1.step_phases(&tinfo(3)));
    let mut seen = Vec::new();
    while let Ok(batch) = out_rx.try_recv() {
        for f in batch {
            if f.op == 0x7100 {
                seen.extend_from_slice(&f.payload);
            }
        }
    }
    assert_eq!(
        seen.len(),
        16,
        "one 16-byte record total (own + filtered borrowed)"
    );
    let wire = u64::from_le_bytes(seen[0..8].try_into().unwrap());
    let x = i32::from_le_bytes(seen[8..12].try_into().unwrap());
    let y = i32::from_le_bytes(seen[12..16].try_into().unwrap());
    assert_eq!(
        (wire, x, y),
        (w_own, 0, 0),
        "the OWN record won over the delta-applied stale copy"
    );
}
// -----------------------------------------------------------------
// Rich-strip locks: the visibility-strip payload is the GAME's type
// ([`GameLogic::Strip`]). These locks prove the generalization does
// what the core-fixed record could not: a payload field beyond
// position must survive BOTH exchange paths (Full bootstrap and
// Delta upsert), and a change in ANY payload field — not just the
// coordinates — must fire the delta diff.
// -----------------------------------------------------------------
