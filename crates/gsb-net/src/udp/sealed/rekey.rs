//! The key-phase policy (BACKLOG B5b, RUDP-SECURITY §6): WHEN a sealed
//! session's send half moves to its next key generation, and the
//! reliable band's ACK → record counter mapping that confirms the peer
//! holds the current one. Sans-IO: every method takes `now`; the writer
//! (server → client) and the client (client → server) each own one, so
//! the two directions rekey independently.
//!
//! - **The trigger:** a phase ends after [`RekeyPolicy::after`] (default
//!   [`DEFAULT_REKEY_AFTER`], 2 minutes — WireGuard's REKEY_AFTER_TIME) or
//!   [`RekeyPolicy::after_records`] records (default
//!   [`DEFAULT_REKEY_AFTER_RECORDS`], 2^20), whichever comes first. A
//!   key update narrows what one captured key exposes: REKEY is one-way,
//!   so the phases before it stay sealed. It is not a limit's answer —
//!   ChaCha20-Poly1305 has no practical confidentiality limit below the
//!   2^62 counter ceiling (RFC 9001 §6.6), the counter runs on across
//!   phases (the ceiling is a session's, whatever the phase), and the
//!   2^36 integrity limit counts forgeries across all keys. At game
//!   rates (tens to hundreds of records a second) the timer fires first;
//!   the record count bounds a phase of a bulk sender.
//! - **The seal core's two rules stay the gate** (`Sealer::rekey`): at
//!   least [`REKEY_MIN_DISTANCE`] (1024) records in the phase — a slow
//!   session simply rekeys later, uncounted — and the peer's
//!   confirmation of the current phase.
//! - **Confirmation:** the peer acknowledged a datagram of the current
//!   phase. The reliable band's cumulative ACK is that evidence: each REL
//!   frame's FIRST record counter is kept (a re-send gets a new counter;
//!   the ACK may answer any copy, so only the first — the lowest — is
//!   safe to vouch for), and an ACK that covers the frame hands it to
//!   `Sealer::note_peer_ack`. Bounded by [`RETRANSIT_CAP`], released
//!   with the band's own frames. In a live session the heartbeat keeps
//!   both directions' REL bands busy.
//! - **A peer that never confirms** does not stall anything: the
//!   session seals on under its current key, each attempt that found no
//!   confirmation is counted at most once per [`REKEY_RETRY`]
//!   (`udp_rekeys_unconfirmed`, the client's `rekeys_unconfirmed`), and
//!   the first confirmation after it rekeys at once.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::seal::{REKEY_MIN_DISTANCE, SealError, Sealer};
use crate::udp::RETRANSIT_CAP;

/// The default phase length: WireGuard's REKEY_AFTER_TIME.
pub const DEFAULT_REKEY_AFTER: Duration = Duration::from_secs(120);
/// The default phase size in records: a bulk sender's bound (a 60 Hz
/// session needs ~4.8 hours for it, so the timer always fires first).
pub const DEFAULT_REKEY_AFTER_RECORDS: u64 = 1 << 20;
/// How often an unconfirmed attempt is counted again.
pub(in crate::udp) const REKEY_RETRY: Duration = Duration::from_secs(10);

/// When a sealed session's send half rekeys (module docs). Both bounds
/// are upper bounds: the seal core's rules may defer a rekey, never
/// bring it forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RekeyPolicy {
    /// The longest a key phase lasts.
    pub after: Duration,
    /// The most records a key phase seals.
    pub after_records: u64,
}

impl Default for RekeyPolicy {
    fn default() -> Self {
        Self {
            after: DEFAULT_REKEY_AFTER,
            after_records: DEFAULT_REKEY_AFTER_RECORDS,
        }
    }
}

impl RekeyPolicy {
    /// Never rekey (the session before B5b).
    pub const NEVER: Self = Self {
        after: Duration::MAX,
        after_records: u64::MAX,
    };
}

/// A sealed session's send half with its key-phase policy (module docs).
pub(in crate::udp) struct SendHalf {
    sealer: Sealer,
    policy: RekeyPolicy,
    /// When the current phase began, and its first record counter.
    since: Instant,
    first: u64,
    /// Until when an unconfirmed attempt is not counted again.
    quiet_until: Option<Instant>,
    /// REL seq → the record counter of the frame's first send, in seq
    /// order (bounded: the band's own outstanding frames).
    rel: VecDeque<(u32, u64)>,
    /// The counter of the latest seal (`None`: it failed).
    last: Option<u64>,
    /// Key updates done, and attempts deferred for want of confirmation.
    pub(in crate::udp) rekeys: u64,
    pub(in crate::udp) unconfirmed: u64,
}

impl SendHalf {
    pub(in crate::udp) fn new(sealer: Sealer, policy: RekeyPolicy, now: Instant) -> Self {
        Self {
            sealer,
            policy,
            since: now,
            first: 0,
            quiet_until: None,
            rel: VecDeque::new(),
            last: None,
            rekeys: 0,
            unconfirmed: 0,
        }
    }

    /// Seal one inner datagram into `out` — after the next key update,
    /// when the policy calls for one and the seal core allows it.
    pub(in crate::udp) fn seal(
        &mut self,
        inner: &[u8],
        out: &mut Vec<u8>,
        now: Instant,
    ) -> Result<u64, SealError> {
        self.turn(now);
        self.last = None;
        let c = self.sealer.seal(inner, out)?;
        self.last = Some(c);
        Ok(c)
    }

    /// Whether the current phase is over by the policy.
    fn due(&self, now: Instant) -> bool {
        self.sealer.next_counter() - self.first >= self.policy.after_records
            || now.saturating_duration_since(self.since) >= self.policy.after
    }

    fn turn(&mut self, now: Instant) {
        if !self.due(now) {
            return;
        }
        match self.sealer.rekey() {
            Ok(()) => {
                self.rekeys += 1;
                self.since = now;
                self.first = self.sealer.next_counter();
                self.quiet_until = None;
            }
            Err(SealError::RekeyUnconfirmed) => {
                if self.quiet_until.is_none_or(|t| now >= t) {
                    self.unconfirmed += 1;
                    self.quiet_until = Some(now + REKEY_RETRY);
                }
            }
            // Too soon: fewer than REKEY_MIN_DISTANCE records in the
            // phase — they come with the session's own traffic.
            Err(_) => debug_assert!(self.sealer.next_counter() - self.first < REKEY_MIN_DISTANCE),
        }
    }

    /// The REL frame `seq` was just sealed for the FIRST time (the
    /// latest seal): remember its counter for the ACK that covers it.
    pub(in crate::udp) fn sent_rel(&mut self, seq: u32) {
        if let Some(c) = self.last
            && self.rel.len() < RETRANSIT_CAP
        {
            self.rel.push_back((seq, c));
        }
    }

    /// A cumulative ACK (`ack`: the next seq the peer expects): every
    /// frame below it reached the peer, so the counter of its first send
    /// is evidence of the phase it was sealed under.
    pub(in crate::udp) fn on_ack(&mut self, ack: u32) {
        while let Some(&(seq, counter)) = self.rel.front() {
            if seq >= ack {
                break;
            }
            self.sealer.note_peer_ack(counter);
            self.rel.pop_front();
        }
    }

    /// The current key generation (tests).
    #[cfg(test)]
    pub(in crate::udp) fn generation(&self) -> u64 {
        self.sealer.generation()
    }

    /// The seal core underneath (tests reach its counter).
    #[cfg(test)]
    pub(in crate::udp) fn sealer_mut(&mut self) -> &mut Sealer {
        &mut self.sealer
    }
}
