//! Core actor machinery for gsb game servers.
//!
//! This crate contains **no** game logic and **no** concrete ECS type. It
//! provides the channel-driven actor skeleton:
//!
//! - [`id`]: identifier newtypes.
//! - [`channel`]: bounded channel aliases.
//! - [`ticker`]: the global tick service (one broadcast channel, one task).
//! - [`registry`]: the singleton control-plane actor (room table + conn
//!   routing).
//! - [`room`]: the per-room actor running the five-phase tick (CONTROL →
//!   READ → CONVERT → SYSTEMS → BROADCAST), and the generic
//!   [`room::RoomLogic`] trait the game crate implements.
//! - [`conn`]: the per-connection actor (auth/join/leave state machine).
//!
//! Design invariants (enforced by `gsb-lint` in every crate):
//! - actors only ever `await` a single channel receive — no
//!   `tokio::select!` multiplexing in the hot path;
//! - there are no locks; all shared state is owned by exactly one actor and
//!   exchanged as channel senders.

pub mod channel;
pub mod conn;
pub mod error;
pub mod id;
pub mod registry;
pub mod room;
pub mod ticker;

pub use error::CoreError;
pub use id::{ConnectionId, EntityId, RoomId};
