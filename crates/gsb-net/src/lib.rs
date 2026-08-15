//! Network layer for gsb servers.
//!
//! The actor layer (in `gsb-core`) only ever sees *frames*
//! ([`gsb_protocol::FrameBody`]). This crate turns sockets into frames:
//!
//! ```text
//! TcpListener ──accept──▶ Endpoint ──start_pump──▶ [reader pump] ─▶ ConnIn::Frame
//!                                          └────▶ [writer pump] ◀── FrameBatch
//! ```
//!
//! The transport is abstracted behind three traits ([`Transport`],
//! [`Listener`], [`Endpoint`]) so that a different wire protocol — e.g. a
//! custom reliable-UDP transport — can be plugged in without touching the
//! connection/room actors. The default implementation is
//! [`tcp::TcpTransport`]: TCP with a 4-byte little-endian length prefix
//! around each frame body (the de-facto industry standard for this stack).

pub mod pump;
pub mod tcp;
pub mod transport;

pub use transport::{BoxFuture, Endpoint, Listener, Transport};
