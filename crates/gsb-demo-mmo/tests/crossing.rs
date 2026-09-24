//! Shard-border crossings through the real shard actors: a PLAYER runs
//! across a seam, a flying MOB (no player, no speed-like component)
//! patrols across one, and a player TELEPORTS into the diagonally
//! opposite shard (not a neighbour of its own — the kit routes it hop by
//! hop). Each keeps its wire id and the state its `MmoMig` carries, and
//! the clients on both sides keep a consistent stream.

mod common;

use common::{Client, Mmo};
use gsb_demo_mmo::mmo::Kind;
use gsb_demo_mmo::{MobSpawn, Pos3, Realm, components};

/// Player P runs from shard 0 across the x = 0 seam into shard 1 while
/// A (shard 0) and B (shard 1) watch: every tick P's own view holds P
/// (no vanished self), P only ever moves forward, A and B see P before,
/// during and after the crossing (through the border strip on the side
/// that does not own it), and P arrives with its wire id and hit points,
/// having kept running on the pending walk its `MmoMig` carried.
#[tokio::test]
async fn a_player_crossing_a_seam_keeps_its_id_state_and_stream() {
    let realm = Realm::empty()
        .with_login(1, Pos3::new(-30.0, 0.0, -200.0))
        .with_login(2, Pos3::new(-60.0, 0.0, -200.0))
        .with_login(3, Pos3::new(60.0, 0.0, -200.0));
    let mut room = Mmo::new(&realm);
    let mut cs: Vec<Client> = Vec::new();
    for conn in 1..=3 {
        let c = room.join(conn, "", &mut cs).await;
        cs.push(c);
    }
    room.steps(&mut cs, 3).await;
    assert_eq!(room.members(), [2, 1, 0, 0]);
    let p = cs[0].id;
    cs[0].move_to(40.0, -200.0).await;

    let mut last_x = i32::MIN;
    let mut crossed_at = None;
    let mut lost = Vec::new();
    for _ in 0..360 {
        room.step(&mut cs).await;
        if crossed_at.is_none() && room.members() == [1, 2, 0, 0] {
            crossed_at = Some(room.tick);
        }
        for (who, c) in [("A", &cs[1]), ("B", &cs[2])] {
            assert!(c.get(p).is_some(), "{who} lost P at tick {}", room.tick);
        }
        let Some(me) = cs[0].me() else {
            lost.push(room.tick);
            continue;
        };
        assert!(me.x >= last_x, "P moved backwards: {} -> {}", last_x, me.x);
        last_x = me.x;
    }
    assert!(
        crossed_at.is_some(),
        "the session moved to shard 1 with its entity"
    );
    // KIT FINDING (docs/KIT-ARCHITECTURE.md §10, "Faz 4 sonucu", F1): the
    // arrival tick's one-shot full omits the arriving entity itself — the
    // sharded × spatial strip ledger exits the stale borrowed copy from
    // the very bucket the dirty pass just placed the own record in. The
    // next frame restores it; nothing else may ever lose it.
    assert!(
        lost.iter().all(|t| Some(*t) == crossed_at),
        "P lost itself outside the arrival tick {crossed_at:?}: {lost:?}"
    );
    let me = *cs[0].me().expect("P");
    assert_eq!(
        (me.x, me.y, me.z, me.hp),
        (400, 0, -2000, 100),
        "arrived on the walk"
    );
    assert_eq!(me.kind, Kind::Player as i32);
    for c in &cs[1..] {
        assert_eq!(c.get(p), Some(&me), "A and B agree with P about P");
    }
    assert_eq!(room.members(), [1, 2, 0, 0]);
}

/// A flyer (20 m up) spawned by shard 0's camp patrols across the seam
/// into shard 1 at its own pace — an entity with no player and no speed
/// component. B (shard 1) sees the SAME wire id all along; after the
/// crossing the mob still flies at its altitude, still carries the
/// damage A dealt it on shard 0, walks on to its route's second leg, and
/// dies on shard 1 on the tick shard 0's camp scheduled.
#[tokio::test]
async fn a_mob_crossing_a_seam_keeps_its_id_and_its_brain() {
    let spawn = MobSpawn::once(
        components::Kind::Flyer,
        Pos3::new(-40.0, 20.0, -300.0),
        5,
        600,
        100,
    )
    .walking(vec![[40.0, -300.0], [40.0, -260.0]], 8.0, false);
    let realm = Realm::empty()
        .with_login(1, Pos3::new(-55.0, 0.0, -300.0))
        .with_login(2, Pos3::new(60.0, 0.0, -300.0))
        .with_spawn(spawn);
    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "", &mut []).await];
    let b = room.join(2, "", &mut cs).await;
    cs.push(b);
    room.steps(&mut cs, 4).await; // tick 6: the flyer is up
    let flyers = cs[0].of_kind(Kind::Flyer);
    assert_eq!(flyers.len(), 1, "A sees the flyer: {flyers:?}");
    let mob = flyers[0].0;
    cs[0].attack(mob).await;
    cs[0].attack(mob).await;

    let mut on_shard_1 = false;
    for _ in 0..560 {
        room.step(&mut cs).await;
        let seen = cs[1].of_kind(Kind::Flyer);
        assert_eq!(
            seen.len(),
            1,
            "B sees exactly one flyer at tick {}",
            room.tick
        );
        assert_eq!(seen[0].0, mob, "the same wire id on both sides");
        let r = seen[0].1;
        assert_eq!(r.y, 200, "the altitude travelled");
        on_shard_1 |= r.x > 0;
        if on_shard_1 {
            assert_eq!(r.hp, 50, "the damage dealt on shard 0 travelled");
        }
    }
    assert!(on_shard_1);
    let r = *cs[1].get(mob).expect("still up");
    assert_eq!((r.x, r.z), (400, -2600), "walked on to the second leg");

    // Scheduled on shard 0 (spawned at 5, lifetime 600): dies at 605 on
    // shard 1, game code — and both clients forget it.
    while room.tick < 604 {
        room.step(&mut cs).await;
    }
    assert!(cs[1].get(mob).is_some(), "alive until its tick");
    room.steps(&mut cs, 2).await;
    assert!(
        cs[1].get(mob).is_none() && cs[0].get(mob).is_none(),
        "despawned on schedule"
    );
}

/// P (shard 0) uses the waystone of shard 3 — the diagonal, not a
/// neighbour. The session is never held by two shards; it is in flight
/// for exactly two ticks (two hops through an edge neighbour, which
/// installs and forwards it within one tick — §8.4) and lands on shard 3
/// once, with the same wire id, the cancelled walk staying cancelled.
#[tokio::test]
async fn a_teleport_into_a_non_adjacent_shard_lands_once_with_the_same_id() {
    let realm = Realm::empty()
        .with_login(1, Pos3::new(-256.0, 0.0, -250.0))
        .with_login(2, Pos3::new(250.0, 0.0, 250.0));
    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "", &mut []).await];
    let d = room.join(2, "", &mut cs).await;
    cs.push(d);
    room.steps(&mut cs, 3).await;
    assert_eq!(room.members(), [1, 0, 0, 1]);
    let p = cs[0].id;
    cs[0].move_to(-256.0, -400.0).await; // a walk the teleport cancels
    room.steps(&mut cs, 2).await;
    assert_eq!(room.members(), [1, 0, 0, 1]);
    cs[0].travel(3).await;

    // Who holds P's session, tick by tick (`None` = in flight: the core
    // moves the row inside the `Migrate` message, installed next tick).
    let mut owners: Vec<Option<usize>> = vec![Some(0)];
    let mut in_flight = 0;
    for _ in 0..40 {
        room.step(&mut cs).await;
        let m = room.members();
        assert!(
            m[3] >= 1 && m.iter().sum::<u32>() <= 2,
            "P held twice: {m:?}"
        );
        let holder = (0..3).find(|&s| m[s] == 1).or((m[3] == 2).then_some(3));
        in_flight += usize::from(holder.is_none());
        if owners.last() != Some(&holder) {
            owners.push(holder);
        }
        if holder == Some(3) {
            // Landed: wherever P shows, it is standing on the waystone
            // (the walk the teleport cancelled does not resume).
            for r in [cs[0].me(), cs[1].get(p)].into_iter().flatten() {
                assert_eq!(
                    (r.x, r.z),
                    (2560, 2560),
                    "P walks again at tick {}",
                    room.tick
                );
            }
        }
    }
    assert_eq!(owners, [Some(0), None, Some(3)], "owned by shard 3, once");
    assert_eq!(in_flight, 2, "two hops, one tick each");

    // The waystone's cell (4,4): P stands there alone, and the arrival
    // is invisible to every full (KIT FINDING F1 — pinned in
    // `tests/findings.rs`). One step west into cell (3,4) and it shows.
    cs[0].move_to(250.0, 256.0).await;
    room.steps(&mut cs, 40).await;
    let me = *cs[0].me().expect("P sees itself");
    assert_eq!(
        (me.x, me.y, me.z),
        (2500, 0, 2560),
        "the old walk stayed cancelled"
    );
    assert_eq!(cs[1].get(p), Some(&me), "D sees P, same wire id");
    assert_eq!(room.members(), [0, 0, 0, 2]);
}
