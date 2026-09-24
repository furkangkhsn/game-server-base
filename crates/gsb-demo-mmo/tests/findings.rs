//! KIT DESIGN FINDINGS, pinned (`docs/KIT-ARCHITECTURE.md` §10, "Faz 4
//! sonucu", "Tasarım bulguları"). Phase 4 wrote each test against the
//! unchanged kit, asserting the behaviour the MMO needed something else
//! from — the executable record of the finding. The kit's Phase 5 fixed
//! them ("Faz 5 sonucu"), and each test was flipped to assert the
//! behaviour its doc names as correct.

mod common;

use common::Mmo;
use gsb_demo_mmo::components::Kind;
use gsb_demo_mmo::{MobSpawn, Pos3, Realm};

/// F1 — an entity that migrates into a shard whose strip already lent
/// it, in the cell it arrives in, stays in that cell's bucket.
///
/// M creeps across the x = 0 seam by centimetres and stops: from spawn
/// to rest its wire value is `x = 0 dm` — ground cell (0, -5), shard 1's
/// side of the seam — while its simulation position is still owned by
/// shard 0 for its first ~120 ticks. So shard 1 lends-in M in cell
/// (0, -5) through the border strip, tick after tick. When M migrates,
/// shard 1's dirty pass places the OWN record in (0, -5), and the strip
/// ledger (`ShardedSpatialRoom::integrate_borrowed`) sees M missing from
/// the strip (the core's own-wins filter). Before the kit's fix it
/// `record_exit`ed M from the very bucket the own record lived in — M,
/// alone there, vanished from every client on shard 1 for good, while a
/// client on shard 0 kept seeing it through the strip. Now the ledger
/// skips that exit.
///
/// Correct (asserted): O (shard 1, next cell) sees M on every tick —
/// before, during and after the crossing — and so does A across the seam.
#[tokio::test]
async fn f1_an_entity_arriving_in_its_lent_cell_stays_visible_on_its_new_shard() {
    let creeper = MobSpawn::once(Kind::Mob, Pos3::new(-0.04, 0.0, -300.0), 1, 100_000, 60).walking(
        vec![[0.02, -300.0]],
        0.01,
        false,
    );
    let realm = Realm::empty()
        .with_login(1, Pos3::new(100.0, 0.0, -300.0)) // O: shard 1, cell (1,-5)
        .with_login(2, Pos3::new(-20.0, 0.0, -300.0)) // A: shard 0, cell (-1,-5)
        .with_spawn(creeper);
    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "", &mut []).await];
    let a = room.join(2, "", &mut cs).await;
    cs.push(a);
    room.steps(&mut cs, 60).await; // M still on shard 0
    let m = cs[0].of_kind(gsb_demo_mmo::mmo::Kind::Mob);
    assert_eq!(m.len(), 1, "before the crossing O sees M through the strip");
    let m = m[0].0;
    assert!(cs[1].get(m).is_some());
    // A's attack resolves on A's shard only: it lands while M is shard
    // 0's (the hit shows on both sides)…
    cs[1].attack(m).await;
    room.steps(&mut cs, 2).await;
    assert_eq!(cs[0].get(m).map(|r| r.hp), Some(35), "hit on shard 0");

    // Crossed at ~121, at rest from ~181.
    for _ in 0..258 {
        room.step(&mut cs).await;
        assert!(
            cs[0].get(m).is_some(),
            "O lost M at tick {} (F1: the lent-cell exit erased it)",
            room.tick
        );
        assert!(cs[1].get(m).is_some(), "A, across the seam, sees M");
    }
    // …and no longer does: M did cross into shard 1.
    cs[1].attack(m).await;
    room.steps(&mut cs, 2).await;
    assert_eq!(cs[0].get(m).map(|r| r.hp), Some(35), "M is shard 1's now");
}

/// F2 — `GridPartition2` is a 4-neighbourhood, and a border strip is
/// only exchanged between neighbours: across a region CORNER nothing is
/// lent. X stands 10 m from the map's centre in shard 0; its 3×3 ground
/// block reaches one cell into shards 1, 2 and 3. It sees the mobs of
/// the two edge neighbours but not the one of the diagonal shard 3,
/// although that mob is exactly as close.
///
/// Correct: X sees all three.
#[tokio::test]
async fn f2_the_diagonal_shard_lends_nothing_across_a_corner() {
    let at = |x: f32, z: f32| MobSpawn::once(Kind::Mob, Pos3::new(x, 0.0, z), 1, 100_000, 60);
    let realm = Realm::empty()
        .with_login(1, Pos3::new(-10.0, 0.0, -10.0)) // X: shard 0, cell (-1,-1)
        .with_spawn(at(10.0, -10.0)) // shard 1, cell (0,-1)
        .with_spawn(at(-10.0, 10.0)) // shard 2, cell (-1,0)
        .with_spawn(at(10.0, 10.0)); // shard 3, cell (0,0) — the diagonal
    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "", &mut []).await];
    room.steps(&mut cs, 40).await;
    let seen: Vec<(i32, i32)> = cs[0].view.values().map(|r| (r.x, r.z)).collect();
    assert!(
        seen.contains(&(100, -100)) && seen.contains(&(-100, 100)),
        "{seen:?}"
    );
    assert!(
        !seen.contains(&(100, 100)),
        "F2 fixed? X sees the diagonal shard's mob — flip this test"
    );
}
