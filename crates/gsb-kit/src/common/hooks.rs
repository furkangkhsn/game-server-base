//! The shared room accounting of the rooms that are generic over the
//! game (KIT-ARCHITECTURE §4.3/§4.4), split between kit machinery and
//! [`Game`] hooks: the kit mints the identities, keeps the tables, feeds
//! the bot-fed players and owns the change window; the game spawns,
//! decodes and simulates.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, Admission, TickCtx};

use crate::codec::RecordCodec;
use crate::common::{InputSeq, ParkEntry};
use crate::game::Game;
use crate::identity::Minter;

/// The join path: a fresh stable player identity (the room's
/// `PlayerId` counter — resume stability comes from the park ledger
/// carrying it, not from re-minting), a fresh input session (a (re)join
/// is a new session — see [`InputSeq`]), the GAME's spawn
/// ([`Game::spawn_player`]), then the KIT's identity stamp from the
/// room's single [`Minter`] and the player→entity table. The wire id
/// also goes to the joiner in `JOIN_ROOM_RESULT`, so both paths share
/// one space.
pub(crate) fn join<G: Game>(
    game: &mut G,
    players: &mut HashMap<PlayerId, Entity>,
    next_player_id: &mut u64,
    minter: &mut Minter,
    world: &mut World,
    conn: ConnectionId,
    input: &mut InputSeq,
) -> Admission {
    *next_player_id += 1;
    let player = PlayerId(*next_player_id);
    input.begin(player);
    let entity = game.spawn_player(world, conn);
    debug_assert!(
        world
            .entity(entity)
            .contains::<<G::Codec as RecordCodec>::Marker>(),
        "Game::spawn_player must spawn the codec's Marker (the broadcast set)"
    );
    let wire = minter.mint();
    world.entity_mut(entity).insert(wire);
    players.insert(player, entity);
    Admission {
        player,
        entity: wire.get(),
    }
}

/// The input path: the bot-fed parked players' synthesized frames
/// ([`Game::bot_actions`] — RECONNECT §9: the bot is an input source
/// without a connection) join the SAME action list as the wire input,
/// then the game decodes and applies all of it under the kit's sequence
/// rule ([`Game::ingest`]).
pub(crate) fn ingest<G: Game>(
    game: &mut G,
    world: &mut World,
    ctx: &TickCtx,
    actions: &mut Vec<Action>,
    players: &HashMap<PlayerId, Entity>,
    park_ledger: &HashMap<String, ParkEntry>,
    input: &mut InputSeq,
) {
    let bots = park_ledger
        .values()
        .filter(|e| e.bot)
        .map(|e| (e.player, e.entity));
    game.bot_actions(world, ctx, bots, actions);
    guard_change_window(world, |w| game.ingest(w, ctx, actions, players, input));
}

/// Run the game's systems for this tick (single-threaded, ordered — the
/// room actor is the only owner of the world).
pub(crate) fn systems<G: Game>(game: &mut G, world: &mut World, ctx: &TickCtx) {
    guard_change_window(world, |w| game.systems(w, ctx));
}

/// Close this tick's change-detection window — the ONE
/// `World::clear_trackers` call of the tick (§4.4: the call is
/// world-wide, so it cannot have two owners). The codec's `Dirty` filter
/// sees exactly the writes since the previous call; the removed-component
/// buffers are swapped (they grow without bound when nobody calls it).
/// Every generic room calls this once, at the end of `update`.
pub(crate) fn close_change_window(world: &mut World) {
    world.clear_trackers();
}

/// Run a game hook and check (debug builds) that it left the change
/// window alone: a hook calling `World::clear_trackers` would silently
/// hide its own writes from the codec's `Dirty` filter.
fn guard_change_window(world: &mut World, hook: impl FnOnce(&mut World)) {
    let before = world.last_change_tick();
    hook(world);
    debug_assert_eq!(
        world.last_change_tick(),
        before,
        "a Game hook called World::clear_trackers — the kit owns the change window"
    );
}
