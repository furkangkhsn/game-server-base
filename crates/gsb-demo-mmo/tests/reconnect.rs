//! The MMO's disconnect policy through the real shard actors, on the
//! kit's park machinery (RECONNECT §3/§9, the registry's broadcast
//! detach/resume shape):
//!
//! 1. a dropped session's character is PARKED: it stays in the world
//!    (others keep seeing it), holds its slot, and a resume reclaims it
//!    with the same wire id and working numbered input;
//! 2. when the grace runs out the logout completes — the MMO's logout
//!    timer: the slot is released, the character leaves every view, and
//!    nothing is left to resume (Phase 4 could not express this: the
//!    kit ended every hold in AI handover — KIT-ARCHITECTURE §10, F4);
//! 3. a room built to end the hold in AI handover instead hands the
//!    character to the MMO's logout bot: it stays alive, holds its slot,
//!    and walks to the nearest waystone;
//! 4. a zero grace releases the slot at once (no park at all).

mod common;

use std::time::Duration;

use common::Mmo;
use gsb_core::room::ExpireTo;
use gsb_demo_mmo::{Pos3, Realm};

fn realm() -> Realm {
    Realm::empty()
        .with_login(1, Pos3::new(-100.0, 0.0, -100.0))
        .with_login(2, Pos3::new(-120.0, 0.0, -100.0))
}

#[tokio::test]
async fn a_parked_character_stays_and_resumes_with_its_wire_id() {
    let mut room = Mmo::with(&realm(), Duration::from_secs(3600), 2.0);
    let p = room.join(1, "ann", &mut []).await;
    let mut cs = vec![room.join(2, "obs", &mut []).await];
    room.steps(&mut cs, 3).await;
    assert!(cs[0].get(p.id).is_some());

    room.detach(&p, "ann", &mut cs).await;
    room.steps(&mut cs, 30).await;
    assert!(
        cs[0].get(p.id).is_some(),
        "the parked character stays in the world"
    );
    let s = room.sample(0);
    assert_eq!((s.members, s.detached), (2, 1), "parked: slot held");

    let (shard, back) = room
        .resume(9, 2, "ann", &mut cs)
        .await
        .expect("the ledger holds ann");
    assert_eq!((shard, back.id), (0, p.id), "same shard, same wire id");
    cs.push(back);
    room.steps(&mut cs, 2).await;
    assert!(
        cs[1].me().is_some(),
        "the resumed session is baselined on itself"
    );
    cs[1].move_to(-100.0, -90.0).await;
    room.steps(&mut cs, 60).await;
    assert_eq!(cs[1].acks.last(), Some(&1), "numbered input works again");
    let me = *cs[1].me().expect("me");
    assert_eq!((me.x, me.z), (-1000, -900), "the human drives it again");
    let s = room.sample(0);
    assert_eq!((s.members, s.detached, s.resumes), (2, 0, 1));
}

#[tokio::test]
async fn grace_expiry_logs_the_character_out() {
    let mut room = Mmo::with(&realm(), Duration::from_millis(100), 2.0);
    let p = room.join(1, "ann", &mut []).await;
    let mut cs = vec![room.join(2, "obs", &mut []).await];
    room.steps(&mut cs, 3).await;
    room.detach(&p, "ann", &mut cs).await;
    tokio::time::sleep(Duration::from_millis(250)).await; // the grace is wall-clock
    room.steps(&mut cs, 3).await;

    let s = room.sample(0);
    assert_eq!(
        (s.detach_expired_despawn, s.detach_expired_ai),
        (1, 0),
        "the hold ended in a logout, not in AI handover"
    );
    assert_eq!((s.members, s.detached), (1, 0), "the slot is released");
    assert!(cs[0].get(p.id).is_none(), "the character left the world");
    assert!(
        room.resume(9, 2, "ann", &mut cs).await.is_none(),
        "nothing is left to resume"
    );
}

#[tokio::test]
async fn an_ai_handover_policy_hands_the_character_to_the_logout_bot() {
    let grace = Duration::from_millis(100);
    let mut room = Mmo::with_policy(&realm(), grace, ExpireTo::AiHandover, 2.0);
    let p = room.join(1, "ann", &mut []).await;
    let mut cs = vec![room.join(2, "obs", &mut []).await];
    room.steps(&mut cs, 3).await;
    room.detach(&p, "ann", &mut cs).await;
    tokio::time::sleep(Duration::from_millis(250)).await; // the grace is wall-clock
    room.steps(&mut cs, 3).await;

    let s = room.sample(0);
    assert_eq!(s.detach_expired_ai, 1, "the hold ended in AI handover");
    assert_eq!(
        (s.detach_expired_despawn, s.members),
        (0, 2),
        "the slot is kept"
    );

    // The logout bot walks the character toward the nearest waystone
    // (-256, -256) through the ordinary input path.
    let dist =
        |r: &gsb_demo_mmo::mmo::EntityRecord| ((r.x + 2560) as f32).hypot((r.z + 2560) as f32);
    let before = dist(cs[0].get(p.id).expect("still in the world"));
    room.steps(&mut cs, 60).await;
    let after = dist(cs[0].get(p.id).expect("alive under bot control"));
    assert!(
        before - after > 100.0,
        "the bot walked it: {before} -> {after} dm"
    );
}

#[tokio::test]
async fn zero_grace_releases_the_slot_at_once() {
    let mut room = Mmo::with(&realm(), Duration::ZERO, 2.0);
    let p = room.join(1, "ann", &mut []).await;
    let mut cs = vec![room.join(2, "obs", &mut []).await];
    room.steps(&mut cs, 3).await;
    assert!(cs[0].get(p.id).is_some());

    room.detach(&p, "ann", &mut cs).await;
    room.steps(&mut cs, 2).await;
    assert!(cs[0].get(p.id).is_none(), "despawned");
    let s = room.sample(0);
    assert_eq!((s.members, s.detached), (1, 0), "the slot is released");
    assert!(
        room.resume(9, 2, "ann", &mut cs).await.is_none(),
        "nothing parked"
    );
}
