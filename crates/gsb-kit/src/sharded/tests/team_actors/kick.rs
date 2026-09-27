//! The kit's kick verb ([`crate::game::kick`], E8) on the REAL actors:
//! a game kicks a player by its entity, and the team room — four shard
//! actors under a live registry, and the single-world room actor — hands
//! it to the core's verb. The room's disconnect policy decides the
//! entity's fate: the kit's default parks it (the bot takes it over when
//! the grace runs out), so the ally still sees it and its slot stays
//! taken — and the connection's queue is released, so its socket closes.

use super::*;

async fn a_kicked_player_is_parked_and_its_connection_released(mut rig: Rig) {
    let kicked = rig.join(1, "0:-50:-50").await;
    let ally = rig.join(2, "0:-40:-50").await;
    rig.steps(3).await;
    let w = rig.clients[kicked].wire;
    let members: u32 = rig.members.iter().sum();
    assert!(!rig.clients[kicked].out.is_closed(), "fed before the kick");
    rig.clients[kicked].ask_kick();
    rig.steps(3).await;
    let out = &mut rig.clients[kicked].out;
    while out.try_recv().is_ok() {}
    assert!(out.is_closed(), "nothing feeds the kicked connection");
    assert!(rig.clients[ally].sees(w), "the policy parked the entity");
    assert_eq!(
        rig.members.iter().sum::<u32>(),
        members,
        "the park holds its slot"
    );
}

#[tokio::test(start_paused = true)]
async fn a_kicked_player_leaves_the_sharded_team_room_parked() {
    a_kicked_player_is_parked_and_its_connection_released(Rig::new(true).await).await;
}

#[tokio::test(start_paused = true)]
async fn a_kicked_player_leaves_the_single_team_room_parked() {
    a_kicked_player_is_parked_and_its_connection_released(Rig::single(true).await).await;
}
