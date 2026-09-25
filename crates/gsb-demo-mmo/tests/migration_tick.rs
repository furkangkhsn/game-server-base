//! The migration tick through the real shard actors
//! (`docs/CROSS-SHARD.md` "D sonucu"): an entity that leaves a shard in
//! tick `h`'s migrate phase stays in that shard's world until the
//! migrate phase of `h + 1`. A blow a local attacker lands on it in
//! `h + 1` must reach the entity where it now lives — once — instead of
//! the copy the old shard is about to despawn. Two handovers: a region
//! crossing in a duel, and a crystallized duel going back to its region.

mod common;

use common::{Client, Mmo};
use gsb_demo_mmo::combat::Hit;
use gsb_demo_mmo::world::{ATTACK_DAMAGE, CRYSTALLIZE, PLAYER_HP, WAYSTONES};
use gsb_demo_mmo::{Pos3, Realm};

/// The hits on `target`: `(shard, attacker, hp after, killed, tick)`.
fn on(hits: &[Hit], target: u64) -> Vec<(usize, u64, u16, bool, u64)> {
    hits.iter()
        .filter(|h| h.target == target)
        .map(|h| (h.shard, h.attacker, h.hp, h.killed, h.tick))
        .collect()
}

/// Every hit on `target` took exactly one blow's damage from the hp the
/// previous one left (the first from full health), and the damage adds
/// up to what the target lost.
fn hp_arithmetic(hits: &[Hit], target: u64) {
    let mut hp = PLAYER_HP;
    for h in hits.iter().filter(|h| h.target == target) {
        assert_eq!(h.hp, hp - ATTACK_DAMAGE, "one blow's damage: {hits:?}");
        hp = h.hp;
    }
}

/// P (conn 1) and Q (conn 2) duel on shard 0, 10 m apart, Q half a
/// metre short of the x = 0 seam.
fn duel_realm() -> Realm {
    Realm::empty()
        .with_login(1, Pos3::new(-10.5, 0.0, -300.0))
        .with_login(2, Pos3::new(-0.5, 0.0, -300.0))
}

/// P strikes Q twice on shard 0, then Q walks east across the seam
/// (0.23 m a tick: it crosses in its third step, `h`) while the duel
/// goes on: both strike in `h` — Q's last tick on shard 0, the blows
/// are local there and travel in Q's state — and in `h + 1`, when Q is
/// shard 1's and shard 0 still holds the copy it despawns at the end of
/// that tick. P's blow of `h + 1` is the fourth: it defeats Q on shard
/// 1, credited to P; Q's blow of `h + 1` lands on P as a remote effect.
#[tokio::test]
async fn a_blow_in_the_tick_after_a_crossing_lands_once_where_the_target_lives() {
    let mut room = Mmo::new(&duel_realm());
    let mut cs = vec![room.join(1, "p", &mut []).await];
    let q = room.join(2, "q", &mut cs).await;
    cs.push(q);
    room.steps(&mut cs, 3).await;
    assert_eq!(room.members(), [2, 0, 0, 0], "both on shard 0");
    let (p, q) = (cs[0].id, cs[1].id);
    for _ in 0..2 {
        cs[0].attack(q).await;
        room.step(&mut cs).await;
    }

    cs[1].move_to(20.0, -300.0).await;
    let h = room.tick + 3;
    while room.tick < h - 1 {
        room.step(&mut cs).await;
        assert_eq!(room.members(), [2, 0, 0, 0], "tick {}", room.tick);
    }
    cs[0].attack(q).await;
    cs[1].attack(p).await;
    room.step(&mut cs).await;
    assert_eq!(room.members(), [1, 0, 0, 0], "Q leaves in h");
    cs[0].attack(q).await;
    cs[1].attack(p).await;
    room.step(&mut cs).await;
    assert_eq!(room.members(), [1, 1, 0, 0], "Q is shard 1's in h + 1");
    room.steps(&mut cs, 3).await;

    let hits = room.hits();
    let first = h - 3;
    assert_eq!(
        on(&hits, q),
        [
            (0, p, 75, false, first - 1),
            (0, p, 50, false, first),
            (0, p, 25, false, h),
            (1, p, 0, true, h + 2),
        ],
        "the blow of h + 1 lands on shard 1, not on shard 0's copy"
    );
    assert_eq!(
        on(&hits, p),
        [(0, q, 75, false, h), (0, q, 50, false, h + 2)]
    );
    hp_arithmetic(&hits, q);
    hp_arithmetic(&hits, p);

    // Q fell on shard 1: back on its feet at the waystone nearest to
    // where it fell (shard 1's), whole.
    let me = cs[1].me().expect("Q sees itself");
    assert_eq!(me.hp, u32::from(PLAYER_HP), "defeated, back at full health");
    let [x, z] = WAYSTONES[1];
    assert_eq!((me.x, me.z), (x as i32 * 10, z as i32 * 10));
    assert_eq!(cs[0].me().map(|r| r.hp), Some(50));
    assert_eq!(room.members(), [1, 1, 0, 0]);
}

/// P (conn 1) on shard 0 and Q (conn 2) on shard 1, 20 m apart across
/// the x = 0 seam (the crystallization test's duel).
fn seam_realm() -> Realm {
    Realm::empty()
        .with_login(1, Pos3::new(-10.0, 0.0, -300.0))
        .with_login(2, Pos3::new(10.0, 0.0, -300.0))
}

/// A duel crystallizes onto shard 0 (Q, the higher wire, moves there in
/// `h`), goes quiet, and Q goes back to shard 1 in the migrate phase of
/// `out`. P strikes in `out + 1` — a local blow on shard 0 as far as P
/// can tell, on the copy shard 0 despawns at the end of that tick: it
/// is Q's fourth, and defeats Q on shard 1, credited to P.
#[tokio::test]
async fn a_blow_in_the_tick_after_a_crystal_release_lands_once_where_the_target_lives() {
    let mut room = Mmo::new(&seam_realm());
    let mut cs: Vec<Client> = vec![room.join(1, "p", &mut []).await];
    let q = room.join(2, "q", &mut cs).await;
    cs.push(q);
    room.steps(&mut cs, 3).await;
    assert_eq!(room.members(), [1, 1, 0, 0]);
    let (p, q) = (cs[0].id, cs[1].id);

    // The crystallization test's script: P at s, h, h + 1; Q at h, h + 1
    // and once more at `last`. Q is held on shard 0 from h + 1.
    let s = room.tick + 1;
    let h = s + 1 + CRYSTALLIZE.after;
    let last = h + 20;
    let out = last + CRYSTALLIZE.release + 1;
    while room.tick < out {
        let t = room.tick + 1;
        if t == s || t == h || t == h + 1 {
            cs[0].attack(q).await;
        }
        if t == h || t == h + 1 || t == last {
            cs[1].attack(p).await;
        }
        room.step(&mut cs).await;
        if t == h + 1 {
            assert_eq!(room.members(), [2, 0, 0, 0], "held on shard 0");
        }
    }
    assert_eq!(room.members(), [1, 0, 0, 0], "Q leaves in `out`");

    cs[0].attack(q).await;
    room.step(&mut cs).await;
    assert_eq!(room.members(), [1, 1, 0, 0], "Q is shard 1's in out + 1");
    room.steps(&mut cs, 3).await;

    let hits = room.hits();
    assert_eq!(
        on(&hits, q),
        [
            (1, p, 75, false, s + 1),
            (0, p, 50, false, h + 1),
            (0, p, 25, false, h + 2),
            (1, p, 0, true, out + 2),
        ],
        "the blow of out + 1 lands on shard 1, not on shard 0's copy"
    );
    hp_arithmetic(&hits, q);
    hp_arithmetic(&hits, p);
    let me = cs[1].me().expect("Q sees itself");
    assert_eq!(me.hp, u32::from(PLAYER_HP), "defeated, back at full health");
    assert_eq!(room.members(), [1, 1, 0, 0]);
}
