//! Applying one report (see the parent's module docs for every rule):
//! the validation that keeps a client's claim from poisoning the
//! session's state, and the interval arithmetic that turns two counts
//! into loss. A child of [`super`], so the state stays private.

use std::time::Instant;

use super::{Feedback, GameEstimate, Probe, Report};

impl Feedback {
    /// Apply one report (see the module docs for every rule).
    pub(in crate::udp) fn on_report(&mut self, id: u32, received: u32, now: Instant) -> Report {
        if id == 0 {
            self.counts.announces += 1;
            self.probing = true;
            return Report::Announce;
        }
        let Some(p) = self.ring.iter().position(|q| q.id == id) else {
            // Ids run up from 1 (wrapping past 0 after 2^32 probes, some
            // 136 years at the interval): one below the next was sent.
            return if self.probing && id < self.next_id {
                self.counts.late += 1;
                Report::Late
            } else {
                self.counts.invalid += 1;
                Report::Invalid
            };
        };
        let mut delta = u64::from(received.wrapping_sub(self.recv_base));
        if delta > u64::from(u32::MAX / 2) {
            // The client's counter ran backwards.
            self.counts.invalid += 1;
            return Report::Invalid;
        }
        let ceiling = self.sent - self.recv_total;
        if delta > ceiling {
            // More than was ever sent by now (a duplicated datagram, or a
            // false claim): no more than the truth can be delivered.
            self.counts.clamped += 1;
            delta = ceiling;
        }
        let probe = self.ring[p];
        self.counts.probes_unanswered += p as u64;
        self.ring.drain(..=p);
        self.apply(probe, received, delta, now)
    }

    fn apply(&mut self, probe: Probe, received: u32, delta: u64, now: Instant) -> Report {
        let rtt = now.saturating_duration_since(probe.at);
        let sent = probe.sent - self.base_sent;
        let delivered = delta + self.carry;
        let lost = sent.saturating_sub(delivered);
        self.carry = delivered.saturating_sub(sent);
        let interval = probe.at.saturating_duration_since(self.base_at);
        (self.base_at, self.base_sent) = (probe.at, probe.sent);
        self.recv_base = received;
        self.recv_total += delta;
        let rtt_us = u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX);
        self.echo_us = u32::try_from(rtt_us).unwrap_or(u32::MAX).max(1);
        let c = &mut self.counts;
        c.reports += 1;
        c.reported_sent += sent;
        c.reported_lost += lost;
        c.rtt_samples += 1;
        c.rtt_sum_us = c.rtt_sum_us.saturating_add(rtt_us);
        let e = self.estimate.get_or_insert(GameEstimate {
            min_rtt: rtt,
            ..Default::default()
        });
        e.latest_rtt = rtt;
        e.min_rtt = e.min_rtt.min(rtt);
        (e.interval, e.interval_sent, e.interval_lost) = (interval, sent, lost);
        if sent > 0 {
            let frac = lost as f64 / sent as f64;
            e.loss = match e.loss_intervals {
                0 => frac,
                _ => e.loss + (frac - e.loss) / 4.0,
            };
            e.loss_intervals += 1;
        }
        e.reports += 1;
        Report::Applied(rtt)
    }
}
