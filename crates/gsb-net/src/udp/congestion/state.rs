//! The congestion response's public surface: the opt-in switch, and the
//! per-session path state a game (through its kit) can read to thin its
//! content. A CHILD of [`super`].

use std::time::Duration;

/// Whether rUDP writers respond to their sessions' congestion (the
/// server's `udp_congestion`; module `crate::udp::congestion`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UdpCongestion {
    /// No response: every frame is sent at once, as the writer always
    /// did (the default).
    #[default]
    Off,
    /// Pace a reporting session's game band to its path's estimated rate
    /// and drop the oldest frames it cannot carry (counted). A client
    /// that does not report is never paced.
    Pace,
}

/// Where a session's path stands (see the module docs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PathPhase {
    /// The path keeps up (or the client does not report): nothing is
    /// paced.
    #[default]
    Open,
    /// One congestion signal: probed faster, not paced yet.
    Suspect,
    /// Paced to [`PathState::rate`]; frames past the queue budget are
    /// dropped.
    Paced,
}

/// One session's path, as its writer last estimated it: small, `Copy`,
/// cheap to carry in a message (DESIGN §6 "Tıkanıklık tepkisi": the
/// follow-up carries it writer → connection actor → room → `TickCtx`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathState {
    pub phase: PathPhase,
    /// The rate the game band is paced to, bytes per second (only while
    /// [`PathPhase::Paced`]).
    pub rate: Option<u32>,
    /// What the room offered the game band over the last report
    /// interval, bytes per second.
    pub demand: u32,
    /// The smoothed game-band loss, per mille.
    pub loss_permille: u16,
    /// The newest round trip over the windowed minimum: the queue the
    /// path holds.
    pub queue_delay: Duration,
}

impl PathState {
    /// The game-band bytes the path carries per `period` (a tick, a
    /// snapshot interval) while paced — what a game that thins its
    /// content would plan for. `None`: not paced, no limit known.
    pub fn budget(&self, period: Duration) -> Option<usize> {
        self.rate
            .map(|r| (f64::from(r) * period.as_secs_f64()) as usize)
    }
}
