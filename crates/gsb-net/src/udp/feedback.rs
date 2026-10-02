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
//! - **The cadence is the writer's** (rUDP hardening round 3): a session
//!   the congestion response suspects or paces is probed faster
//!   (`congestion::FAST_PROBE_INTERVAL`, [`Feedback::set_interval`]); a
//!   client that stopped answering — a whole ring of probes evicted
//!   unanswered — is probed at a doubling interval, up to
//!   2^[`SILENT_BACKOFF_MAX`] of it, until it answers again (BACKLOG B91:
//!   a client that stopped reading costs a probe every 8 s, not every
//!   second).

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
/// The silent client's backoff (B91): after this many consecutive probes
/// evicted unanswered, the interval stops doubling (2^3 = 8×).
pub(super) const SILENT_BACKOFF_MAX: u32 = 3;

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
    pub(super) probes_open_at_end: u64,
    pub(super) reports: u64,
    pub(super) late: u64,
    pub(super) invalid: u64,
    pub(super) clamped: u64,
    pub(super) reported_sent: u64,
    pub(super) reported_lost: u64,
    pub(super) rtt_samples: u64,
    pub(super) rtt_sum_us: u64,
}

/// A probe on the wire: its id, when it left, and the game datagrams
/// (and their bytes) the socket had taken before it.
#[derive(Debug, Clone, Copy)]
struct Probe {
    id: u32,
    at: Instant,
    sent: u64,
    sent_bytes: u64,
}

/// One session's feedback state, server side. Pure: every method takes
/// `now`; the writer does the I/O.
#[derive(Debug)]
pub(super) struct Feedback {
    probing: bool,
    next_id: u32,
    last_probe: Option<Instant>,
    ring: VecDeque<Probe>,
    /// The probe interval the writer asked for, and the probes evicted
    /// unanswered in a row since the last answer (B91).
    interval: Duration,
    silent: u32,
    /// Game datagrams the socket took, and their bytes, since the session
    /// began.
    sent: u64,
    sent_bytes: u64,
    /// The baseline of the next interval: the last answered probe's send
    /// time and counts, the client's counter then (as it reported it, and
    /// as accepted), and the surplus carried forward.
    base_at: Instant,
    base_sent: u64,
    base_sent_bytes: u64,
    recv_base: u32,
    recv_total: u64,
    carry: u64,
    /// The newest RTT sample not yet echoed, in µs (0 = none).
    echo_us: u32,
    window: WindowMin,
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
            interval: PROBE_INTERVAL,
            silent: 0,
            sent: 0,
            sent_bytes: 0,
            base_at: now,
            base_sent: 0,
            base_sent_bytes: 0,
            recv_base: 0,
            recv_total: 0,
            carry: 0,
            echo_us: 0,
            window: WindowMin::new(now),
            estimate: None,
            counts: Counts::default(),
        }
    }

    /// One more game-band datagram (RAW or FRAG), `bytes` long, went on
    /// the wire.
    pub(super) fn game_sent(&mut self, bytes: usize) {
        self.sent += 1;
        self.sent_bytes += bytes as u64;
    }

    /// The estimate, once a probe was answered (never for a session whose
    /// client does not report).
    pub(super) fn estimate(&self) -> Option<GameEstimate> {
        self.estimate
    }

    /// Whether a probe is due: the session announced, and the last probe
    /// (sent or refused by the socket) is an interval old — the writer's
    /// interval, doubled for each probe evicted unanswered in a row (B91).
    pub(super) fn probe_due(&self, now: Instant) -> bool {
        let every = self.interval * (1 << self.silent.min(SILENT_BACKOFF_MAX));
        self.probing
            && self
                .last_probe
                .is_none_or(|t| now.saturating_duration_since(t) >= every)
    }

    /// The probe interval from now on (the congestion response's: fast
    /// while a session is suspected or paced).
    pub(super) fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// The next probe's fields: its id and the echo.
    pub(super) fn next_probe(&self) -> (u32, u32) {
        (self.next_id, self.echo_us)
    }

    /// The probe of [`Self::next_probe`] went out. True when it pushed an
    /// unanswered probe off a full ring: a whole ring of probes without
    /// an answer — the client is silent (B91), or the path lost them all.
    pub(super) fn probe_sent(&mut self, now: Instant) -> bool {
        let evicted = self.ring.len() == PROBE_RING;
        if evicted {
            self.ring.pop_front();
            self.counts.probes_unanswered += 1;
            self.silent = self.silent.saturating_add(1);
        }
        self.ring.push_back(Probe {
            id: self.next_id,
            at: now,
            sent: self.sent,
            sent_bytes: self.sent_bytes,
        });
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.echo_us = 0;
        self.last_probe = Some(now);
        self.counts.probes_sent += 1;
        evicted
    }

    /// The session moved to a new path (module `crate::udp::path`): the
    /// path's estimate starts over — its windowed minimum RTT, its
    /// smoothed loss, the echo still to send (RFC 9000 §9.4). The probes
    /// in flight and the interval baseline stay: a report still names its
    /// probe, and every datagram sent is still counted once (sent on the
    /// old path or the new, it was sent).
    pub(super) fn new_path(&mut self, now: Instant) {
        self.window = WindowMin::new(now);
        self.estimate = None;
        self.echo_us = 0;
    }

    /// The socket refused the probe: counted, tried again an interval on.
    pub(super) fn probe_failed(&mut self, now: Instant) {
        self.counts.probes_send_failed += 1;
        self.last_probe = Some(now);
    }

    /// The session is over: what is still outstanding was cut off, not
    /// lost — a client that stopped reading (it left, its app closed)
    /// answers nothing — so it is counted apart from the unanswered.
    pub(super) fn end(&mut self) {
        self.counts.probes_open_at_end += self.ring.len() as u64;
        self.ring.clear();
    }
}

/// Applying a report: its validation and the interval arithmetic. A
/// CHILD module, so it reaches the state above directly.
mod report;

/// The estimate and its windowed minimum RTT (B93). A CHILD module too.
mod estimate;
pub(crate) use estimate::GameEstimate;
use estimate::WindowMin;

#[cfg(test)]
mod tests;
