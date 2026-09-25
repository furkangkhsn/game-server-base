//! Combat across a shard seam (`docs/CROSS-SHARD.md` §2–§4), through the
//! real shard actors: an attack on an entity a neighbour LENDS is
//! validated on the attacker's shard and applied by the target's owner —
//! the owner credits the kill; a duplicate delivery applies once; a
//! forged or stale strike is refused by the owner's policy; strikes of
//! one tick on one target resolve in a fixed order. (Player duels and the
//! logout veto: `cross_seam_players.rs`.)

mod common;

use bytes::Bytes;
use common::{Client, Mmo};
use gsb_core::shard::{EffectId, RemoteEffect, ShardMsg};
use gsb_demo_mmo::effect::MmoEffect;
use gsb_demo_mmo::mmo::Kind;
use gsb_demo_mmo::{MobSpawn, Pos3, Realm, components};

/// A mob with `hp` at `(x, z)` (spawned at tick 1, long-lived).
fn mob(x: f32, z: f32, hp: u16) -> MobSpawn {
    MobSpawn::once(components::Kind::Mob, Pos3::new(x, 0.0, z), 1, 100_000, hp)
}

/// A (conn 1) on shard 0, 20 m from mob M on shard 1 (5 m past the
/// x = 0 seam, hp 50: two hits); observer O (conn 2) on shard 1.
fn realm() -> Realm {
    Realm::empty()
        .with_login("a", Pos3::new(-15.0, 0.0, -300.0))
        .with_login("o", Pos3::new(30.0, 0.0, -300.0))
        .with_spawn(mob(5.0, -300.0, 50))
}

/// A and O logged in, M in A's view through the strip. Returns M.
async fn seam_fight(room: &mut Mmo) -> (Vec<Client>, u64) {
    let mut cs = vec![room.join(1, "a", &mut []).await];
    let o = room.join(2, "o", &mut cs).await;
    cs.push(o);
    room.steps(&mut cs, 3).await;
    assert_eq!(room.members(), [1, 1, 0, 0], "A on shard 0, O on shard 1");
    let mobs = cs[0].of_kind(Kind::Mob);
    assert_eq!(mobs.len(), 1, "A sees M through the strip: {mobs:?}");
    (cs, mobs[0].0)
}

/// A hits M through the strip: shard 1 applies the damage (both sides
/// see it) and credits A; the second hit kills M, which disappears on
/// both sides.
#[tokio::test]
async fn an_attack_across_the_seam_lands_on_the_owner_and_kills_there() {
    let mut room = Mmo::new(&realm());
    let (mut cs, m) = seam_fight(&mut room).await;
    cs[0].attack(m).await;
    room.steps(&mut cs, 3).await;
    assert_eq!(cs[1].get(m).map(|r| r.hp), Some(25), "the owner applied it");
    assert_eq!(cs[0].get(m).map(|r| r.hp), Some(25), "and lends the result");

    cs[0].attack(m).await;
    room.steps(&mut cs, 3).await;
    assert!(cs[1].get(m).is_none(), "M died on its owner");
    assert!(cs[0].get(m).is_none(), "and left the attacker's view");
    let credit: Vec<_> = room
        .hits()
        .iter()
        .map(|h| (h.shard, h.attacker, h.target, h.hp, h.killed))
        .collect();
    let a = cs[0].id;
    assert_eq!(
        credit,
        [(1, a, m, 25, false), (1, a, m, 0, true)],
        "applied on shard 1, the kill credited to A"
    );
}

/// An effect as a neighbour's link would deliver it: A strikes M.
fn strike(m: u64, a: u64, seq: u64, at_tick: u64, damage: u16) -> common::Msg {
    ShardMsg::RemoteEffect(RemoteEffect {
        target: m,
        source: a,
        // Shard 3 has no fighter in this realm: its id space is free.
        id: EffectId {
            origin: 3,
            epoch: 0,
            seq,
        },
        at_tick,
        hops: 0,
        payload: MmoEffect::Strike { damage }.encode(),
    })
}

/// The owner's terms: one effect delivered TWICE (an at-least-once
/// link) applies once; a forged damage is capped; a stale strike, an
/// undecodable payload and a strike from beyond reach (re-checked
/// against the attacker as the owner sees it) are refused. And the
/// attacker's own shard does not send what it cannot validate.
#[tokio::test]
async fn the_owner_applies_a_strike_once_and_on_its_own_terms() {
    // M as before; F (hp 50) 45 m from A, still lent to A's shard.
    let realm = realm().with_spawn(mob(5.0, -340.0, 50));
    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "a", &mut []).await];
    room.steps(&mut cs, 3).await;
    let mobs: Vec<u64> = cs[0].of_kind(Kind::Mob).iter().map(|m| m.0).collect();
    let [m, f] = mobs[..] else {
        panic!("A sees both mobs: {mobs:?}")
    };
    let (m, f) = if cs[0].get(m).map(|r| r.z) == Some(-3000) {
        (m, f)
    } else {
        (f, m)
    };
    let a = cs[0].id;

    let now = room.tick;
    room.deliver(1, strike(m, a, 1, now, 999)); // forged…
    room.deliver(1, strike(m, a, 1, now, 999)); // …and delivered twice
    room.steps(&mut cs, 2).await;
    assert_eq!(cs[0].get(m).map(|r| r.hp), Some(25), "capped, applied once");
    room.deliver(1, strike(m, a, 1, room.tick, 25));
    room.steps(&mut cs, 2).await;
    assert_eq!(cs[0].get(m).map(|r| r.hp), Some(25), "a late copy too");
    assert_eq!(room.hits().len(), 1);

    let now = room.tick;
    room.deliver(1, strike(m, a, 2, now - 5, 25)); // older than the policy
    let mut junk = strike(m, a, 3, now, 25);
    if let ShardMsg::RemoteEffect(e) = &mut junk {
        e.payload = Bytes::from_static(b"??");
    }
    room.deliver(1, junk);
    room.deliver(1, strike(f, a, 4, now, 25)); // A is 45 m from F
    cs[0].attack(f).await; // …and A's own shard will not send it
    room.steps(&mut cs, 3).await;
    assert_eq!(
        cs[0].get(m).map(|r| r.hp),
        Some(25),
        "stale and junk refused"
    );
    assert_eq!(cs[0].get(f).map(|r| r.hp), Some(50), "out of reach refused");
    assert!(room.hits().is_empty());

    room.deliver(1, strike(m, a, 5, room.tick, 25));
    room.steps(&mut cs, 2).await;
    assert!(cs[0].get(m).is_none(), "M dies");
    assert_eq!(room.hits().iter().map(|h| h.hp).collect::<Vec<_>>(), [0]);
}

/// Two attackers on DIFFERENT shards strike the same last-25-hp mob on a
/// third in the same tick: the owner applies the tick's strikes in
/// `(source, …)` order, so the lower wire id always lands the kill —
/// whichever link delivered first, whichever client sent first.
#[tokio::test]
async fn simultaneous_strikes_resolve_in_a_fixed_order() {
    // M on shard 1 near the map centre; X (conn 1) on shard 0, Y (conn
    // 3) on shard 3 — both 15 m from M, each seeing it lent.
    let realm = Realm::empty()
        .with_login("x", Pos3::new(-10.0, 0.0, -5.0))
        .with_login("y", Pos3::new(5.0, 0.0, 10.0))
        .with_spawn(mob(5.0, -5.0, 25));
    for x_first in [true, false] {
        let mut room = Mmo::new(&realm);
        let mut cs = vec![room.join(1, "x", &mut []).await];
        let y = room.join(3, "y", &mut cs).await;
        cs.push(y);
        room.steps(&mut cs, 3).await;
        assert_eq!(room.members(), [1, 0, 0, 1]);
        let m = cs[0].of_kind(Kind::Mob)[0].0;
        assert!(cs[1].get(m).is_some(), "Y sees M too");
        let (x, y) = (cs[0].id, cs[1].id);
        assert!(x < y, "shard 0's range is below shard 3's");
        let order = if x_first { [0, 1] } else { [1, 0] };
        for i in order {
            cs[i].attack(m).await;
        }
        room.steps(&mut cs, 3).await;
        let kills: Vec<_> = room
            .hits()
            .iter()
            .map(|h| (h.shard, h.attacker, h.killed))
            .collect();
        assert_eq!(kills, [(1, x, true)], "X's strike first, Y's found nothing");
        assert!(cs[0].get(m).is_none() && cs[1].get(m).is_none());
    }
}
