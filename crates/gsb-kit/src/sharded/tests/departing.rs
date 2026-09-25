//! The migration tick (`docs/CROSS-SHARD.md` "D sonucu") on the fixture
//! game: in the tick after an entity was handed on, its copy is still in
//! the old shard's world. A local hit on it is not lost there but lands
//! once on the new owner; the copy does not act a second time for its
//! entity; a refused send changes nothing; and a game that never
//! touches a departing entity sees the room behave as before. Driven
//! through the kit's hooks over a `SeamStage` whose forwarding table
//! stands for the core's committed send (`SeamStage::depart`); the real
//! actors are the MMO's tests (`migration_tick.rs`).

use bevy_ecs::prelude::Entity;
use gsb_core::shard::{EffectId, EffectOutcome, RemoteEffect, SeamStage, ShardLogic};

use super::*;
use crate::common::ParkEntry;
use crate::sharded::Crystallize;
use crate::testing::WirePos;

mod brawl;

use brawl::{Auto, View, brawl, hp, none_disabled, spawn};

/// P hits Q on shard 0 in tick 1 as Q crosses the seam; shard 0's
/// migrate phase hands Q on (90 hp). In tick 2 the core has committed
/// the move and shard 0 still holds Q's copy: P's local hit — a world
/// query — does not find it; the seam shows Q lent by shard 1 with the
/// record it left with (once, whether or not shard 1's own export is
/// in yet), and the hit goes there as an effect. Shard 1
/// applies it in tick 3: 80, every hit once. After the game's hooks the
/// copy is back for the kit (exported, then despawned by the core).
#[test]
fn a_local_hit_on_a_departing_entity_lands_once_on_its_new_owner() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = brawl(0).with_crystallize(Crystallize::default());
    let mut s1 = brawl(1);
    let p = spawn(&mut w0, &mut s0, 1, -5.0);
    let q = spawn(&mut w0, &mut s0, 2, -1.0);

    s0.game_mut().swings.push((p, q));
    let copy = s0.wire_entity[&q];
    w0.entity_mut(copy).insert(Position { x: 1.0, y: 0.0 });
    s0.update_seam(&mut w0, &ctx(1), &mut SeamStage::new(0, 1).seam());
    assert_eq!(hp(&w0, &s0, q), 90, "a local hit, before the move");
    let m = s0.collect_migrations(&mut w0, 1).pop().expect("Q crosses");
    assert_eq!((m.wire, m.state.hp), (q, 90));
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);

    let mut stage = SeamStage::new(0, 2);
    stage.depart(q, 1);
    // Shard 1's first export of Q may or may not be in yet (scheduling):
    // either way the game sees Q once, as it left.
    let early = BorderRecord {
        wire: q,
        state: WirePos { x: 2, y: 0 },
    };
    stage.lend(1, early);
    // The tick's first hook is an effect on P (0d): the copy is already
    // out of the game's view there.
    let on_p = RemoteEffect {
        target: p,
        source: 0,
        id: EffectId {
            origin: 1,
            epoch: 0,
            seq: 1,
        },
        at_tick: 1,
        hops: 0,
        payload: bytes::Bytes::from_static(&[10]),
    };
    let got = s0.apply_remote_effect(&mut w0, 2, &on_p, &mut stage.seam());
    assert_eq!(got, EffectOutcome::Applied);
    assert_eq!(s0.game().effect_world, [p]);
    s0.game_mut().probe = q;
    s0.game_mut().swings.push((p, q));
    s0.update_seam(&mut w0, &ctx(2), &mut stage.seam());
    let view = std::mem::take(&mut s0.game_mut().view);
    let want = View {
        local: None,
        lent: Some((1, WirePos { x: 1, y: 0 })),
        world: vec![p],
        lent_all: vec![q],
    };
    assert_eq!(view, want, "Q is shard 1's, seen from shard 0");
    assert!(s0.game().emits.iter().all(Result::is_ok));
    let sent: Vec<_> = stage
        .emitted()
        .iter()
        .map(|(to, e)| (*to, e.target, e.source))
        .collect();
    assert_eq!(sent, [(1, q, p)], "the hit goes to Q's new owner");
    assert_eq!(hp(&w0, &s0, q), 90, "the copy was not hit");
    assert!(none_disabled(&mut w0), "the copy is back after the hooks");
    assert!(s0.border_cache.iter().any(|r| r.wire == q), "and exported");
    let fights = &s0.crystal.as_ref().expect("opted in").book.fights;
    assert!(fights.contains_key(&(p, q)), "a contact across the seam");
    s0.on_migrate_out(&mut w0, q);
    assert!(s0.departures.hidden().is_empty());
    assert!(s0.departures.departing(q, &stage.seam()).is_none(), "done");

    let effect = stage.emitted()[0].1.clone();
    let got = s1.apply_remote_effect(&mut w1, 3, &effect, &mut SeamStage::new(1, 3).seam());
    assert_eq!(got, EffectOutcome::Applied);
    assert_eq!(hp(&w1, &s1, q), 80, "both hits, each once");
}

/// A send the core refused (no forwarding record): the entity stayed,
/// so it is local — hit locally, nothing emitted — and the captured
/// record is forgotten.
#[test]
fn a_refused_send_leaves_the_entity_local() {
    let mut w0 = World::new();
    let mut s0 = brawl(0);
    let p = spawn(&mut w0, &mut s0, 1, -5.0);
    let q = spawn(&mut w0, &mut s0, 2, 1.0);
    assert_eq!(s0.collect_migrations(&mut w0, 1).len(), 1);

    s0.game_mut().probe = q;
    s0.game_mut().swings.push((p, q));
    let mut stage = SeamStage::new(0, 2);
    s0.update_seam(&mut w0, &ctx(2), &mut stage.seam());
    assert_eq!(s0.game().view.local, s0.wire_entity.get(&q).copied());
    assert_eq!(s0.game().view.world, [p, q]);
    assert_eq!(hp(&w0, &s0, q), 90, "hit where it is");
    assert!(stage.emitted().is_empty());
    let mut probe = SeamStage::new(0, 3);
    probe.depart(q, 1);
    assert!(
        s0.departures.departing(q, &probe.seam()).is_none(),
        "the refused send's record is gone"
    );
}

/// The departing entity does not act twice for tick 2. Q auto-attacks
/// P: in tick 1 on shard 0 (locally, Q's last tick there); in tick 2 on
/// shard 1 (P is lent there: an effect) — and NOT from its copy on
/// shard 0. A bot-fed Q is driven by shard 1's bot only.
#[test]
fn a_departing_entity_does_not_act_a_second_time() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let (mut s0, mut s1) = (brawl(0), brawl(1));
    let p = spawn(&mut w0, &mut s0, 1, -5.0);
    let q = spawn(&mut w0, &mut s0, 2, 1.0);
    let copy = s0.wire_entity[&q];
    w0.entity_mut(copy).insert(Auto(p));
    let parked = ParkEntry {
        player: s0.entity_player[&copy],
        entity: copy,
        bot: true,
    };
    s0.park_ledger.insert("q".into(), parked);

    s0.update_seam(&mut w0, &ctx(1), &mut SeamStage::new(0, 1).seam());
    assert_eq!(hp(&w0, &s0, p), 90, "Q's blow of its last tick here");
    let m = s0.collect_migrations(&mut w0, 1).pop().expect("Q crosses");
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);

    let mut stage0 = SeamStage::new(0, 2);
    stage0.depart(q, 1);
    s0.ingest_seam(&mut w0, &ctx(2), &mut Vec::new(), &mut stage0.seam());
    s0.update_seam(&mut w0, &ctx(2), &mut stage0.seam());
    let mut stage1 = SeamStage::new(1, 2);
    let lent_p = BorderRecord {
        wire: p,
        state: WirePos { x: -5, y: 0 },
    };
    stage1.lend(0, lent_p);
    s1.ingest_seam(&mut w1, &ctx(2), &mut Vec::new(), &mut stage1.seam());
    s1.update_seam(&mut w1, &ctx(2), &mut stage1.seam());

    assert_eq!(hp(&w0, &s0, p), 90, "the copy does not strike");
    assert!(stage0.emitted().is_empty());
    let sent: Vec<_> = stage1
        .emitted()
        .iter()
        .map(|(to, e)| (*to, e.target, e.source))
        .collect();
    assert_eq!(sent, [(0, p, q)], "Q strikes from shard 1, once");
    assert_eq!(
        s0.game().botted,
        [Vec::<Entity>::new()],
        "no bot on the copy"
    );
    let q1 = s1.wire_entity[&q];
    assert_eq!(s1.game().botted, [vec![q1]], "the new owner's bot");
}

/// A game that never touches a departing entity (the plain fixture): the
/// tick after a committed move looks exactly as when the kit knows
/// nothing of it — same world, same border export, same snapshot, same
/// crossings, nothing emitted, nothing left disabled.
#[test]
fn a_game_that_never_touches_a_departing_entity_is_unchanged() {
    let run = |commit: bool| {
        let mut w = World::new();
        let mut s = ShardedRoom::new(0, 2, 50.0);
        let _p = place(&mut w, &mut s, ConnectionId(1), -5.0, 0.0);
        let q = place(&mut w, &mut s, ConnectionId(2), 1.0, 0.0);
        s.update_seam(&mut w, &ctx(1), &mut SeamStage::new(0, 1).seam());
        let moved: Vec<u64> = s
            .collect_migrations(&mut w, 1)
            .iter()
            .map(|m| m.wire)
            .collect();
        let mut stage = SeamStage::new(0, 2);
        if commit {
            stage.depart(q, 1);
        }
        s.ingest_seam(&mut w, &ctx(2), &mut Vec::new(), &mut stage.seam());
        s.update_seam(&mut w, &ctx(2), &mut stage.seam());
        let border: Vec<_> = s
            .collect_border(&w)
            .iter()
            .map(|r| (r.wire, r.state))
            .collect();
        let mut own = s.own_wires(&w);
        own.sort_unstable();
        let mut snap = bytes::BytesMut::new();
        s.snapshot(&mut w, &ctx(2), &(), &[], &mut snap);
        assert!(stage.emitted().is_empty());
        assert!(none_disabled(&mut w));
        (moved, border, own, snap)
    };
    assert_eq!(run(true), run(false));
}
