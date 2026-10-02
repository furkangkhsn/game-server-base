//! The rUDP transport's configuration (set by the composition root from
//! the server config). A child of [`super`].

use std::sync::Arc;
use std::time::Duration;

use crate::udp::*;

/// rUDP transport configuration (set by the composition root from the
/// server config — the channel capacities mirror `conn_inbox`/`conn_out`
/// because the demux pre-creates them at handshake).
#[derive(Debug, Clone)]
pub struct UdpTransportConfig {
    pub inbox_capacity: usize,
    pub outbox_capacity: usize,
    /// The datagram budget (feature 3; default [`DEFAULT_MAX_DATAGRAM_BYTES`]).
    pub max_datagram_bytes: usize,
    /// The session idle window (feature 4; `None` disables the sweep).
    pub idle_timeout: Option<Duration>,
    /// Operator-supplied cookie key (16 bytes; the composition root
    /// parses the config's 32-hex-char string into these). `None` = draw
    /// from the OS entropy source at bind time. See `CookieKey`.
    pub cookie_key: Option<[u8; 16]>,
    /// Where the demux and the writers send their loss counters (B58;
    /// `None` = counted in their stop logs only).
    pub metrics: crate::TransportMetrics,
    /// The shared socket's kernel buffers (BACKLOG B4; unset = the
    /// system default, untouched). See [`crate::listen::bind_udp`].
    pub buffers: crate::listen::UdpBuffers,
    /// The writers' congestion response (rUDP hardening round 3; the
    /// server's `udp_congestion`). `Off` (the default): the writer as it
    /// was. See `crate::udp::congestion`.
    pub congestion: UdpCongestion,
    /// Connection migration (BACKLOG B3; the server's `udp_migration`):
    /// `true` grants a connection id to every client that asks, so its
    /// session survives an address change after path validation. `false`
    /// (the default): no CID is granted and the door is byte for byte
    /// what it was. Pre-crypto a CID is a bearer token — see
    /// `crate::udp` module `path` and `docs/RUDP-SECURITY.md` §7.
    pub migration: bool,
    /// The per-source cap on pending sessions (BACKLOG B89; the server's
    /// `max_handshakes_per_source`, D11's key): one source — an IPv4
    /// address, an IPv6 /64 — holds at most this many sessions the demux
    /// has established (a verified proof) and the accept loop has not
    /// taken yet. A verified proof over it creates nothing, gets no
    /// accept and is counted (`udp_proofs_refused_per_source`); the
    /// client re-sends it. `None` (the default) or `0`: no cap.
    pub max_handshakes_per_source: Option<usize>,
    /// The record layer (B5a, `crate::udp` module `sealed`):
    /// [`UdpSecurity::Sealed`] runs Noise NK in the cookie handshake and
    /// seals every session datagram under the given static key;
    /// [`UdpSecurity::Plaintext`] (this struct's default — a library
    /// cannot invent a server's identity) is the door before B5a. The
    /// server's own default is sealed (its `udp_security`).
    pub security: UdpSecurity,
    /// The sealed door's global handshake budget (B119; the server's
    /// `udp_handshakes_per_sec`): Diffie-Hellmans per second the demux
    /// runs, a token bucket checked after the cookie and the per-source
    /// cap. A verified proof over it creates nothing and is counted
    /// (`udp_proofs_refused_budget`); the client re-sends it. `None` or
    /// `0`: no budget. Default [`DEFAULT_HANDSHAKES_PER_SEC`].
    pub handshakes_per_sec: Option<u32>,
    /// The sealed door's stateless reset key (B5b, decision 9; the
    /// server's `udp_reset_key`): `None` derives it from the static key
    /// (`seal::ResetKey::derived_from`), so resets survive every restart
    /// the static key survives. Either way it is bound to the door's
    /// address. See module `sealed`.
    pub reset_key: Option<Arc<crate::seal::ResetKey>>,
    /// The sealed door's stateless reset budget (B5b; the server's
    /// `udp_stateless_resets_per_sec`): resets per second the demux sends
    /// in answer to records whose CID it does not know (a token bucket of
    /// 50 ms). Over it a trigger is dropped and counted
    /// (`udp_stateless_resets_rate_limited`). `0`: no resets. Default
    /// [`DEFAULT_STATELESS_RESETS_PER_SEC`].
    pub stateless_resets_per_sec: u32,
    /// When a sealed session's writer rekeys its server → client key
    /// (B5b; module `sealed`). Default [`RekeyPolicy::default`].
    pub rekey: RekeyPolicy,
}

impl Default for UdpTransportConfig {
    fn default() -> Self {
        Self {
            inbox_capacity: 1024,
            outbox_capacity: 256,
            max_datagram_bytes: DEFAULT_MAX_DATAGRAM_BYTES,
            idle_timeout: Some(Duration::from_secs(30)),
            cookie_key: None,
            metrics: None,
            buffers: crate::listen::UdpBuffers::default(),
            congestion: UdpCongestion::Off,
            migration: false,
            max_handshakes_per_source: None,
            security: UdpSecurity::Plaintext,
            handshakes_per_sec: Some(DEFAULT_HANDSHAKES_PER_SEC),
            reset_key: None,
            stateless_resets_per_sec: DEFAULT_STATELESS_RESETS_PER_SEC,
            rekey: RekeyPolicy::default(),
        }
    }
}
