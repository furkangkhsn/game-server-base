//! The wire additions (docs/EVENTS.md §11) — all additive (DESIGN §5):
//! a new base-band opcode `PUSH = 17` with its envelope, and a new
//! `Auth.features` field whose `FEATURE_PUSH` bit says the client reads
//! it. `PROTOCOL_VERSION` does not move: an old client never sends the
//! bit, so it is never sent a `PUSH` frame (the router counts
//! `client_unsupported` instead).

use bytes::Bytes;

/// The next free base-band opcode (1..=16 are taken; 15 and 16 are
/// server-internal markers that never reach the wire). Reliable band: a
/// lost PUSH would be a silent loss the client cannot heal from the next
/// snapshot (DESIGN §5.1's "cost of loss" rule), so it rides the ordered
/// stream like `RpcResponse`'s frames.
pub const PUSH: u16 = 17;

/// `Auth.features` (new field 4, `uint64`): bit 0 — the client reads
/// `PUSH` frames.
pub const FEATURE_PUSH: u64 = 1;

/// `gsb.base.Push` (server → client, opcode `PUSH`).
#[derive(Clone, PartialEq, prost::Message)]
pub struct Push {
    /// Who published (`PublisherId`; 0 = the engine).
    #[prost(uint32, tag = "1")]
    pub publisher: u32,
    /// The per-(publisher, identity) CLIENT-leg sequence number, from 1.
    #[prost(uint64, tag = "2")]
    pub seq: u64,
    /// The event's op (game band).
    #[prost(uint32, tag = "3")]
    pub op: u32,
    /// The event body, encoded once by the publisher.
    #[prost(bytes = "bytes", tag = "4")]
    pub body: Bytes,
}

/// A frame body (`[u16 op][payload]`, the transport adds the length).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The opcode.
    pub op: u16,
    /// The payload.
    pub payload: Bytes,
}
