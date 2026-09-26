//! The client building block for gsb servers.
//!
//! What every Rust client of a gsb server needs and none should retype:
//!
//! - [`frame`]: the stream wire, `[u32 LE length][u16 LE opcode]
//!   [payload]` — a cancel-safe reader with a frame-size guard and a
//!   writer ([`frame::FrameRx`], [`frame::FrameTx`]).
//! - [`Conn`]: one connection over any door — a byte stream (TCP, TLS,
//!   the QUIC bi-stream) or the rUDP client half of `gsb_net` — with one
//!   `send` / bounded `recv` for all of them ([`connect`], [`tls`],
//!   [`quic`] open one).
//! - [`session`]: the base protocol's steps — AUTH (with or without a
//!   ticket), JOIN, HEARTBEAT, LEAVE, the resume key — as plain async
//!   functions; an `ERROR` frame comes back as a typed [`ServerError`]
//!   (`ErrorCode`, the raw number, the message).
//!
//! What it is not: a game client. Game-band payloads stay opaque
//! (`Bytes` + opcode) — decoding them, and rebuilding a view from
//! snapshots (`gsb_kit::client`), is the caller's. No policy either:
//! reconnect, backoff and retry decisions are the caller's; this crate
//! reports what happened.
//!
//! No WebSocket client: the WS door's clients are browsers and engines
//! with their own WebSocket stacks; the in-tree WS clients are protocol
//! test rigs (`gsb-net`'s ws suite), not a building block.
//!
//! A client is a task, not an actor, but the workspace rules hold: no
//! locks, and every wait is a single bounded await (no multiplexing) —
//! a caller that reads and writes concurrently splits the stream
//! ([`Conn::into_split`]) and gives each half its own task.

pub mod conn;
pub mod connect;
pub mod error;
pub mod frame;
pub mod quic;
pub mod session;
pub mod tls;

pub use conn::{Conn, Recv};
pub use error::{ClientError, ServerError};
pub use session::{Credentials, Joined};
