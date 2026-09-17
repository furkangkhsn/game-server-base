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

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        };
        f.write_str(s)
    }
}
