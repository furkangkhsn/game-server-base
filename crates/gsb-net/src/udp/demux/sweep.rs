//! The idle sweep (feature 4): the demux's deadline heap, popped when
//! the socket read's deadline fires. A child of [`super`], so the
//! demux's session table stays private to its own module tree.

use std::time::Instant;

use gsb_core::conn::ConnIn;
use tracing::{debug, warn};

impl super::Demux {
    /// The idle sweep (feature 4): pop overdue entries; a LIVE one
    /// (still equal to `last_seen + idle`) means the session has been
    /// silent for the whole window: notify the actor (best-effort) and
    /// remove it. Stale entries (superseded by a datagram) are discarded.
    pub(super) fn sweep(&mut self) {
        let Some(idle) = self.idle else {
            return;
        };
        let now = Instant::now();
        while let Some(&(deadline, peer)) = self.deadlines.first() {
            if deadline > now {
                break;
            }
            self.deadlines.remove(&(deadline, peer));
            let live = self
                .sessions
                .get(&peer)
                .map(|s| s.last_seen + idle == deadline)
                .unwrap_or(false);
            if !live {
                continue; // stale entry
            }
            let Some(session) = self.sessions.remove(&peer) else {
                continue;
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
}
