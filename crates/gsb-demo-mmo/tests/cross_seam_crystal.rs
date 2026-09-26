//! Crystallization through the real shard actors (`docs/CROSS-SHARD.md`
//! §4 layer 4, "C2 sonucu"): a duel that keeps going across the x = 0
//! seam moves onto one shard — Q (shard 1, the higher wire) joins P on
//! shard 0 — with every blow applied exactly once through the handover
//! and both players seeing each other on every tick; when the fight has
//! been quiet for the policy's release, Q goes back to its region, once.
//! A brief exchange does not move anyone.

mod common;

use common::{Client, Mmo};
use gsb_demo_mmo::combat::Hit;
use gsb_demo_mmo::world::{ATTACK_DAMAGE, CRYSTALLIZE, PLAYER_HP};
use gsb_demo_mmo::{Pos3, Realm};

/// P (conn 1) on shard 0 and Q (conn 2) on shard 1, 20 m apart across
/// the x = 0 seam (inside the band: Q stands 10 m past it).
fn realm() -> Realm {
    Realm::empty()
        .with_login("p", Pos3::new(-10.0, 0.0, -300.0))
        .with_login("q", Pos3::new(10.0, 0.0, -300.0))
}

async fn both(room: &mut Mmo) -> Vec<Client> {
    let mut cs = vec![room.join(1, "p", &mut []).await];
    let q = room.join(2, "q", &mut cs).await;
    cs.push(q);
    room.steps(&mut cs, 3).await;
    assert_eq!(room.members(), [1, 1, 0, 0]);
    cs
}

/// One step, then the invariant of the whole round: P and Q see each
/// other. Returns the members per shard after the step.
async fn step_seeing(room: &mut Mmo, cs: &mut [Client]) -> Vec<u32> {
    room.step(cs).await;
    let (p, q) = (cs[0].id, cs[1].id);
    assert!(cs[0].get(q).is_some(), "P lost Q at tick {}", room.tick);
    assert!(cs[1].get(p).is_some(), "Q lost P at tick {}", room.tick);
    room.members()
}

/// The duel: P strikes at `s`, both at `h` and `h + 1`, Q once more at
/// `h + 20`, where `h = s + 1 + K` is the tick Q's shard sees the fight
/// span K (P's first blow lands there at `s + 1`, Q's of `h` answers it)
/// — the move goes out in `h`'s migrate phase, so P's blow of `h` is
/// sent to Q's OLD shard and forwarded, Q's blow of `h` lands on P's
/// shard as Q arrives there, and the later blows are local (the last
/// one only the game can report — it keeps the hold). Three blows each:
/// nobody falls.
#[tokio::test]
async fn a_sustained_duel_crystallizes_onto_one_shard_and_returns_once_it_is_over() {
    let mut room = Mmo::new(&realm());
    let mut cs = both(&mut room).await;
    let (p, q) = (cs[0].id, cs[1].id);
    assert!(p < q, "Q is the higher wire: the mover");
    let s = room.tick + 1;
    let h = s + 1 + CRYSTALLIZE.after;
    let mut members = Vec::new();
    while room.tick < h + 1 {
        let t = room.tick + 1;
        if t == s || t == h || t == h + 1 {
            cs[0].attack(q).await;
        }
        if t == h || t == h + 1 {
            cs[1].attack(p).await;
        }
        members.push((t, step_seeing(&mut room, &mut cs).await));
    }
    let before: Vec<_> = members.iter().filter(|(t, _)| *t < h).collect();
    assert!(
        before.iter().all(|(_, m)| m == &[1, 1, 0, 0]),
        "{members:?}"
    );
    assert_eq!(
        members[members.len() - 2],
        (h, vec![1, 0, 0, 0]),
        "Q leaves in h"
    );
    assert_eq!(
        members[members.len() - 1],
        (h + 1, vec![2, 0, 0, 0]),
        "held on 0"
    );

    // Q's last blow; then the fight is over. It stays on shard 0 until
    // `release` quiet ticks after its last contact: Q leaves in the
    // migrate phase of the first tick past that, arrives on shard 1 the
    // next — and stays.
    let last = h + 20;
    let out = last + CRYSTALLIZE.release + 1;
    while room.tick < out + 300 {
        if room.tick + 1 == last {
            cs[1].attack(p).await;
        }
        let m = step_seeing(&mut room, &mut cs).await;
        let want = match room.tick {
            t if t < out => [2, 0, 0, 0],
            t if t == out => [1, 0, 0, 0],
            _ => [1, 1, 0, 0],
        };
        assert_eq!(m, want, "tick {} (release at {out})", room.tick);
    }

    // Every blow applied once: three each, P's on Q by shard 1 before the
    // move and by shard 0 after (its blow of h forwarded there), Q's all
    // by shard 0 (P's shard: its blow of h as a remote effect).
    let hits: Vec<Hit> = room.hits();
    let blows = |from: u64| -> Vec<(usize, u64)> {
        let mut v: Vec<_> = hits
            .iter()
            .filter(|h| h.attacker == from)
            .map(|h| (h.shard, h.tick))
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(blows(p), [(0, h + 1), (0, h + 2), (1, s + 1)], "{hits:?}");
    assert_eq!(blows(q), [(0, h + 1), (0, h + 1), (0, last)], "{hits:?}");
    let left = u32::from(PLAYER_HP - 3 * ATTACK_DAMAGE);
    assert_eq!(cs[1].me().map(|r| r.hp), Some(left));
    assert_eq!(cs[0].get(q).map(|r| r.hp), Some(left));
    assert_eq!(cs[0].me().map(|r| r.hp), Some(left));

    // What the operator reads of it (F9), off the shard actors' own
    // metrics samples: the move on Q's shard, the pair's two holds
    // ended quietly on P's — and every shard reports the six.
    let count = |shard: usize, name: &str| room.sample(shard).logic.get(name);
    assert_eq!(count(1, "crystal_moves"), Some(1), "Q's shard pinned Q");
    assert_eq!(count(0, "crystal_moves"), Some(0));
    assert_eq!(count(0, "crystal_release_quiet"), Some(2), "P and Q");
    assert_eq!(count(0, "crystal_fights_peak"), Some(1));
    for shard in 0..4 {
        assert_eq!(room.sample(shard).logic.slots().len(), 6, "shard {shard}");
    }
}

/// A single cross-seam blow, and a brief exchange that ends before it
/// has spanned K ticks: nobody moves, over a long wait.
#[tokio::test]
async fn a_brief_exchange_does_not_crystallize() {
    let mut room = Mmo::new(&realm());
    let mut cs = both(&mut room).await;
    let (p, q) = (cs[0].id, cs[1].id);
    cs[0].attack(q).await;
    for _ in 0..CRYSTALLIZE.after * 3 {
        assert_eq!(step_seeing(&mut room, &mut cs).await, [1, 1, 0, 0]);
    }
    for i in 0..CRYSTALLIZE.after / 2 {
        if i % 5 == 0 {
            cs[1].attack(p).await;
        }
        if i == 7 {
            cs[0].attack(q).await;
        }
        assert_eq!(step_seeing(&mut room, &mut cs).await, [1, 1, 0, 0]);
    }
    for _ in 0..CRYSTALLIZE.release * 2 {
        assert_eq!(step_seeing(&mut room, &mut cs).await, [1, 1, 0, 0]);
    }
    assert_eq!(room.hits().len(), 5, "the blows landed");
    assert_eq!(room.sample(1).logic.get("crystal_moves"), Some(0));
    assert_eq!(room.sample(0).logic.get("crystal_moves"), Some(0));
}
