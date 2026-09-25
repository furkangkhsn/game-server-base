//! What a shard can be told: the control messages it shares with the
//! room actor, plus the two payloads that only exist on the grid — a
//! migrating entity and the player riding with it.
use std::fmt::Debug;

use tokio::sync::{mpsc, oneshot};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, PlayerId};
use crate::room::{Action, ExpireTo};
use crate::shard::*;

/// A player's channel halves, moved with a migrating player entity
/// (ownership transfer — see module docs, "Migration protocol").
#[derive(Debug)]
pub struct PlayerMigration {
    /// The stable player identity (Faz 2): the receiving shard keys the
    /// row under it — unchanged by the move, exactly like `entity`.
    pub player: PlayerId,
    /// The transport session bound to this player at send time: the
    /// receiving shard installs ITS binding row (`conn → player`), so
    /// control broadcasts (Leave/Detach) still find the owner after the
    /// move. Session-keyed on purpose — it changes on resume, the
    /// player key does not.
    pub conn: ConnectionId,
    /// The join's epoch (the leave/migration race gate).
    pub epoch: u64,
    /// The entity this connection owns in the sending shard (the
    /// receiving shard re-registers it under the same id).
    pub entity: EntityId,
    /// The connection's outbound channel (fan-out).
    pub out: mpsc::Sender<FrameBatch>,
    /// The connection's action inbox (input).
    pub actions: Inbox<Action>,
    // -- Detach state that travels with the row (a PARKED player's
    //    entity migrates exactly like a live one — passive systems keep
    //    running on it, §3.2 — and the receiving shard must re-attach the
    //    same flags or it would start broadcasting into the dead outbound
    //    half and polluting its drop counter). --------------------------
    /// See [`crate::room::RoomConn::detached`].
    pub detached: bool,
    /// See [`crate::room::RoomConn::detach_deadline`].
    pub detach_deadline: Option<std::time::Instant>,
    /// See [`crate::room::RoomConn::detach_ceiling`] — measured from the
    /// DETACH, so a crossing must not restart it (a parked entity walked
    /// back and forth across a seam would otherwise never reach it).
    pub detach_ceiling: Option<std::time::Instant>,
    /// See [`crate::room::RoomConn::expire_to`].
    pub expire_to: ExpireTo,
    /// See [`crate::room::RoomConn::bot_fed`].
    pub bot_fed: bool,
    /// See [`crate::room::RoomConn::session_epoch`] (the resume guard's
    /// stamp survives migrations).
    pub session_epoch: u64,
    /// See [`crate::room::RoomConn::identity`] — the resume key travels
    /// with the row, or a migrated member could not be parked under it.
    pub identity: String,
    /// The input-idle clock's stamp for this player (see
    /// [`crate::room::IdleView`]): the clock is per-ACTOR, so a crossing
    /// has to carry it or a player would reset its idleness every time it
    /// changes shard. `None` = the player is off the clock (parked or
    /// bot-fed), which the receiving shard reproduces by not starting one.
    pub last_input: Option<std::time::Instant>,
}

/// A migrating entity: the full game state plus the owning player, when
/// the entity is a player (NPCs have no player).
pub struct Migrating<S> {
    /// The entity's wire identity (kept across the migration).
    pub wire: u64,
    /// The full component state (game-shaped).
    pub state: S,
    pub player: Option<PlayerId>,
}

/// The per-shard answer to a broadcast resume (`ShardMsg::Resume`):
/// `Ok(Some(..))` = this shard held the identity and rebound it;
/// `Ok(None)` = not here; `Err` = the epoch guard tripped.
pub type ResumeReply = Result<Option<(EntityId, Mailbox<Action>)>, CoreError>;

/// Messages between shards and from the registry to a shard. One bounded
/// channel per shard: control (join/leave/shutdown) and the shard
/// protocol (migrate/border) share it — all are drained with `try_recv`
/// at the tick boundary, and the exchange traffic is small (a few
/// messages per neighbor per tick). `S` is the migration state and `B`
/// the boundary-strip payload (both game-owned; see [`ShardLogic`]).
#[derive(Debug)]
pub enum ShardMsg<S, B> {
    /// A player joins this shard (the registry routed it here through
    /// `home_shard`; `epoch` is the join's epoch — see module docs).
    Join {
        conn: ConnectionId,
        epoch: u64,
        /// The resume key this session authenticated under (empty =
        /// anonymous). Carried so the shard's member row can remember it,
        /// exactly like the single room's: an identified player reaches a
        /// SHARDED room through this arm whenever no ledger held it (the
        /// broadcast-resume's transparent fallback, §5), and the
        /// input-idle ceiling has to be able to hand `on_disconnect` a
        /// real resume key. The logic's join hook receives it too
        /// (`GameLogic::on_join_as`, K4).
        identity: String,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// A player left (broadcast to ALL shards of the room — exactly one of
    /// them owns the connection; the entity-id guard makes the others
    /// no-ops). `epoch` is the epoch of the join being left.
    Leave {
        conn: ConnectionId,
        entity: EntityId,
        epoch: u64,
    },
    /// The connection's transport died (the registry's `ConnClosed`
    /// broadcast, mirroring `RoomControl::Detach`): exactly the owning
    /// shard runs the policy ([`GameLogic::on_disconnect`]); the others
    /// no-op on the same entity-id guard a `Leave` uses.
    Detach {
        conn: ConnectionId,
        entity: EntityId,
        identity: String,
    },
    /// An identified join whose park ledger may hold this identity — the
    /// implicit resume attempt of §14.3, BROADCAST to every shard (§6):
    /// only the shard whose ledger holds it accepts (`Ok(Some(..))`);
    /// all others answer `Ok(None)` ("not here"); a tripped epoch guard
    /// answers `Err(CoreError::ResumeStale)`. The single-winner property
    /// is structural (the record exists on exactly one shard — the
    /// migration protocol's exactly-once invariant), locked by test in
    /// the reconnect suite.
    Resume {
        conn: ConnectionId,
        /// The new session's dispatcher-minted join epoch (the §7 guard:
        /// a stale duplicate is rejected by one integer comparison).
        epoch: u64,
        identity: String,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<ResumeReply>,
    },
    /// An entity (with, for player entities, its connection) moves into
    /// this shard's region: installed in phase 0, before this tick's step.
    Migrate {
        from: usize,
        /// The tick index at which the sender sampled the crossing.
        at_tick: u64,
        wire: u64,
        state: S,
        /// Boxed: `PlayerMigration` carries the whole core-side row (two
        /// channel halves, the detach flags, the resume key, the
        /// input-idle stamp) and is by far the biggest thing a
        /// `ShardMsg` can hold — inlining it would make EVERY shard
        /// message, and every `Result` a link send returns, that large.
        player: Option<Box<PlayerMigration>>,
    },
    /// A neighbor's boundary update (§6.4): a Full replaces this shard's
    /// view of that neighbor wholesale; a Delta applies its upserts/exits
    /// when its sequence number matches the expected one exactly.
    Border {
        from: usize,
        exchange: BorderExchange<B>,
    },
    /// A neighbor rejected our delta stream (sequence gap or a view it
    /// had marked stale): serve that neighbor a FULL on the next phase 5.
    /// Deliberately a tiny standalone message on the same bounded mailbox
    /// instead of a shared resync flag: it rides the existing FIFO, so no
    /// new channel, no await, and the ordering against in-flight deltas
    /// is the natural one (the Full is generated after everything already
    /// queued was sent).
    ResyncRequest { from: usize },
    /// An effect on an entity this shard owns — or owned until a recent
    /// migration, in which case it is forwarded to the new owner
    /// (`docs/CROSS-SHARD.md` §2; the delivery class EFFECT of
    /// `docs/DISTRIBUTED.md` §4). Applied in CONTROL, at the tick after
    /// the one it was emitted in, in `(source, origin, seq)` order;
    /// idempotent on its [`EffectId`].
    RemoteEffect(RemoteEffect),
    /// Stop the shard (drops the world).
    Shutdown,
}
