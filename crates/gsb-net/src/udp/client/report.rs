//! The client's half of the game band's feedback (module
//! `crate::udp::feedback`): count the game-band datagrams, announce that
//! this client reports, answer every probe at once, and take the
//! server's echoed RTT sample into the reliable band's estimator. A
//! child of [`super`], so it reaches the client's private state directly.

use super::*;

/// How a [`UdpClient`] connects. `Default` is what [`UdpClient::connect`]
/// uses.
#[derive(Debug, Clone, Copy)]
pub struct UdpClientConfig {
    /// Answer the server's game-band probes (on by default: the reports
    /// are additive — a server that predates them drops at most three
    /// announcements per session and counts them). `false` is the client
    /// before reports existed, byte for byte: no announcement, so the
    /// server never probes.
    pub game_reports: bool,
}

impl Default for UdpClientConfig {
    fn default() -> Self {
        Self { game_reports: true }
    }
}

/// The client's feedback state.
#[derive(Debug, Default)]
pub(super) struct Reports {
    on: bool,
    /// Game-band datagrams (RAW and FRAG) received since the session
    /// began — the count a report carries (wrapping, as on the wire).
    received: u32,
    announces: u32,
    announced_at: Option<Instant>,
    probed: bool,
}

impl Reports {
    pub(super) fn new(config: UdpClientConfig) -> Self {
        Self {
            on: config.game_reports,
            ..Self::default()
        }
    }
}

impl UdpClient {
    /// One game-band datagram arrived (whole or not — the path delivered
    /// it).
    pub(super) fn game_datagram(&mut self) {
        self.reports.received = self.reports.received.wrapping_add(1);
        self.stats.game_datagrams_received += 1;
    }

    /// Announce that this client reports, until a probe shows the server
    /// heard it: at once when connected, then every `ANNOUNCE_EVERY`, at
    /// most `ANNOUNCE_MAX` times (an older server never probes).
    pub(super) fn announce(&mut self, now: Instant) {
        let r = &self.reports;
        if !r.on || r.probed || r.announces >= ANNOUNCE_MAX {
            return;
        }
        if r.announced_at
            .is_some_and(|t| now.saturating_duration_since(t) < ANNOUNCE_EVERY)
        {
            return;
        }
        self.reports.announces += 1;
        self.reports.announced_at = Some(now);
        let d = encode_report(0, self.reports.received);
        match self.sock.try_send_to(&d, self.peer) {
            Ok(_) => self.stats.announces_sent += 1,
            Err(_) => self.stats.reports_send_failed += 1,
        }
    }

    /// A probe (`d` is the whole datagram): answer it at once with the
    /// count, and take its echo — the server's newest RTT sample of this
    /// path — as a sample of this band's own estimator (BACKLOG B87: the
    /// control band is too sparse to keep it fresh). An echo longer than
    /// the band's liveness bound is no round trip of a live band: refused.
    pub(super) fn on_probe(&mut self, d: &[u8]) {
        let Some((id, echo_us)) = parse_two_u32(&d[1..]) else {
            return;
        };
        self.stats.probes_received += 1;
        if !self.reports.on {
            return;
        }
        self.reports.probed = true;
        let d = encode_report(id, self.reports.received);
        match self.sock.try_send_to(&d, self.peer) {
            Ok(_) => self.stats.reports_sent += 1,
            Err(_) => self.stats.reports_send_failed += 1,
        }
        if echo_us == 0 {
            return;
        }
        let echo = Duration::from_micros(u64::from(echo_us));
        if echo > REL_NO_ACK_FATAL {
            self.stats.probe_echoes_refused += 1;
        } else {
            self.rel.sample(echo);
        }
    }
}
