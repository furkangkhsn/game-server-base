//! Round 5's shared-bottleneck model (BACKLOG B142): the B104 bottleneck
//! run — a room's sessions behind netem's `rate … limit …` on the
//! loopback — with the writer's real parts. Deterministic: a seeded
//! generator, a millisecond clock.
//!
//! - **The link** is netem's tfifo: one FIFO that both directions share
//!   (snapshots and probes one way, reports and the clients' input the
//!   other), a wire rate that counts the loopback's headers, a packet
//!   limit (tail drop), optionally a normal delay per packet.
//! - **The sessions** run the controller, the pacing queue and the
//!   feedback state (probes, their horizon, reports, the windowed floor,
//!   the silent backoff) as the writer drives them (`writer::pace`): the
//!   room offers a snapshot a tick, spread over the tick; a probe goes
//!   only when the writer wakes — a frame, a report, a release, its
//!   housekeeping tick — so it lands right behind the session's own
//!   snapshot, where the queue is fullest.
//!
//! Against the B104 measurement (2026-10-03, round 4's code: `off`
//! reported loss 44.85 %, probe round trip 1 443 ms, ~580 probes of
//! ~1 300 unanswered; `pace` 29.53 %, 605 ms, 1 038–1 561 unanswered,
//! 54–66 episodes, 1 369–1 508 cuts, 25 624–28 528 frames dropped) the
//! model gives, round 4: `off` 41–45 %, 1 565–1 590 ms, 539–583; `pace`
//! 30–33 %, 323–901 ms, 887–1 357, 47–64, 1 031–1 215, 22 811–25 308.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::*;
use crate::udp::feedback::{Feedback, Report};

/// The run's parameters (the B104 bottleneck by default).
#[derive(Debug, Clone, Copy)]
struct Run {
    sessions: usize,
    secs: u64,
    /// The link: bytes a second on the wire, the packet limit, the
    /// per-packet header bytes (the loopback's 14 + 20 + 8), and netem's
    /// `delay mean sd` per packet (ms; a zero mean: none).
    wire_rate: f64,
    limit: usize,
    header: usize,
    delay: (f64, f64),
    /// The room: a `snapshot`-byte frame to every session at `tick_hz`;
    /// each client's input packet every `input_ms`.
    snapshot: usize,
    tick_hz: u64,
    input_ms: u64,
    /// The response on (`pace`) or off.
    pace: bool,
    seed: u64,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            sessions: 64,
            secs: 30,
            // 3610 kbit: half the room's measured demand, as the script
            // derives it; netem's `limit 1000`.
            wire_rate: 3_610_000.0 / 8.0,
            limit: 1000,
            header: 42,
            delay: (0.0, 0.0),
            // ~14 kB/s a session, the B104 room's.
            snapshot: 470,
            tick_hz: 30,
            input_ms: 150,
            pace: true,
            seed: 1,
        }
    }
}

/// What a run measured: the script's columns, and the last third's.
#[derive(Debug, Default, Clone, Copy)]
struct Measured {
    counts: ControlCounts,
    /// Reported lost / reported sent (the script's `lost%`), and the
    /// mean probe round trip in ms (`rtt_ms`).
    lost: f64,
    rtt_ms: f64,
    /// The last third: the link's mean queue (ms) and the share of the
    /// game datagrams it dropped.
    late_queue_ms: f64,
    late_lost: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Packet {
    /// To the client: a game datagram, a probe (its id).
    Game(usize),
    Probe(usize, u32),
    /// To the server: a report (the probe's id, the count it carries),
    /// the client's input.
    Report(usize, u32, u32),
    Input,
}

/// One session: the writer's parts and the client's count.
struct Node {
    c: Control,
    f: Feedback,
    q: PaceQueue,
    received: u32,
    /// When the writer last woke (ms), and whether it wakes now; the
    /// room's next snapshot for it (ms).
    woke: u64,
    wake: bool,
    next_snap: u64,
}

/// netem's tfifo with a rate and a limit: packets by arrival (µs).
struct Wire {
    r: Run,
    heap: BinaryHeap<Reverse<(u64, u64, Packet)>>,
    /// When the newest packet leaves the rate, µs.
    tail: f64,
    serial: u64,
}

impl Wire {
    /// Enqueue one packet: false when the limit drops it.
    fn send(&mut self, now_us: u64, p: Packet, bytes: usize, rng: &mut Rng) -> bool {
        if self.heap.len() >= self.r.limit {
            return false;
        }
        let wire = (bytes + self.r.header) as f64 * 1e6 / self.r.wire_rate;
        self.tail = (now_us as f64).max(self.tail) + wire;
        let (mean, sd) = self.r.delay;
        let delay = match mean > 0.0 {
            true => rng.leg(mean, sd) * 1000.0,
            false => 0.0,
        };
        self.serial += 1;
        let at = (self.tail + delay) as u64;
        self.heap.push(Reverse((at, self.serial, p)));
        true
    }

    /// The next packet that arrived by `now_us`.
    fn next(&mut self, now_us: u64) -> Option<Packet> {
        let Reverse((at, _, _)) = self.heap.peek()?;
        (*at <= now_us).then(|| self.heap.pop().expect("peeked").0.2)
    }
}

/// The game datagrams the link was given and dropped: all, and the last
/// third's.
#[derive(Default)]
struct Ledger {
    given: [u64; 2],
    dropped: [u64; 2],
}

impl Ledger {
    fn count(&mut self, ok: bool, late: bool) {
        let mut add = |i: usize| {
            self.given[i] += 1;
            self.dropped[i] += u64::from(!ok);
        };
        add(0);
        if late {
            add(1);
        }
    }
}

/// Run the model.
fn run(r: Run) -> Measured {
    let t0 = Instant::now();
    let mut rng = Rng(r.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut nodes: Vec<Node> = (0..r.sessions)
        .map(|_| Node {
            c: Control::new(BUDGET, t0),
            f: Feedback::new(t0),
            q: PaceQueue::new(BUDGET, t0),
            received: 0,
            woke: 0,
            wake: false,
            next_snap: 0,
        })
        .collect();
    let mut wire = Wire {
        r,
        heap: BinaryHeap::new(),
        tail: 0.0,
        serial: 0,
    };
    let mut ledger = Ledger::default();
    let (mut late_queue, mut late_ticks) = (0.0, 0u64);
    let late_from = r.secs * 2000 / 3;
    let tick = 1000 / r.tick_hz;
    for ms in 0..r.secs * 1000 {
        let (now_us, now) = (ms * 1000, t0 + Duration::from_millis(ms));
        let late = ms >= late_from;
        while let Some(p) = wire.next(now_us) {
            match p {
                Packet::Game(i) => nodes[i].received += 1,
                // The client answers at once.
                Packet::Probe(i, id) => {
                    let got = nodes[i].received;
                    wire.send(now_us, Packet::Report(i, id, got), 9, &mut rng);
                }
                Packet::Report(i, id, got) => {
                    let n = &mut nodes[i];
                    n.wake = true;
                    if let Report::Applied(_) = n.f.on_report(id, got, now)
                        && r.pace
                    {
                        let e = n.f.estimate().expect("applied");
                        let before = n.c.paced();
                        n.c.on_estimate(&e, now);
                        follow(n, before, now);
                    }
                }
                Packet::Input => {}
            }
        }
        if late {
            late_queue += (wire.tail - now_us as f64).max(0.0) / 1000.0;
            late_ticks += 1;
        }
        for (i, n) in nodes.iter_mut().enumerate() {
            // The clients announce within the first quarter second.
            if ms == (250 * i / r.sessions) as u64 {
                n.f.on_report(0, 0, now);
            }
            if ms % r.input_ms == (i as u64 * 7) % r.input_ms {
                wire.send(now_us, Packet::Input, 60, &mut rng);
            }
            // The room's tick: a snapshot each, somewhere in the tick.
            if ms == n.next_snap {
                n.wake = true;
                n.next_snap = (ms / tick + 1) * tick + (rng.unit() * tick as f64) as u64;
                n.c.offered(r.snapshot);
                let dg = vec![vec![0u8; r.snapshot]];
                match n.c.paced() {
                    Some(rate) if r.pace => n.q.push(dg, false, cap(rate)),
                    None if r.pace && !n.q.is_empty() => n.q.push(dg, false, usize::MAX),
                    _ => {
                        n.f.game_sent(r.snapshot);
                        let ok = wire.send(now_us, Packet::Game(i), r.snapshot, &mut rng);
                        ledger.count(ok, late);
                    }
                }
            }
            // The pacing pass.
            loop {
                let released = match n.c.paced() {
                    Some(rate) => n.q.pop(now, rate),
                    None => n.q.pop_any(),
                };
                let Some(d) = released else { break };
                n.wake = true;
                n.f.game_sent(d.datagram.len());
                let ok = wire.send(now_us, Packet::Game(i), d.datagram.len(), &mut rng);
                ledger.count(ok, late);
            }
            // The probe pass, when the writer woke.
            n.wake |= ms >= n.woke + 50;
            if !std::mem::take(&mut n.wake) {
                continue;
            }
            n.woke = ms;
            if n.f.probe_due(now) {
                let (id, _) = n.f.next_probe();
                wire.send(now_us, Packet::Probe(i, id), 9, &mut rng);
                if n.f.probe_sent(now) && r.pace {
                    let before = n.c.paced();
                    n.c.on_silence();
                    follow(n, before, now);
                }
            }
        }
    }
    let mut m = Measured {
        late_queue_ms: late_queue / late_ticks.max(1) as f64,
        late_lost: ledger.dropped[1] as f64 / ledger.given[1].max(1) as f64,
        ..Default::default()
    };
    let (mut sent, mut lost, mut rtt_us, mut rtts) = (0, 0, 0, 0);
    for n in &nodes {
        m.counts.episodes += n.c.counts.episodes;
        m.counts.cuts += n.c.counts.cuts;
        m.counts.drains += n.c.counts.drains;
        (sent, lost) = (
            sent + n.f.counts.reported_sent,
            lost + n.f.counts.reported_lost,
        );
        (rtt_us, rtts) = (
            rtt_us + n.f.counts.rtt_sum_us,
            rtts + n.f.counts.rtt_samples,
        );
    }
    m.lost = lost as f64 / sent.max(1) as f64;
    m.rtt_ms = rtt_us as f64 / rtts.max(1) as f64 / 1000.0;
    m
}

/// The writer's `pace_follow`: the queue follows the rate, the probes
/// the cadence.
fn follow(n: &mut Node, before: Option<f64>, now: Instant) {
    if let Some(rate) = n.c.paced() {
        if before.is_none() {
            n.q.start(now, rate);
        }
        n.q.trim(cap(rate));
    }
    n.f.set_interval(n.c.probe_interval());
}

fn cap(rate: f64) -> usize {
    (rate * QUEUE_BUDGET.as_secs_f64()) as usize
}

/// The B104 bottleneck, 30 s, `pace` against `off`, three seeds each: the
/// script's verdict holds — `lost%` at most half of `off`'s, the probe
/// round trip at most a third — and the link's last ten seconds queue
/// briefly and drop nothing. Without jitter and with the
/// `bottleneck_jitter` scenario's 20 ± 10 ms. Measured: `pace` 20.8–21.8
/// % and 214–225 ms against `off`'s 45.2–46.6 % and 1 595–1 622 ms (with
/// jitter 20.4–21.3 % and 254–259 ms against 49.8–52.3 % and 1 698–1 700
/// ms); the last third 47–56 ms of queue, nothing dropped, no drain.
/// Round 4: 30–33 % and 323–901 ms against 41–45 % — a ratio of
/// 0.68–0.77, the measurement's FAIL.
#[test]
fn the_b104_bottleneck_passes_its_verdict() {
    for delay in [(0.0, 0.0), (20.0, 10.0)] {
        for seed in 1..=3 {
            let base = Run {
                delay,
                seed,
                ..Run::default()
            };
            let off = run(Run {
                pace: false,
                ..base
            });
            let pace = run(base);
            let what = format!("{delay:?} seed {seed}: pace {pace:?}, off {off:?}");
            assert!(pace.counts.episodes > 0, "{what}");
            assert!(pace.lost <= off.lost / 2.0, "{what}");
            assert!(pace.rtt_ms <= off.rtt_ms / 3.0, "{what}");
            assert!(pace.late_queue_ms < 100.0, "{what}");
            assert!(pace.late_lost < 0.001, "{what}");
            assert_eq!(pace.counts.drains, 0, "{what}");
        }
    }
}

/// Half again as many sessions (the link a third of their demand) for
/// 90 s: the startup is harder — the queue stands longer than the
/// window, and the base keeps it a queue (round 4's floor drifted with
/// it) — but the link's last thirty seconds queue briefly and drop
/// nothing. Measured: 18 % lost, 168 ms; the last third 72–73 ms, nothing
/// dropped. Round 4: 47 %, 540–670 ms, the last third 122–214 ms and
/// 0.5–1.6 % dropped (half the sessions never paced in its first 30 s).
#[test]
fn a_harder_bottleneck_settles_too() {
    for seed in 1..=2 {
        let m = run(Run {
            sessions: 96,
            secs: 90,
            seed,
            ..Run::default()
        });
        assert!(m.lost < 0.25, "seed {seed}: {m:?}");
        assert!(m.late_queue_ms < 100.0, "seed {seed}: {m:?}");
        assert!(m.late_lost < 0.001, "seed {seed}: {m:?}");
    }
}
