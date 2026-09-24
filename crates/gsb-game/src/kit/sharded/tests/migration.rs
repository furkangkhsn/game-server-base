//! Migration of entities that are not players (KIT-ARCHITECTURE §8.5):
//! every broadcast entity crosses a shard border with its owner region,
//! whatever components the game gave it.

use super::*;

/// An NPC the GAME spawned with a position and nothing else (no
/// `Speed`, no player) crosses from shard 0 into shard 1: it is reported
/// to shard 1 with its state, installed there under the SAME wire id
/// without components it never had, and despawned on shard 0.
#[test]
fn speedless_npc_migrates_across_the_seam() {
    let mut w0 = World::new();
    let mut w1 = World::new();
    let mut s0 = ShardedRoom::new(0, 4, 50.0); // 2×2: x∈[-50,0], y∈[-50,0]
    let mut s1 = ShardedRoom::new(1, 4, 50.0); // x∈[0,50], y∈[-50,0]

    let npc = w0.spawn(Position { x: -1.0, y: -10.0 }).id();
    s0.update(&mut w0, &ctx(1)); // stamps the NPC from shard 0's range
    let wire = w0.get::<WireId>(npc).expect("stamped").get();
    w0.entity_mut(npc).insert(Position { x: 1.0, y: -10.0 });

    let to1 = s0.collect_migrations(&mut w0, 1);
    assert_eq!(to1.len(), 1, "the NPC's crossing is reported to shard 1");
    let m = to1.into_iter().next().expect("one migration");
    assert_eq!(m.wire, wire, "the wire id travels");
    assert_eq!(m.player, None, "an NPC has no player");

    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    let mut q = w1.query::<(&WireId, &Position, Option<&Speed>)>();
    let arrived: Vec<_> = q
        .iter(&w1)
        .map(|(w, p, s)| (w.get(), *p, s.copied()))
        .collect();
    assert_eq!(
        arrived,
        vec![(wire, Position { x: 1.0, y: -10.0 }, None)],
        "installed on shard 1 as it was: same id, same position, still no Speed"
    );

    s0.on_migrate_out(&mut w0, wire);
    assert!(w0.get_entity(npc).is_err(), "despawned on shard 0");
}
