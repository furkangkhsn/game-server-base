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

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use gsb_core::rpc::{RequestDecision, RpcRequest};

use crate::kit::codec::RecordCodec;
use crate::kit::common::InputSeq;

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

/// The wire value of game `G`'s records.
pub type Wire<G> = <<G as Game>::Codec as RecordCodec>::Wire;
