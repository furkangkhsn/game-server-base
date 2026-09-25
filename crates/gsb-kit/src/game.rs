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
use gsb_core::shard::{EffectOutcome, RemoteEffect};

use crate::codec::RecordCodec;
// Named in `Game::ingest`'s signature, so a game crate must be able to
// name it (the `common` module itself is private).
pub use crate::common::InputSeq;
use crate::sharded::Seam;
use crate::team::Team;

/// A game the kit's rooms can run. Statically dispatched: every room is
/// generic over it (`OpenRoom<G>`, `AoiRoom<G, S>`), and the only type
/// erasure is the server's room factory (`Box<dyn RoomLogic<…>>`).
///
/// The two frame opcodes have no default: every game picks its own
/// block, so two games cannot share a number by forgetting to (a server
/// hosting several games registers them in one message table). A game
/// that names them compiles:
///
/// ```
/// # use std::collections::HashMap;
/// # use bevy_ecs::prelude::{Changed, Component, Entity, World};
/// # use gsb_core::id::{ConnectionId, PlayerId};
/// # use gsb_core::room::{Action, TickCtx};
/// # use gsb_kit::codec::RecordCodec;
/// # use gsb_kit::game::{Game, InputSeq};
/// # #[derive(Component)]
/// # struct Pos(i32);
/// # struct Codec;
/// # impl RecordCodec for Codec {
/// #     type Marker = Pos;
/// #     type Query = &'static Pos;
/// #     type Dirty = Changed<Pos>;
/// #     type Wire = i32;
/// #     fn wire(&self, pos: &Pos) -> i32 { pos.0 }
/// #     fn encode(&self, _id: u64, _wire: &i32, _out: &mut bytes::BytesMut) {}
/// # }
/// struct MyGame(Codec);
///
/// impl Game for MyGame {
///     type Codec = Codec;
///     const SNAPSHOT_OP: u16 = 1301;
///     const PRIVATE_OP: u16 = 1302;
///     fn codec(&self) -> &Codec { &self.0 }
///     fn spawn_player(&mut self, world: &mut World, _: ConnectionId) -> Entity {
///         world.spawn(Pos(0)).id()
///     }
///     fn ingest(&mut self, _: &mut World, _: &TickCtx, _: &mut Vec<Action>,
///               _: &HashMap<PlayerId, Entity>, _: &mut InputSeq) {}
///     fn systems(&mut self, _: &mut World, _: &TickCtx) {}
/// }
/// ```
///
/// …and the same game without them does not:
///
/// ```compile_fail,E0046
/// # use std::collections::HashMap;
/// # use bevy_ecs::prelude::{Changed, Component, Entity, World};
/// # use gsb_core::id::{ConnectionId, PlayerId};
/// # use gsb_core::room::{Action, TickCtx};
/// # use gsb_kit::codec::RecordCodec;
/// # use gsb_kit::game::{Game, InputSeq};
/// # #[derive(Component)]
/// # struct Pos(i32);
/// # struct Codec;
/// # impl RecordCodec for Codec {
/// #     type Marker = Pos;
/// #     type Query = &'static Pos;
/// #     type Dirty = Changed<Pos>;
/// #     type Wire = i32;
/// #     fn wire(&self, pos: &Pos) -> i32 { pos.0 }
/// #     fn encode(&self, _id: u64, _wire: &i32, _out: &mut bytes::BytesMut) {}
/// # }
/// struct MyGame(Codec);
///
/// impl Game for MyGame {
///     type Codec = Codec;
///     fn codec(&self) -> &Codec { &self.0 }
///     fn spawn_player(&mut self, world: &mut World, _: ConnectionId) -> Entity {
///         world.spawn(Pos(0)).id()
///     }
///     fn ingest(&mut self, _: &mut World, _: &TickCtx, _: &mut Vec<Action>,
///               _: &HashMap<PlayerId, Entity>, _: &mut InputSeq) {}
///     fn systems(&mut self, _: &mut World, _: &TickCtx) {}
/// }
/// ```
pub trait Game: Send + 'static {
    /// How an entity becomes a wire record (§4.1).
    type Codec: RecordCodec;

    /// The opcode of the snapshot frame (`WorldSnapshot`) — the game's
    /// own, in the game band (≥ 1000; the 2D demo: 1003, the arena:
    /// 1101, the MMO: 1201).
    const SNAPSHOT_OP: u16;
    /// The opcode of the per-connection private frame (`Private`) — the
    /// game's own (the 2D demo: 1004, the arena: 1102, the MMO: 1202).
    const PRIVATE_OP: u16;

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

    /// The veto on ending a disconnect hold (RECONNECT §14.4/§17): asked
    /// every tick about the parked player's `entity` once the hold's
    /// grace has run out — a timed hold (`with_disconnect_policy(Some(d),
    /// to)`, a logout timer) from its deadline on, an untimed one
    /// (`None`, combat-held) from the first tick. `false` keeps holding
    /// it (the character is in combat), `true` ends the hold toward the
    /// policy's `to`. The core bounds a standing veto
    /// (`RoomConfig::max_detach_hold`, from the disconnect): past it the
    /// hold ends anyway. Default: `true` — no veto, a timed hold ends at
    /// its deadline, an untimed one at the next tick.
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
/// (§2 — "takım ataması"), decided once per join.
///
/// A strategy-specific extension of [`Game`] rather than a `Game` hook:
/// only the team room calls it, and a game that never runs team fog
/// should not have to answer it.
pub trait TeamGame: Game {
    /// Spawn a joining player's entity AND choose its team — what the
    /// team room calls on every join, in place of
    /// [`Game::spawn_player`]. Override it when the spawn depends on the
    /// team (an arena spawns a unit at its team's base): the decision is
    /// made once, where the spawn point needs it. The default is the
    /// two-step answer of a game whose spawn does not care —
    /// [`Game::spawn_player`], then [`Self::team_of`]. Like
    /// `spawn_player`, the entity must carry the codec's `Marker`; the
    /// kit stamps its wire identity right after and writes the team into
    /// the world as the entity's
    /// [`TeamMember`](crate::team::TeamMember) (later team changes are
    /// plain component writes).
    fn spawn_team_player(&mut self, world: &mut World, conn: ConnectionId) -> (Entity, Team) {
        let entity = self.spawn_player(world, conn);
        let team = self.team_of(world, conn, entity);
        (entity, team)
    }

    /// The team of the player whose entity [`Game::spawn_player`] just
    /// spawned for `conn` — asked by the default
    /// [`Self::spawn_team_player`] right after that spawn, before the kit
    /// stamps the entity's wire identity. A game that overrides
    /// `spawn_team_player` is not asked (it still answers: the team of
    /// `entity`, e.g. read back from its `TeamMember`).
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
    /// despawned there on the next tick by the core's protocol). Once
    /// the move has committed, the game's hooks of that next tick no
    /// longer find the copy — it is lent by its new owner
    /// ([`Seam`], "the migration tick"): what they do to it lands there.
    fn capture(&self, world: &World, entity: Entity) -> Self::Mig;

    /// Rebuild a migrated entity on the receiving shard from its
    /// captured state and return it; like [`Game::spawn_player`], the
    /// spawned entity must carry the codec's `Marker`. The kit stamps
    /// the identity the entity travelled with right after.
    fn restore(&mut self, world: &mut World, mig: Self::Mig) -> Entity;

    // -- Across the seam (CROSS-SHARD §2–§4). Defaults keep a game that
    //    never looks across exactly as it was: the sharded rooms call
    //    these, and they fall back to the plain hooks. ------------------

    /// [`Game::ingest`] on the sharded path, with the [`Seam`]: read the
    /// neighbours' lent records and act on them through remote effects.
    /// The sharded rooms call THIS (after [`Game::bot_actions`], under
    /// the same sequence rule); the default is `ingest`.
    fn ingest_seam(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
        _seam: &mut Seam<'_, '_, Wire<Self>>,
    ) {
        self.ingest(world, ctx, actions, players, seq);
    }

    /// [`Game::systems`] on the sharded path, with the [`Seam`]. The
    /// default is `systems`.
    fn systems_seam(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        _seam: &mut Seam<'_, '_, Wire<Self>>,
    ) {
        self.systems(world, ctx);
    }

    /// Apply a remote effect a neighbour sent to `target`, this shard's
    /// entity (the kit resolved `effect.target` to it; the core already
    /// dropped duplicates, stale epochs and effects past the transport
    /// envelope, and orders one tick's effects by `(source, origin,
    /// seq)`). `tick` is this shard's tick: `tick - effect.at_tick` is
    /// the effect's age, the game's staleness policy. `effect.source`
    /// is the acting entity (credit a kill to it); the seam shows its
    /// lent record when a neighbour lends it (re-validate as policy).
    /// Runs in CONTROL, before the tick's input. Default:
    /// [`EffectOutcome::Rejected`] — a game that emits none has none to
    /// apply.
    fn apply_remote_effect(
        &mut self,
        _world: &mut World,
        _target: Entity,
        _effect: &RemoteEffect,
        _tick: u64,
        _seam: &mut Seam<'_, '_, Wire<Self>>,
    ) -> EffectOutcome {
        EffectOutcome::Rejected
    }
}

/// The wire value of game `G`'s records.
pub type Wire<G> = <<G as Game>::Codec as RecordCodec>::Wire;
