//! The retransmit timer of the reliable band (BACKLOG B2): an RFC 6298
//! round-trip estimate (SRTT/RTTVAR) with Karn's rule and exponential
//! backoff, bounded by [`MIN_RTO`] and [`MAX_RTO`]. Pure arithmetic —
//! the caller hands in the samples and the timeouts; see the module
//! docs of `crate::udp`, "Retransmit timer", for the rules and the
//! bounds' rationale.

use std::time::Duration;

/// The floor of the retransmit timeout: the old fixed RTO, which the
/// handshake's join storm measurement chose (see "Handshake loss"). A
/// peer's ACK comes from a scheduled task on both sides (the demux, the
/// writer's channel, a client's read loop), so a loopback or LAN RTT of
/// well under a millisecond still sees ACKs that wait on a scheduler;
/// below ~3 ticks at 60 Hz a busy host retransmits frames that were
/// never lost.
pub(in crate::udp) const MIN_RTO: Duration = Duration::from_millis(50);

/// The ceiling of the retransmit timeout, backoff included: a path that
/// comes back from a blackout (a Wi-Fi roam, a cellular switch) is
/// retried within a second, and the liveness bound
/// ([`crate::udp::REL_NO_ACK_FATAL`], 5 s) sees at least four attempts
/// at the ceiling before it declares the band dead (a compile-time
/// assertion in `crate::udp`).
pub(in crate::udp) const MAX_RTO: Duration = Duration::from_secs(1);

/// The timeout before the first sample: the floor. RFC 6298 starts at
/// 1 s for an unknown internet path; here the first control frame is the
/// AUTH/JOIN answer a player is waiting on, a lost one would cost a
/// second of join latency, and a spurious copy of a tens-of-bytes frame
/// costs next to nothing — while the backoff (and Karn's rule keeping it
/// until a clean sample) walks a long path's timer up within a few
/// copies.
pub(in crate::udp) const INITIAL_RTO: Duration = MIN_RTO;

/// RFC 6298's clock granularity `G`: the timer's resolution (tokio's
/// timer wheel runs in milliseconds).
const GRANULARITY: Duration = Duration::from_millis(1);

/// The estimator of one direction of one session.
#[derive(Debug, Clone, Default)]
pub(in crate::udp) struct Rto {
    /// Smoothed RTT; `None` until the first sample.
    srtt: Option<Duration>,
    /// RTT variation (meaningful once `srtt` is set).
    rttvar: Duration,
    /// Consecutive timeouts since the last valid sample: the timeout is
    /// doubled this many times (bounded — it stops growing at the
    /// ceiling).
    backoff: u32,
}

impl Rto {
    /// The smoothed RTT, once a sample was taken.
    pub(in crate::udp) fn srtt(&self) -> Option<Duration> {
        self.srtt
    }

    /// The RTT variation (zero before the first sample).
    #[cfg(test)]
    pub(in crate::udp) fn rttvar(&self) -> Duration {
        self.rttvar
    }

    /// The current retransmit timeout: `SRTT + max(G, 4·RTTVAR)` (or
    /// [`INITIAL_RTO`] before a sample), clamped to
    /// `[MIN_RTO, MAX_RTO]`, then doubled once per consecutive timeout,
    /// never past [`MAX_RTO`].
    pub(in crate::udp) fn current(&self) -> Duration {
        let base = match self.srtt {
            None => INITIAL_RTO,
            Some(srtt) => srtt.saturating_add(GRANULARITY.max(self.rttvar.saturating_mul(4))),
        };
        let mut rto = base.clamp(MIN_RTO, MAX_RTO);
        for _ in 0..self.backoff {
            rto = rto.saturating_mul(2).min(MAX_RTO);
        }
        rto
    }

    /// Take one RTT sample `r` (RFC 6298 §2.2/2.3, α = 1/8, β = 1/4). The
    /// caller applies Karn's rule: never a sample from a frame that was
    /// retransmitted (its ACK cannot say which copy it answers). A valid
    /// sample also ends the backoff: the timer is recomputed from the
    /// estimate (Karn's algorithm keeps the backed-off timer only until
    /// then).
    pub(in crate::udp) fn sample(&mut self, r: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2;
            }
            Some(srtt) => {
                // RTTVAR first, with the OLD SRTT (the RFC's order).
                self.rttvar = self.rttvar - self.rttvar / 4 + srtt.abs_diff(r) / 4;
                self.srtt = Some(srtt - srtt / 8 + r / 8);
            }
        }
        self.backoff = 0;
    }

    /// The band was idle (nothing outstanding) for longer than the
    /// current timer: drop the backoff, keep the estimate. The backoff
    /// says "the path is not answering right now"; after an idle spell
    /// that ended with everything ACKed it is stale, and on a band that
    /// carries a control frame a minute, keeping it would make the next
    /// frame (a LEAVE after a join storm, say) wait out a storm's timer.
    /// Frames sent close together keep it (Karn's algorithm: that is
    /// how a path whose RTT grew past the timer still yields a clean
    /// sample).
    pub(in crate::udp) fn restart_after_idle(&mut self) {
        self.backoff = 0;
    }

    /// The timer expired and the frame was sent again: back off (RFC 6298
    /// §5.5), until the ceiling.
    pub(in crate::udp) fn timed_out(&mut self) {
        if self.current() < MAX_RTO {
            self.backoff += 1;
        }
    }
}

#[cfg(test)]
mod tests;
