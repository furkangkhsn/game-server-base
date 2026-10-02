//! The client's inbound half: one datagram at a time, plus the
//! reliable band's retransmit pass. A child module, so it reaches
//! [`super::UdpClient`]'s private state directly.

use super::*;
use std::time::Instant;

impl UdpClient {
    /// Handle one inbound datagram (from the server). Returns `true` when
    /// it produced a game-band frame (already in `self.raw`) — a RAW
    /// datagram, or the FRAG that completed a message — so the awaiting
    /// loop can return it immediately; REL/ACK/HELLO return `false`.
    pub(super) fn process_datagram(&mut self, d: &[u8]) -> bool {
        if !self.sealed() {
            return self.process_inner(d);
        }
        // A sealed session (module `seal`): only a record that opens is
        // read, and what it carries is the plaintext door's datagram.
        match self.open_record(d) {
            Some(inner) => self.process_inner(&inner),
            None => false,
        }
    }

    /// [`Self::process_datagram`] on the plaintext (or opened) datagram.
    fn process_inner(&mut self, d: &[u8]) -> bool {
        if d.is_empty() {
            return false;
        }
        match d[0] {
            KIND_RAW => {
                self.game_datagram();
                if let Some(fb) = body_of(&d[1..], 0) {
                    // Lossy band: no seq, no dedupe — hand it straight to
                    // the loop (unordered by design).
                    self.raw = Some(fb);
                    return true;
                }
                false
            }
            KIND_FRAG => {
                // A fragment of an over-budget game-band frame: the
                // message joins the lossy band once it is whole.
                self.game_datagram();
                match self.reasm.accept(d, Instant::now(), &mut self.stats) {
                    Some(fb) => {
                        self.raw = Some(fb);
                        true
                    }
                    None => false,
                }
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
                        if let Some(ack) = self.wire(encode_ack(self.in_expected)) {
                            let _ = self.sock.try_send_to(&ack, self.peer);
                        }
                    }
                    std::cmp::Ordering::Less => {
                        // The server retransmitted a frame already ACKed:
                        // count it (its retransmit, not our loss) and
                        // re-ACK; never re-deliver (control runs exactly
                        // once, in order).
                        self.stats.dup_in += 1;
                        if let Some(ack) = self.wire(encode_ack(self.in_expected)) {
                            let _ = self.sock.try_send_to(&ack, self.peer);
                        }
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
                // Release, liveness clock, RTT sample (Karn's rule); on a
                // sealed session the key phase's confirmation.
                self.rel.on_ack(ack, Instant::now());
                self.seal.on_ack(ack);
                false
            }
            KIND_PROBE => {
                // The game band's feedback (module `feedback`).
                self.on_probe(d);
                false
            }
            KIND_PATH_CHALLENGE => {
                // Connection migration (module `migrate`).
                self.on_path_challenge(d);
                false
            }
            // The server only sends HELLO during the handshake (already
            // complete here): ignore any late one — and any kind this
            // client does not know (the evolution rule's half here).
            _ => false,
        }
    }

    /// Retransmit the oldest un-ACKed outbound control frame whose timer
    /// expired (the timer then doubles). The mirror of the server
    /// writer's rule: an individual frame is NEVER abandoned (that wedges
    /// the direction silently); the band as a whole dies when the
    /// cumulative ACK has not advanced at all for [`REL_NO_ACK_FATAL`].
    pub(super) fn retransmit_pass(&mut self) {
        let now = Instant::now();
        match self.rel.poll(now) {
            Due::Dead(_) => self.declare_rel_dead(),
            Due::Resend(datagram) => {
                // The band keeps the inner datagram: on a sealed session
                // every re-send is a fresh record (module `seal`).
                let Some(datagram) = self.wire(datagram.to_vec()) else {
                    return;
                };
                if self.sock.try_send_to(&datagram, self.peer).is_ok() {
                    self.rel.resent(now);
                    self.stats.retrans_out += 1;
                }
            }
            Due::Idle | Due::Wait => {}
        }
        // The game band's feedback: re-announce while no probe came.
        self.announce(now);
    }

    /// The client's half of the session-fatal rule: count what will never
    /// be delivered and flip the liveness flag. There is no actor here to
    /// tear down — the caller sees [`UdpClient::is_established`] go
    /// `false` and decides (reconnect, report, exit).
    pub(super) fn declare_rel_dead(&mut self) {
        self.stats.gave_up += self.rel.abandon();
        self.established = false;
    }
}
