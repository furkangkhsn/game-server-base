//! The global Diffie-Hellman budget of a sealed door (BACKLOG B119,
//! RUDP-SECURITY §4): a token bucket in the demux, checked after the
//! cookie and the per-source cap and BEFORE the responder's X25519 work.
//!
//! - **Why.** The demux is one task: every sealed handshake's ~180 µs of
//!   key agreement (B110) stops every session's inbound traffic for that
//!   long. The per-source cap (B89) bounds concurrent pending sessions
//!   per source, not their RATE; a return-routable flood of valid proofs
//!   could still keep the demux busy with DH. The bucket caps that rate
//!   door-wide.
//! - **What it costs an honest client.** A proof refused here creates
//!   nothing and gets no accept; the client re-sends it on its handshake
//!   timer (≤ 200 ms) and gets in when a token is free. Counted
//!   (`udp_proofs_refused_budget`).
//! - **The burst.** The bucket holds 1/[`BURST`] of a second's tokens
//!   (50 ms worth, at least one): the longest run of back-to-back DH the
//!   demux does is that — ~9 ms at the default rate — below the reliable
//!   band's 50 ms timer floor, so a storm of proofs cannot stall the
//!   other sessions' ACKs for longer than one of them waits anyway.
//! - **State:** two numbers (the credit and the last refill instant) and
//!   one clock read per proof that reaches it. The tokio clock, so a
//!   paused-clock test drives it.
//! - **Fairness** (B120) and moving DH off the demux (B121) stay open:
//!   the bucket protects the demux, not each source's share.

use std::time::Duration;

use tokio::time::Instant;

/// The bucket's depth as a fraction of a second's rate: 1/20 (50 ms).
pub(in crate::udp) const BURST: u32 = 20;

/// The token bucket (module docs). Integer arithmetic in nanoseconds of
/// credit: one handshake costs `1 s / rate`.
pub(in crate::udp) struct DhBudget {
    /// `None`: no budget (every proof is admitted).
    cost: Option<u64>,
    cap: u64,
    credit: u64,
    last: Instant,
    /// Verified proofs refused on an empty bucket.
    pub(in crate::udp) refused: u64,
}

impl DhBudget {
    /// `per_sec`: the door's `udp_handshakes_per_sec` (`None` or `0`: no
    /// budget). The bucket starts full.
    pub(in crate::udp) fn new(per_sec: Option<u32>) -> Self {
        let cost = per_sec
            .filter(|&r| r > 0)
            .map(|r| Duration::from_secs(1).as_nanos() as u64 / u64::from(r));
        let cost_or_0 = cost.unwrap_or(0).max(1);
        let burst = u64::from(per_sec.unwrap_or(0) / BURST).max(1);
        let cap = cost_or_0.saturating_mul(burst);
        Self {
            cost,
            cap,
            credit: cap,
            last: Instant::now(),
            refused: 0,
        }
    }

    /// One verified proof wants a Diffie-Hellman at `now`: `true` takes a
    /// token; `false` is a refusal (counted).
    pub(in crate::udp) fn admits(&mut self, now: Instant) -> bool {
        let Some(cost) = self.cost else {
            return true;
        };
        let elapsed = now.saturating_duration_since(self.last).as_nanos() as u64;
        self.last = now;
        self.credit = self.credit.saturating_add(elapsed).min(self.cap);
        if self.credit >= cost {
            self.credit -= cost;
            true
        } else {
            self.refused += 1;
            false
        }
    }

    /// Handshakes the full bucket admits back to back.
    #[cfg(test)]
    pub(in crate::udp) fn burst(&self) -> u64 {
        self.cost.map_or(u64::MAX, |c| self.cap / c)
    }
}

impl std::fmt::Debug for DhBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DhBudget")
            .field("cost_ns", &self.cost)
            .field("refused", &self.refused)
            .finish_non_exhaustive()
    }
}
