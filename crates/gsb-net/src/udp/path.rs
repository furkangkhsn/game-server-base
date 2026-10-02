//! Connection migration (BACKLOG B3): a session named by a connection id
//! (CID) instead of the client's address, and the path validation that
//! moves it to a new address. The pure half lives here (the pending
//! validation's rules, sans-IO, every method takes `now`); the demux's
//! half is `demux::migrate`, the writer's `writer::path`, the client's
//! `client::migrate`.
//!
//! ## Why
//!
//! Before B3 the demux keyed a session by the client's 4-tuple, so a
//! NAT rebinding (a new source port) or a Wi-Fi ↔ cellular handover (a
//! new address) was a new session: a new handshake and a resume
//! (`docs/RECONNECT.md` §5). Players on mobile data change address
//! constantly; closing a session for it is wrong (maintainer, 2026-10-02).
//!
//! ## Opt-in (`docs/RUDP-SECURITY.md` §2 decision 5)
//!
//! Without crypto a CID is a **bearer token**: an on-path sniffer that
//! reads one can answer the challenge from its own address and steer the
//! server → client stream to itself. So the server grants CIDs only when
//! its `udp_migration` is on (default off: the door as it was, byte for
//! byte). After B5a (the sealed record layer) the challenge is encrypted
//! and the default turns on.
//!
//! ## Wire (additive — `docs/DESIGN.md` §5)
//!
//! ```text
//! proof    [3][u64 nonce][u64 cookie][0][u8 caps]  19 B; caps bit 0 = CID
//! accept   [2][u32 1][u64 cid]                   ACK{1} + the CID (13 B)
//! tagged   [k | 0x80][u64 cid][kind k's body]    every c→s datagram after it
//! PATH_CHALLENGE   [7][u64 nonce]                s→c, 9 B
//! PATH_RESPONSE    [0x88][u64 cid][u64 nonce]    c→s, 17 B
//! ```
//!
//! Byte 17 is the HELLO's zero pad, as it always was (an 18-byte HELLO
//! carries 17 bytes of fields); the caps byte is appended after it, at
//! 18. An older server reads the proof's bytes 1..17 and ignores the
//! rest; an older client reads the accept's bytes 1..5 and ignores the
//! CID. A server whose migration is off never grants a CID, so the
//! client never tags; and server → client datagrams are never tagged (the
//! client knows its server by its socket).
//!
//! ## The rules (RFC 9000 §9, decision 10)
//!
//! - **Routing.** An untagged datagram is routed by address (as before);
//!   a tagged one by CID — an unknown CID is dropped and counted
//!   (`udp_cid_unknown`).
//! - **A tagged datagram from a new address** starts a validation: the
//!   server sends `PATH_CHALLENGE` with a fresh random nonce to that
//!   address and **keeps sending everything else on the old path**
//!   (decision 10: no early sends to an unvalidated address). The
//!   challenge is re-sent at most every [`CHALLENGE_RESEND`], only when
//!   the new address speaks again, and never past
//!   [`AMPLIFICATION`]× the bytes received from it.
//! - **Inbound from the unvalidated address is accepted** into the
//!   session (game, control, ACK, report). RFC 9000 §9 processes such
//!   packets; before crypto a datagram from the new address is exactly as
//!   trustworthy as one from the old (not at all — spoofing the old
//!   address injects just as well), and dropping it would cost every
//!   honest rebinding one round trip of inputs. Replies (the demux's
//!   ACKs) still go to the old, validated address.
//! - **A matching `PATH_RESPONSE`** (the pending nonce, from the
//!   candidate address) migrates: the session's address index moves, and
//!   the writer is told through its outbound channel (`UDP_PATH`, like
//!   the piggybacked ACK); from then on everything goes to the new
//!   address. A response the writer's full channel refuses is counted and
//!   the validation stays pending (the next challenge round retries).
//! - **The old address keeps working** until the migration completes.
//! - **A validation ends** migrated, timed out ([`VALIDATION_TIMEOUT`]
//!   without a matching response — noticed on the session's next
//!   datagram or at its end), superseded (a tagged datagram from a third
//!   address: the newest candidate wins — RFC 9000 §9.3's "the highest
//!   numbered non-probing packet", without numbers before B5a), or open
//!   at the session's end. Each under its own counter.
//! - **An address that is another session's** is never a candidate
//!   (`udp_path_address_in_use`): one address, one session.
//! - **The path estimate** (RFC 9000 §9.4): on a new IP the writer resets
//!   the reliable band's RTT estimator, the game band's windowed RTT and
//!   estimate, and the congestion response (open, unpaced); a change of
//!   port only (a NAT rebinding: the same path) keeps them.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::udp::*;

/// Bytes before a tagged datagram's body: the kind and the CID.
pub(super) const TAGGED_HEADER: usize = 9;
/// A `PATH_CHALLENGE`'s size: the kind and the nonce.
pub(super) const CHALLENGE_LEN: usize = 9;
/// A `PATH_RESPONSE`'s size: tagged, then the nonce.
pub(super) const RESPONSE_LEN: usize = TAGGED_HEADER + 8;
/// The most the server sends toward an unvalidated address, as a
/// multiple of what it received from it (RFC 9000 §8: 3).
pub(super) const AMPLIFICATION: u64 = 3;
/// The shortest interval between two challenges of one validation: the
/// handshake steps' ceiling (an 8-byte question is cheap to repeat).
pub(super) const CHALLENGE_RESEND: Duration = HANDSHAKE_MAX_RTO;
/// A validation without a matching response for this long is over (RFC
/// 9000 §8.2.4: three probe timeouts of a new path at the initial RTT).
/// Below the REL liveness bound: a client whose old path is dead must
/// migrate before its reliable band gives up on the old one.
pub(super) const VALIDATION_TIMEOUT: Duration = Duration::from_secs(3);
const _: () = assert!(VALIDATION_TIMEOUT.as_millis() < REL_NO_ACK_FATAL.as_millis());

/// One session's pending path validation (at most one at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PathProbe {
    /// The candidate address.
    pub(super) addr: SocketAddr,
    /// The challenge's value; the response must echo it.
    pub(super) nonce: u64,
    started: Instant,
    /// When the last challenge went out (or the socket refused it).
    last_sent: Option<Instant>,
    /// Bytes received from the candidate, and sent to it (challenges).
    received: u64,
    sent: u64,
}

impl PathProbe {
    /// A validation of `addr`, begun at `now` by a datagram of `bytes`.
    pub(super) fn new(addr: SocketAddr, nonce: u64, bytes: usize, now: Instant) -> Self {
        Self {
            addr,
            nonce,
            started: now,
            last_sent: None,
            received: bytes as u64,
            sent: 0,
        }
    }

    /// Another datagram of `bytes` from the candidate.
    pub(super) fn heard(&mut self, bytes: usize) {
        self.received = self.received.saturating_add(bytes as u64);
    }

    /// Whether a challenge is due: none yet, or the last one is a
    /// [`CHALLENGE_RESEND`] old.
    pub(super) fn challenge_due(&self, now: Instant) -> bool {
        self.last_sent
            .is_none_or(|t| now.saturating_duration_since(t) >= CHALLENGE_RESEND)
    }

    /// Whether `bytes` more toward the candidate stay within the
    /// amplification budget.
    pub(super) fn affordable(&self, bytes: usize) -> bool {
        self.sent.saturating_add(bytes as u64) <= AMPLIFICATION.saturating_mul(self.received)
    }

    /// A challenge of `bytes` went out at `now` (`taken`: the socket took
    /// it; a refused one is retried an interval later, and costs no
    /// budget).
    pub(super) fn challenged(&mut self, bytes: usize, taken: bool, now: Instant) {
        self.last_sent = Some(now);
        if taken {
            self.sent = self.sent.saturating_add(bytes as u64);
        }
    }

    /// Whether the validation ran out of time.
    pub(super) fn timed_out(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= VALIDATION_TIMEOUT
    }

    /// Whether a response with `nonce` from `from` answers this
    /// validation.
    pub(super) fn answered_by(&self, from: SocketAddr, nonce: u64) -> bool {
        self.addr == from && self.nonce == nonce
    }
}

/// A fresh random `u64` from the OS entropy source (a CID or a challenge
/// nonce: both must be unpredictable to an off-path attacker). `None`
/// when the source fails — the caller counts it and goes without (no
/// CID, no validation), never with a weaker value.
pub(super) fn draw_u64() -> Option<u64> {
    getrandom::u64().ok()
}

/// The in-process `UDP_PATH` payload (demux → writer, never on the
/// wire): the session's new address.
pub(super) fn encode_addr(a: SocketAddr) -> Vec<u8> {
    let mut v = Vec::with_capacity(19);
    match a {
        SocketAddr::V4(a4) => {
            v.push(4);
            v.extend_from_slice(&a4.ip().octets());
        }
        SocketAddr::V6(a6) => {
            v.push(6);
            v.extend_from_slice(&a6.ip().octets());
        }
    }
    v.extend_from_slice(&a.port().to_le_bytes());
    v
}

/// The inverse of [`encode_addr`].
pub(super) fn decode_addr(b: &[u8]) -> Option<SocketAddr> {
    let (ip, rest): (std::net::IpAddr, &[u8]) = match b.first()? {
        4 => {
            let o: [u8; 4] = b.get(1..5)?.try_into().ok()?;
            (o.into(), &b[5..])
        }
        6 => {
            let o: [u8; 16] = b.get(1..17)?.try_into().ok()?;
            (o.into(), &b[17..])
        }
        _ => return None,
    };
    let port = u16::from_le_bytes(rest.get(0..2)?.try_into().ok()?);
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests;
