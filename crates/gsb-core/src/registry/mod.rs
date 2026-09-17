//! The registry actor: the server's control plane.
//!
//! A single channel-driven actor that owns:
//! - the room table: `RoomId → control mailbox`;
//! - the connection table: `ConnectionId → ConnInfo` (kept for the
//!   connection's *whole* lifetime, so the notification path — the inbox —
//!   is never lost mid-session);
//! - the relationship dispatchers: one small task per connection that has
//!   a room relationship in flight, serializing that connection's
//!   join/leave operations (see [`RoomOp`]);
//! - the [`Ticker`] handle (global tick broadcast + rate), which rooms
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
mod msg;
mod table;

pub use actor::Registry;
pub use msg::RegistryMsg;

// Internals shared across this module tree (never leaves the crate).
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
/// is the logic's strip payload ([`ShardLogic::Strip`] via
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
    /// join dispatch and never awaits it). A misrouted join self-heals:
    /// the entity's first boundary crossing migrates it to the right
    /// shard (at most one tick of cross-boundary staleness).
    Sharded {
        shards: Vec<Shard<W, G, St, Sp>>,
        home_shard: Arc<dyn Fn(ConnectionId) -> usize + Send + Sync>,
    },
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
/// plane's result seam — see [`crate::room::RoomLogic::match_result`]):
/// the room id plus the game-encoded payload (opaque to the core; the
/// platform's adapter decodes it).
#[derive(Debug)]
pub struct MatchResult {
    pub room: RoomId,
    pub payload: bytes::Bytes,
}
