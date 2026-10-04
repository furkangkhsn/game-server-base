//! The controller on synthetic estimates and clocks: what is a signal
//! and what is noise, a burst against a sustained signal, the rate it
//! sets (delivered, not sent), how it grows back and opens again, the
//! draining hold, silence, the drain, and two sessions sharing one
//! bottleneck converging to equal shares. Round 4's path models (jitter,
//! a standing queue, a room's sessions behind one link) are in `path`,
//! the delay signal's own rules in `signal`; round 5's base ring in
//! `base`, its model of the B104 bottleneck in `link`.

use super::*;

mod base;
mod link;
mod path;
mod queue;
mod signal;

const BUDGET: usize = 1000;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// xorshift64*: a seeded generator for the jitter models.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((x >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// A normal draw: mean ± sd.
    fn normal(&mut self, mean: f64, sd: f64) -> f64 {
        let (u, v) = (self.unit(), self.unit());
        mean + sd * (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }

    /// One direction's delay in ms: mean ± sd, normal, clipped at zero
    /// (netem's `delay mean sd distribution normal`).
    fn leg(&mut self, mean: f64, sd: f64) -> f64 {
        self.normal(mean, sd).max(0.0)
    }
}

/// One report's estimate: the interval, its datagrams (each `BUDGET`
/// bytes) and losses, the newest RTT and the window's floor.
fn est(interval: u64, sent: u64, lost: u64, rtt: u64, floor: u64) -> GameEstimate {
    GameEstimate {
        latest_rtt: ms(rtt),
        min_rtt: ms(floor),
        window_min_rtt: ms(floor),
        interval: ms(interval),
        interval_sent: sent,
        interval_sent_bytes: sent * BUDGET as u64,
        interval_lost: lost,
        ..Default::default()
    }
}

/// A controller fed `reports` one interval apart, the room offering
/// `offer` bytes in each.
fn feed(c: &mut Control, t: &mut Instant, offer: usize, reports: &[GameEstimate]) {
    for e in reports {
        c.offered(offer);
        *t += e.interval;
        c.on_estimate(e, *t);
    }
}

/// Paced by two loss signals in a row, the room offering what was sent.
fn paced_at(t: &mut Instant, sent: u64, lost: u64) -> Control {
    let mut c = Control::new(BUDGET, *t);
    let bad = est(250, sent, lost, 20, 20);
    feed(&mut c, t, (sent * 1000) as usize, &[bad, bad]);
    assert_eq!(c.state().phase, PathPhase::Paced);
    c
}

/// B104: pacing is the default, both the enum's and the transport
/// config's (a library user who builds the config gets it too); off is
/// the explicit opt-out.
#[test]
fn pace_is_the_default() {
    assert_eq!(UdpCongestion::default(), UdpCongestion::Pace);
    assert_eq!(
        crate::udp::UdpTransportConfig::default().congestion,
        UdpCongestion::Pace
    );
}

/// Clean reports leave a session open, probed once a second; loss below
/// the bar (one datagram, or under a tenth) and a queue below the limit
/// are noise.
#[test]
fn noise_is_not_a_signal() {
    let mut t = Instant::now();
    let mut c = Control::new(BUDGET, t);
    for e in [
        est(1000, 100, 0, 20, 20),
        est(1000, 100, 1, 20, 20),
        est(1000, 100, 9, 20, 20),
        est(1000, 10, 1, 20, 20),
        est(1000, 100, 0, 49, 20),
    ] {
        feed(&mut c, &mut t, 100_000, &[e]);
        assert_eq!(c.state().phase, PathPhase::Open, "{e:?}");
    }
    assert_eq!(c.probe_interval(), PROBE_INTERVAL);
    assert_eq!(c.paced(), None);
    assert_eq!(c.counts, ControlCounts::default());
}

/// One signal is a burst until the suspect window confirms it: the
/// session is probed faster for SUSPECT_REPORTS reports, nothing else;
/// clean ones open it again.
#[test]
fn a_burst_only_quickens_the_probes() {
    let mut t = Instant::now();
    let mut c = Control::new(BUDGET, t);
    feed(&mut c, &mut t, 100_000, &[est(1000, 100, 30, 20, 20)]);
    for _ in 1..SUSPECT_REPORTS {
        assert_eq!(c.state().phase, PathPhase::Suspect);
        assert_eq!(c.probe_interval(), FAST_PROBE_INTERVAL);
        assert_eq!(c.paced(), None, "not paced on one interval");
        feed(&mut c, &mut t, 25_000, &[est(250, 25, 0, 20, 20)]);
    }
    feed(&mut c, &mut t, 25_000, &[est(250, 25, 0, 20, 20)]);
    assert_eq!(c.state().phase, PathPhase::Open);
    assert_eq!(c.probe_interval(), PROBE_INTERVAL);
    assert_eq!(c.counts, ControlCounts::default());
    // A delay hint then one lossy interval: not two loss signals in a
    // row — still only suspected.
    feed(&mut c, &mut t, 100_000, &[est(1000, 100, 0, 60, 20)]);
    feed(&mut c, &mut t, 25_000, &[est(250, 25, 10, 20, 20)]);
    assert_eq!(c.state().phase, PathPhase::Suspect, "one loss signal");
    assert_eq!(c.counts, ControlCounts::default());
}

/// One unlucky round trip — far over the floor, once — is a hint, not a
/// queue: the samples after it are back near the floor, and the session
/// opens again unpaced (round 4: the single-sample test paced jittery
/// paths; B104).
#[test]
fn one_unlucky_sample_is_only_a_hint() {
    let mut t = Instant::now();
    let mut c = Control::new(BUDGET, t);
    feed(&mut c, &mut t, 100_000, &[est(1000, 100, 0, 150, 20)]);
    assert_eq!(c.state().phase, PathPhase::Suspect);
    for rtt in [25, 140, 30, 25] {
        feed(&mut c, &mut t, 25_000, &[est(250, 25, 0, rtt, 20)]);
    }
    assert_eq!(c.state().phase, PathPhase::Open, "no two in a row");
    assert_eq!(c.counts, ControlCounts::default());
}

/// Two loss signals in a row pace the session at what the path
/// delivered, times BETA — not at what was sent.
#[test]
fn sustained_loss_paces_at_the_delivered_rate() {
    let mut t = Instant::now();
    let c = paced_at(&mut t, 100, 40);
    // 60 of 100 1000-byte datagrams in 250 ms: 240 kB/s delivered.
    let rate = c.paced().expect("paced");
    assert!((rate - 240_000.0 * BETA).abs() < 1.0, "{rate}");
    assert_eq!(c.state().rate, Some((240_000.0 * BETA) as u32));
    assert_eq!(
        c.counts,
        ControlCounts {
            episodes: 1,
            cuts: 1,
            drains: 0
        }
    );
}

/// A standing queue is a signal without any loss: the smallest of the
/// newest QUEUE_SAMPLES round trips over the window's floor by the
/// threshold — here a queue over QUEUE_DELAY_MAX, which paces before the
/// path's jitter is known. And a queue that grew within the interval
/// stretched its receive span: the rate is what the client could have
/// received, not what was sent.
#[test]
fn a_standing_queue_is_a_signal_and_a_growing_one_not_capacity() {
    let mut t = Instant::now();
    let mut c = Control::new(BUDGET, t);
    let floor = 20;
    feed(
        &mut c,
        &mut t,
        100_000,
        &[est(1000, 100, 0, floor + 30, floor)],
    );
    assert_eq!(c.state().phase, PathPhase::Suspect);
    assert_eq!(c.state().queue_delay, Some(ms(30)));
    // The queue grows by 250 ms every 250 ms: steadily, so no jitter.
    for rtt in [floor + 280, floor + 530] {
        feed(&mut c, &mut t, 100_000, &[est(250, 100, 0, rtt, floor)]);
        assert_eq!(c.state().phase, PathPhase::Suspect);
    }
    // 100 kB sent in 250 ms while the queue grew by 250 ms more: the
    // client received them over 500 ms — 200 kB/s, not 400.
    feed(
        &mut c,
        &mut t,
        100_000,
        &[est(250, 100, 0, floor + 780, floor)],
    );
    assert_eq!(c.counts.episodes, 1, "over QUEUE_DELAY_MAX: no wait");
    let rate = c.paced().expect("paced");
    assert!((rate - 200_000.0 * BETA).abs() < 1.0, "{rate}");
    assert_eq!(c.state().queue_delay, Some(ms(30)), "the smallest of four");
}

/// While paced: every loss signal cuts (never above the current rate,
/// never below the floor); every clean report adds INCREASE of the
/// episode's highest demand per second; at EXIT_HEADROOM × that demand
/// the session opens — not at a quieter interval's.
#[test]
fn paced_rates_cut_grow_and_open() {
    let mut t = Instant::now();
    // Entered at a demand of 100 kB per 250 ms: 400 kB/s.
    let mut c = paced_at(&mut t, 100, 40);
    let r0 = c.paced().unwrap();
    // A signal whose delivered rate is above the paced rate still cuts.
    feed(&mut c, &mut t, 60_000, &[est(250, 100, 20, 20, 20)]);
    let r1 = c.paced().unwrap();
    assert!((r1 - r0 * BETA).abs() < 1.0, "{r0} → {r1}");
    for _ in 0..40 {
        feed(&mut c, &mut t, 1, &[est(250, 2, 2, 20, 20)]);
    }
    assert_eq!(c.paced(), Some(MIN_RATE * BUDGET as f64), "the floor");
    assert_eq!(c.counts.cuts, 42);
    // Clean: + INCREASE × 400 kB/s × 0.25 s per report, whatever the
    // room offers now (15 kB per 250 ms); it opens at 1.25 × 400 kB/s.
    let step = INCREASE * 400_000.0 * 0.25;
    let mut last = c.paced().unwrap();
    for _ in 0..100 {
        let Some(r) = c.paced() else { break };
        assert!((r - last).abs() < 1.0 || (r - last - step).abs() < 1.0);
        last = r;
        feed(&mut c, &mut t, 15_000, &[est(250, 15, 0, 20, 20)]);
    }
    assert!(last + step >= 400_000.0 * EXIT_HEADROOM);
    assert!(last < 400_000.0 * EXIT_HEADROOM);
    assert_eq!(c.state().phase, PathPhase::Open);
    assert_eq!(c.probe_interval(), PROBE_INTERVAL);
}

/// While paced, a round trip over the threshold cuts at once (the path
/// is known to be congested); a queue that is draining — the newest
/// round trip fell by over DRAIN_MIN — holds the rate, neither cut nor
/// grown; loss cuts, draining or not.
#[test]
fn a_draining_queue_holds_the_rate() {
    let mut t = Instant::now();
    let mut c = paced_at(&mut t, 100, 40);
    // A calm stretch: the jitter estimate learns the path is steady…
    for _ in 0..8 {
        feed(&mut c, &mut t, 100_000, &[est(250, 50, 0, 20, 20)]);
    }
    assert_eq!(c.counts.cuts, 1, "calm: grown, not cut");
    assert_eq!(c.track.threshold(), QUEUE_DELAY_LIMIT);
    // …then a queue builds: over the limit, cut at once, and again.
    let r0 = c.paced().unwrap();
    feed(&mut c, &mut t, 100_000, &[est(250, 50, 0, 60, 20)]);
    let r1 = c.paced().unwrap();
    assert!(r1 < r0, "a queue over the threshold cuts");
    feed(&mut c, &mut t, 100_000, &[est(250, 50, 0, 100, 20)]);
    let r2 = c.paced().unwrap();
    assert!(r2 < r1, "still over and growing: cut again");
    // A fall under DRAIN_MIN is no drain: still over, cut again.
    feed(&mut c, &mut t, 100_000, &[est(250, 50, 0, 97, 20)]);
    let r2 = c.paced().unwrap();
    assert_eq!(c.counts.cuts, 4, "97 ms: over and not draining");
    // …and drains: 97 → 70 ms.
    feed(&mut c, &mut t, 100_000, &[est(250, 50, 0, 70, 20)]);
    assert_eq!(c.paced(), Some(r2), "a draining queue holds");
    feed(&mut c, &mut t, 100_000, &[est(250, 50, 10, 60, 20)]);
    assert!(c.paced().unwrap() < r2, "loss cuts, draining or not");
    assert_eq!(c.counts.cuts, 5);
}

/// A probe unanswered for a whole horizon (round 5): an open session is
/// suspected, nothing cut; a suspected one is paced at half the room's
/// demand; a paced one halves its rate, down to the floor.
#[test]
fn silence_suspects_then_paces_then_halves() {
    let mut t = Instant::now();
    let mut c = Control::new(BUDGET, t);
    feed(&mut c, &mut t, 100_000, &[est(1000, 100, 0, 20, 20)]);
    c.on_silence();
    assert_eq!(c.state().phase, PathPhase::Suspect);
    assert_eq!(c.probe_interval(), FAST_PROBE_INTERVAL);
    assert_eq!(c.counts, ControlCounts::default(), "only suspected");
    c.on_silence();
    assert_eq!(c.paced(), Some(50_000.0), "half of 100 kB/s");
    assert_eq!((c.counts.episodes, c.counts.cuts), (1, 1));
    c.on_silence();
    assert_eq!(c.paced(), Some(25_000.0));
    for _ in 0..20 {
        c.on_silence();
    }
    assert_eq!(c.paced(), Some(MIN_RATE * BUDGET as f64));
    assert_eq!(c.counts.cuts, 22);
}

/// A loss signal over a long interval (SUSTAINED_LOSS, 2 s) is no burst
/// — its probes, or their reports, were lost too: it paces an open
/// session at once, and a suspected one without a second signal. Over a
/// shorter one it only suspects, as before.
#[test]
fn a_long_lossy_interval_paces_at_once() {
    let mut t = Instant::now();
    let mut c = Control::new(BUDGET, t);
    feed(&mut c, &mut t, 100_000, &[est(1999, 100, 30, 20, 20)]);
    assert_eq!(c.state().phase, PathPhase::Suspect, "short: a burst");
    // Suspected by a delay hint, then one lossy interval: short, still
    // suspected; long, paced.
    let mut s = Control::new(BUDGET, t);
    feed(&mut s, &mut t, 100_000, &[est(1000, 100, 0, 60, 20)]);
    feed(&mut s, &mut t, 25_000, &[est(1999, 25, 10, 20, 20)]);
    assert_eq!(s.state().phase, PathPhase::Suspect, "one short signal");
    feed(&mut s, &mut t, 25_000, &[est(250, 25, 0, 20, 20)]);
    feed(&mut s, &mut t, 25_000, &[est(2000, 25, 10, 20, 20)]);
    assert_eq!(s.state().phase, PathPhase::Paced, "one long signal");
    let mut o = Control::new(BUDGET, t);
    feed(&mut o, &mut t, 200_000, &[est(2000, 200, 60, 20, 20)]);
    assert_eq!(o.state().phase, PathPhase::Paced, "open: at once");
    // 140 of 200 1000-byte datagrams in 2 s, times BETA.
    let rate = o.paced().unwrap();
    assert!((rate - 70_000.0 * BETA).abs() < 1.0, "{rate}");
    assert_eq!((o.counts.episodes, o.counts.cuts), (1, 1));
}

/// The base (round 5, module `base`): a queue that stood longer than the
/// window lifted the window's floor to itself (`window_min_rtt` 380 ms
/// here) — round 4 read it as no queue at all and grew the rate. The
/// floor is never a threshold over the base, so the queue still cuts.
#[test]
fn a_floor_that_rose_with_the_queue_is_still_a_queue() {
    let mut t = Instant::now();
    let mut c = paced_at(&mut t, 100, 40);
    for _ in 0..8 {
        feed(&mut c, &mut t, 100_000, &[est(250, 50, 0, 20, 20)]);
    }
    assert_eq!(c.track.threshold(), QUEUE_DELAY_LIMIT);
    let r0 = c.paced().unwrap();
    feed(&mut c, &mut t, 100_000, &[est(250, 50, 0, 400, 380)]);
    assert!(c.paced().unwrap() < r0, "a 380 ms queue cuts");
    // A new path starts the base over.
    c.new_path(t);
    assert_eq!(c.base.base(), None);
}

/// The drain (round 5, BBR's ProbeRTT): a paced session that saw no round
/// trip near its base for DRAIN_AFTER (10 s) paces at DRAIN_GAIN (half)
/// of its rate for one report, counted; the room is told the rate, not
/// the drain. Round trips near the base keep it from draining.
#[test]
fn a_paced_session_far_from_its_base_drains_once_in_a_while() {
    let mut t = Instant::now();
    let mut c = paced_at(&mut t, 100, 40);
    // Near the base for 15 s, grown and cut in turn: no drain.
    for k in 0..60 {
        feed(&mut c, &mut t, 100_000, &[est(250, 50, k % 2 * 10, 25, 20)]);
    }
    assert!(c.paced().is_some());
    assert_eq!(c.counts.drains, 0, "near: never");
    // A queue far over the base, draining 6 ms a report: the rate is
    // held — and after 10 s without a round trip near the base, it
    // drains for one report.
    feed(&mut c, &mut t, 100_000, &[est(250, 50, 0, 420, 20)]);
    let rate = c.paced().unwrap();
    let mut drained = vec![];
    for k in 1..48u64 {
        feed(&mut c, &mut t, 100_000, &[est(250, 50, 0, 420 - 6 * k, 20)]);
        assert_eq!(c.state().rate, Some(rate as u32), "held, told as held");
        if c.paced() != Some(rate) {
            assert_eq!(c.paced(), Some(rate / 2.0), "half");
            drained.push(k);
        }
    }
    assert_eq!(drained, [39], "10 s on, one report");
    assert_eq!(c.counts.drains, 1);
}

/// Two paced sessions behind one bottleneck, one starting with nine
/// tenths of it: each interval over capacity both lose their share of
/// the excess (and cut once that is a signal), each interval under it
/// both grow by the same step (the same demand) — the shares converge
/// (Chiu–Jain: additive increase, multiplicative decrease).
#[test]
fn two_sessions_on_one_bottleneck_converge_to_equal_shares() {
    let cap = 400_000.0;
    let mut t = Instant::now();
    let mut u = t; // both paced at the same instant
    let mut s = [paced_at(&mut u, 100, 40), paced_at(&mut t, 100, 40)];
    s[0].rate = 0.9 * cap;
    s[1].rate = 0.1 * cap;
    for _ in 0..400 {
        let total: f64 = s.iter().map(|c| c.rate).sum();
        t += ms(250);
        for c in &mut s {
            let sent = (c.rate * 0.25 / BUDGET as f64).round() as u64;
            let lost = match total > cap {
                true => (sent as f64 * (total - cap) / total).round() as u64,
                false => 0,
            };
            c.offered(100_000); // the room asks for the link
            c.on_estimate(&est(250, sent, lost, 20, 20), t);
        }
    }
    let (a, b) = (s[0].rate, s[1].rate);
    assert!((a - b).abs() / (a + b) < 0.1, "{a} vs {b}");
}

/// The state the room reads (B103, `gsb_core::path::PathState`): before
/// any report only the phase; once one was applied, the measurements —
/// the room's demand, the smoothed loss, the newest round trip and the
/// queue over the floor; a new path starts it over.
#[test]
fn the_state_carries_the_measurements_once_reported() {
    let mut t = Instant::now();
    let mut c = Control::new(BUDGET, t);
    assert_eq!(c.state(), PathState::default(), "nothing measured yet");
    let e = GameEstimate {
        loss: 0.125,
        ..est(1000, 100, 0, 50, 20)
    };
    feed(&mut c, &mut t, 40_000, &[e]);
    let s = c.state();
    assert_eq!(s.phase, PathPhase::Suspect, "a 30 ms queue");
    assert_eq!(s.rate, None);
    assert_eq!(s.demand, Some(40_000), "40 kB over a second");
    assert_eq!(s.loss_permille, Some(125));
    assert_eq!(s.rtt, Some(ms(50)));
    assert_eq!(s.queue_delay, Some(ms(30)));
    c.new_path(t);
    assert_eq!(c.state(), PathState::default(), "a new path: unmeasured");
    assert_eq!(c.track.recent_min(1), None, "its round trips forgotten");
    let floor = Control::new(BUDGET, t).min_rate;
    assert_eq!(c.min_rate, floor, "the floor stays");
}
