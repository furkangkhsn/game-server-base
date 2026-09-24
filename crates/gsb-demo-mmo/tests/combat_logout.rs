//! The MMO's reconnect policy — a logout timer, but no logout in combat
//! — through the real shard actors (`docs/RECONNECT.md` §17):
//!
//! 1. a character disconnected mid-fight is held PAST its logout grace
//!    for as long as it is in combat (the game's `may_release` veto,
//!    asked by the core from the grace's deadline on), and logs out on
//!    the first tick after the fight has cooled down;
//! 2. a fight that outlasts the room's veto ceiling
//!    (`RoomConfig::max_detach_hold`) does not keep it: the logout is
//!    forced.

mod common;

use std::time::Duration;

use common::{Client, Mmo};
use gsb_demo_mmo::mmo::Kind;
use gsb_demo_mmo::world::COMBAT_TICKS;
use gsb_demo_mmo::{MobSpawn, Pos3, Realm, components};

const GRACE: Duration = Duration::from_millis(100);

/// Ann (conn 1) stands 10 m from a sturdy mob; an observer (conn 2)
/// watches from nearby. Both on shard 0.
fn realm() -> Realm {
    Realm::empty()
        .with_login(1, Pos3::new(-100.0, 0.0, -100.0))
        .with_login(2, Pos3::new(-120.0, 0.0, -100.0))
        .with_spawn(MobSpawn::once(
            components::Kind::Mob,
            Pos3::new(-90.0, 0.0, -100.0),
            1,
            100_000,
            1000,
        ))
}

/// Ann joins, hits the mob once (a landed hit: combat starts), and her
/// transport dies. Returns Ann's client and the tick the hit landed on.
async fn fight_then_disconnect(room: &mut Mmo, cs: &mut Vec<Client>) -> (Client, u64) {
    let mut ann = room.join(1, "ann", &mut []).await;
    cs.push(room.join(2, "obs", &mut []).await);
    room.steps(cs, 3).await;
    let mobs = cs[0].of_kind(Kind::Mob);
    assert_eq!(mobs.len(), 1, "the observer sees the mob: {mobs:?}");
    let mob = mobs[0].0;
    ann.attack(mob).await;
    room.step(cs).await;
    let hit = room.tick;
    room.step(cs).await;
    assert_eq!(cs[0].get(mob).map(|r| r.hp), Some(975), "the hit landed");
    room.detach(&ann, "ann", cs).await;
    (ann, hit)
}

#[tokio::test]
async fn a_character_disconnected_in_combat_logs_out_when_the_fight_ends() {
    let mut room = Mmo::with(&realm(), GRACE, 2.0);
    let mut cs = Vec::new();
    let (ann, hit) = fight_then_disconnect(&mut room, &mut cs).await;
    tokio::time::sleep(GRACE * 3).await; // the grace is wall-clock
    room.steps(&mut cs, 3).await;

    let s = room.sample(0);
    assert_eq!(
        (s.members, s.detached, s.detach_expired_despawn),
        (2, 1, 0),
        "past the grace, in combat: still parked, slot held"
    );
    assert!(
        cs[0].get(ann.id).is_some(),
        "the character stays in the world"
    );

    // The fight cools down at `hit + COMBAT_TICKS` (the combat system of
    // that tick); the sweep of every tick up to it still sees the veto.
    while room.tick < hit + COMBAT_TICKS {
        room.step(&mut cs).await;
    }
    let s = room.sample(0);
    assert_eq!(
        (s.detached, s.detach_expired_despawn),
        (1, 0),
        "held through the whole fight"
    );

    // The next tick's sweep finds the veto lifted: the logout completes.
    room.step(&mut cs).await;
    let s = room.sample(0);
    assert_eq!(
        (s.detach_expired_despawn, s.detach_expired_ai),
        (1, 0),
        "the hold ended in a logout on the first tick out of combat"
    );
    assert_eq!((s.members, s.detached), (1, 0), "the slot is released");
    room.step(&mut cs).await;
    assert!(cs[0].get(ann.id).is_none(), "the character left the world");
    assert!(
        room.resume(9, 2, "ann", &mut cs).await.is_none(),
        "nothing is left to resume"
    );
}

#[tokio::test]
async fn a_fight_that_outlasts_the_ceiling_is_logged_out_anyway() {
    let ceiling = Duration::from_secs(1);
    let mut room = Mmo::with_ceiling(&realm(), GRACE, ceiling);
    let mut cs = Vec::new();
    let (ann, hit) = fight_then_disconnect(&mut room, &mut cs).await;
    tokio::time::sleep(GRACE * 3).await;
    room.steps(&mut cs, 2).await;
    let s = room.sample(0);
    assert_eq!(
        (s.detached, s.detach_expired_despawn),
        (1, 0),
        "past the grace, short of the ceiling: the fight holds it"
    );

    tokio::time::sleep(ceiling).await; // the ceiling is wall-clock too
    room.steps(&mut cs, 2).await;
    assert!(
        room.tick < hit + COMBAT_TICKS,
        "the fight is still on (tick {} < {})",
        room.tick,
        hit + COMBAT_TICKS
    );
    let s = room.sample(0);
    assert_eq!(
        (s.members, s.detached, s.detach_expired_despawn),
        (1, 0, 1),
        "the ceiling forced the logout mid-fight"
    );
    room.step(&mut cs).await;
    assert!(cs[0].get(ann.id).is_none(), "the character left the world");
}
