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

// The `#[service]` macro's generated code names this crate `::gsb_core`;
// this makes the path resolve inside the crate too (its own tests).
extern crate self as gsb_core;

/// `#[gsb_core::service]`: turn a plain async trait into an in-process
/// service (`docs/SERVICES.md` §4.1; see [`calls`] for the host).
pub use gsb_macros::service;

#[doc(hidden)]
pub mod __private {
    //! What `#[service]`'s generated code needs besides [`crate::calls`].
    pub use crate::calls::{BoxError, BoxFuture};
    pub use tower;

    pub mod record {
        //! `ServiceKind::record_codecs`'s probe (BACKLOG B173): the macro
        //! knows a method's `Response`/`Error` types but not whether they
        //! have a wire form, so it asks per call type with autoref
        //! dispatch — `(&&Probe::<C>::new()).register(calls)` resolves to
        //! [`ViaWire`] when both are [`WireMessage`](crate::calls::WireMessage)s
        //! (the codec is registered) and falls back to [`ViaNone`] (a
        //! no-op) otherwise. Works on the concrete types the macro writes;
        //! not for generic code.

        use std::marker::PhantomData;

        use crate::calls::{Call, Calls, WireMessage};

        /// A call type, probed.
        pub struct Probe<C>(PhantomData<fn() -> C>);

        impl<C> Probe<C> {
            /// The probe of `C`.
            #[allow(clippy::new_without_default)]
            pub const fn new() -> Self {
                Self(PhantomData)
            }
        }

        /// `C`'s outcomes have a wire form: register its codec.
        pub trait ViaWire {
            /// Register (a no-op without the `record` feature).
            fn register(self, calls: &mut Calls);
        }

        impl<C> ViaWire for &&Probe<C>
        where
            C: Call<Response: WireMessage, Error: WireMessage>,
        {
            fn register(self, calls: &mut Calls) {
                #[cfg(feature = "record")]
                calls.record_as::<C>();
                #[cfg(not(feature = "record"))]
                let _ = calls;
            }
        }

        /// `C`'s outcomes have no wire form: nothing to register.
        pub trait ViaNone {
            /// Nothing.
            fn register(self, calls: &mut Calls);
        }

        impl<C> ViaNone for &Probe<C> {
            fn register(self, _calls: &mut Calls) {}
        }
    }
}

pub mod auth;
pub mod calls;
pub mod channel;
pub mod conn;
pub mod determinism;
pub mod error;
pub mod events;
pub mod id;
pub mod metrics;
pub mod owner;
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
pub use owner::Owner;
pub use registry::{MatchResult, RoomStatus};
pub use room::{Admission, Detach, ExpireTo, GameLogic, ResumeFound};
