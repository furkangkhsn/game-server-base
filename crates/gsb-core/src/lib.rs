//! Core actor machinery for gsb game servers.
//!
//! This crate contains **no** game logic and **no** concrete ECS type. It
//! provides the channel-driven actor skeleton:
//!
//! - [`id`]: identifier newtypes.
//! - [`channel`]: bounded channel aliases.
//! - [`ticker`]: the global tick service (one broadcast channel, one task).
//! - [`registry`]: the singleton control-plane actor (room table + conn
//!   routing), including the control-plane room lifecycle (the idempotent
//!   create/destroy/status) and the match-result seam.
//! - [`room`]: the per-room actor running the five-phase tick (CONTROL →
//!   READ → CONVERT → SYSTEMS → BROADCAST), and the generic
//!   [`room::RoomLogic`] trait the game crate implements.
//! - [`conn`]: the per-connection actor (auth/join/leave state machine).
//! - [`auth`]: the ticket-validation hook (control-plane auth; the base
//!   defines the hook, the platform implements the validator).
//! - [`rpc`]: the correlated-request (RPC) pattern — the common
//!   deferred-completion machinery the room and the connection share.
//!
//! Design invariants (enforced by `gsb-lint` in every crate):
//! - actors only ever `await` a single channel receive — no
//!   `tokio::select!` multiplexing in the hot path;
//! - there are no locks; all shared state is owned by exactly one actor and
//!   exchanged as channel senders.

pub mod auth;
pub mod channel;
pub mod conn;
pub mod error;
pub mod id;
pub mod metrics;
pub mod registry;
pub mod room;
pub mod rpc;
pub mod shard;
pub mod ticker;

pub use auth::{TicketAuth, TicketError, TicketValidator, ValidatedTicket};
pub use error::CoreError;
pub use id::{ConnectionId, EntityId, RoomId};
pub use metrics::{
    MetricAccumulator, MetricReport, MetricSink, MetricsCollector, MetricsEvent,
};
pub use registry::{MatchResult, RoomStatus};
pub use room::{Detach, ExpireTo, ResumeFound};
