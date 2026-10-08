//! Determinism at the room boundary (`docs/SERVICES.md` §10, Faz 4 —
//! round q4).
//!
//! Live timing is not deterministic; reproducibility comes from what a
//! room's logic is allowed to read and from recording what enters it:
//!
//! - **time:** [`TickCtx::dt`](crate::room::TickCtx::dt) and
//!   [`TickCtx::time`](crate::room::TickCtx::time) (the sum of the
//!   steps' `dt`) — never a wall clock in logic;
//! - **randomness:** [`TickCtx::rng`](crate::room::TickCtx::rng), the
//!   room's seeded generator ([`RoomRng`]; the seed is
//!   [`RoomConfig::seed`](crate::room::RoomConfig::seed), or a fresh one
//!   per incarnation, and is recorded);
//! - **call keys:** a room's key origin is derived from that seed
//!   (`mix(seed ^ mix(room))`), so a replay issues the same keys;
//! - **state hash:** [`GameLogic::state_hash`](crate::room::GameLogic::state_hash),
//!   folded with the generator's state and the room clock;
//! - **TEST mode:** [`Sim`] (fake services over the [`Route`](crate::calls::Route)
//!   seam, answered when a [`Delay`] says) and [`SimRoom`] (a real room
//!   actor stepped at a fixed `dt`, with scripted clients);
//! - **record / REPLAY** (feature `record`): the room's input log
//!   (`InputLog`: per tick, `dt`, the logic-visible inputs in the order
//!   the room processed them, the state hash) and `Replay`, which runs
//!   a log against the same game without calling any service.

mod hash;
mod rng;
mod sim;

#[cfg(test)]
mod tests;

#[cfg(feature = "record")]
pub(crate) mod file;
#[cfg(feature = "record")]
mod log;

pub use crate::room::sim::{SimClient, SimRoom};
pub use hash::StateHasher;
pub(crate) use hash::fold;
pub use rng::{Rng, RoomRng, fresh_seed};
pub use sim::{Delay, Handler, SIM_ROUTER_CAPACITY, Sim};

#[cfg(feature = "record")]
pub use crate::room::replay::{Replay, ReplayReport, ReplayTick};
#[cfg(feature = "record")]
pub use file::{ReadError, WriterStats, read_log, spawn_writer, write_log};
#[cfg(feature = "record")]
pub use log::{
    Arrival, Codecs, FORMAT_VERSION, Input, InputLog, LogHeader, RecordMsg, Recorded,
    RecordedAction, RecordedEvent, RecordedOutcome, Recorder, TickRecord,
};
#[cfg(feature = "record")]
pub(crate) use log::{Codec, decode_with, encode_with};
