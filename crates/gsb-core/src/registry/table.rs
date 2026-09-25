//! What the registry keeps: the room handle (single actor or shard
//! group), the per-connection dispatcher's operation queue, and the
//! two tables that answer every question without awaiting a room.

use tokio::sync::{mpsc, oneshot};

use crate::channel::{FrameBatch, Mailbox};
use crate::conn::ConnIn;
use crate::error::CoreError;
use crate::id::{EntityId, RoomId};
use crate::registry::HomeShard;
use crate::room::{Action, RoomConfig, RoomControl};
use crate::shard::ShardMsg;

/// How a connection's room-relationship ops reach the room side: the
/// single room's control channel, or a sharded room's shard mailboxes.
/// `Clone` because the registry hands a copy to each dispatcher op.
/// `St` is the sharded room's migration state (see [`BuiltRoom`]).
/// The dispatcher's per-connection room state while ops are in flight
/// (see `spawn_conn_ops`): room id, the entity the join created, the
/// room handle, the join's epoch and the resume identity. A named alias
/// because the tuple grew past clippy's complexity eye when the strip
/// payload joined the handle's generics.
pub(crate) type InRoom<St, Sp> = (RoomId, EntityId, RoomHandle<St, Sp>, u64, String);

#[derive(Clone)]
pub(crate) enum RoomHandle<St, Sp> {
    /// A single room: one control mailbox.
    Single(Mailbox<RoomControl>),
    /// A sharded room: one mailbox per shard (indices = shard indices).
    /// `Join` goes to the home shard (the registry picked it via
    /// `home_shard`); `Leave`/`Shutdown` go to ALL of them (exactly one
    /// shard owns the connection — the entity-id guard makes the others
    /// no-ops; see `crate::shard`, "Connection ownership").
    Sharded(Vec<Mailbox<ShardMsg<St, Sp>>>),
}

/// Operations on a connection's room relationship, processed by that
/// connection's dispatcher task — **in order**, which is what makes
/// leave→rejoin race-free: a `Leave` can never overtake (or be overtaken
/// by) the `Join` it follows.
pub(crate) enum RoomOp<St, Sp> {
    /// Join `room`: round-trip the control `Join` (the home shard, when the
    /// room is sharded — `shard` carries the registry's pick), reply to
    /// the connection actor (with the per-connection action channel),
    /// report [`RegistryMsg::SpawnDone`] to the registry.
    ///
    /// A NON-empty `identity` turns this op into the implicit resume
    /// attempt of §14.3: the dispatcher sends `Resume` instead of `Join`
    /// (broadcast to every shard — §6 — for sharded rooms). When nothing
    /// accepts (no ledger holds the identity anywhere), the dispatcher
    /// falls back to the ordinary join below — transparently to the
    /// client (§5).
    Join {
        room: RoomId,
        handle: RoomHandle<St, Sp>,
        /// The home shard index (sharded rooms only).
        shard: Option<usize>,
        /// The room incarnation this handle was taken from (stamped by the
        /// registry; echoed back on SpawnDone/SpawnFailed so a settled join
        /// of a dead incarnation can be recognized — supervision, see
        /// `RegistryMsg::RoomDied`).
        generation: u64,
        /// The join's guard epoch, minted GLOBALLY by the registry (one
        /// monotonically increasing counter across ALL connections). A
        /// per-connection counter resets to 1 on every new session, so a
        /// parked row from the previous session (`session_epoch = k`)
        /// rejected the k-th reconnect's first resume as stale and cost a
        /// wasted round trip per reconnect (measured: one rejection for
        /// EVERY resume beyond an identity's first). Global minting keeps
        /// each connection's epochs a strictly increasing subsequence AND
        /// makes every new session strictly newer than every parked row —
        /// the §7 single-comparison guarantee holds on first attempt.
        epoch: u64,
        out: mpsc::Sender<FrameBatch>,
        /// The resume key; empty = anonymous plain join.
        identity: String,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// Leave `room`: send control `Leave` (with the entity this dispatcher
    /// saw the join create), then report [`RegistryMsg::LeaveDone`].
    Leave { room: RoomId },
    /// Drain the queue (processing whatever is left, including a final
    /// leave), report [`RegistryMsg::OpsClosed`], exit.
    Close,
}

/// The folded outcome of one dispatched join/resume round trip (see
/// [`Self::dispatch_plain_join`] / [`Self::dispatch_resume`]).
pub(crate) enum OpOutcome {
    Joined(EntityId, Mailbox<Action>),
    /// A structural rejection from the room side (full room, stale-resume
    /// epoch guard): propagated to the connection actor as-is.
    Rejected(CoreError),
    /// The room side is unreachable or dropped the reply.
    Gone,
}

/// A sharded room's registry-side state (see `crate::shard`): the shard
/// mailboxes, the home-shard router, and the room-level capacity
/// accounting (the registry is the only actor that sees every join, so
/// it enforces the room cap for sharded rooms — a shard cannot count the
/// room without shared state).
#[derive(Clone)]
pub(crate) struct ShardGroup<St, Sp> {
    /// One mailbox per shard index (the registry's own senders; the
    /// shards' neighbors hold clones of the same channels).
    pub(crate) mailboxes: Vec<Mailbox<ShardMsg<St, Sp>>>,
    /// Maps a joining connection (and its authenticated identity) to its
    /// home shard (pure; see [`HomeShard`]).
    pub(crate) home: HomeShard,
    /// The room's membership cap (`RoomConfig::max_players`, `None` =
    /// unlimited).
    pub(crate) cap: Option<u64>,
    /// Connections accepted (SpawnDone, deduplicated per connection).
    pub(crate) members: u64,
    /// Joins dispatched but not yet settled (SpawnDone/SpawnFailed):
    /// reserved against the cap so a concurrent join burst cannot race
    /// past it.
    pub(crate) pending: u64,
}

pub(crate) struct RoomEntry<St, Sp> {
    /// The single room's control mailbox (single rooms only).
    pub(crate) control: Option<Mailbox<RoomControl>>,
    /// The sharded room's state (sharded rooms only).
    pub(crate) shards: Option<ShardGroup<St, Sp>>,
    /// The config the room was created with (the idempotent-create
    /// comparison: a resent create must match it EXACTLY to be a no-op —
    /// see `RegistryMsg::CreateRoom`).
    pub(crate) config: RoomConfig,
    /// Which incarnation of this room id this entry is (0 = first create,
    /// +1 per rebuild — see [`Registry::room_gen`]). Copied into each
    /// death watcher so a late death report can be attributed to its own
    /// incarnation and a stale one rejected (see
    /// `RegistryMsg::RoomDied`).
    pub(crate) generation: u64,
}

#[derive(Default)]
pub(crate) struct ConnInfo {
    pub(crate) room: Option<RoomId>,
    pub(crate) entity: Option<EntityId>,
    /// Set via [`RegistryMsg::ConnOpened`]; kept for the connection's whole
    /// lifetime so `RoomGone`/`Shutdown` can always reach it.
    pub(crate) inbox: Option<Mailbox<ConnIn>>,
    /// The resume key this connection joined with (see
    /// [`RegistryMsg::SpawnPlayer::identity`]); empty = anonymous.
    /// Recorded at dispatch so the resume re-affiliation can find and
    /// release the detached entry it supersedes.
    pub(crate) identity: String,
    /// Whether this connection completed AUTH (docs/SECURITY.md §4):
    /// `ConnOpened` opens as unauthenticated; the connection actor reports
    /// success via [`RegistryMsg::Authed`]. Only unauthenticated entries
    /// count against `max_unauth_conns`. Detached entries are always
    /// authenticated by construction (an affiliation requires auth), so a
    /// parked/resumed session never consumes unauthenticated capacity —
    /// the flag just stays as the session carried it.
    pub(crate) authed: bool,
    /// The transport died and the entity MAY be parked room-side: the
    /// affiliation is kept (slot held, §4) with this mark, which is set
    /// speculatively — before the room's policy has answered. Released by
    /// exactly three events, which between them cover every way a detach
    /// can end: a resumed or fresh session for the same identity, a room
    /// destroy/death (like any affiliation), and the room's own
    /// [`RegistryMsg::DetachDespawned`] report — the policy declining to
    /// park, or a hold running out toward despawn.
    ///
    /// That third one used to be missing entirely, and its absence was not
    /// the bounded imprecision it was documented as. A detach that reached
    /// despawn without the identity ever returning left this entry
    /// standing forever — the room had despawned the entity, but the row
    /// kept a `max_connections` slot (and, on the grid, a `ShardGroup`
    /// member slot) reserved for a session that no longer existed. In a
    /// persistent room, which never ends, that accumulates one dead
    /// reservation per abandoned session until the caps refuse live
    /// players on behalf of nobody.
    ///
    /// It was then closed for only ONE of the two ways a detach reaches
    /// despawn — the hold-expiry sweep. The `Detach::Despawn` arm, where
    /// the policy declines to park at all, starts no hold and so is never
    /// swept; with the default-off `disconnect_grace_secs = 0` that is
    /// EVERY disconnect. Both arms report now; see
    /// [`RegistryMsg::DetachDespawned`].
    ///
    /// The AI-handover arm is deliberately NOT reported: that hold ends
    /// with the entity alive under a bot, genuinely holding its slot and
    /// still a valid resume target (`docs/RECONNECT.md` §9).
    pub(crate) detached: bool,
}
