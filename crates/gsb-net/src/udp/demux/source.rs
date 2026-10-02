//! The per-source cap on the demux's pre-registration sessions (BACKLOG
//! B89, D11's rUDP half): one source holds at most `cap` sessions the
//! demux has established and the accept loop has not taken yet.
//!
//! - **Where state begins.** The cookie exchange holds nothing; a
//!   verified proof creates the session (and, after B5a, costs the DH).
//!   So the cap is checked there, AFTER the cookie: a refused proof
//!   creates nothing and gets no accept (the client re-sends it; it gets
//!   in once a place is free). Only a source that received its challenge
//!   can pass the cookie, so an off-path spoofer can neither take a place
//!   nor fill a victim's count — what is counted is return-routable.
//! - **What it counts.** A session from its verified proof until the
//!   accept loop takes its endpoint (or the endpoint is dropped): the
//!   [`Pending`] claim rides in the queued endpoint and names the session
//!   back as it drops. Then the registry's per-source cap on
//!   unauthenticated connections (D12) counts it — `ConnOpened` goes
//!   after the take, so never twice at once (D11's hand-off, the same).
//! - **No lock, bounded.** The tables live in the demux task. An entry
//!   exists only while a claim does; claims ride in queued endpoints
//!   (the endpoint channel's capacity, plus one per pending accept), and
//!   the release queue holds at most the claims dropped since the last
//!   decision — so neither table nor queue outgrows the endpoint channel,
//!   whatever the sources.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::net::IpAddr;

use crossbeam_channel::{Receiver, Sender};
use gsb_core::source::Source;
use tracing::{debug, warn};

use super::SessionKey;

/// A pending session's place in its source's count, carried by its
/// queued endpoint: dropped (taken by the accept loop, or the endpoint
/// dropped), it names the session back to the demux.
#[derive(Debug)]
pub(in crate::udp) struct Pending {
    key: SessionKey,
    release: Sender<SessionKey>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        // The demux gone: its tables went with it.
        let _ = self.release.send(self.key);
    }
}

/// A source's pending sessions, and whether its spell at the cap was
/// logged.
struct Held {
    pending: usize,
    warned: bool,
}

/// The demux's per-source table (module docs). `cap: None`: no table.
pub(super) struct PerSource {
    cap: Option<usize>,
    held: HashMap<Source, Held>,
    by_key: HashMap<SessionKey, Source>,
    release: Sender<SessionKey>,
    released: Receiver<SessionKey>,
    /// Verified proofs refused at the cap (no session, no accept).
    pub(super) refused: u64,
}

impl PerSource {
    /// `cap`: the server's `max_handshakes_per_source` (`None` or `0`:
    /// no cap — the door as it was).
    pub(super) fn new(cap: Option<usize>) -> Self {
        let (release, released) = crossbeam_channel::unbounded();
        Self {
            cap: cap.filter(|&n| n > 0),
            held: HashMap::new(),
            by_key: HashMap::new(),
            release,
            released,
            refused: 0,
        }
    }

    /// Give back every place released since the last decision.
    fn reclaim(&mut self) {
        while let Ok(key) = self.released.try_recv() {
            if let Some(source) = self.by_key.remove(&key) {
                self.give_back(source);
            }
        }
    }

    fn give_back(&mut self, source: Source) {
        if let Entry::Occupied(mut e) = self.held.entry(source) {
            e.get_mut().pending -= 1;
            if e.get().pending == 0 {
                e.remove();
            }
        }
    }

    /// Whether `source` holds its cap now.
    fn full(&self, cap: usize, source: Source) -> bool {
        self.held.get(&source).is_some_and(|h| h.pending >= cap)
    }

    /// A verified proof from `ip`: whether it may become a session (always,
    /// with no cap). Refused: counted, and logged once per spell.
    pub(super) fn admits(&mut self, ip: IpAddr) -> bool {
        let Some(cap) = self.cap else {
            return true;
        };
        self.reclaim();
        let source = Source::of(ip);
        if !self.full(cap, source) {
            return true;
        }
        self.refused += 1;
        if let Some(h) = self.held.get_mut(&source)
            && !std::mem::replace(&mut h.warned, true)
        {
            warn!(
                %source,
                cap,
                "rUDP: pending sessions from one source at the per-source cap; refusing its proofs"
            );
        }
        debug!(%source, "rUDP: proof refused at the per-source cap");
        false
    }

    /// The session `key` from `ip` was established: its place, carried by
    /// its queued endpoint (`None` with no cap).
    pub(super) fn claim(&mut self, key: SessionKey, ip: IpAddr) -> Option<Pending> {
        self.cap?;
        let source = Source::of(ip);
        self.held
            .entry(source)
            .or_insert(Held {
                pending: 0,
                warned: false,
            })
            .pending += 1;
        self.by_key.insert(key, source);
        Some(Pending {
            key,
            release: self.release.clone(),
        })
    }

    /// The pending sessions `ip`'s source holds (after reclaiming).
    #[cfg(test)]
    pub(super) fn pending_from(&mut self, ip: IpAddr) -> usize {
        self.reclaim();
        self.held.get(&Source::of(ip)).map_or(0, |h| h.pending)
    }

    /// Sources holding a place (after reclaiming).
    #[cfg(test)]
    pub(super) fn sources(&mut self) -> usize {
        self.reclaim();
        self.held.len()
    }
}

impl std::fmt::Debug for PerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerSource")
            .field("cap", &self.cap)
            .field("sources", &self.held.len())
            .field("pending", &self.by_key.len())
            .field("refused", &self.refused)
            .finish()
    }
}
