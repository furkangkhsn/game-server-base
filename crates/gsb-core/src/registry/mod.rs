//! The registry actor: the server's control plane.
//!
//! A single channel-driven actor that owns:
//! - the room table: `RoomId → control mailbox`;
//! - the connection table: `ConnectionId → ConnInfo` (kept for the
//!   connection's *whole* lifetime, so the notification path — the inbox —
//!   is never lost mid-session);
//! - the relationship dispatchers: one small task per connection that has
//!   a room relationship in flight, serializing that connection's
//!   join/leave operations (see `RoomOp`);
//! - the [`Ticker`](crate::ticker::Ticker) handle (global tick broadcast + rate), which rooms
//!   subscribe to at creation;
//! - **sharded rooms** (see [`crate::shard`]): one logical room can be
//!   backed by N shard actors (disjoint spatial regions, each its own
//!   task). The registry spawns the shards for one [`RoomId`], routes
//!   each join to the home shard (the factory's pure `home_shard`
//!   router), enforces the room's membership cap for sharded rooms
//!   (it is the only actor that sees every join — `ShardGroup`), and
//!   broadcasts leaves to all shards (the entity-id guard in exactly one
//!   of them matches). The registry never awaits a shard: its only
//!   interaction with the room side is channel sends, exactly as with a
//!   single room.
//! - the [`RoomFactory`], which is how the (game-specific) room logic gets
//!   into the core without the core knowing any game types.
//! - **supervision** (the death watch): every spawned room/shard task gets
//!   ONE watcher task that awaits only that task's `JoinHandle` and reports
//!   [`RegistryMsg::RoomDied`] through the registry's own mailbox. A panic
//!   in game logic (inside `logic.update()`) kills a room's task silently;
//!   without the watcher the table kept answering `Running { members }`
//!   forever while joins vanished into a dead control channel. The watcher
//!   adds no multiplexing: it is the same "one task per source, awaiting a
//!   single receive" idiom as the signal handler and the relationship
//!   dispatchers below.
//!
//! No locks: every cross-actor value (mailboxes, one-shot replies) is moved
//! through channels. In particular the registry **never awaits a room**:
//! `SpawnPlayer` hands the room round-trip to the connection's dispatcher
//! and returns immediately, so one slow room can never block the control
//! plane (joins elsewhere, room creation, shutdown).
//!
//! Room lifecycle is channel-driven: creating a room is a `subscribe` on
//! the ticker plus a control channel; destroying one removes the table
//! entry and sends a control `Shutdown` (processed on the room's next
//! tick) — there is no cancellation plumbing anywhere, not even in
//! supervision: the death watch only *observes* task exits, it never causes
//! them.

mod actor;
mod close;
mod hub;
mod msg;
mod result;
mod seat;
mod table;

pub use actor::Registry;
pub use close::{CloseRequest, LeaveRequest};
pub(crate) use close::{flush_close_requests, flush_despawn_reports, flush_leave_requests};
pub use msg::RegistryMsg;
pub(crate) use result::send_match_result;
pub use seat::Seat;

// Internals shared across this module tree (never leaves the crate).
pub(crate) use hub::{RelayDrops, TeamHub};
pub(crate) use table::*;

use std::fmt::Debug;
use std::sync::Arc;

use crate::id::{ConnectionId, RoomId};
use crate::room::{RoomConfig, RoomLogic};
use crate::shard::ShardLogic;

/// The retirement set's eviction cap (see the `retired` field): 65 536
/// ended-room ids — far beyond any realistic matchmaker churn for one
/// process lifetime, small enough to be a fixed bound rather than a
/// growing table.
const RETIRED_SET_CAP: usize = 65_536;

/// One shard of a sharded room: its `World` + its [`ShardLogic`]. `Sp`
/// is the logic's strip payload ([`GameLogic::Strip`](crate::room::GameLogic::Strip) via
/// [`crate::room::GameLogic::Strip`]).
pub type Shard<W, G, St, Sp> = (
    W,
    Box<dyn ShardLogic<W, GroupKey = G, State = St, Strip = Sp>>,
);

/// The outcome of a room factory: one room actor (the pre-sharding shape)
/// or a **sharded room** — N shard actors forming one logical room (see
/// [`crate::shard`]).
///
/// `St` is the sharded room's migration state and `Sp` its boundary-strip
/// payload ([`crate::shard::ShardLogic::State`] / the shared
/// `GameLogic::Strip`); both are unused by the [`BuiltRoom::Single`] arm
/// (a single-room-only factory can pick any types, e.g. `()`).
pub enum BuiltRoom<W, G, St, Sp> {
    /// One room actor (today's shape).
    Single {
        world: W,
        logic: Box<dyn RoomLogic<W, GroupKey = G, Strip = Sp>>,
    },
    /// N shard actors (indices `0..N`, the vec order) forming one logical
    /// room. `home_shard` maps a joining connection to the shard that owns
    /// its spawn point — pure and synchronous (the registry calls it at
    /// join dispatch and never awaits it); see [`HomeShard`]. A misrouted
    /// join self-heals: the entity's first boundary crossing migrates it
    /// to the right shard (at most one tick of cross-boundary staleness).
    Sharded {
        shards: Vec<Shard<W, G, St, Sp>>,
        home_shard: HomeShard,
    },
}

/// A sharded room's join router: `(connection, identity) → shard index`.
///
/// `identity` is the joiner's AUTHENTICATED identity — the same string
/// the resume path keys on and the room's
/// [`GameLogic::on_join_as`](crate::room::GameLogic::on_join_as) hook
/// receives: the ticket's validated `player` when a ticket hook is
/// configured, the client-claimed `Auth.name` on the legacy local-auth
/// path (a development path: nothing authoritative stands behind the
/// name), empty for an anonymous session. A game that places a
/// player's saved character routes by it (docs/GAME-MODULE.md, K4); a
/// game whose spawn derives from the transport session ignores it.
///
/// Pure and synchronous: the registry calls it at join dispatch and
/// never awaits it. It is consulted only for a FRESH join: an
/// identified join first asks every shard's park ledger (the resume
/// broadcast), so a parked player resumes wherever it was parked.
///
/// Any `Fn(ConnectionId, &str) -> usize` closure is a router (the
/// identity only, as before B21); a router that also reads the game's
/// verified claims ([`crate::auth::Joiner::claims`] — "this character
/// lives in the north region") is a [`HomeRoute`] of its own, or a
/// closure wrapped by [`route_verified`].
pub type HomeShard = Arc<dyn HomeRoute>;

/// A sharded room's join router (see [`HomeShard`]): the joiner →
/// the index of the shard that owns its spawn point.
pub trait HomeRoute: Send + Sync {
    /// The home shard of `joiner` arriving on `conn`.
    fn route(&self, conn: ConnectionId, joiner: &crate::auth::Joiner<'_>) -> usize;
}

/// The identity-only router: every `(connection, identity)` closure.
impl<F> HomeRoute for F
where
    F: Fn(ConnectionId, &str) -> usize + Send + Sync,
{
    fn route(&self, conn: ConnectionId, joiner: &crate::auth::Joiner<'_>) -> usize {
        self(conn, joiner.identity)
    }
}

/// A router over the whole [`crate::auth::Joiner`] (identity AND the
/// game's verified claims), as a [`HomeShard`]:
/// `home_shard: route_verified(|conn, joiner| …)`.
pub fn route_verified<F>(f: F) -> HomeShard
where
    F: Fn(ConnectionId, &crate::auth::Joiner<'_>) -> usize + Send + Sync + 'static,
{
    Arc::new(Verified(f))
}

/// [`route_verified`]'s wrapper.
struct Verified<F>(F);

impl<F> HomeRoute for Verified<F>
where
    F: Fn(ConnectionId, &crate::auth::Joiner<'_>) -> usize + Send + Sync,
{
    fn route(&self, conn: ConnectionId, joiner: &crate::auth::Joiner<'_>) -> usize {
        (self.0)(conn, joiner)
    }
}

/// Builds a room's world + logic. Provided by the composition root; the core
/// never names the concrete game types. `G` is the game logic's group key
/// (`RoomLogic::GroupKey` / `ShardLogic::GroupKey`); the room stores
/// per-group state under it. `St` is the sharded room's migration state
/// and `Sp` its strip payload (see [`BuiltRoom`]).
pub type RoomFactory<W, G, St, Sp> =
    Arc<dyn Fn(RoomId, &RoomConfig) -> BuiltRoom<W, G, St, Sp> + Send + Sync>;

/// A room's status, as known to the registry's table (the control plane's
/// vocabulary — see [`RegistryMsg::RoomStatus`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomStatus {
    /// The room is running (a `Shutdown` has not been accepted for it).
    /// `members` is the registry-side count of connections affiliated
    /// with the room (the same source the room-cap enforcement reads).
    Running { members: u32 },
    /// The room existed and its shutdown was just accepted by this
    /// message (the room actor stops on its next tick).
    Destroyed,
    /// The room is not known to the registry (never created, or already
    /// destroyed).
    Absent,
}

/// The match result the room reports when it shuts down (the control
/// plane's result seam — see [`crate::room::GameLogic::match_result`]):
/// the room id plus the game-encoded payload (opaque to the core; the
/// platform's adapter decodes it).
#[derive(Debug)]
pub struct MatchResult {
    pub room: RoomId,
    pub payload: bytes::Bytes,
}
