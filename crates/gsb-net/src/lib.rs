//! Network layer for gsb servers.
//!
//! The actor layer (in `gsb-core`) only ever sees *frames*
//! ([`gsb_protocol::FrameBody`]). This crate turns sockets into frames.
//! Two transports exist, both behind the same three traits ([`Transport`],
//! [`Listener`], [`Endpoint`]) so the connection/room actors never learn
//! which one is underneath:
//!
//! ```text
//! TCP   TcpListener ──accept──▶ Endpoint ──start_pump──▶ [reader pump] ─▶ ConnIn::Frame
//!                                                       └────▶ [writer pump] ◀── FrameBatch
//!
//! rUDP  UdpTransport::bind ──▶ UdpListener ─accept─▶ Endpoint ─start_pump─▶ [writer pump] ◀── FrameBatch
//!            └── one shared DEMUX task: every datagram → the right
//!                session's mailbox (reader is SHARED; per-connection
//!                there is only a writer). See `udp` for the handshake,
//!                the reliable/lossy band split, the MTU rule and the
//!                idle deadline heap.
//! ```
//!
//! - [`tcp::TcpTransport`]: TCP with a 4-byte little-endian length prefix
//!   around each frame body (the de-facto industry standard for this
//!   stack). One socket per connection; the reader pump carries the
//!   idle-timeout clock.
//! - [`udp::UdpTransport`]: rUDP — one socket for every session, a
//!   stateless cookie handshake (anti-amplification), a reliable control
//!   band (cumulative ACK + RTO retransmit) over a loss-tolerant snapshot
//!   band, a datagram budget (drop+count), and idle teardown via the
//!   demux's deadline heap (no FIN in UDP).

pub mod pump;
pub mod tcp;
pub mod transport;
pub mod udp;

pub use transport::{BoxFuture, Endpoint, Listener, Transport};
