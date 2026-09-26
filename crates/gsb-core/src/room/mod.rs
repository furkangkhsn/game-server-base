//! The room actor: one per room/map, owning its world and its tick loop.
//!
//! A room runs the five-phase tick driven by the **global ticker** (a
//! `broadcast` channel, see [`crate::ticker`]):
//!
//! ```text
//! global ticker ──TickInfo (broadcast)──▶ room actor   (the only await)
//!                                            │
//!  Phase 0  │  CONTROL:   pull join/leave/shutdown from the control channel
//!  Phase 1  │  READ:      pull actions from each connection's channel
//!  Phase 2  │  CONVERT:   actions → component writes (GameLogic)
//!  Phase 3  │  SYSTEMS:   run the ordered game systems   (GameLogic)
//!  Phase 4  │  BROADCAST: one snapshot per group (GameLogic::snapshot)
//!           │             → freeze → shared Bytes fan-out + private frames
//!             └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Everything is pulled with `try_recv` — the tick body is fully
//! synchronous. Per-connection action channels isolate users: one flooding
//! connection can only fill its own channel, never delay a tick or another
//! connection.
//!
//! **Broadcast model (per-group full snapshots).** Connections are
//! partitioned into snapshot groups by the game logic
//! ([`GameLogic::group_of`]): `()` means "one group per room" (the demo),
//! `ConnectionId` means "one snapshot per connection". Each tick the room
//! encodes each group's **entire** snapshot **once**, `freeze()`s it, and
//! fans the resulting `Bytes` out to the group's members by reference
//! (Arc refcount — the payload is never copied). Membership (join/leave)
//! is expressed by presence in the snapshot: there are no spawn/remove
//! events. The game logic decides "nothing changed for this group"
//! (`GameLogic::snapshot` returning `false`, including membership
//! changes); when nothing changed anywhere, the room ships nothing except
//! on a keep-alive tick, when each group re-sends its last cached snapshot
//! so a client that lost its last packet cannot stay stale forever
//! (`RoomConfig::keepalive_hz`). A dropped batch (slow client) costs at
//! most one snapshot of staleness: every snapshot is self-contained
//! (no delta, no history), so the next one heals the gap.
//!
//! Time handling: each room tracks the last tick it stepped at. The step
//! `dt` is the wall-clock difference, so ticks missed while busy are
//! absorbed into a single catch-up step and the simulation stays
//! frame-rate independent (the same real-time displacement at 15 Hz or
//! 100 Hz). `dt` is capped at `RoomConfig::max_catchup` periods so a
//! pathological stall produces temporary slow-motion instead of a giant
//! step. A room running slower than the global ticker simply steps on
//! every k-th global tick.
//!
//! The room actor owns **no** game types: the world is an opaque `W` and
//! the group key an opaque `G`; all game behaviour is delegated to the
//! logic traits — [`GameLogic`] (the shared supertrait, one source for
//! what used to be a ~17-method duplicate on both actors) narrowed here
//! by [`RoomLogic`] with the room-exclusive request/result seams; the
//! sharded sibling is [`ShardLogic`](crate::shard::ShardLogic) — see
//! `docs/TRAIT-ARCHITECTURE.md`.
//!
//! **Metrics:** the room's counters live in the room's own local state
//! (`RoomCounters`) and are flushed once per step over the *bounded*
//! metrics channel with a synchronous `try_send` (a full channel drops
//! the sample and counts it — harmless, the counters are cumulative);
//! the room's only `await` stays `tick_rx.recv()` and the tick body
//! stays fully synchronous (see [`crate::metrics`] for the design and
//! the constraint rationale).

use std::fmt::Debug;

use crate::id::{ConnectionId, EntityId, PlayerId};

mod actor;
mod config;
mod control;
mod counters;
mod idle;
mod logic;

#[cfg(test)]
mod tests;

pub use actor::RoomActor;
pub use config::{DEFAULT_MAX_DETACH_HOLD, InputRate, RoomConfig};
pub use control::{Detach, ExpireTo, ResumeFound, RoomControl, TickCtx};
pub(crate) use counters::{GroupState, HoldEnd, RoomConn, RoomCounters};
pub(crate) use idle::IdleClock;
pub use idle::IdleView;
pub use logic::{GameLogic, RoomLogic};

/// A client action forwarded by the connection actor. The payload is still
/// encoded; the game crate decodes it against its own message types.
///
/// Two routing keys, two jobs (Faz 2, `docs/TRAIT-ARCHITECTURE.md` §5):
/// `conn` is the transport-session key (the wire contract — unchanged);
/// `player` is the stable player identity the ROOM resolves for ingest.
/// A connection actor cannot know `player` (the identity is minted inside
/// the game logic), so it fills the placeholder `PlayerId(0)`; the room
/// stamps the authoritative value from its binding table at READ→CONVERT
/// (see the phase comment). An action whose `conn` is not bound drops
/// there — an old session's stray frame after a resume has no binding
/// row left and can never reach the world.
#[derive(Debug)]
pub struct Action {
    pub conn: ConnectionId,
    /// The room-resolved player this action belongs to (placeholder `0`
    /// at construction; stamped by the room/shard before ingest).
    pub player: PlayerId,
    pub op: u16,
    pub payload: bytes::Bytes,
}

/// What a successful join hands back to the room actor (Faz 2): the
/// player identity the LOGIC minted plus the entity (`EntityId`) it
/// created/restored — the same value `on_join` always returned and the
/// reply carries. The core needs both: `player` keys every internal
/// table, `entity` stays the stale-leave/detach guard and wire id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    pub player: PlayerId,
    pub entity: EntityId,
}
