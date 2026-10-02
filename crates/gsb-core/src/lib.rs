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
//!   READ → CONVERT → SYSTEMS → BROADCAST), the shared [`room::GameLogic`]
//!   supertrait (the single-source game contract both actor shapes
//!   implement — see `docs/TRAIT-ARCHITECTURE.md`), and its
//!   [`room::RoomLogic`] extension (the room-exclusive request/result
//!   seams; [`shard::ShardLogic`] is the sharded sibling).
//! - [`conn`]: the per-connection actor (auth/join/leave state machine).
//! - [`auth`]: the ticket-validation hook (control-plane auth; the base
//!   defines the hook, the platform implements the validator).
//! - [`rpc`]: the correlated-request (RPC) pattern — the common
//!   deferred-completion machinery the room and the connection share.
//! - [`source`]: what a per-source limit counts (an IPv4 address, an
//!   IPv6 /64), shared by the doors and the registry.
//! - [`path`]: a connection's path state (the congestion signal), carried
//!   from its transport to the room that plays it (`TickCtx::budget`).
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
pub mod path;
pub mod registry;
pub mod room;
pub mod rpc;
pub mod service;
pub mod shard;
pub mod source;
pub mod ticker;

pub use auth::{TicketAuth, TicketError, TicketValidator, ValidatedTicket};
pub use error::CoreError;
pub use id::{ConnectionId, EntityId, PlayerId, RoomId};
pub use metrics::{
    Exporter, MetricAccumulator, MetricReport, MetricSink, MetricsCollector, MetricsEvent,
};
pub use registry::{MatchResult, RoomStatus};
pub use room::{Admission, Detach, ExpireTo, GameLogic, ResumeFound};
