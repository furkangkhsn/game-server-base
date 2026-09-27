//! The per-cause disconnect policy (BACKLOG F27) on the REAL actors:
//! a team room built with "kicked → despawn" — four shard actors under a
//! live registry, and the single-world room actor — despawns the player
//! the game kicks while it parks the one whose transport died, in the
//! same room and the same tick.

use std::time::Duration;

use gsb_core::room::{DisconnectCause, ExpireTo};

use super::*;

async fn kicked_despawns_while_dropped_parks(mut rig: Rig) {
    let kicked = rig.join(1, "0:-50:-50").await;
    let dropped = rig.join(2, "0:-45:-50").await;
    let ally = rig.join(3, "0:-40:-50").await;
    rig.steps(3).await;
    let (k, d) = (rig.clients[kicked].wire, rig.clients[dropped].wire);
    assert!(rig.clients[ally].sees(k) && rig.clients[ally].sees(d));
    let members: u32 = rig.members.iter().sum();

    rig.clients[kicked].ask_kick();
    rig.close(dropped).await;
    rig.steps(3).await;
    assert!(
        !rig.clients[ally].sees(k),
        "the kicked player's entity is gone"
    );
    assert!(
        rig.clients[ally].sees(d),
        "the dropped player's entity is parked"
    );
    assert_eq!(
        rig.members.iter().sum::<u32>(),
        members - 1,
        "only the kicked member's slot came back; the park holds its own"
    );
    // The park is a park: the dropped identity resumes its entity; the
    // kicked one has nothing to resume and joins as a new entity.
    let back = rig.join(4, "0:-45:-50").await;
    assert_eq!(rig.clients[back].wire, d, "the dropped player resumed");
    let again = rig.join(5, "0:-50:-50").await;
    assert_ne!(rig.clients[again].wire, k, "the kicked player starts over");
}

#[tokio::test(start_paused = true)]
async fn a_sharded_team_room_despawns_the_kicked_and_parks_the_dropped() {
    let rig = Rig::new_with(false, |room| {
        room.with_disconnect_policy_for(
            DisconnectCause::Kicked,
            Some(Duration::ZERO),
            ExpireTo::Despawn,
        )
    })
    .await;
    kicked_despawns_while_dropped_parks(rig).await;
}

#[tokio::test(start_paused = true)]
async fn a_single_team_room_despawns_the_kicked_and_parks_the_dropped() {
    let rig = Rig::single_with(false, |room| {
        room.with_disconnect_policy_for(
            DisconnectCause::Kicked,
            Some(Duration::ZERO),
            ExpireTo::Despawn,
        )
    })
    .await;
    kicked_despawns_while_dropped_parks(rig).await;
}
