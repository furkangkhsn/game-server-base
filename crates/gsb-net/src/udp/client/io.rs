//! The client's inbound half: one datagram at a time, plus the
//! reliable band's retransmit pass. A child module, so it reaches
//! [`super::UdpClient`]'s private state directly.

use super::*;
use std::time::Instant;

impl UdpClient {
    /// Handle one inbound datagram (from the server). Returns `true` when
    /// it produced a RAW frame (already in `self.raw`) so the awaiting
    /// loop can return it immediately; REL/ACK/HELLO return `false`.
    pub(super) fn process_datagram(&mut self, d: &[u8]) -> bool {
        if d.is_empty() {
            return false;
        }
        match d[0] {
            KIND_RAW => {
                if let Some(fb) = body_of(&d[1..], 0) {
                    // Lossy band: no seq, no dedupe — hand it straight to
                    // the loop (unordered by design).
                    self.raw = Some(fb);
                    return true;
                }
                false
            }
            KIND_REL => {
                if d.len() < 7 {
                    return false;
                }
                let seq = u32::from_le_bytes(d[1..5].try_into().unwrap());
                let body = d[5..].to_vec();
                match seq.cmp(&self.in_expected) {
                    std::cmp::Ordering::Equal => {
                        if let Some(fb) = body_of(&body, 0) {
                            self.pending.push_back(fb);
                        }
                        self.in_expected = self.in_expected.wrapping_add(1);
                        while let Some(gap) = self.in_oob.remove(&self.in_expected) {
                            if let Some(fb) = body_of(&gap, 0) {
                                self.pending.push_back(fb);
                            }
                            self.in_expected = self.in_expected.wrapping_add(1);
                        }
                        let _ = self
                            .sock
                            .try_send_to(&encode_ack(self.in_expected), self.peer);
                    }
                    std::cmp::Ordering::Less => {
                        // The server retransmitted a frame already ACKed:
                        // count it (its retransmit, not our loss) and
                        // re-ACK; never re-deliver (control runs exactly
                        // once, in order).
                        self.stats.dup_in += 1;
                        let _ = self
                            .sock
                            .try_send_to(&encode_ack(self.in_expected), self.peer);
                    }
                    std::cmp::Ordering::Greater => {
                        if self.in_oob.len() < OOB_CAP {
                            self.in_oob.insert(seq, body);
                        } else {
                            self.stats.oob_dropped += 1;
                        }
                        // No ACK for a gap: the missing frame is still
                        // outstanding server-side and will be re-sent.
                    }
                }
                false
            }
            KIND_ACK => {
                if d.len() < 5 {
                    return false;
                }
                let ack = u32::from_le_bytes(d[1..5].try_into().unwrap());
                if ack > self.acked {
                    self.acked = ack;
                    // Real progress: the server confirmed something new.
                    self.ack_progress = Instant::now();
                }
                while let Some(&(s, _, _)) = self.out_retransmit.front() {
                    if s < self.acked {
                        self.out_retransmit.pop_front();
                    } else {
                        break;
                    }
                }
                false
            }
            // The server only sends HELLO during the handshake (already
            // complete here): ignore any late one.
            _ => false,
        }
    }

    /// Retransmit the oldest un-ACKed outbound control frame whose RTO
    /// has passed. The mirror of the server writer's rule: an individual
    /// frame is NEVER abandoned (that wedges the direction silently);
    /// the band as a whole dies when the cumulative ACK has not advanced
    /// at all for [`REL_NO_ACK_FATAL`].
    pub(super) fn retransmit_pass(&mut self) {
        let now = Instant::now();
        if self.out_retransmit.is_empty() {
            // Nothing outstanding: the server has nothing to prove.
            self.ack_progress = now;
            return;
        }
        if now.saturating_duration_since(self.ack_progress) >= REL_NO_ACK_FATAL {
            self.declare_rel_dead();
            return;
        }
        if let Some((_, datagram, sent)) = self.out_retransmit.front_mut()
            && *sent + RETRANSIT_RTO <= now
            && self.sock.try_send_to(datagram, self.peer).is_ok()
        {
            *sent = now;
            self.stats.retrans_out += 1;
        }
    }

    /// The client's half of the session-fatal rule: count what will never
    /// be delivered and flip the liveness flag. There is no actor here to
    /// tear down — the caller sees [`UdpClient::is_established`] go
    /// `false` and decides (reconnect, report, exit).
    pub(super) fn declare_rel_dead(&mut self) {
        self.stats.gave_up += self.out_retransmit.len() as u64;
        self.out_retransmit.clear();
        self.established = false;
    }
}
