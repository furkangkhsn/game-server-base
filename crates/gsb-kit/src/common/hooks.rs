//! The shared room accounting of the rooms that are generic over the
//! game (KIT-ARCHITECTURE §4.3/§4.4), split between kit machinery and
//! [`Game`] hooks: the kit mints the identities, keeps the tables, feeds
//! the bot-fed players and owns the change window; the game spawns,
//! decodes and simulates.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::auth::Joiner;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, Admission, TickCtx};

use crate::codec::RecordCodec;
use crate::common::{InputSeq, ParkEntry};
use crate::game::Game;
use crate::identity::Minter;

/// The join path: a fresh stable player identity (the room's
/// `PlayerId` counter — resume stability comes from the park ledger
/// carrying it, not from re-minting), a fresh input session (a (re)join
/// is a new session — see [`InputSeq`]), the GAME's spawn of the player
/// who joins as `joiner` ([`Game::spawn_player_verified`]: the
/// authenticated identity and the verified claims), then the
/// KIT's identity stamp from the room's single [`Minter`] and the
/// player→entity table. The wire id also goes to the joiner in
/// `JOIN_ROOM_RESULT`, so both paths share one space.
#[allow(clippy::too_many_arguments)] // the tables, the joiner, and its identity
pub(crate) fn join<G: Game>(
    game: &mut G,
    players: &mut HashMap<PlayerId, Entity>,
    next_player_id: &mut u64,
    minter: &mut Minter,
    world: &mut World,
    conn: ConnectionId,
    joiner: &Joiner<'_>,
    input: &mut InputSeq,
) -> Admission {
    join_with(
        game,
        players,
        next_player_id,
        minter,
        world,
        conn,
        input,
        |g, w, c| g.spawn_player_verified(w, c, joiner),
    )
}

/// [`join`] with the spawn step given: `spawn` stands in for
/// [`Game::spawn_player`] (the team room's
/// [`TeamGame::spawn_team_player`](crate::game::TeamGame::spawn_team_player),
/// which decides the team in the same step).
#[allow(clippy::too_many_arguments)] // `join`'s seven plus the spawn step
pub(crate) fn join_with<G: Game>(
    game: &mut G,
    players: &mut HashMap<PlayerId, Entity>,
    next_player_id: &mut u64,
    minter: &mut Minter,
    world: &mut World,
    conn: ConnectionId,
    input: &mut InputSeq,
    spawn: impl FnOnce(&mut G, &mut World, ConnectionId) -> Entity,
) -> Admission {
    *next_player_id += 1;
    let player = PlayerId(*next_player_id);
    input.begin(player);
    let entity = spawn(game, world, conn);
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

/// [`ingest`] on the sharded path: the same bot input and sequence rule,
/// with the game's seam hook
/// ([`ShardGame::ingest_seam`](crate::game::ShardGame::ingest_seam)).
/// No bot drives an entity in `handed_on` (the copies of the entities
/// the shard handed on last tick: their new owner's bot drives them).
#[allow(clippy::too_many_arguments)] // `ingest`'s seven plus the seam and the copies
pub(crate) fn ingest_seam<G: crate::game::ShardGame>(
    game: &mut G,
    world: &mut World,
    ctx: &TickCtx,
    actions: &mut Vec<Action>,
    players: &HashMap<PlayerId, Entity>,
    park_ledger: &HashMap<String, ParkEntry>,
    handed_on: &[Entity],
    input: &mut InputSeq,
    seam: &mut crate::sharded::Seam<'_, '_, crate::game::Wire<G>>,
) {
    let bots = park_ledger
        .values()
        .filter(|e| e.bot && !handed_on.contains(&e.entity))
        .map(|e| (e.player, e.entity));
    game.bot_actions(world, ctx, bots, actions);
    guard_change_window(world, |w| {
        game.ingest_seam(w, ctx, actions, players, input, seam)
    });
}

/// Run the game's systems for this tick (single-threaded, ordered — the
/// room actor is the only owner of the world), then forward the kicks
/// the game asked for this tick ([`crate::game::kick`], E8), resolved
/// against the room's player→entity table.
pub(crate) fn systems<G: Game>(
    game: &mut G,
    world: &mut World,
    ctx: &TickCtx,
    players: &HashMap<PlayerId, Entity>,
) {
    guard_change_window(world, |w| game.systems(w, ctx));
    crate::game::forward_kicks(world, ctx, |entity| {
        players.iter().find(|(_, e)| **e == entity).map(|(p, _)| *p)
    });
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
pub(crate) fn guard_change_window<R>(world: &mut World, hook: impl FnOnce(&mut World) -> R) -> R {
    let before = world.last_change_tick();
    let answer = hook(world);
    debug_assert_eq!(
        world.last_change_tick(),
        before,
        "a Game hook called World::clear_trackers — the kit owns the change window"
    );
    answer
}
