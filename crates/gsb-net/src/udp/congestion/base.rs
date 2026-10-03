//! The path's base delay (rUDP hardening round 5, BACKLOG B142): what
//! the delay signal's floor may not rise far above, kept as a short ring
//! of per-slot minima — LEDBAT's base delay history (RFC 6817 §3.4.2;
//! uTP's), sized for game sessions. Pure: every sample carries its own
//! clock. A CHILD of [`super`].
//!
//! - **Why not the window's floor alone.** `GameEstimate::window_min_rtt`
//!   (B93) is the smallest round trip of the last five to ten seconds. A
//!   queue that stands longer than that becomes the floor: the delay
//!   signal goes silent and only loss drives the rate — a loss-based
//!   controller that fills a deep buffer. The B104 bottleneck run as a
//!   model (`tests::link`): 64 sessions fill netem's 1000-packet queue
//!   within the first second, so even their first answered probes are
//!   queued, and with round 4's starved probes the window's floor climbed
//!   with the queue, 0.6 → 1.1–1.2 s, while it stood for some 15 s.
//! - **The ring.** The minimum of each [`BASE_SLOT`] is kept for the
//!   newest [`BASE_SLOTS`] slots; the base is the smallest of them: 40 s
//!   at least, 60 at most. RFC 6817 keeps ten one-minute minima (a
//!   background flow can afford to stand back for ten minutes after a
//!   route change); a game cannot, so the ring is shorter — still well
//!   over the longest the model's queue stood.
//! - **Not the floor itself.** The controller keeps the window's floor
//!   and only caps it at the base plus the path's threshold
//!   (`Control::on_estimate`): a minute's luckiest sample lies some 0.6 σ
//!   under ten seconds' on a jittery path, and as the floor it paced
//!   jitter four times as often (`tests::path::jitter_alone_paces_rarely`:
//!   42 episodes against 12 allowed). A window floor that rose by more
//!   than a threshold is a queue that stood; one that rose by less is
//!   noise either way.
//! - **A route that got longer** reads as a standing queue until the
//!   slots from before it age out: [`BASE_SLOTS`] × [`BASE_SLOT`] at
//!   most (LEDBAT's answer: a new path's base is learned by forgetting
//!   the old one). A new path the session migrated to (B3) starts the
//!   ring over at once (`Control::new_path`: RFC 9000 §9.4 resets the
//!   controller with it).

use std::time::{Duration, Instant};

/// One slot of the ring: its minimum is kept…
pub(in crate::udp) const BASE_SLOT: Duration = Duration::from_secs(20);
/// …for this many slots (the base looks back 40–60 s).
pub(in crate::udp) const BASE_SLOTS: usize = 3;

/// See the module docs.
#[derive(Debug, Clone, Copy)]
pub(in crate::udp) struct BaseDelay {
    /// The slots' minima, oldest first; the newest is the current slot.
    slots: [Option<Duration>; BASE_SLOTS],
    /// When the current slot began.
    started: Instant,
}

impl BaseDelay {
    pub(in crate::udp) fn new(now: Instant) -> Self {
        Self {
            slots: [None; BASE_SLOTS],
            started: now,
        }
    }

    /// One more round trip at `now`; the base, this sample included.
    pub(in crate::udp) fn sample(&mut self, rtt: Duration, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.started);
        // Whole slots passed (a silent spell may pass several): each
        // one rolls the ring once, at most all of it.
        let rolls = (elapsed.as_secs_f64() / BASE_SLOT.as_secs_f64()) as usize;
        if rolls > 0 {
            let n = rolls.min(BASE_SLOTS);
            self.slots.rotate_left(n);
            self.slots[BASE_SLOTS - n..].fill(None);
            self.started = match u32::try_from(rolls) {
                Ok(r) if n < BASE_SLOTS => self.started + BASE_SLOT * r,
                _ => now,
            };
        }
        let cur = &mut self.slots[BASE_SLOTS - 1];
        *cur = Some(cur.map_or(rtt, |c| c.min(rtt)));
        self.base().unwrap_or(rtt)
    }

    /// The smallest slot minimum (`None`: no sample yet).
    pub(in crate::udp) fn base(&self) -> Option<Duration> {
        self.slots.iter().flatten().min().copied()
    }
}
