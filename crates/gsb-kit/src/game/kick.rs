//! The kick verb on the kit's surface (BACKLOG E8,
//! `docs/KIT-ARCHITECTURE.md` §4.3): a game kicks the player whose
//! ENTITY it holds — the kit knows which player owns it and hands the
//! kick to the core's verb (`TickCtx::kick`), which ends the membership
//! through the room's disconnect policy and closes the connection with
//! the game's reason (`ERROR` code 9, `kicked: <reason>`).
//!
//! Any hook that holds the world mutably may ask — `ingest`, `systems`,
//! `handle_request`, `may_release`, a shard's `apply_remote_effect` and
//! seam hooks. The kit rooms forward the requests right after the game's
//! systems ran (the end of the room's `update`): everything asked up to
//! then in the tick reaches the core in the same tick, which applies it
//! after SYSTEMS (`docs/RECONNECT.md` §16.3).

use bevy_ecs::prelude::{Entity, Resource, World};
use gsb_core::id::PlayerId;
use gsb_core::room::TickCtx;
use tracing::debug;

/// The kicks the game asked for since the last forward, by entity. A
/// world resource, inserted by the first [`kick`] only: a game that never
/// kicks never has one.
#[derive(Debug, Default, Resource)]
pub(crate) struct KitKicks(Vec<(Entity, String)>);

/// Kick the player whose entity is `entity` from the server, with
/// `reason` (the client reads `kicked: <reason>` in the `ERROR` code 9
/// that precedes the close; the engine keeps at most
/// [`KICK_REASON_MAX_BYTES`](gsb_core::room::KICK_REASON_MAX_BYTES) of
/// it). The room's disconnect policy — the one a dropped transport gets
/// (`with_disconnect_policy`) — then decides the entity's fate: by
/// default the kit PARKS it for
/// [`DEFAULT_DISCONNECT_GRACE`](crate::DEFAULT_DISCONNECT_GRACE) and
/// then hands it to the bot ([`Game::bot_actions`](crate::game::Game::bot_actions)),
/// and the same identity reconnecting within the grace resumes it; a
/// room built with `with_disconnect_policy(Some(Duration::ZERO), _)`
/// despawns it at once, and one built with
/// `with_disconnect_policy_for(DisconnectCause::Kicked, Some(Duration::ZERO), _)`
/// despawns only the kicked while a dropped transport still parks
/// (BACKLOG F27). (An anonymous session — no identity to resume with — is
/// always despawned.)
///
/// Queued, never applied inside the calling hook. An entity no player
/// owns here (an NPC, an entity already gone, a player's entity that
/// crossed to another shard since) is ignored, and so is a player that is
/// no live member when the kick is applied (parked, bot-fed, gone).
pub fn kick(world: &mut World, entity: Entity, reason: impl Into<String>) {
    world
        .get_resource_or_insert_with(KitKicks::default)
        .0
        .push((entity, reason.into()));
}

/// Forward the kicks asked since the last call to the core's verb,
/// resolving each entity to its owning player with `player_of`. Called
/// by every kit room right after the game's systems; a world without
/// kicks costs one resource lookup.
pub(crate) fn forward_kicks(
    world: &mut World,
    ctx: &TickCtx,
    player_of: impl Fn(Entity) -> Option<PlayerId>,
) {
    let Some(mut asked) = world.get_resource_mut::<KitKicks>() else {
        return;
    };
    if asked.0.is_empty() {
        return;
    }
    for (entity, reason) in std::mem::take(&mut asked.0) {
        match player_of(entity) {
            Some(player) => ctx.kick(player, reason),
            None => debug!(?entity, %reason, "kick of an entity no player owns here: ignored"),
        }
    }
}
