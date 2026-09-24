//! Height matters, and movement moves visibility — through the real room
//! actor. Every unit here shares ONE spot on the floor (x = z = 0) or
//! walks along a line through it, so a 2D (ground-plane) fog would put
//! everyone in everyone's view: these tests are the ones a planar fog
//! fails. Vision radius: 15 m, 3D. Joins 1, 2, 3 → teams 0, 1, 2.

mod common;

use common::{Arena, Client, SETTLE, set};
use gsb_demo_arena::ArenaGame;

/// Three units stacked on one floor spot: A (team 0) on the floor, B
/// (team 1) 20 m straight above it — beyond the radius — and C (team 2)
/// 10 m up, within it of both. A does not see B nor B see A (planar
/// distance 0!); both see C; C sees both.
#[tokio::test]
async fn a_unit_straight_above_beyond_the_radius_is_hidden() {
    let mut arena = Arena::new(ArenaGame::default());
    let mut cs = vec![
        arena.join(1).await,
        arena.join(2).await,
        arena.join(3).await,
    ];
    cs[0].move_to(0.0, 0.0, 0.0, 0).await; // A
    cs[1].move_to(0.0, 20.0, 0.0, 0).await; // B: 20 m above A
    cs[2].move_to(0.0, 10.0, 0.0, 0).await; // C: 10 m from both
    arena.advance(&mut cs, SETTLE).await;
    let [a, b, c] = [cs[0].id, cs[1].id, cs[2].id];

    assert_eq!(cs[0].sees(), set(&[a, c]), "team 0: B is 20 m above A");
    assert_eq!(cs[1].sees(), set(&[b, c]), "team 1: A is 20 m below B");
    assert_eq!(cs[2].sees(), set(&[a, b, c]), "team 2: both within 10 m");
}

/// Step `ticks` ticks one at a time, returning for each tick whether
/// `observer` had `unit` in view and, if so, at what height (cm).
async fn watch(
    arena: &mut Arena,
    cs: &mut [Client],
    observer: usize,
    unit: u64,
    ticks: u32,
) -> Vec<Option<i32>> {
    let mut seen = Vec::new();
    for _ in 0..ticks {
        arena.advance(cs, 1).await;
        seen.push(cs[observer].view.get(&unit).map(|r| r.y));
    }
    seen
}

/// An enemy drops into view and climbs back out of it, tick by tick. B
/// (team 1) hovers 25 m above A (team 0), descends to 5 m, then climbs
/// back: it enters A's team package on the FIRST tick it is within
/// 15 m (at ≈ 15 m height, mid-descent — not only once it stops) and
/// leaves it on the first tick beyond; whenever it is in the package it
/// is within the radius. C (team 2) meanwhile walks in along the floor
/// and back out.
#[tokio::test]
async fn movement_brings_enemies_into_view_and_takes_them_out() {
    let mut arena = Arena::new(ArenaGame::default());
    let mut cs = vec![
        arena.join(1).await,
        arena.join(2).await,
        arena.join(3).await,
    ];
    cs[0].move_to(0.0, 0.0, 0.0, 0).await; // A
    cs[1].move_to(0.0, 25.0, 0.0, 0).await; // B hovers out of reach
    cs[2].move_to(30.0, 0.0, 0.0, 0).await; // C, 30 m away on the floor
    arena.advance(&mut cs, SETTLE).await;
    let [a, b, c] = [cs[0].id, cs[1].id, cs[2].id];
    assert_eq!(cs[0].sees(), set(&[a]), "nobody in range yet");

    // B descends 25 → 5 m at 12 m/s (0.4 m per tick).
    cs[1].move_to(0.0, 5.0, 0.0, 0).await;
    let seen = watch(&mut arena, &mut cs, 0, b, 90).await;
    let first = seen
        .iter()
        .position(Option::is_some)
        .expect("B came into view");
    let entry = seen[first].expect("seen");
    assert!(
        (1455..=1500).contains(&entry),
        "B appears the first tick it is within 15 m: at {entry} cm"
    );
    assert!(
        first > 0 && seen[first - 1].is_none(),
        "absent the tick before"
    );
    assert!(
        seen[first..].iter().all(|y| y.is_some_and(|y| y <= 1500)),
        "B stays in view, always within the radius: {seen:?}"
    );
    assert_eq!(cs[0].view[&b].y, 500, "B settled 5 m above A");

    // B climbs back to 25 m: it leaves on the first tick beyond 15 m.
    cs[1].move_to(0.0, 25.0, 0.0, 0).await;
    let seen = watch(&mut arena, &mut cs, 0, b, 90).await;
    let gone = seen.iter().position(Option::is_none).expect("B left view");
    assert!(gone > 0, "B was still in view while climbing");
    assert!(
        seen[..gone].iter().all(|y| y.is_some_and(|y| y <= 1500)),
        "in view only within the radius: {seen:?}"
    );
    assert!(seen[gone..].iter().all(Option::is_none), "and stays out");

    // C walks along the floor to 5 m from A, then back out to 30 m.
    cs[2].move_to(5.0, 0.0, 0.0, 0).await;
    arena.advance(&mut cs, SETTLE).await;
    assert_eq!(cs[0].sees(), set(&[a, c]), "C walked into view");
    cs[2].move_to(30.0, 0.0, 0.0, 0).await;
    arena.advance(&mut cs, SETTLE).await;
    assert_eq!(cs[0].sees(), set(&[a]), "C walked back out");
}
