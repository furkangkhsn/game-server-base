//! The cross-seam surface (`docs/CROSS-SHARD.md` §2–§4), driven on BARE
//! actors (the test holds the neighbour links' receiving ends and feeds
//! CONTROL by hand): the tick hooks read the borrowed strip; an effect
//! goes to the neighbour that lent its target; a duplicate applies once
//! and the duplicate state stays bounded; the effects of one tick apply
//! in a deterministic order. Forwarding, the full-link policy and the
//! envelope checks are in `effects/flow.rs`.

use super::*;

mod flow;

/// One effect as a neighbour would send it (epoch 0 — the rigs' epoch).
pub(in crate::shard::tests) fn fx(
    target: u64,
    source: u64,
    origin: usize,
    seq: u64,
    at_tick: u64,
) -> RemoteEffect {
    RemoteEffect {
        target,
        source,
        id: EffectId {
            origin,
            epoch: 0,
            seq,
        },
        at_tick,
        hops: 0,
        payload: bytes::Bytes::from_static(b"hit"),
    }
}

/// A Full border exchange from `from` lending `wires` (at x = 0).
pub(in crate::shard::tests) fn lend(from: usize, wires: &[u64]) -> ShardMsg<TState, TStrip> {
    ShardMsg::Border {
        from,
        exchange: BorderExchange::Full {
            seq: 0,
            tick: 0,
            entities: wires.iter().map(|&w| rec(w, 0, 0)).collect(),
        },
    }
}

/// The remote effects among `msgs`.
pub(in crate::shard::tests) fn effects_in(
    msgs: Vec<ShardMsg<TState, TStrip>>,
) -> Vec<RemoteEffect> {
    msgs.into_iter()
        .filter_map(|m| match m {
            ShardMsg::RemoteEffect(e) => Some(e),
            _ => None,
        })
        .collect()
}

pub(in crate::shard::tests) fn drain<T>(rx: &mut Inbox<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        out.push(m);
    }
    out
}

/// Part 1: the tick hooks see the borrowed strip as the snapshot does —
/// every neighbour's latest exchange, a quarantined view excluded.
#[test]
fn the_tick_hooks_read_the_borrowed_strip() {
    let (tx, _rx) = mpsc::channel(16);
    let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
    let mut s1 = rig_actor(1, vec![tx, d]);
    assert!(s1.handle_msg(lend(0, &[7, 5]), 1));
    assert!(s1.step_phases(&tinfo(1)));
    assert_eq!(s1.world.seen_lent, [(5, 0), (7, 0)], "both lent records");

    // A delta out of sequence quarantines the view: the hooks stop
    // seeing it exactly when the snapshot stops rendering it.
    let gap = ShardMsg::Border {
        from: 0,
        exchange: BorderExchange::Delta {
            seq: 9,
            tick: 2,
            upserts: vec![rec(8, 0, 0)],
            exits: vec![],
        },
    };
    assert!(s1.handle_msg(gap, 2));
    assert!(s1.step_phases(&tinfo(2)));
    assert!(s1.world.seen_lent.is_empty(), "{:?}", s1.world.seen_lent);
}

/// An effect goes to the neighbour that LENDS its target (the entity's
/// authority as of that neighbour's last exchange); an unlent target is
/// refused at the source, and nothing is sent.
#[test]
fn an_effect_goes_to_the_neighbour_that_lent_its_target() {
    let (tx1, mut rx1) = mpsc::channel(64);
    let (tx2, mut rx2) = mpsc::channel(64);
    let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
    let mut s0 = rig_actor(0, vec![d, tx1, tx2]);
    assert!(s0.handle_msg(lend(1, &[11]), 4));
    assert!(s0.handle_msg(lend(2, &[22]), 4));
    s0.world.script = vec![(22, 3), (11, 3), (99, 3)];
    assert!(s0.step_phases(&tinfo(5)));

    let id = |seq| EffectId {
        origin: 0,
        epoch: 0,
        seq,
    };
    assert_eq!(
        s0.world.emits,
        [Ok(id(1)), Ok(id(2)), Err(EmitRefused::NotLent)]
    );
    let to2 = effects_in(drain(&mut rx2));
    let to1 = effects_in(drain(&mut rx1));
    assert_eq!(to2, [fx(22, 3, 0, 1, 5)], "the lender of 22");
    assert_eq!(to1, [fx(11, 3, 0, 2, 5)], "the lender of 11");
    assert_eq!((s0.effects.stats.emitted, s0.effects.stats.refused), (2, 1));
    assert_eq!(s0.sample().effects_refused, 1, "the refusal, as sampled");
}

/// The per-tick budget refuses synchronously — nothing is dropped
/// behind the caller's back.
#[test]
fn the_emit_budget_refuses_at_the_source() {
    let (tx1, mut rx1) = mpsc::channel(1024);
    let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
    let mut s0 = rig_actor(0, vec![d, tx1]);
    assert!(s0.handle_msg(lend(1, &[11]), 1));
    s0.world.script = vec![(11, 3); EFFECT_BUDGET_PER_TICK + 1];
    assert!(s0.step_phases(&tinfo(2)));
    assert_eq!(s0.world.emits.last(), Some(&Err(EmitRefused::Budget)));
    let sent = effects_in(drain(&mut rx1)).len();
    assert_eq!(sent, EFFECT_BUDGET_PER_TICK);
}

/// An authority on `wires`, its links dead-ended (nothing sent matters).
fn authority(wires: &[u64]) -> ShardActor<TWorld, (), TState, TStrip> {
    let (tx, _rx) = mpsc::channel(4096);
    let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
    let mut s1 = rig_actor(1, vec![tx, d]);
    for &w in wires {
        put(&mut s1, w, 5.0, 0.0);
    }
    s1
}

/// Idempotency: the same effect delivered twice in one tick and again a
/// tick later (an at-least-once link) applies ONCE.
#[test]
fn a_duplicate_effect_applies_once() {
    let mut s1 = authority(&[40]);
    let e = fx(40, 3, 0, 1, 1);
    assert!(s1.handle_msg(ShardMsg::RemoteEffect(e.clone()), 2));
    assert!(s1.handle_msg(ShardMsg::RemoteEffect(e.clone()), 2));
    assert!(s1.step_phases(&tinfo(2)));
    assert!(s1.handle_msg(ShardMsg::RemoteEffect(e.clone()), 3));
    assert!(s1.step_phases(&tinfo(3)));
    assert_eq!(s1.world.applied, [e]);
    assert_eq!(s1.effects.stats.duplicates, 2);
    // The sample reports the one application; duplicates are none of
    // its five counters.
    let s = s1.sample();
    assert_eq!(s.effects_applied, 1);
    assert_eq!(
        (s.effects_dropped, s.effects_orphaned, s.effects_refused),
        (0, 0, 0)
    );
}

/// The duplicate state is one fixed-size window per origin shard, and it
/// never refuses a genuine effect: 20 000 effects from one origin — the
/// full budget every tick — all apply, the table does not grow, and a
/// replay (inside the window or long past it) is refused.
#[test]
fn the_duplicate_state_is_bounded() {
    let mut s1 = authority(&[40]);
    let per_tick = EFFECT_BUDGET_PER_TICK as u64;
    let mut seq = 0;
    for tick in 2..2 + 20_000 / per_tick {
        // Delivered newest-first: reordering inside a tick is harmless.
        for _ in 0..per_tick {
            seq += 1;
            let e = fx(40, 3, 0, seq, tick - 1);
            assert!(s1.handle_msg(ShardMsg::RemoteEffect(e), tick));
        }
        s1.effects.pending.reverse();
        assert!(s1.step_phases(&tinfo(tick)));
    }
    let tick = 2 + 20_000 / per_tick;
    assert_eq!(s1.world.applied.len() as u64, seq, "no false duplicate");
    assert_eq!(s1.effects.windows.len(), 2, "one window per origin shard");
    assert_eq!(
        std::mem::size_of_val(&s1.effects.windows[0]),
        8 + EFFECT_WINDOW as usize / 8,
        "a window is a high-water mark and a fixed bitmap"
    );
    for old in [seq, seq - EFFECT_WINDOW + 1, seq - EFFECT_WINDOW, 1] {
        assert!(s1.handle_msg(ShardMsg::RemoteEffect(fx(40, 3, 0, old, tick - 1)), tick));
    }
    assert!(s1.step_phases(&tinfo(tick)));
    assert_eq!(s1.world.applied.len() as u64, seq, "no replay applied");
    assert_eq!(s1.effects.stats.duplicates, 4);
}

/// CROSS-SHARD §4 layer 3: the effects due in one tick apply sorted by
/// `(source, origin, seq)`, whatever order the links delivered them in —
/// and an effect that arrived EARLY (stamped this very tick: this shard
/// ran behind its origin) waits for the next tick instead of splitting
/// its tick's batch by scheduling luck.
#[test]
fn the_effects_of_one_tick_apply_in_a_deterministic_order() {
    let mut s1 = authority(&[40, 41]);
    let delivered = [
        fx(40, 9, 0, 4, 6),
        fx(41, 3, 1, 2, 6),
        fx(40, 3, 0, 7, 6),
        fx(41, 5, 0, 1, 6),
        fx(40, 3, 0, 5, 6),
        fx(40, 1, 0, 9, 7), // early: its origin is already at tick 7
    ];
    for e in delivered.iter().cloned() {
        assert!(s1.handle_msg(ShardMsg::RemoteEffect(e), 7));
    }
    assert!(s1.step_phases(&tinfo(7)));
    let order: Vec<(u64, usize, u64)> = s1
        .world
        .applied
        .iter()
        .map(|e| (e.source, e.id.origin, e.id.seq))
        .collect();
    assert_eq!(
        order,
        [(3, 0, 5), (3, 0, 7), (3, 1, 2), (5, 0, 1), (9, 0, 4)]
    );
    assert!(s1.step_phases(&tinfo(8)));
    assert_eq!(
        s1.world.applied.len(),
        6,
        "the early one applied a tick later"
    );
    assert_eq!(s1.world.applied[5], delivered[5]);
}

/// The sample's five effect counters from the shard's full split: each
/// maps one-to-one, except `dropped`, which is every in-transit loss
/// (full retry buffer, closed link, hop bound, age bound) — and nothing
/// else (the game's refusals, duplicates and foreign effects are not
/// losses on the way).
#[test]
fn the_sample_folds_the_effect_counters_into_five() {
    let mut s = authority(&[]);
    let st = &mut s.effects.stats;
    st.emitted = 1000;
    st.refused = 3;
    st.sent = 1000;
    st.retried = 1000;
    st.dropped_full = 5;
    st.dropped_closed = 7;
    st.expired = 11;
    st.received = 1000;
    st.applied = 13;
    st.rejected = 1000;
    st.orphaned = 17;
    st.duplicates = 1000;
    st.forwarded = 19;
    st.dropped_hops = 23;
    st.foreign = 1000;
    let x = s.sample();
    assert_eq!(
        [
            x.effects_applied,
            x.effects_forwarded,
            x.effects_orphaned,
            x.effects_dropped,
            x.effects_refused,
        ],
        [13, 19, 17, 5 + 7 + 11 + 23, 3]
    );
}
