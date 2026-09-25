//! Capture points on the real actors — the kit's neutral rule
//! (`docs/CROSS-SHARD.md` §8b.5 point 8, BACKLOG A27) as the game meets
//! it: an UNCLAIMED point is shown to every player on its own shard,
//! however far, and to a player elsewhere only through vision; a
//! CAPTURED point is its faction's unit — seen by that faction map-wide,
//! fogged for the others like any enemy.

mod common;

use common::{War, realm};
use gsb_demo_war::war::{Kind, UnitRecord};
use gsb_demo_war::world::{CAPTURE_TICKS, POINTS};

/// The point at `(x, z)` metres in client `i`'s view.
fn point(war: &War, i: usize, [x, z]: [f32; 2]) -> Option<UnitRecord> {
    war.clients[i]
        .of_kind(Kind::Point)
        .into_iter()
        .find(|r| (r.x, r.z) == ((x * 10.0) as i32, (z * 10.0) as i32))
}

#[tokio::test(start_paused = true)]
async fn an_unclaimed_point_follows_the_shard_a_captured_one_the_faction() {
    let [mx, mz] = POINTS[0];
    let r = realm(&[
        ("near0", 0, 700.0, 100.0),  // shard 3, 640 m from the middle
        ("far0", 0, -100.0, -600.0), // shard 0
        ("taker1", 1, mx + 2.0, mz), // shard 3, on the middle point
        ("home1", 1, 650.0, -150.0), // shard 1
    ]);
    let mut war = War::new(&r).await;
    let near = war.join(1, "near0").await;
    let far = war.join(2, "far0").await;
    let taker = war.join(3, "taker1").await;
    let home = war.join(4, "home1").await;
    war.steps(3).await;

    // Unclaimed: on shard 3 everyone sees both points, however far; off
    // shard 3 a point is seen only through a unit that sees it — faction
    // 1's player on shard 1 sees the middle one through its ally standing
    // on it, not the other; faction 0's player on shard 0 sees neither.
    for p in POINTS {
        assert_eq!(point(&war, near, p).map(|r| r.faction), Some(0), "{p:?}");
        assert!(point(&war, taker, p).is_some(), "{p:?}");
        assert!(point(&war, far, p).is_none(), "{p:?}");
    }
    assert_eq!(point(&war, home, POINTS[0]).map(|r| r.faction), Some(0));
    assert!(point(&war, home, POINTS[1]).is_none());

    // Faction 1 alone on the middle point takes it.
    let taken = |w: &War| point(w, taker, POINTS[0]).is_some_and(|r| r.faction == 2);
    assert!(war.until(CAPTURE_TICKS + 10, taken).await, "captured");
    war.steps(2).await;
    let middle = point(&war, taker, POINTS[0]).expect("its own point");
    assert_eq!(point(&war, home, POINTS[0]), Some(middle), "faction 1's");
    // Its taker gone, the point stays in faction 1's view: a member now,
    // not a unit someone sees.
    war.leave(taker).await;
    war.steps(4).await;
    assert_eq!(war.members()[3], 1, "the taker left shard 3");
    assert_eq!(
        point(&war, home, POINTS[0]),
        Some(middle),
        "faction 1, map-wide"
    );
    assert!(
        point(&war, near, POINTS[0]).is_none(),
        "an enemy unit now: fogged"
    );
    assert!(point(&war, far, POINTS[0]).is_none());
    // The other point is still neutral: still the shard's.
    assert_eq!(point(&war, near, POINTS[1]).map(|r| r.faction), Some(0));
    assert!(point(&war, home, POINTS[1]).is_none());
}
