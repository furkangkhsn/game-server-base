//! Round 4's path models (BACKLOG B104), deterministic: a seeded
//! generator draws netem's `delay mean sd distribution normal` per
//! direction, the session probes at its controller's cadence, and the
//! window's floor is the smallest round trip of the last ten seconds (a
//! floor at least as low as the estimate's two buckets: no kinder to the
//! controller). Jitter alone must never pace a session; a standing queue
//! must pace it within the suspect window; and the floor must let a
//! room's sessions behind one link go below it together.

use std::collections::VecDeque;

use super::*;

/// xorshift64*: the generator, seeded per test.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((x >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// One direction's delay in ms: mean ± sd, normal, clipped at zero.
    fn leg(&mut self, mean: f64, sd: f64) -> f64 {
        let (u, v) = (self.unit(), self.unit());
        let z = (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos();
        (mean + sd * z).max(0.0)
    }
}

/// One session on a modelled path: a report at every probe interval the
/// controller asks for, the room sending `demand` bytes a second in
/// `datagram`-byte datagrams (the datagram budget, the floor's unit, is
/// `BUDGET`).
struct Session {
    c: Control,
    t: Instant,
    secs: f64,
    window: VecDeque<(f64, Duration)>,
    demand: f64,
    datagram: f64,
}

impl Session {
    fn new(demand: f64) -> Self {
        let t = Instant::now();
        Self {
            c: Control::new(BUDGET, t),
            t,
            secs: 0.0,
            window: VecDeque::new(),
            demand,
            datagram: BUDGET as f64,
        }
    }

    /// The next report, a probe interval on: its round trip, and the
    /// datagrams the interval lost (the room's demand all sent). Returns
    /// the phase after it.
    fn report(&mut self, rtt_ms: f64, lost: u64) -> PathPhase {
        let interval = self.c.probe_interval();
        let sent = self.demand * interval.as_secs_f64();
        self.apply(interval, rtt_ms, sent, lost)
    }

    /// A report `interval` after the last: its round trip, the bytes the
    /// session sent (the room offered its demand) and the datagrams lost.
    fn apply(&mut self, interval: Duration, rtt_ms: f64, sent: f64, lost: u64) -> PathPhase {
        (self.t, self.secs) = (self.t + interval, self.secs + interval.as_secs_f64());
        let rtt = Duration::from_secs_f64(rtt_ms / 1000.0);
        self.window.push_back((self.secs, rtt));
        while self
            .window
            .front()
            .is_some_and(|&(s, _)| s < self.secs - 10.0)
        {
            self.window.pop_front();
        }
        let floor = self.window.iter().map(|&(_, r)| r).min().unwrap();
        self.c
            .offered((self.demand * interval.as_secs_f64()) as usize);
        let bytes = sent as u64;
        let sent = (sent / self.datagram).round() as u64;
        let e = GameEstimate {
            latest_rtt: rtt,
            min_rtt: floor,
            window_min_rtt: floor,
            interval,
            interval_sent: sent,
            interval_sent_bytes: bytes,
            interval_lost: lost.min(sent),
            ..Default::default()
        };
        self.c.on_estimate(&e, self.t);
        self.c.state().phase
    }
}

/// The B104 jitter scenarios (per direction, ms): no loss, no queue —
/// only jitter, ten minutes each, thirty seeds. After a session's first
/// minute and a half — when its jitter estimate rests on a few suspect
/// windows' fast samples — not one report paces it; before that an
/// episode is rare (at most one session in five) and short (two cuts at
/// most, on average). Measured: 2, 4 and 5 young episodes of 3, 7 and 9
/// cuts in all, the last at 62 s. Round 3's single-sample test paced
/// all three scenarios all the time (the B104 run cut snapshots by up to
/// 34 %).
#[test]
fn jitter_alone_paces_rarely_and_never_once_measured() {
    for (mean, sd) in [(20.0, 10.0), (40.0, 20.0), (60.0, 40.0)] {
        let mut counts = ControlCounts::default();
        for seed in 1..=30u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut s = Session::new(15_000.0);
            while s.secs < 600.0 {
                let rtt = rng.leg(mean, sd) + rng.leg(mean, sd);
                let paced = s.report(rtt, 0) == PathPhase::Paced;
                assert!(
                    !(paced && s.secs > 90.0),
                    "{mean}±{sd} seed {seed}: paced at {:.1} s",
                    s.secs
                );
            }
            counts.episodes += s.c.counts.episodes;
            counts.cuts += s.c.counts.cuts;
        }
        assert!(counts.episodes <= 6, "{mean}±{sd}: {counts:?}");
        assert!(
            counts.cuts <= 2 * counts.episodes,
            "{mean}±{sd}: {counts:?}"
        );
    }
}

/// A standing queue appears and stays: the first report that carries it
/// suspects the session, the suspect window confirms it — paced within
/// 1 + SUSPECT_REPORTS reports, on a steady path (a queue just over the
/// limit), a jittery one (a queue over its learned threshold) and the
/// wildest (a queue over QUEUE_DELAY_MAX, however high the jitter).
#[test]
fn a_standing_queue_paces_within_the_suspect_window() {
    for (mean, sd, queue) in [(20.0, 0.0, 40.0), (20.0, 10.0, 100.0), (60.0, 40.0, 250.0)] {
        let mut rng = Rng(7);
        let mut s = Session::new(15_000.0);
        while s.secs < 60.0 {
            let rtt = rng.leg(mean, sd) + rng.leg(mean, sd);
            s.report(rtt, 0);
        }
        assert_ne!(s.c.state().phase, PathPhase::Paced, "{mean}±{sd}: jitter");
        let mut reports = 0;
        while s.c.state().phase != PathPhase::Paced {
            reports += 1;
            assert!(
                reports <= 1 + SUSPECT_REPORTS,
                "{mean}±{sd}, a {queue} ms queue: still {:?}",
                s.c.state().phase
            );
            let rtt = queue + rng.leg(mean, sd) + rng.leg(mean, sd);
            s.report(rtt, 0);
        }
        assert_eq!(s.c.counts.episodes, 1);
    }
}

/// A room's 64 sessions behind one link, as in the B104 bottleneck run:
/// the link carries two datagram budgets a second per session, the room
/// asks for ten, and the link's buffer holds a second of it (a deep,
/// netem-like tail-drop queue; the round trip is 20 ms plus its queue).
/// Paced, the sessions must together go below the link — the queue
/// drains and stays short — and share it fairly. Round 3's floor (four
/// budgets each) is twice the link: the queue could never drain.
#[test]
fn the_floor_lets_a_shared_link_drain() {
    const N: usize = 64;
    let budget = BUDGET as f64;
    let (cap, demand) = (N as f64 * 2.0 * budget, 10.0 * budget);
    let buffer = cap; // one second
    let dt = 0.010;
    // Snapshot-sized datagrams: ten a report at the room's demand.
    let datagram = budget / 4.0;
    let mut s: Vec<Session> = (0..N)
        .map(|_| Session {
            datagram,
            ..Session::new(demand)
        })
        .collect();
    // Each session's last and next report (s), and its bytes sent and
    // lost since.
    let mut last = vec![0.0f64; N];
    // The first probe goes as the client announces (`feedback`): within
    // the first quarter second, before the room filled the link.
    let mut due: Vec<f64> = (0..N).map(|i| 0.25 * i as f64 / N as f64).collect();
    let mut acc = vec![(0.0f64, 0.0f64); N];
    let (mut queue, mut clock) = (0.0f64, 0.0f64);
    let (mut late_delay, mut late_ticks, mut drained) = (0.0, 0u32, false);
    let mut late_sent = vec![0.0f64; N];
    while clock < 60.0 {
        clock += dt;
        let rates: Vec<f64> = s
            .iter()
            .map(|x| x.c.paced().map_or(demand, |r| r.min(demand)))
            .collect();
        let arrived = rates.iter().sum::<f64>() * dt;
        let room = buffer + cap * dt - queue;
        let dropped = (arrived - room).max(0.0);
        queue = (queue + arrived - dropped - cap * dt).max(0.0);
        for (i, r) in rates.iter().enumerate() {
            acc[i].0 += r * dt;
            acc[i].1 += r * dt * dropped / arrived.max(1.0);
            if clock > 40.0 {
                late_sent[i] += r * dt;
            }
        }
        if clock > 40.0 {
            late_delay += queue / cap;
            late_ticks += 1;
        }
        drained |= clock > 10.0 && queue == 0.0;
        for i in 0..N {
            if clock < due[i] {
                continue;
            }
            let interval = Duration::from_secs_f64(clock - last[i]);
            let (sent, lost) = std::mem::take(&mut acc[i]);
            let rtt = 20.0 + 1000.0 * queue / cap;
            s[i].apply(interval, rtt, sent, (lost / datagram).round() as u64);
            last[i] = clock;
            due[i] = clock + s[i].c.probe_interval().as_secs_f64();
        }
    }
    let mean_delay = late_delay / f64::from(late_ticks);
    let rates: Vec<f64> = late_sent.iter().map(|b| b / 20.0).collect();
    let sum: f64 = rates.iter().sum();
    let jain = sum * sum / (N as f64 * rates.iter().map(|r| r * r).sum::<f64>());
    assert!(drained, "the queue never drained");
    assert!(mean_delay < 0.1, "the last 20 s queued {mean_delay:.3} s");
    assert!(sum > 0.5 * cap, "the link idled: {sum:.0} of {cap:.0} B/s");
    assert!(jain > 0.9, "unfair: Jain {jain:.3}");
}
