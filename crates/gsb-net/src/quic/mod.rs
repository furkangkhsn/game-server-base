//! The QUIC transport (docs/DISTRIBUTED.md §6b, ROADMAP "QUIC taşıması"):
//! quinn over a single UDP socket, feeding the SAME length-prefix framing
//! and the SAME reader/writer pumps as [`crate::tcp`] and [`crate::tls`].
//!
//! # Stream mapping (v1 design decision)
//!
//! **Single bi-stream + length-prefix frames = TCP semantics over QUIC;
//! datagram mode is out of scope for v1.** The client opens exactly ONE
//! bidirectional stream immediately after connecting; the server accepts
//! that stream and wraps its halves in the generic `FrameReader` /
//! `FrameWriter` adapters. Every gsb connection therefore looks byte-for-
//! byte like a TCP connection below the pump seam: ordered, reliable,
//! backpressured frames — the actor layer cannot tell QUIC from TCP,
//! which is the whole point of the [`Transport`](crate::Transport) trait seam (same argument
//! as TLS). Per-frame QUIC streams (one stream per frame, native
//! head-of-line freedom) and unreliable datagrams would change the
//! framing contract and the pump shapes; they stay undone until the
//! rehome/0-RTT round actually needs them (docs/DISTRIBUTED.md §6b).
//!
//! # Crypto provider
//!
//! WHY rustls-on-ring again (docs/SECURITY.md §2): quinn's rustls backend
//! wraps the SAME `rustls` 0.23 crate the TLS transport already pins, and
//! each config is built with a fresh `ring` provider instance (never a
//! global install — several transports bind in one process, and a second
//! `install_default` would panic). PEM cert-chain/key loading follows
//! [`crate::tls`] exactly; a malformed or missing file fails the BIND
//! with the offending path named (no silent fallback).
//!
//! # Guardrails
//!
//! - **Handshake timeout**: a slow/hostile client can hold an accept slot
//!   only for [`HANDSHAKE_TIMEOUT`] — and the window covers the client's
//!   promised bi-stream open too: a client that handshakes and then opens
//!   nothing — or opens the stream but never writes (QUIC is lazy: an
//!   untouched stream is never put on the wire) — is just as able to pin
//!   slots as one that stalls mid-handshake. Clients speak first (AUTH)
//!   or hang up cleanly. One awaited sequence wrapped in a single
//!   deadline (the pump-timeout idiom — no multiplexing).
//! - **Protocol idle timeout**: QUIC has no FIN/half-open detection; the
//!   negotiated `max_idle_timeout` ([`IDLE_TIMEOUT`]) is the ONLY signal
//!   that a vanished peer is gone. Long-silent sessions must heartbeat —
//!   the same discipline as the TCP reader-pump idle window, which remains
//!   the session-lifecycle clock on top of this backstop.

mod config;
mod listener;

#[cfg(test)]
mod tests;

// Re-homed internals, named here so every child reaches them by one path.
use config::load_server_config;

use std::time::Duration;

use crate::framed::FrameReader;
use crate::framed::FrameWriter;

/// How long a client may spend between the first packet and a fully
/// accepted bi-stream before the server drops it. Same rationale as the
/// TLS transport's constant of the same name: long enough for any
/// legitimate WAN round-trip, short enough that a flood of hand-shy
/// clients cannot pin accept slots forever.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The QUIC `max_idle_timeout` set on both endpoints. Why it exists at
/// all: QUIC connections do not observe FINs or cable pulls — without a
/// negotiated idle window, a vanished peer's state lives forever (the
/// same problem the rUDP demux solves with its deadline heap, solved
/// here protocol-side). Sessions that may legitimately sit silent longer
/// than this must send application heartbeats.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The single ALPN protocol id both sides MUST agree on. QUIC mandates
/// ALPN negotiation (an empty mismatch is a failed handshake), so unlike
/// TLS-over-TCP this transport cannot skip protocol naming.
pub const ALPN_PROTOCOL: &[u8] = b"gsb-net/1";

/// QUIC transport configuration: paths to the server's certificate chain
/// and private key, both PEM. Loaded once at bind; a missing or malformed
/// file is a bind error (see the module docs: no silent fallback).
#[derive(Debug, Clone)]
pub struct QuicTransportConfig {
    /// Path to the PEM-encoded certificate chain (leaf first).
    pub cert_chain_pem: String,
    /// Path to the PEM-encoded private key matching the leaf certificate.
    pub key_pem: String,
    /// Maximum frame body size (same guard as TCP's `max_frame_bytes`,
    /// enforced by the shared framing codec).
    pub max_frame_bytes: usize,
}

/// QUIC transport: binds one UDP socket via quinn; each accepted QUIC
/// connection carries exactly one bidirectional stream framed like TCP.
#[derive(Clone)]
pub struct QuicTransport {
    pub config: QuicTransportConfig,
}

/// The accepted connection's reader half: length-delimited frames off the
/// server's side of the client-opened bi-stream.
type QuicReader = FrameReader<quinn::RecvStream>;
/// The accepted connection's writer half: length-prefixed frames into the
/// same bi-stream.
type QuicWriter = FrameWriter<quinn::SendStream>;

struct QuicListenerHandle {
    endpoint: quinn::Endpoint,
    max_frame_bytes: usize,
}
