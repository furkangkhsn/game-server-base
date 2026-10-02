//! The congestion response's public surface: the opt-in switch, and the
//! per-session path state a game (through its kit) reads to thin its
//! content. A CHILD of [`super`].
//!
//! The path state is the CORE's (`gsb_core::path`, BACKLOG B103): one
//! type every transport fills with what it measures, carried writer →
//! connection actor → room → `TickCtx::budget`. Re-exported here, so
//! `gsb_net::udp::{PathPhase, PathState}` keep naming it. rUDP fills
//! every field once its client has reported (`Control::state`).

pub use gsb_core::path::{PathPhase, PathState};

/// Whether rUDP writers respond to their sessions' congestion (the
/// server's `udp_congestion`; module `crate::udp::congestion`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UdpCongestion {
    /// No response: every frame is sent at once, as the writer always
    /// did (the default) — and nothing is told to the room.
    #[default]
    Off,
    /// Pace a reporting session's game band to its path's estimated rate
    /// and drop the oldest frames it cannot carry (counted); tell the
    /// room what the path carries (`ConnIn::Path`, on change). A client
    /// that does not report is never paced and its path is never told.
    Pace,
}
