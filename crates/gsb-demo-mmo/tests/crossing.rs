//! Shard-border crossings through the real shard actors: a PLAYER runs
//! across a seam, a flying MOB (no player, no speed-like component)
//! patrols across one, and a player TELEPORTS into the diagonally
//! opposite shard (a neighbour across the corner — the MMO's grid is the
//! kit's 8-neighbourhood). Each keeps its wire id and the state its
//! `MmoMig` carries, and the clients on both sides keep a consistent
//! stream.

mod common;

use common::{Client, Mmo};
use gsb_demo_mmo::mmo::Kind;
use gsb_demo_mmo::{MobSpawn, Pos3, Realm, components};

/// Player P runs from shard 0 across the x = 0 seam into shard 1 while
/// A (shard 0) and B (shard 1) watch: every tick P's own view holds P
/// (no vanished self — the arrival tick included), P only ever moves forward, A and B see P before,
/// during and after the crossing (through the border strip on the side
/// that does not own it), and P arrives with its wire id and hit points,
/// having kept running on the pending walk its `MmoMig` carried.
#[tokio::test]
async fn a_player_crossing_a_seam_keeps_its_id_state_and_stream() {
    let realm = Realm::empty()
        .with_login("c1", Pos3::new(-30.0, 0.0, -200.0))
        .with_login("c2", Pos3::new(-60.0, 0.0, -200.0))
        .with_login("c3", Pos3::new(60.0, 0.0, -200.0));
    let mut room = Mmo::new(&realm);
    let mut cs: Vec<Client> = Vec::new();
    for conn in 1..=3 {
        let c = room.join(conn, &format!("c{conn}"), &mut cs).await;
        cs.push(c);
    }
    room.steps(&mut cs, 3).await;
    assert_eq!(room.members(), [2, 1, 0, 0]);
    let p = cs[0].id;
    cs[0].move_to(40.0, -200.0).await;

    let mut last_x = i32::MIN;
    let mut crossed = false;
    for _ in 0..360 {
        room.step(&mut cs).await;
        crossed |= room.members() == [1, 2, 0, 0];
        for (who, c) in [("A", &cs[1]), ("B", &cs[2])] {
            assert!(c.get(p).is_some(), "{who} lost P at tick {}", room.tick);
        }
        // Never lost — not even on the arrival tick, whose one-shot full
        // omitted P itself before the kit's F1 fix (KIT-ARCHITECTURE §10).
        let me = cs[0]
            .me()
            .unwrap_or_else(|| panic!("P lost itself at tick {}", room.tick));
        assert!(me.x >= last_x, "P moved backwards: {} -> {}", last_x, me.x);
        last_x = me.x;
    }
    assert!(crossed, "the session moved to shard 1 with its entity");
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
        .with_login("c1", Pos3::new(-55.0, 0.0, -300.0))
        .with_login("c2", Pos3::new(60.0, 0.0, -300.0))
        .with_spawn(spawn);
    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "c1", &mut []).await];
    let b = room.join(2, "c2", &mut cs).await;
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

/// P (shard 0) uses the waystone of shard 3 — the diagonal, a neighbour
/// across the corner (`GridPartition2::with_diagonals`). The session is
/// never held by two shards; it is in flight for exactly one tick (one
/// hop, straight to shard 3 — with the kit's default 4-neighbourhood it
/// took two, through an edge neighbour: KIT-ARCHITECTURE §10, F2) and
/// lands on shard 3 once, with the same wire id, the cancelled walk
/// staying cancelled.
#[tokio::test]
async fn a_teleport_into_the_diagonal_shard_lands_once_with_the_same_id() {
    let realm = Realm::empty()
        .with_login("c1", Pos3::new(-256.0, 0.0, -250.0))
        .with_login("c2", Pos3::new(250.0, 0.0, 250.0));
    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "c1", &mut []).await];
    let d = room.join(2, "c2", &mut cs).await;
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
            // Landed: from the landing tick on, P shows to itself and to
            // D, standing on the waystone (the walk the teleport
            // cancelled does not resume).
            for (who, r) in [("P", cs[0].me()), ("D", cs[1].get(p))] {
                let r = r.unwrap_or_else(|| panic!("{who} misses P at tick {}", room.tick));
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
    assert_eq!(in_flight, 1, "one hop, one tick");

    // The waystone's cell (4,4): P stands there alone — and shows, to
    // itself and to D (before the kit's F1 fix the landing erased it from
    // that cell for good; the test walked it one cell west to see it).
    let me = *cs[0].me().expect("P sees itself on the waystone");
    assert_eq!(
        (me.x, me.y, me.z),
        (2560, 0, 2560),
        "the old walk stayed cancelled"
    );
    assert_eq!(cs[1].get(p), Some(&me), "D sees P, same wire id");
    assert_eq!(room.members(), [0, 0, 0, 2]);
}
