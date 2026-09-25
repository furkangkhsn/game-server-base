//! Game-code despawn through the real shard actors (KIT-ARCHITECTURE
//! §8.2): a mob killed by a player — no leave, no hook, just
//! `World::despawn` in the MMO's attack handler — disappears from every
//! client that saw it, including the client on the NEIGHBOURING shard
//! that saw it only through the border strip, and never comes back as a
//! ghost in the later keep-alive fulls.

mod common;

use common::Mmo;
use gsb_demo_mmo::mmo::{Kind, Private, WorldSnapshot, private};
use gsb_demo_mmo::op;
use gsb_demo_mmo::{MobSpawn, Pos3, Realm, components};
use prost::Message;

/// The snapshot a raw frame carries (a group frame, or a private
/// frame's one-shot full).
fn snapshot(op: u16, raw: &[u8]) -> Option<WorldSnapshot> {
    match op {
        op::MMO_SNAPSHOT => Some(WorldSnapshot::decode(raw).expect("snapshot")),
        _ => match Private::decode(raw).expect("private").payload {
            Some(private::Payload::Snapshot(s)) => Some(s),
            _ => None,
        },
    }
}

#[tokio::test]
async fn a_mob_killed_by_game_code_vanishes_everywhere_for_good() {
    // M stands on shard 0, 30 m west of the seam; A (shard 0) is in
    // reach, B (shard 1) sees M through the border strip only.
    let realm = Realm::empty()
        .with_login("c1", Pos3::new(-50.0, 0.0, -100.0))
        .with_login("c2", Pos3::new(40.0, 0.0, -100.0))
        .with_spawn(MobSpawn::once(
            components::Kind::Mob,
            Pos3::new(-30.0, 0.0, -100.0),
            1,
            100_000,
            50,
        ));
    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "c1", &mut []).await];
    let b = room.join(2, "c2", &mut cs).await;
    cs.push(b);
    room.steps(&mut cs, 3).await;
    let mobs = cs[1].of_kind(Kind::Mob);
    assert_eq!(mobs.len(), 1, "B sees M through the strip: {mobs:?}");
    let m = mobs[0].0;
    assert_eq!(cs[0].get(m), cs[1].get(m), "A and B agree about M");

    // The first hit is a vitals-only change (M did not move): it reaches
    // both sides, the strip included (`Dirty` = position OR vitals).
    cs[0].attack(m).await;
    room.steps(&mut cs, 2).await;
    for c in &cs {
        assert_eq!(
            c.get(m).map(|r| r.hp),
            Some(25),
            "the hit is news on both sides"
        );
    }

    // The killing blow despawns M (game code).
    cs[0].attack(m).await;
    room.steps(&mut cs, 2).await;
    for c in &cs {
        assert!(c.get(m).is_none(), "M left {}'s view", c.conn.0);
    }
    let marks: Vec<usize> = cs.iter().map(|c| c.raw.len()).collect();

    // Four keep-alive periods (fulls every 15 ticks): M is in no frame,
    // full or delta, on either shard — no ghost re-carried.
    room.steps(&mut cs, 60).await;
    for (c, mark) in cs.iter().zip(marks) {
        let snaps: Vec<WorldSnapshot> = c.raw[mark..]
            .iter()
            .filter_map(|(o, raw)| snapshot(*o, raw))
            .collect();
        let fulls = snaps.iter().filter(|s| !s.delta).count();
        assert!(
            fulls >= 4,
            "{} got its keep-alive fulls ({fulls})",
            c.conn.0
        );
        for s in &snaps {
            assert!(
                s.entities.iter().all(|r| r.entity != m),
                "M came back to {}",
                c.conn.0
            );
        }
        assert!(c.get(m).is_none() && c.me().is_some());
    }
}
