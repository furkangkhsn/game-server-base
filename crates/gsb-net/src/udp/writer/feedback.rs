//! The game band's feedback, writer side (module `crate::udp::feedback`):
//! the probe pass, and the client's reports the demux hands over. A
//! CHILD of [`super`], so the writer's state stays private.

use std::time::Instant;

use gsb_protocol::FrameBody;

use crate::udp::*;

impl super::UdpWriter {
    /// Apply a report (the demux's piggyback, `UDP_REPORT`). An answer to
    /// one of this session's probes is an RTT sample, and it feeds the
    /// reliable band's estimator too (BACKLOG B87): the probe was never
    /// re-sent and the report names it, so the sample is unambiguous.
    pub(super) fn apply_report(&mut self, frame: &FrameBody) {
        let Some((id, received)) = parse_two_u32(&frame.payload) else {
            return; // the demux forwards only whole reports
        };
        let now = Instant::now();
        if let Report::Applied(rtt) = self.feedback.on_report(id, received, now) {
            self.rel.sample(rtt);
            self.pace_report(now);
        }
    }

    /// Send the session's probe when one is due (only a session whose
    /// client announced it reports). Synchronous, like the retransmit
    /// pass: a probe the socket refuses is counted and tried again an
    /// interval later.
    pub(super) fn probe_pass(&mut self) {
        let now = Instant::now();
        if !self.feedback.probe_due(now) {
            return;
        }
        let (id, echo) = self.feedback.next_probe();
        let Some(probe) = self.wire(&encode_probe(id, echo)).map(|d| d.into_owned()) else {
            return; // the record counter ran out: ending
        };
        match self.sock.try_send_to(&probe, self.peer) {
            // A whole ring unanswered: silence (B91), and for a paced
            // session the timeout response.
            Ok(_) if self.feedback.probe_sent(now) => self.pace_silence(now),
            Ok(_) => {}
            Err(_) => self.feedback.probe_failed(now),
        }
    }

    /// The session's game-band estimate, once its client answered a
    /// probe (`None` for a client that does not report). Congestion
    /// control (round 3) reads it with the reliable band's smoothed RTT,
    /// [`Rto::srtt`], which the probes keep fresh.
    pub(super) fn game_estimate(&self) -> Option<crate::udp::feedback::GameEstimate> {
        self.feedback.estimate()
    }
}
