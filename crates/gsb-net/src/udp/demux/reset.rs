//! Stateless reset, demux side (BACKLOG B5b, RUDP-SECURITY §8): a SEALED
//! record whose CID no session holds — the session ended, or this server
//! restarted and never knew it — is answered with that CID's reset
//! (`seal::reset_datagram`), so the client ends its session at once and
//! starts over (a new handshake and a resume) instead of waiting out the
//! reliable band's 5 s liveness bound. A child of [`super`].
//!
//! - **Stateless:** the token is `HMAC(door's reset key, cid)`; the key
//!   survives restarts (module `crate::udp::sealed`, `door`), so a
//!   restarted door answers the CIDs of the sessions it lost.
//! - **Never an amplifier:** the reset is strictly shorter than the
//!   datagram that triggered it and at most 41 bytes; it goes only to the
//!   trigger's source.
//! - **Rate-limited before any work:** a door-wide token bucket
//!   (`udp_stateless_resets_per_sec`, 50 ms deep) is checked before the
//!   entropy draw and the HMAC; a trigger over it is dropped and counted
//!   (`udp_stateless_resets_rate_limited`). The datagram itself stays
//!   `udp_cid_unknown` either way.

use std::net::SocketAddr;

use crate::seal::{RESET_LEN_MAX, reset_datagram};

impl super::Demux {
    /// The SEALED record of `n` bytes from `from` names `cid`, which no
    /// session holds: answer with its stateless reset, budget allowing.
    pub(super) fn stateless_reset(&mut self, cid: u64, n: usize, from: SocketAddr) {
        let Some(seal) = self.seal.as_mut() else {
            return;
        };
        let Some(budget) = seal.resets.as_mut() else {
            return; // resets are off on this door
        };
        if !budget.admits(tokio::time::Instant::now()) {
            return; // counted by the bucket
        }
        let mut random = [0u8; RESET_LEN_MAX];
        if getrandom::fill(&mut random).is_err() {
            self.mig.entropy_failed += 1;
            return;
        }
        let Some(d) = reset_datagram(&seal.reset.token(cid), n, &random) else {
            return; // a trigger too short to answer below its size
        };
        match self.sock.try_send_to(&d, from) {
            Ok(_) => seal.counts.resets_sent += 1,
            Err(_) => seal.counts.resets_send_failed += 1,
        }
    }
}
