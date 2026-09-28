//! Players fighting across a shard seam, through the real shard actors:
//! two players on opposite sides trading blows in the same ticks resolve
//! the same way every time; and a parked character hit across the seam
//! is marked in combat by its own shard — the logout veto sees the fight.

mod common;

use std::time::Duration;

use common::{Client, Mmo};
use gsb_demo_mmo::combat::Hit;
use gsb_demo_mmo::world::{COMBAT_TICKS, PLAYER_HP};
use gsb_demo_mmo::{Pos3, Realm};

/// P (conn 1) on shard 0 and Q (conn 2) on shard 1, 20 m apart across
/// the x = 0 seam.
fn realm() -> Realm {
    Realm::empty()
        .with_login("p", Pos3::new(-10.0, 0.0, -300.0))
        .with_login("q", Pos3::new(10.0, 0.0, -300.0))
}

/// One applied hit, as a duel's record keeps it: `(shard, attacker, hp
/// after, killed, ticks since the fight began)`.
type Blow = (usize, u64, u16, bool, u64);

async fn both(room: &mut Mmo) -> Vec<Client> {
    let mut cs = vec![room.join(1, "p", &mut []).await];
    let q = room.join(2, "q", &mut cs).await;
    cs.push(q);
    room.steps(&mut cs, 3).await;
    assert_eq!(room.members(), [1, 1, 0, 0]);
    cs
}

/// P and Q swing at each other in the same four ticks. Each blow is
/// applied by the victim's own shard and credited to the other; both
/// fall on the same tick and get up at their nearest waystone. A second
/// room replaying the fight produces the identical record.
#[tokio::test]
async fn a_duel_across_the_seam_resolves_the_same_way_every_time() {
    let mut records: Vec<Vec<Blow>> = Vec::new();
    for _ in 0..2 {
        let mut room = Mmo::new(&realm());
        let mut cs = both(&mut room).await;
        let (p, q) = (cs[0].id, cs[1].id);
        let start = room.tick;
        for _ in 0..4 {
            cs[0].attack(q).await;
            cs[1].attack(p).await;
            room.step(&mut cs).await;
        }
        room.steps(&mut cs, 3).await;
        let mut hits: Vec<Hit> = room.hits();
        hits.sort_unstable_by_key(|h| (h.tick, h.shard));
        for h in &hits {
            let (victim, credited) = if h.shard == 0 { (p, q) } else { (q, p) };
            assert_eq!((h.target, h.attacker), (victim, credited), "{h:?}");
        }
        let falls: Vec<u64> = hits.iter().filter(|h| h.killed).map(|h| h.tick).collect();
        assert_eq!(falls.len(), 2, "both fell: {hits:?}");
        assert_eq!(falls[0], falls[1], "on the same tick");
        for (c, x) in [(0, -2560), (1, 2560)] {
            let me = cs[c].me().expect("still in the world");
            assert_eq!((me.x, me.z, me.hp), (x, -2560, u32::from(PLAYER_HP)));
        }
        records.push(
            hits.iter()
                .map(|h| (h.shard, h.attacker, h.hp, h.killed, h.tick - start))
                .collect(),
        );
    }
    assert_eq!(records[0], records[1], "the same fight, the same record");
    assert_eq!(records[0].len(), 8);
}

/// Ann is parked (her transport died) on shard 0; Bob, on shard 1, keeps
/// hitting her across the seam. Shard 0 applies the hits and marks her in
/// combat, so her logout waits past its grace — the veto sees the fight
/// although she never swung — and completes once it has cooled down.
///
/// On the paused clock (the grace is read off the tick clock): the hit
/// must land within the 100 ms grace of the detach, and on the wall
/// clock a starved process could spend that between two steps — Ann
/// logged out before the hit (BACKLOG F34).
#[tokio::test(start_paused = true)]
async fn a_parked_character_hit_across_the_seam_is_held_by_the_fight() {
    let grace = Duration::from_millis(100);
    let mut room = Mmo::with(&realm(), grace, 2.0);
    let mut cs = both(&mut room).await;
    let ann = cs.remove(0);
    room.detach(&ann, "p", &mut cs).await;
    cs[0].attack(ann.id).await;
    room.step(&mut cs).await;
    room.step(&mut cs).await;
    let hit = room.hits();
    assert_eq!(hit.len(), 1, "applied by Ann's shard: {hit:?}");
    assert_eq!((hit[0].shard, hit[0].attacker), (0, cs[0].id));
    assert_eq!(cs[0].get(ann.id).map(|r| r.hp), Some(75));

    tokio::time::sleep(grace * 3).await; // the grace is the tick clock's
    room.steps(&mut cs, 3).await;
    let s = room.sample(0);
    assert_eq!(
        (s.detached, s.detach_expired_despawn),
        (1, 0),
        "past the grace, but in combat: held"
    );
    while room.tick <= hit[0].tick + COMBAT_TICKS {
        room.step(&mut cs).await;
    }
    let s = room.sample(0);
    assert_eq!(
        (s.detached, s.detach_expired_despawn),
        (0, 1),
        "the fight cooled down: logged out"
    );
}
