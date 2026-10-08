//! The worked examples' events (docs/EVENTS.md §8), written as
//! `gsb-build` would generate them from
//!
//! ```proto
//! message GoldReceived   { option (gsb.event) = { op: 2001, to: WORLD  }; … }
//! message ChatMessage    { option (gsb.event) = { op: 2002, to: CLIENT }; … }
//! message FriendOnline   { option (gsb.event) = { op: 2003, to: CLIENT }; … }
//! message FriendOffline  { option (gsb.event) = { op: 2004, to: CLIENT }; … }
//! message BalanceChanged { option (gsb.event) = { op: 2005, to: BOTH   }; … }
//! ```
//!
//! (`tests/proto/game.proto` holds that file; `tests/codegen.rs` runs
//! protoc over it and checks the generated text below is what the plan
//! produces.)

use crate::contract::{ClientEvent, Dest, Event, WorldEvent};

/// Bob got gold from someone (bank service → bob's room).
#[derive(Clone, PartialEq, prost::Message)]
pub struct GoldReceived {
    #[prost(string, tag = "1")]
    pub from: String,
    #[prost(uint64, tag = "2")]
    pub amount: u64,
    /// The ledger's balance after the transfer…
    #[prost(uint64, tag = "3")]
    pub balance: u64,
    /// …and its version: the room keeps the newest (responses and
    /// events have no order between them — §10).
    #[prost(uint64, tag = "4")]
    pub version: u64,
}

/// A chat line (chat service → each member's client).
#[derive(Clone, PartialEq, prost::Message)]
pub struct ChatMessage {
    #[prost(string, tag = "1")]
    pub channel: String,
    #[prost(string, tag = "2")]
    pub from: String,
    #[prost(string, tag = "3")]
    pub text: String,
}

/// A friend came online (friends service → each friend's client).
#[derive(Clone, PartialEq, prost::Message)]
pub struct FriendOnline {
    #[prost(string, tag = "1")]
    pub who: String,
}

/// A friend went offline.
#[derive(Clone, PartialEq, prost::Message)]
pub struct FriendOffline {
    #[prost(string, tag = "1")]
    pub who: String,
}

/// The balance changed outside the game (web shop): the room updates the
/// wallet, the client shows a toast.
#[derive(Clone, PartialEq, prost::Message)]
pub struct BalanceChanged {
    #[prost(uint64, tag = "1")]
    pub balance: u64,
    #[prost(uint64, tag = "2")]
    pub version: u64,
    #[prost(string, tag = "3")]
    pub reason: String,
}

/// A REQUEST (an rpc's input), not an event: pushing it does not compile.
#[derive(Clone, PartialEq, prost::Message)]
pub struct TransferGold {
    #[prost(string, tag = "1")]
    pub to: String,
    #[prost(uint64, tag = "2")]
    pub amount: u64,
}

// ---- what gsb-build writes (the exact text `codegen::generate` emits) ----
impl Event for GoldReceived { const OP: u16 = 2001; const TO: Dest = Dest::World; }
impl WorldEvent for GoldReceived {}
impl Event for ChatMessage { const OP: u16 = 2002; const TO: Dest = Dest::Client; }
impl ClientEvent for ChatMessage {}
impl Event for FriendOnline { const OP: u16 = 2003; const TO: Dest = Dest::Client; }
impl ClientEvent for FriendOnline {}
impl Event for FriendOffline { const OP: u16 = 2004; const TO: Dest = Dest::Client; }
impl ClientEvent for FriendOffline {}
impl Event for BalanceChanged { const OP: u16 = 2005; const TO: Dest = Dest::Both; }
impl WorldEvent for BalanceChanged {}
impl ClientEvent for BalanceChanged {}
