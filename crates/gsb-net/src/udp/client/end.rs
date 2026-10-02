//! The end of a session, client side (BACKLOG B128). UDP has no FIN, so
//! the client decides the end itself, for one of three reasons
//! ([`UdpEnd`]); from then on the session is over as a stream after its
//! EOF is:
//!
//! - **received frames drain first**: the control frames already ordered
//!   (and a game-band frame already handed back) are returned by
//!   [`UdpClient::recv_frame`] as before — what arrived before the end is
//!   not lost;
//! - **then the end, at once**: `recv_frame` returns `Ok(None)` without
//!   waiting its window and without reading the socket again (a datagram
//!   after the end belongs to no session), and
//!   [`UdpClient::is_established`] is `false` — the pair
//!   `gsb_client::Conn::recv` reports as `Recv::Closed`;
//! - **nothing more is sent**: [`UdpClient::send_frame`] (and `rebind`)
//!   refuse with `NotConnected`, touching no counter — the end is counted
//!   once, under its reason, when it happens.
//!
//! What the end strands is counted beside it: the outstanding control
//! frames (`gave_up`) and the received ones still waiting behind a gap
//! (`oob_at_end`), which no read can deliver in order any more.
//! A child of [`super`], so it reaches the client's private state.

use super::*;

/// Why a client's session ended — the first reason wins; a session ends
/// once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpEnd {
    /// The reliable control band died: no cumulative-ACK progress for
    /// the liveness bound (5 s) while something was outstanding, or the
    /// retransmit backlog cap crossed (the module docs, "The REL liveness
    /// bound"). A server that went away without a word on a plaintext
    /// door ends this way, once the client has something to say.
    RelDead,
    /// A stateless reset carrying the session's token (B5b): the server
    /// no longer holds the session (a restart, or it already closed it).
    Reset,
    /// A record-layer limit (B5a): the send counter exhausted
    /// (`seal_exhausted`) or the forged-record integrity limit crossed
    /// (`seal_integrity_limit`).
    SealLimit,
}

impl UdpEnd {
    /// The reason's name (the load generator's `udp_ends_<name>` key).
    pub fn label(self) -> &'static str {
        match self {
            Self::RelDead => "rel_dead",
            Self::Reset => "reset",
            Self::SealLimit => "seal_limit",
        }
    }
}

impl UdpClient {
    /// Why the session ended, once it has (`None` while it is live).
    pub fn ended(&self) -> Option<UdpEnd> {
        self.end
    }

    /// End the session for `why` (once: a later reason is ignored, and
    /// counts nothing). There is no actor here to tear down — the caller
    /// sees the end on its next read (module docs above).
    pub(super) fn end_session(&mut self, why: UdpEnd) {
        if self.end.is_some() {
            return;
        }
        self.end = Some(why);
        self.stats.gave_up += self.rel.abandon();
        self.stats.oob_at_end += self.in_oob.len() as u64;
        self.in_oob.clear();
        self.established = false;
    }

    /// The error of a send (or a rebind) on an ended session.
    pub(super) fn ended_error(&self) -> std::io::Error {
        let why = self.end.map_or("ended", UdpEnd::label);
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            format!("rUDP session is over ({why}): nothing more is sent"),
        )
    }
}
