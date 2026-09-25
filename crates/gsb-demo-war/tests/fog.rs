//! Team fog over the sharded map, on the real actors (a live registry
//! relaying four shards' exports): allies map-wide, an enemy seen
//! through a faction's tower on another shard — exactly while the tower
//! sees it — and a third faction that never sees what it has no eyes
//! on.

mod common;

use common::{War, realm};
use gsb_demo_war::war::Kind;
use gsb_demo_war::world::{VISION_RADIUS, tower};
use gsb_kit::team::Team;

/// Ground distance of a record (decimetres) from `at` (metres).
fn from(r: &gsb_demo_war::war::UnitRecord, at: [f32; 2]) -> f32 {
    let (x, z) = (r.x as f32 / 10.0, r.z as f32 / 10.0);
    ((x - at[0]).powi(2) + (z - at[1]).powi(2)).sqrt()
}

/// Two faction-0 players far apart on shards 0 and 3, a faction-1 player
/// on shard 1 and a faction-2 player on shard 2, nobody within sight of
/// an enemy unit. Each faction sees its own players and its four towers
/// wherever they stand — and nothing of the others, for the whole run;
/// the faction-0 player on shard 3 also sees shard 3's two unclaimed
/// capture points (the kit's neutral rule), the one on shard 0 does not.
#[tokio::test(start_paused = true)]
async fn allies_are_seen_map_wide_and_a_faction_sees_nothing_it_has_no_eyes_on() {
    let r = realm(&[
        ("a0", 0, -100.0, -600.0),
        ("b0", 0, 650.0, 150.0),
        ("c1", 1, 650.0, -150.0),
        ("d2", 2, -650.0, 150.0),
    ]);
    let mut war = War::new(&r).await;
    let a = war.join(1, "a0").await;
    let b = war.join(2, "b0").await;
    let c = war.join(3, "c1").await;
    let d = war.join(4, "d2").await;
    war.steps(40).await;
    assert_eq!(
        war.members(),
        [1, 1, 1, 1],
        "every player on its home shard"
    );

    let players = |i: usize| -> Vec<u64> {
        war.clients[i]
            .of_kind(Kind::Player)
            .iter()
            .map(|r| r.entity)
            .collect()
    };
    let mut allies = vec![war.wire(a), war.wire(b)];
    allies.sort_unstable();
    assert_eq!(players(a), allies, "a0 sees its ally on shard 3");
    assert_eq!(players(b), allies, "b0 sees its ally on shard 0");
    assert_eq!(players(c), [war.wire(c)]);
    assert_eq!(players(d), [war.wire(d)]);
    for (i, faction) in [(a, 1), (b, 1), (c, 2), (d, 3)] {
        let towers = war.clients[i].of_kind(Kind::Tower);
        assert_eq!(towers.len(), 4, "client {i}: its faction's towers");
        assert!(towers.iter().all(|t| t.faction == faction), "{towers:?}");
    }
    assert_eq!(
        war.clients[b].of_kind(Kind::Point).len(),
        2,
        "shard 3's neutrals"
    );
    for i in [a, c, d] {
        assert!(war.clients[i].of_kind(Kind::Point).is_empty(), "client {i}");
    }
    // Faction 2 never held another faction's record, nor faction 1.
    for (i, faction) in [(c, 2), (d, 3)] {
        let client = &war.clients[i];
        assert!(client.view.values().all(|r| r.faction == faction));
        for (tick, ids) in &client.history {
            assert!(
                ids.iter()
                    .all(|w| client.get(*w).is_none_or(|r| r.faction == faction)),
                "tick {tick}: {ids:?}"
            );
            assert!(!ids.contains(&war.wire(a)) && !ids.contains(&war.wire(b)));
        }
    }
}

/// A faction-1 player walks up to faction 0's tower on shard 2 and back
/// out. The faction-0 player on shard 3 — 850 m away, with no unit of
/// its own near — sees the enemy from the first record the tower sees
/// (within 60 m of it) until the last; never before, never after. The
/// faction-2 player, with no eyes there, never sees it.
///
/// On the REAL clock: the ticker stamps its ticks with the wall clock, so
/// under tokio's paused clock the game's `dt` — and every walk — would
/// stand still (the other scenarios place units and never walk).
#[tokio::test]
async fn an_enemy_is_seen_through_a_far_tower_on_another_shard() {
    let t = tower(Team(0), 2);
    let r = realm(&[
        ("p0", 0, 650.0, 150.0),
        ("e1", 1, t[0] + 64.0, t[1]),
        ("q2", 2, 650.0, -150.0),
    ]);
    let mut war = War::new(&r).await;
    let p = war.join(1, "p0").await;
    let e = war.join(2, "e1").await;
    let q = war.join(3, "q2").await;
    war.steps(10).await;
    let enemy = war.wire(e);
    assert!(!war.clients[p].sees(enemy), "64 m from the tower: unseen");

    war.clients[e].move_to(t[0] + 52.0, t[1]);
    assert!(
        war.until(300, |w| w.clients[p].sees(enemy)).await,
        "spotted"
    );
    let first = *war.clients[p].get(enemy).expect("in view");
    let d = from(&first, t);
    assert!(
        d <= VISION_RADIUS + 0.05 && d > 52.0,
        "seen from a record within the tower's sight: {d} m"
    );
    assert_eq!(
        first.faction, 2,
        "an enemy (faction 1, 1-based on the wire)"
    );

    war.clients[e].move_to(t[0] + 70.0, t[1]);
    let mut last = first;
    while war.clients[p].sees(enemy) {
        last = *war.clients[p].get(enemy).expect("in view");
        war.step().await;
        assert!(war.tick < 3_000, "never lost from view");
    }
    let d = from(&last, t);
    assert!(d <= VISION_RADIUS + 0.05, "held only while in sight: {d} m");
    war.steps(20).await;
    assert!(!war.clients[p].sees(enemy), "and gone for good");

    let seen = |i: usize| {
        war.clients[i]
            .history
            .iter()
            .any(|(_, ids)| ids.contains(&enemy))
    };
    assert!(!seen(q), "faction 2 has no eyes there");
}
