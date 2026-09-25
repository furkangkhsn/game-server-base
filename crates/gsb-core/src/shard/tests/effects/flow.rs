//! Remote effects in flight: the target migrated on (forward to the new
//! owner, bounded by hops and by the table's TTL), a full link (retry
//! next tick, bounded, then expire), and the envelope checks (epoch,
//! origin, age).

use super::*;

type Rig = ShardActor<TWorld, (), TState, TStrip>;

/// Shard 0 owning wire 50 just across its edge: its next tick migrates
/// it to shard 1 (whose link the test holds).
fn crossing() -> (Rig, Inbox<ShardMsg<TState, TStrip>>) {
    let (tx1, rx1) = mpsc::channel(4096);
    let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
    let mut s0 = rig_actor(0, vec![d, tx1]);
    put(&mut s0, 50, 0.5, 0.0); // x ≥ 0: shard 1's region
    (s0, rx1)
}

/// The target left for shard 1 at tick 3. An effect aimed at the copy
/// shard 0 still lent — arriving while that copy is still in shard 0's
/// world (the despawn is phase 4 of the next tick) and after — is handed
/// on to shard 1, never applied to the doomed copy. Once the table's
/// TTL has passed, a late one is orphaned; the hop bound stops loops.
#[test]
fn an_effect_on_a_migrated_target_goes_on_to_its_new_owner() {
    let (mut s0, mut rx1) = crossing();
    assert!(s0.step_phases(&tinfo(3)));
    let sent = drain(&mut rx1);
    assert!(
        sent.iter()
            .any(|m| matches!(m, ShardMsg::Migrate { wire: 50, .. })),
        "the crossing was sent"
    );
    assert!(s0.world.ents.contains_key(&50), "still here until tick 4");

    assert!(s0.handle_msg(ShardMsg::RemoteEffect(fx(50, 9, 1, 1, 3)), 4));
    assert!(s0.step_phases(&tinfo(4)));
    let mut on = fx(50, 9, 1, 1, 3);
    on.hops = 1;
    assert_eq!(effects_in(drain(&mut rx1)), [on], "handed on, hop counted");
    assert!(
        s0.world.applied.is_empty(),
        "never applied to the doomed copy"
    );

    // At the hop bound it stops.
    let mut looping = fx(50, 9, 1, 2, 4);
    looping.hops = EFFECT_MAX_HOPS;
    assert!(s0.handle_msg(ShardMsg::RemoteEffect(looping), 5));
    assert!(s0.step_phases(&tinfo(5)));
    assert!(effects_in(drain(&mut rx1)).is_empty());
    assert_eq!(s0.effects.stats.dropped_hops, 1);

    // The table forgets it after the TTL: a late effect is orphaned.
    let lapse = 3 + EFFECT_FORWARD_TTL_TICKS;
    assert!(s0.step_phases(&tinfo(lapse)));
    assert!(
        s0.effects.forwarded.is_empty(),
        "the table is bounded by its TTL"
    );
    assert!(s0.handle_msg(ShardMsg::RemoteEffect(fx(50, 9, 1, 3, lapse)), lapse + 1));
    assert!(s0.step_phases(&tinfo(lapse + 1)));
    assert!(effects_in(drain(&mut rx1)).is_empty());
    assert_eq!(s0.effects.stats.orphaned, 1);
}

/// A target that comes BACK is this shard's again: effects on it apply
/// here instead of chasing its old move.
#[test]
fn a_target_that_migrates_back_is_hit_where_it_is() {
    let (mut s0, mut rx1) = crossing();
    assert!(s0.step_phases(&tinfo(3)));
    assert!(s0.step_phases(&tinfo(4)));
    assert!(!s0.world.ents.contains_key(&50), "gone at tick 4");
    let back = ShardMsg::Migrate {
        from: 1,
        at_tick: 4,
        wire: 50,
        state: TState {
            x: -0.5,
            y: 0.0,
            mode: 0,
        },
        player: None,
    };
    assert!(s0.handle_msg(back, 5));
    assert!(s0.handle_msg(ShardMsg::RemoteEffect(fx(50, 9, 1, 1, 4)), 5));
    assert!(s0.step_phases(&tinfo(5)));
    assert_eq!(s0.world.applied, [fx(50, 9, 1, 1, 4)]);
    assert!(effects_in(drain(&mut rx1)).is_empty());
}

/// The full-channel policy: a send into a full neighbour inbox waits in
/// the retry buffer and goes out next tick with its identity and stamp
/// unchanged; an effect that cannot go out within the age envelope
/// expires; the buffer never grows past its cap.
#[test]
fn a_full_link_retries_next_tick_within_bounds() {
    let (tx1, mut rx1) = mpsc::channel(1);
    let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
    let mut s0 = rig_actor(0, vec![d, tx1.clone()]);
    assert!(s0.handle_msg(lend(1, &[11]), 1));
    let plug = || ShardMsg::ResyncRequest { from: 0 };
    tx1.try_send(plug()).expect("room for the plug");
    s0.world.script = vec![(11, 3)];
    assert!(s0.step_phases(&tinfo(2)));
    assert_eq!(s0.effects.stats.retried, 1);
    assert_eq!(s0.effects.retry.len(), 1);

    drain(&mut rx1); // the neighbour catches up
    assert!(s0.step_phases(&tinfo(3)));
    assert_eq!(
        effects_in(drain(&mut rx1)),
        [fx(11, 3, 0, 1, 2)],
        "same id, same stamp"
    );
    assert!(s0.effects.retry.is_empty());

    // A neighbour that stays stuck: the effect ages out, it is not kept.
    tx1.try_send(plug()).expect("room for the plug");
    s0.world.script = vec![(11, 3)];
    for tick in 4..=4 + EFFECT_MAX_AGE_TICKS + 1 {
        assert!(s0.step_phases(&tinfo(tick)));
    }
    assert!(s0.effects.retry.is_empty());
    assert_eq!(s0.effects.stats.expired, 1);

    // At the cap, a failed send is dropped and counted.
    for seq in 0..EFFECT_RETRY_CAP as u64 {
        let e = fx(11, 3, 0, 100 + seq, 20);
        s0.effects.retry.push_back((1, e));
    }
    s0.world.script = vec![(11, 3)];
    assert!(s0.step_phases(&tinfo(20)));
    assert_eq!(s0.effects.retry.len(), EFFECT_RETRY_CAP);
    assert_eq!(s0.effects.stats.dropped_full, 1);
}

/// The envelope: an effect of another room incarnation, from an origin
/// outside the room, or older than the age envelope never reaches the
/// game.
#[test]
fn the_envelope_refuses_foreign_and_stale_effects() {
    let (tx, _rx) = mpsc::channel(64);
    let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
    let mut s1 = rig_actor(1, vec![tx, d]).with_effect_epoch(4);
    put(&mut s1, 40, 5.0, 0.0);
    let mut other_epoch = fx(40, 3, 0, 1, 9);
    other_epoch.id.epoch = 3;
    let mut outside = fx(40, 3, 7, 1, 9);
    outside.id.epoch = 4;
    let mut stale = fx(40, 3, 0, 2, 9 - EFFECT_MAX_AGE_TICKS);
    stale.id.epoch = 4;
    let mut fine = fx(40, 3, 0, 3, 9);
    fine.id.epoch = 4;
    for e in [other_epoch, outside, stale, fine.clone()] {
        assert!(s1.handle_msg(ShardMsg::RemoteEffect(e), 10));
    }
    assert!(s1.step_phases(&tinfo(10)));
    assert_eq!(s1.world.applied, [fine]);
    assert_eq!((s1.effects.stats.foreign, s1.effects.stats.expired), (2, 1));
}
