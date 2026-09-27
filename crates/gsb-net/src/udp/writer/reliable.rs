//! The reliable control band's half of the writer: cumulative-ACK
//! bookkeeping, the retransmit pass, and the liveness bound whose
//! verdict ends the session (see the module docs of `crate::udp`,
//! "The REL liveness bound"). A child of [`super`], so the writer's
//! state stays private to its own module tree.

use std::time::Instant;

use gsb_core::conn::ConnIn;
use gsb_protocol::FrameBody;
use tracing::{debug, warn};

use crate::udp::*;

impl super::UdpWriter {
    /// Apply an inbound cumulative ACK: advance the state, prune the
    /// retransmit buffer, and — only when it actually moved — restart the
    /// liveness clock.
    pub(super) fn apply_ack(&mut self, frame: &FrameBody) {
        if frame.payload.len() < 4 {
            return;
        }
        let ack = u32::from_le_bytes(frame.payload[..4].try_into().unwrap());
        if ack > self.acked {
            self.acked = ack;
            // Real progress: the peer confirmed something new, so the
            // channel is demonstrably alive.
            self.ack_progress = Instant::now();
        }
        while let Some(&(s, _, _)) = self.retransmit.front() {
            if s < self.acked {
                self.retransmit.pop_front();
            } else {
                break;
            }
        }
    }

    /// Retransmit the oldest un-ACKed control frame whose RTO has passed,
    /// and answer the liveness question. An individual frame is NEVER
    /// abandoned: abandoning one wedges the whole direction (the
    /// receiver's cumulative stream never advances past the hole) while
    /// the session lives on — the silent failure this bound replaces.
    /// Returns the fatal reason when the band is dead.
    pub(super) fn retransmit_pass(&mut self) -> Option<String> {
        let now = Instant::now();
        if self.retransmit.is_empty() {
            // Nothing outstanding: the peer has nothing to prove.
            self.ack_progress = now;
            return None;
        }
        let stalled = now.saturating_duration_since(self.ack_progress);
        if stalled >= REL_NO_ACK_FATAL {
            return Some(format!(
                "rUDP reliable control band: no ACK progress for {stalled:?}"
            ));
        }
        if let Some((_, datagram, sent)) = self.retransmit.front_mut()
            && *sent + RETRANSIT_RTO <= now
        {
            if self.sock.try_send_to(datagram, self.peer).is_ok() {
                *sent = now;
                self.retransmits += 1;
            } else {
                // Refused (B66): the next pass tries again.
                self.control_send_failed += 1;
            }
        }
        None
    }

    /// End the session, loudly. An undeliverable control frame is not a
    /// statistic: its direction is wedged, so the session is over. The
    /// close travels the actor's mailbox — an IN-PROCESS channel, never
    /// the socket — and the actor then runs its ordinary teardown (final
    /// metrics flush, `RegistryMsg::ConnClosed`). From there the death is
    /// indistinguishable from a dropped TCP socket: the registry releases
    /// the row of an unaffiliated session outright and routes a DETACH for
    /// a room member, whose `on_disconnect` policy owns the entity and its
    /// slot from then on.
    pub(super) fn die(&mut self, reason: String) {
        self.abandoned = self.retransmit.len() as u64;
        warn!(
            conn = %self.conn,
            peer = %self.peer,
            %reason,
            outstanding = self.abandoned,
            "rUDP: the reliable control band is dead; ending the session"
        );
        // The notice goes into the mailbox slot reserved at the writer's
        // birth (B66, `crate::pump::verdict`): synchronous, and it lands
        // even in a full mailbox — before, a `try_send` that a saturated
        // mailbox refused left the actor to find its outbound channel
        // closed with nothing explaining it, and the close was booked as
        // `outbound_dead`. The notice is in the mailbox BEFORE the run
        // loop closes the outbound channel. (A closed mailbox: the actor
        // is already gone — nothing to tell.)
        let notice = ConnIn::ServerClosed {
            cause: gsb_core::conn::ServerClose::RelDead,
            reason,
        };
        let verdict = self
            .verdict
            .take()
            .unwrap_or(crate::pump::verdict::Verdict::Never);
        if let Some(deferred) = verdict.post(notice) {
            // No slot could be reserved at birth and the mailbox is full:
            // delivered after the close (the run loop), counted.
            self.verdicts_deferred += 1;
            debug!(conn = %self.conn, "rUDP: close notice deferred past the close");
            self.deferred_verdict = Some(deferred);
        }
    }
}
