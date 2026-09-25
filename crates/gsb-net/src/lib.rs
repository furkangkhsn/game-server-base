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
//! TLS   TcpListener ──accept + rustls handshake (10 s cap)──▶ Endpoint ──start_pump──▶ same pumps
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
//! - [`tls::TlsTransport`]: the SAME framing over rustls (docs/SECURITY.md
//!   §2): accept, a capped TLS handshake, then the encrypted halves go to
//!   the identical generic reader/writer adapters — the actor layer cannot
//!   tell TLS from plaintext. Server identity = PEM cert chain + key files
//!   loaded at bind (missing/malformed ⇒ bind error, never plaintext).
//! - [`udp::UdpTransport`]: rUDP — one socket for every session, a
//!   stateless cookie handshake (anti-amplification), a reliable control
//!   band (cumulative ACK + RTO retransmit) over a loss-tolerant snapshot
//!   band, a datagram budget (drop+count), and idle teardown via the
//!   demux's deadline heap (no FIN in UDP).

mod framed;
pub mod pump;
pub mod quic;
pub mod tcp;
pub mod tls;
pub mod transport;
// The rUDP module's own docs still link private and renamed items (13
// rustdoc warnings in `udp/`). That directory is being reworked in a
// parallel round (rUDP fragmentation, BACKLOG §1 row 3); its docs are
// fixed there and this allow is removed with it — scoped to the module so
// the rest of the crate stays under the `-D warnings` doc gate.
#[allow(rustdoc::broken_intra_doc_links, rustdoc::private_intra_doc_links)]
pub mod udp;
pub mod ws;

pub use transport::{BoxFuture, Endpoint, Listener, Transport};
