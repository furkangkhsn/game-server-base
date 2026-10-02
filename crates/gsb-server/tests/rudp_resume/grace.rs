//! Scenario 3: the player returns after the park grace — the two
//! documented outcomes (RECONNECT §5, §9), each pinned exactly.

use super::flows::{after_one_drop, in_game, reconnect, vanish};
use super::player::warm_during;
use super::rig::{self, Door, Rig, RoomCounts, Shape};

/// Scenario 3: the grace runs out before the player is back. The demo's
/// policy hands an expired hold to its bot (`ExpireTo::AiHandover`,
/// RECONNECT §9): the entity stays in the world, still parked, still a
/// resume target — the returning player reclaims the SAME entity.
pub async fn resume_after_the_grace(door: Door) {
    let mut rig = Rig::start(rig::config(door, Shape::Single, 1.0)).await;
    let first = in_game(&rig, door, "late").await;
    let entity = first.entity;
    let (_port, old) = vanish(first);

    let expired = rig
        .until("the hold to expire to the bot", |s| s.room.expired_ai == 1)
        .await;
    assert_eq!(expired.room.detached, 1, "a bot-held row is still parked");
    assert_eq!(expired.room.expired_despawn, 0, "{expired:?}");
    assert_eq!(rig.members().await, 1, "the bot keeps the slot");

    let mut again = reconnect(&rig, door, old).await;
    assert_eq!(again.join("late").await, entity, "reclaimed from the bot");
    again.moves().await;
    let s = warm_during(&mut again, async {
        rig.until("the reclaim to be counted", |s| {
            s.room.resumes == 1 && s.room.detached == 0
        })
        .await;
        rig.settle(2).await
    })
    .await;
    assert_eq!(
        s.room,
        RoomCounts {
            joins: 1,
            resumes: 1,
            expired_ai: 1,
            ..Default::default()
        },
        "{door:?}: {s:?}"
    );
    after_one_drop(&s, door, 0);
    rig.stop().await;
}

/// Scenario 3, the other documented outcome: with no grace at all
/// (`disconnect_grace_secs = 0`, `Detach::Despawn`) nothing is parked,
/// and the same credentials make a transparent FRESH join — a new
/// entity, no error, no resume counted (RECONNECT §5).
pub async fn no_grace_is_a_fresh_join(door: Door) {
    let mut rig = Rig::start(rig::config(door, Shape::Single, 0.0)).await;
    let first = in_game(&rig, door, "gone").await;
    let entity = first.entity;
    let (_port, old) = vanish(first);

    let gone = rig
        .until("the session to close and its row to go", |s| {
            s.reg.closes == 1 && s.reg.conns == 0
        })
        .await;
    assert_eq!(gone.room.detached, 0, "nothing is parked: {gone:?}");
    assert_eq!(rig.members().await, 0, "the slot is released");

    let mut again = reconnect(&rig, door, old).await;
    let fresh = again.join("gone").await;
    assert_ne!(fresh, entity, "{door:?}: a fresh entity, not the old one");
    again.moves().await;
    let s = warm_during(&mut again, async {
        rig.until("the fresh join to be counted", |s| s.room.joins == 2)
            .await;
        rig.settle(2).await
    })
    .await;
    assert_eq!(
        s.room,
        RoomCounts {
            joins: 2,
            ..Default::default()
        },
        "{door:?}: two fresh joins; nothing resumed, parked or stale"
    );
    after_one_drop(&s, door, 1);
    assert_eq!(rig.members().await, 1);
    rig.stop().await;
}
