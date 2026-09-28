//! A unit's own sight radius (A8) on the real actors: what a hero sees
//! past the room radius reaches its allies on every shard through the
//! hub, a ward's short sight hides what the room radius would show, and
//! the radius crosses a seam with its unit.

use super::*;

/// Team 0: a hero on shard 0 (sight 45), a ward on shard 2 (sight 5),
/// a far player on shard 3. Team 1: a raider 40 from the hero (past
/// the room radius of 25), a sneak 15 from the ward (within it), and a
/// lurker on shard 1, 56 east of where the hero starts and 40 from
/// where it ends. The far player sees the raider (through the hero, on
/// another shard) but never the sneak; then the hero walks over the
/// x = 0 seam and — its radius carried to shard 1 — sees the lurker,
/// which the room radius would not.
#[tokio::test(start_paused = true)]
async fn a_heros_sight_reaches_every_ally_and_crosses_the_seam() {
    let mut rig = Rig::new(true).await;
    let hero = rig.join(1, "0:-7:-60:45").await;
    let _ward = rig.join(2, "0:-60:60:5").await;
    let far = rig.join(3, "0:50:50").await;
    let raider = rig.join(4, "1:-47:-60").await;
    let sneak = rig.join(5, "1:-60:75").await;
    let lurker = rig.join(6, "1:49:-60").await;
    rig.steps(4).await;
    assert_eq!(rig.members, [2, 1, 2, 1]);
    let wire = |i: usize| rig.clients[i].wire;
    let (r, s, l) = (wire(raider), wire(sneak), wire(lurker));
    assert!(rig.clients[far].sees(r), "the hero's sight, map-wide");
    assert!(rig.clients[hero].sees(r));
    assert!(!rig.clients[far].sees(s), "the ward's short sight");
    assert!(!rig.clients[far].sees(l) && !rig.clients[hero].sees(l));

    // Two units a tick eastward: x = -5, -3, …, 9 — over the seam.
    for i in 0..8 {
        rig.clients[hero].move_to(-5.0 + 2.0 * i as f32, -60.0);
        rig.step().await;
    }
    rig.steps(3).await;
    assert_eq!(rig.members, [1, 2, 2, 1], "the hero is shard 1's");
    assert!(rig.clients[hero].sees(l), "40 away, by the carried radius");
    assert!(rig.clients[far].sees(l));
    assert!(
        rig.clients[far]
            .history
            .iter()
            .all(|(_, ids)| !ids.contains(&s)),
        "the sneak was never seen"
    );
    assert!(rig.clients.iter().all(|c| c.doubled == 0));
}
