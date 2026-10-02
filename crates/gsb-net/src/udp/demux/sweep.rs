//! The idle sweep (feature 4): the demux's deadline heap, popped when
//! the socket read's deadline fires. A child of [`super`], so the
//! demux's session table stays private to its own module tree.
//!
//! **A late fire is the process's stall (BACKLOG F72).** A deadline the
//! sweep finds more than [`IDLE_STALL_GRACE`] overdue was not observed
//! when it fell due — the process was not running (a swap storm, a VM
//! pause, a starved runtime) — so the silence is the server's own: the
//! window restarts instead of the close, counted
//! (`idle_windows_restarted_late`). Once per silence, the stream pumps'
//! rule (`crate::pump::idle`): the restart's entry closes the session
//! however late it fires, and a datagram makes the session's next
//! deadline a first window again. Without a field of its own the heap
//! tells the two apart: a session's first-window entry is exactly
//! `last_seen + idle`, a restart's is later (the stall's wake + idle),
//! and every entry earlier than `last_seen + idle` is stale (a datagram
//! superseded it — a new session under the same address included, its
//! `last_seen` being after the restart).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use gsb_core::conn::ConnIn;
use tracing::{debug, warn};

use crate::pump::IDLE_STALL_GRACE;
use crate::pump::idle::{count_restarts, stalled};

/// What a popped deadline entry is to its session.
enum Entry {
    /// Superseded by a datagram, or its session is gone: discarded.
    Stale,
    /// The session's first window since it was last heard.
    First,
    /// The window a stall restarted: it closes, late or not.
    Restarted,
}

impl super::Demux {
    /// The idle sweep (feature 4): pop overdue entries; a session silent
    /// for its whole window is closed (its actor told, best-effort, and
    /// removed) — unless the entry fired late, a stall (module docs).
    pub(super) fn sweep(&mut self) {
        self.sweep_at(Instant::now());
    }

    /// [`Self::sweep`] at `now`.
    fn sweep_at(&mut self, now: Instant) {
        let Some(idle) = self.idle else {
            return;
        };
        let mut restarted = 0;
        while let Some(&(deadline, peer)) = self.deadlines.first() {
            if deadline > now {
                break;
            }
            self.deadlines.remove(&(deadline, peer));
            let entry = match self.sessions.get(&peer) {
                Some(s) if s.last_seen + idle == deadline => Entry::First,
                Some(s) if s.last_seen + idle < deadline => Entry::Restarted,
                _ => Entry::Stale,
            };
            match entry {
                Entry::Stale => {}
                Entry::First if stalled(now.saturating_duration_since(deadline)) => {
                    self.deadlines.insert((now + idle, peer));
                    restarted += 1;
                }
                Entry::First | Entry::Restarted => self.close_idle(peer, idle),
            }
        }
        if restarted > 0 {
            count_restarts(self.metrics.clone(), restarted);
            warn!(
                sessions = restarted,
                grace = ?IDLE_STALL_GRACE,
                "rUDP: idle deadlines fired late (a stalled process); their windows restart"
            );
        }
    }

    /// Close `peer`'s session for its silence: notify the actor
    /// (best-effort) and remove it.
    fn close_idle(&mut self, peer: SocketAddr, idle: Duration) {
        let Some(session) = self.sessions.remove(&peer) else {
            return;
        };
        self.swept_idle += 1;
        let reason = format!("idle timeout: no client traffic for {idle:?}");
        match session.in_tx.try_send(ConnIn::ServerClosed {
            cause: gsb_core::conn::ServerClose::IdleTimeout,
            reason: reason.clone(),
        }) {
            Ok(()) => {
                // The actor will answer ERROR 9 and tear down (its
                // writer keeps sending until it exits; the session
                // is already un-routable, so stray inbound for this
                // peer is dropped).
                warn!(%peer, %reason, "rUDP: session idle-swept");
            }
            Err(_) => {
                // The actor is already gone: nothing to tell.
                self.removed_actor_gone += 1;
                debug!(%peer, "rUDP: idle sweep found an already-gone session");
            }
        }
    }
}

#[cfg(test)]
mod tests;
