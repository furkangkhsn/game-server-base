//! The server-close counter family: one cumulative count per
//! [`ServerClose`] reason, summed over every connection actor's final
//! sample (see `ConnSample::server_close`).

use crate::conn::ServerClose;

/// Cumulative server-initiated session closes, by reason. Indexed by
/// [`ServerClose::index`], exported in [`ServerClose::ALL`] order.
///
/// A client-side end (EOF, RST, a WebSocket close handshake) is never in
/// here: the family answers "which sessions did the server end on its
/// own", which is the number a capacity measurement must not be silent
/// about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerCloses([u64; ServerClose::COUNT]);

impl ServerCloses {
    /// Build from raw counts in [`ServerClose::ALL`] order (the wire
    /// decoders' constructor).
    pub const fn from_counts(counts: [u64; ServerClose::COUNT]) -> Self {
        Self(counts)
    }

    /// Count one close.
    pub fn add(&mut self, reason: ServerClose) {
        let slot = &mut self.0[reason.index()];
        *slot = slot.saturating_add(1);
    }

    /// Add another set, reason by reason.
    pub fn add_all(&mut self, o: &Self) {
        for (a, b) in self.0.iter_mut().zip(o.0) {
            *a = a.saturating_add(b);
        }
    }

    /// The count for one reason.
    pub fn get(&self, reason: ServerClose) -> u64 {
        self.0[reason.index()]
    }

    /// Every reason's count, summed.
    pub fn total(&self) -> u64 {
        self.0.iter().fold(0u64, |a, n| a.saturating_add(*n))
    }

    /// `(reason, count)` for every reason, in export order.
    pub fn iter(&self) -> impl Iterator<Item = (ServerClose, u64)> + '_ {
        ServerClose::ALL.iter().map(|r| (*r, self.get(*r)))
    }

    /// `label:count` for the NON-zero reasons, comma-joined (`-` when
    /// there are none) — the compact human spelling.
    pub fn nonzero_summary(&self) -> String {
        let parts: Vec<String> = self
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(r, n)| format!("{}:{n}", r.label()))
            .collect();
        if parts.is_empty() {
            "-".to_owned()
        } else {
            parts.join(",")
        }
    }
}
