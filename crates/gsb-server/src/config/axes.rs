//! The transport and listener grammar of the config surface: which
//! door kinds exist and what each entry must carry.

mod listeners;
pub use listeners::*;

/// The wire transport (see `config.example.toml` and `docs/DESIGN.md` §6).
///
/// Both transports sit behind the same `gsb_net::transport` traits, so
/// the actor layer is identical for either; they differ in the wire
/// protocol and the session lifecycle:
///
/// - `Tcp`: one socket per connection, length-prefixed frames, stream
///   semantics (the reader pump's idle timeout is the teardown guard).
/// - `Udp`: rUDP — one socket for every session, a stateless cookie
///   handshake (anti-amplification), a reliable control band over a
///   loss-tolerant snapshot band, a datagram budget, and idle teardown
///   via the demux's deadline heap (no FIN in UDP). See `gsb_net::udp`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    /// Length-prefixed TCP (the default).
    #[default]
    Tcp,
    /// rUDP (one shared socket, one shared demux).
    Udp,
}

/// The rUDP writers' congestion response (`udp_congestion`; rUDP
/// hardening round 3 — `gsb_net::udp::UdpCongestion`, `docs/DESIGN.md` §6
/// "Tıkanıklık tepkisi").
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UdpCongestionKind {
    /// No response: every rUDP writer sends at once, as it always did
    /// (the default).
    #[default]
    Off,
    /// Pace a reporting session's game band to its path's estimated rate
    /// and drop the oldest frames the path cannot carry (counted); a
    /// client that does not report is never paced.
    Pace,
}

/// The rUDP doors' record layer (`udp_security`; BACKLOG B5a,
/// `docs/RUDP-SECURITY.md` decision 6).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UdpSecurityKind {
    /// Noise NK in the cookie handshake and every session datagram
    /// sealed (ChaCha20-Poly1305) under the server's static key
    /// (`udp_static_key` / `udp_static_key_file` — required). The
    /// production default.
    #[default]
    Sealed,
    /// No record layer: the rUDP door before B5a, byte for byte. Dev/LAN
    /// only — nothing is encrypted or authenticated; startup warns.
    Plaintext,
}

impl From<UdpCongestionKind> for gsb_net::udp::UdpCongestion {
    fn from(k: UdpCongestionKind) -> Self {
        match k {
            UdpCongestionKind::Off => Self::Off,
            UdpCongestionKind::Pace => Self::Pace,
        }
    }
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        };
        f.write_str(s)
    }
}
