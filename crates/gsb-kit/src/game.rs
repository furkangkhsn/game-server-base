//! [`Game`] — the gameplay hooks a kit room calls (KIT-ARCHITECTURE
//! §4.3). A strategy room is a complete `GameLogic`: the visibility
//! decision and the bookkeeping are the kit's, and everything that is
//! *the game* — spawning, input decoding, the bot's input, the systems,
//! the request handlers, the record's bytes — comes through this trait.
//!
//! What the kit keeps around the hooks (§4.4): the wire identity (the kit
//! stamps it on the entity [`Game::spawn_player`] returns), the input
//! sequence/ack rule ([`InputSeq`]), the park/resume ledger (it hands the
//! bot-fed players to [`Game::bot_actions`]), and the change-detection
//! window (`World::clear_trackers` is called by the kit exactly once per
//! tick — a hook must never call it).

use std::collections::HashMap;
use std::fmt::Debug;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use gsb_core::rpc::{RequestDecision, RpcRequest};

use crate::codec::RecordCodec;
// Named in `Game::ingest`'s signature, so a game crate must be able to
// name it (the `common` module itself is private).
pub use crate::common::InputSeq;
use crate::team::Team;

/// A game the kit's rooms can run. Statically dispatched: every room is
/// generic over it (`OpenRoom<G>`, `AoiRoom<G, S>`), and the only type
/// erasure is the server's room factory (`Box<dyn RoomLogic<…>>`).
pub trait Game: Send + 'static {
    /// How an entity becomes a wire record (§4.1).
    type Codec: RecordCodec;

    /// The opcode of the snapshot frame (`WorldSnapshot`).
    const SNAPSHOT_OP: u16 = 1003;
    /// The opcode of the per-connection private frame (`Private`).
    const PRIVATE_OP: u16 = 1004;

    /// The game's record codec.
    fn codec(&self) -> &Self::Codec;

    /// Spawn a joining player's entity (spawn point + the player's
    /// components, which must include the codec's `Marker`). The kit
    /// stamps the wire identity on it right after; `conn` is the
    /// transport session the join arrived on.
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity;

    /// Synthesize the input of the bot-fed players (a parked player's
    /// grace ran out toward AI handover — RECONNECT §9): push frames into
    /// `out`, the tick's action list, which [`Game::ingest`] then decodes
    /// like any wire input. `bots` yields `(stable player, entity)`.
    fn bot_actions(
        &mut self,
        _world: &World,
        _ctx: &TickCtx,
        _bots: impl Iterator<Item = (PlayerId, Entity)>,
        _out: &mut Vec<Action>,
    ) {
    }

    /// Decode and apply the tick's input. `players` resolves an action's
    /// stable player to its entity; `seq` is the kit's sequence rule —
    /// the game must process a numbered input only when
    /// [`InputSeq::admit`] says so.
    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    );

    /// Run the game's systems for this tick.
    fn systems(&mut self, world: &mut World, ctx: &TickCtx);

    /// The veto on ending a disconnect hold that has NO deadline — a
    /// room built with `with_disconnect_policy(None, to)` (RECONNECT
    /// §14.4, combat-held): asked every tick while the parked player's
    /// `entity` is held that way. `false` keeps holding it (an enemy is
    /// near, the fight is on), `true` ends the hold toward the policy's
    /// `to`. Never asked for a timed hold: its grace is the ceiling that
    /// makes an endless veto impossible. Default: `true` — no veto, an
    /// untimed hold ends at the next tick.
    fn may_release(&mut self, _world: &mut World, _entity: Entity) -> bool {
        true
    }

    /// Answer an RPC request (`None` = not a request this game handles;
    /// the core answers "no handler"). `players` resolves the requester.
    fn handle_request(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        _req: &RpcRequest,
        _players: &HashMap<PlayerId, Entity>,
    ) -> Option<RequestDecision> {
        None
    }
}

/// A game the team-fog room can run: team assignment is game policy
/// (§2 — "takım ataması"), asked once per join.
///
/// A strategy-specific extension of [`Game`] rather than a `Game` hook:
/// only the team room calls it, and a game that never runs team fog
/// should not have to answer it.
pub trait TeamGame: Game {
    /// The team of the player whose entity [`Game::spawn_player`] just
    /// spawned for `conn` (the kit has stamped its wire identity). The
    /// room writes the answer into the world as the entity's
    /// [`TeamMember`](crate::team::TeamMember); later team changes
    /// are plain component writes.
    fn team_of(&mut self, world: &World, conn: ConnectionId, entity: Entity) -> Team;
}

/// A game the sharded rooms can run: what a migrating entity carries
/// from one shard's world into its neighbour's (§4.3's `Mig`, `capture`,
/// `restore`).
///
/// A strategy-specific extension of [`Game`] rather than part of it:
/// only the sharded composites call these, and stable Rust has no
/// associated-type defaults — inside `Game` every game that never shards
/// would have to name a `Mig` and write two hooks nothing calls.
///
/// What the KIT carries around the game's state (in `KitMig`): the wire
/// identity (the core's `Migrating::wire`), the owning player, and the
/// park record of a detached player (RECONNECT §14.2). Which entities
/// migrate is the kit's rule too: every entity carrying the codec's
/// `Marker` and the partition's position, whatever else it has
/// (§8.5).
pub trait ShardGame: Game {
    /// The game state a migrating entity carries (the demo: position,
    /// speed if any, pending move target).
    type Mig: Debug + Send + 'static;

    /// Capture `entity`'s game state on the sending shard (it is
    /// despawned there on the next tick by the core's protocol).
    fn capture(&self, world: &World, entity: Entity) -> Self::Mig;

    /// Rebuild a migrated entity on the receiving shard from its
    /// captured state and return it; like [`Game::spawn_player`], the
    /// spawned entity must carry the codec's `Marker`. The kit stamps
    /// the identity the entity travelled with right after.
    fn restore(&mut self, world: &mut World, mig: Self::Mig) -> Entity;
}

/// The wire value of game `G`'s records.
pub type Wire<G> = <<G as Game>::Codec as RecordCodec>::Wire;
