//! Round e0 design spike: EVENTS — what the system pushes to us — the
//! twin of the request system (docs/EVENTS.md). Not a workspace member;
//! models the router, the connection actor's CLIENT leg, the room/shard
//! EVENTS phase, the client's gap detection, engine presence and
//! record/replay on its own minimal actors.
//!
//! The type rules are compile errors, not empty lists:
//!
//! A room takes only WORLD/BOTH events:
//!
//! ```compile_fail
//! use e0_events::{events::Events, game::ChatMessage};
//! fn sys(events: &mut Events) {
//!     let _ = events.take::<ChatMessage>(); // CLIENT-only: not a WorldEvent
//! }
//! ```
//!
//! A client listens only for CLIENT/BOTH events:
//!
//! ```compile_fail
//! use e0_events::{client::EventsClient, game::GoldReceived};
//! fn ui(c: &mut EventsClient) {
//!     let _ = c.take::<GoldReceived>(); // WORLD-only: not a ClientEvent
//! }
//! ```
//!
//! Only an event can be pushed (a request type is not one):
//!
//! ```compile_fail
//! use e0_events::{contract::Identity, game::TransferGold, publisher::Publisher};
//! fn svc(p: &Publisher) {
//!     let _ = p.push(&Identity::new("bob"), &TransferGold { to: "bob".into(), amount: 1 });
//! }
//! ```
//!
//! A room cannot accept a CLIENT-only event at boot:
//!
//! ```compile_fail
//! use e0_events::{events::Events, game::FriendOnline};
//! fn boot(events: &mut Events) {
//!     let _ = events.accept::<FriendOnline>();
//! }
//! ```
//!
//! A typed listener's body type is the event's (no stringly payloads):
//!
//! ```compile_fail
//! use e0_events::{client::{EventsClient, Pushed}, game::{ChatMessage, FriendOnline}};
//! fn ui(c: &mut EventsClient) {
//!     c.on::<ChatMessage>(|e: Pushed<FriendOnline>| drop(e));
//! }
//! ```
//!
//! And the right forms compile (BOTH is both):
//!
//! ```
//! use e0_events::{client::EventsClient, contract::Identity, events::Events, publisher::Publisher};
//! use e0_events::game::{BalanceChanged, ChatMessage, GoldReceived};
//! fn ok(events: &mut Events, c: &mut EventsClient, p: &Publisher) {
//!     let _ = events.take::<GoldReceived>();
//!     let _ = events.take::<BalanceChanged>();
//!     let _ = c.take::<ChatMessage>();
//!     let _ = c.take::<BalanceChanged>();
//!     let _ = p.push(&Identity::new("bob"), &GoldReceived::default());
//! }
//! ```

pub mod client;
pub mod codegen;
pub mod conn;
pub mod contract;
pub mod engine;
pub mod events;
pub mod game;
pub mod harness;
pub mod publisher;
pub mod record;
pub mod room;
pub mod router;
pub mod wire;
