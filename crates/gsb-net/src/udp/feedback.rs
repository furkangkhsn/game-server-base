//! Game-band feedback: the server's probe and the client's receiver
//! report (rUDP hardening round 2 — the signals congestion control, B1,
//! needs).
//!
//! RAW and FRAG datagrams are never acknowledged and carry no sequence
//! number, so until this module the server had no loss or delay signal
//! for the band that carries almost all of its bytes — and the reliable
//! band's RTT estimate went stale between its rare control frames
//! (BACKLOG B87). Two ADDITIVE datagram kinds close that, every existing
//! byte unchanged (`docs/DESIGN.md` §5, the evolution rule):
//!
//! ```text
//! 5 PROBE  [u32 LE probe id][u32 LE echo: newest RTT sample, µs]  server → client
//! 6 REPORT [u32 LE probe id][u32 LE game datagrams received]      client → server
//! ```
//!
//! - **The server drives the cadence.** A session's writer sends a PROBE
//!   every [`PROBE_INTERVAL`]; the client answers each one at once with a
//!   REPORT: the probe's id and how many game-band datagrams (RAW and FRAG,
//!   one each) it has received since the session began. The server knows
//!   how many it had sent when the probe left, so the gap between two
//!   answered probes gives the datagrams sent and received in between —
//!   loss without a sequence number on RAW/FRAG — and the probe's own
//!   round trip is an RTT sample. Changing the cadence later is a server
//!   change only: the client is a pure echo.
//! - **The client opts in.** A client that reports sends one REPORT with
//!   probe id 0 (an *announcement*) once connected, again every
//!   [`ANNOUNCE_EVERY`] until a probe arrives, at most [`ANNOUNCE_MAX`]
//!   times; only an announced session is probed. So an OLD client (or one
//!   with reports off) sees exactly the server it always saw — no probe,
//!   no estimate, nothing else changes — and an OLD server sees at most
//!   three REPORTs per session, which its demux drops as an unknown kind
//!   and counts (`udp_datagrams_malformed`), the session untouched.
//! - **Loss without a sequence:** consecutive answered probes `k-1, k`
//!   bound an interval; `sent = S(k) − S(k-1)` (the server's count when
//!   each probe left), `received` = the difference of the two reports.
//!   Datagrams that overtook a probe make `received` exceed `sent` by a
//!   few: the surplus is carried into the next interval instead of being
//!   clamped away (it is not loss of this interval and not delivery of
//!   the next). A report can never claim more than the server has sent by
//!   the time it arrives: over that it is clamped (network duplicates)
//!   and counted; a count that runs backwards, or an id never sent, is
//!   refused and counted; an id already answered or superseded is late.
//!   Nothing a client reports can touch another session.
//! - **The RTT sample feeds the reliable band's estimator** (`rel::Rto`,
//!   B87): one path, one round trip, one estimate. A probe is never
//!   re-sent and its report names it, so the sample is unambiguous
//!   (Karn's rule holds by construction); the band's own rules (Karn on
//!   its frames, backoff, the idle rule) are unchanged. The probe echoes
//!   the server's newest sample to the client, whose band takes it too —
//!   both directions stay fresh on a band that carries a frame a minute.
//! - **Cost:** 9 bytes each way per [`PROBE_INTERVAL`] — 37 B/s down and
//!   up per session on IPv4 (57 on IPv6), plus the announcements. State:
//!   at most [`PROBE_RING`] outstanding probes per session.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How often a reporting session is probed.
pub(super) const PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Probes awaiting their report, per session: a path whose round trip
/// exceeds this many intervals answers too late to be measured.
pub(super) const PROBE_RING: usize = 4;
/// How long a client waits for its first probe before it announces again.
pub(super) const ANNOUNCE_EVERY: Duration = Duration::from_secs(1);
/// How many announcements a client sends before it concludes the server
/// does not probe (an older server).
pub(super) const ANNOUNCE_MAX: u32 = 3;

/// The session's game-band estimate — what congestion control (round 3)
/// reads. The smoothed RTT is the reliable band's (`rel::Rto`), which the
/// probes feed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct GameEstimate {
    /// The newest probe round trip, and the smallest one seen.
    pub(crate) latest_rtt: Duration,
    pub(crate) min_rtt: Duration,
    /// The last interval between two answered probes: its length, the
    /// game datagrams sent in it and how many of them the client missed.
    pub(crate) interval: Duration,
    pub(crate) interval_sent: u64,
    pub(crate) interval_lost: u64,
    /// The loss fraction, smoothed over the intervals that sent anything
    /// (weight 1/4 per interval; the first such interval sets it).
    pub(crate) loss: f64,
    /// The intervals that sent anything.
    pub(crate) loss_intervals: u64,
    /// Answered probes so far.
    pub(crate) reports: u64,
}

/// What a report did (the writer counts and acts on it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Report {
    /// An announcement: the session is probed from now on.
    Announce,
    /// Applied: an RTT sample.
    Applied(Duration),
    /// For a probe already answered or superseded: ignored.
    Late,
    /// Refused: an id never sent, or a count that runs backwards.
    Invalid,
}

/// The feedback counters (cumulative; the writer flushes them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) announces: u64,
    pub(super) probes_sent: u64,
    pub(super) probes_send_failed: u64,
    pub(super) probes_unanswered: u64,
    pub(super) reports: u64,
    pub(super) late: u64,
    pub(super) invalid: u64,
    pub(super) clamped: u64,
    pub(super) reported_sent: u64,
    pub(super) reported_lost: u64,
    pub(super) rtt_samples: u64,
    pub(super) rtt_sum_us: u64,
}

/// A probe on the wire: its id, when it left, and the game datagrams the
/// socket had taken before it.
#[derive(Debug, Clone, Copy)]
struct Probe {
    id: u32,
    at: Instant,
    sent: u64,
}

/// One session's feedback state, server side. Pure: every method takes
/// `now`; the writer does the I/O.
#[derive(Debug)]
pub(super) struct Feedback {
    probing: bool,
    next_id: u32,
    last_probe: Option<Instant>,
    ring: VecDeque<Probe>,
    /// Game datagrams the socket took, since the session began.
    sent: u64,
    /// The baseline of the next interval: the last answered probe's send
    /// time and count, the client's counter then (as it reported it, and
    /// as accepted), and the surplus carried forward.
    base_at: Instant,
    base_sent: u64,
    recv_base: u32,
    recv_total: u64,
    carry: u64,
    /// The newest RTT sample not yet echoed, in µs (0 = none).
    echo_us: u32,
    estimate: Option<GameEstimate>,
    pub(super) counts: Counts,
}

impl Feedback {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            probing: false,
            next_id: 1,
            last_probe: None,
            ring: VecDeque::with_capacity(PROBE_RING),
            sent: 0,
            base_at: now,
            base_sent: 0,
            recv_base: 0,
            recv_total: 0,
            carry: 0,
            echo_us: 0,
            estimate: None,
            counts: Counts::default(),
        }
    }

    /// One more game-band datagram (RAW or FRAG) went on the wire.
    pub(super) fn game_sent(&mut self) {
        self.sent += 1;
    }

    /// The estimate, once a probe was answered (never for a session whose
    /// client does not report).
    pub(super) fn estimate(&self) -> Option<GameEstimate> {
        self.estimate
    }

    /// Whether a probe is due: the session announced, and the last probe
    /// (sent or refused by the socket) is an interval old.
    pub(super) fn probe_due(&self, now: Instant) -> bool {
        self.probing
            && self
                .last_probe
                .is_none_or(|t| now.saturating_duration_since(t) >= PROBE_INTERVAL)
    }

    /// The next probe's fields: its id and the echo.
    pub(super) fn next_probe(&self) -> (u32, u32) {
        (self.next_id, self.echo_us)
    }

    /// The probe of [`Self::next_probe`] went out.
    pub(super) fn probe_sent(&mut self, now: Instant) {
        if self.ring.len() == PROBE_RING {
            self.ring.pop_front();
            self.counts.probes_unanswered += 1;
        }
        self.ring.push_back(Probe {
            id: self.next_id,
            at: now,
            sent: self.sent,
        });
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.echo_us = 0;
        self.last_probe = Some(now);
        self.counts.probes_sent += 1;
    }

    /// The socket refused the probe: counted, tried again an interval on.
    pub(super) fn probe_failed(&mut self, now: Instant) {
        self.counts.probes_send_failed += 1;
        self.last_probe = Some(now);
    }

    /// The session is over: what is still outstanding was never answered.
    pub(super) fn end(&mut self) {
        self.counts.probes_unanswered += self.ring.len() as u64;
        self.ring.clear();
    }
}

/// Applying a report: its validation and the interval arithmetic. A
/// CHILD module, so it reaches the state above directly.
mod report;

#[cfg(test)]
mod tests;
