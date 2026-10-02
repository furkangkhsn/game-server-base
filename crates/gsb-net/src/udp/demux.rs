//! The shared read path. ONE task owns the socket and every session's
//! transport state; per-connection there is only a writer. The inbound
//! handlers live in child modules ([`inbound`], [`handshake`], [`reap`])
//! so they still reach this struct's private fields.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use tokio::net::UdpSocket;
use tracing::{info, warn};

use crate::udp::*;

mod dispatch;
mod flush;
mod handshake;
mod inbound;
mod migrate;
mod reap;
mod report;
mod session;
mod sweep;
mod table;
pub(super) use reap::Reaper;
pub(super) use session::UdpSession;
pub(super) use table::SessionKey;
use table::Sessions;
#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) struct Demux {
    sock: Arc<UdpSocket>,
    end_tx: Sender<Queued>,
    cookie: CookieKey,
    /// The cookie's time term (see [`CookieClock`]): started at bind, read
    /// at every handshake. No timer task, no shared state — the slot is
    /// recomputed from the clock on each use, which is the only shape a
    /// single-awaited actor can have.
    clock: CookieClock,
    inbox_cap: usize,
    outbox_cap: usize,
    max_datagram: usize,
    idle: Option<Duration>,
    /// The sessions, by key, address and CID (`table`, BACKLOG B3).
    sessions: Sessions,
    /// Whether the server grants connection ids (module
    /// `crate::udp::path`; the server's `udp_migration`, default off).
    migration: bool,
    /// Idle deadlines (feature 4): (deadline, session) with lazy
    /// invalidation — an entry is *current* only while it equals
    /// `session.last_seen + idle`; datagrams supersede it (a new entry
    /// is pushed); the sweep discards stale tops.
    deadlines: BTreeSet<(Instant, SessionKey)>,
    /// The reap pass (BACKLOG B6, [`reap`]): the handle cloned into each
    /// session's writer, and the queue of finished sessions it feeds.
    reaper: Reaper,
    reap_rx: Receiver<SessionKey>,
    buf: Vec<u8>,
    // lifetime counters (logged once at demux exit; the losses among them
    // also reach the collector while it runs — B58, `flush`):
    established: u64,
    challenges: u64,
    /// Valid proofs from an already-established peer, answered with the
    /// accept again (the first accept or proof copy was lost).
    proofs_reanswered: u64,
    bad_cookie: u64,
    endpoints_dropped: u64,
    /// Verified proofs whose session was torn down because the accept
    /// side was gone (B74).
    accept_gone: u64,
    swept_idle: u64,
    removed_actor_gone: u64,
    /// Sessions removed by the reap pass, and the wakes that drove it.
    reaped: u64,
    reap_wakes: u64,
    acks_piggybacked: u64,
    ack_piggyback_failed: u64,
    /// Game-band reports the writer's channel refused (full or closed).
    reports_not_forwarded: u64,
    oversized_in: u64,
    bad_datagrams: u64,
    /// Inbound FRAG datagrams (client→server fragmentation is refused).
    frag_refused: u64,
    /// Inbound frames dropped on a session's full inbox, by kind (B58;
    /// the per-session `inbox_full` keeps the total for the warning).
    full_requests: u64,
    full_actions: u64,
    full_controls: u64,
    /// Decoded frames for a session whose actor had closed its inbox, by
    /// kind (B66).
    closed_requests: u64,
    closed_actions: u64,
    closed_controls: u64,
    /// REL/RAW/ACK/REPORT datagrams from an address with no session
    /// (B66).
    no_session: u64,
    /// ACKs and challenges the socket refused (B66).
    acks_send_failed: u64,
    challenges_send_failed: u64,
    /// Connection migration's counters (module `migrate`).
    mig: migrate::Counts,
    /// The loss counters' path to the collector (B58), and the sender
    /// each session's writer gets for its own.
    flusher: crate::metrics::Flusher,
    metrics: crate::TransportMetrics,
    /// The writers' congestion response (module `crate::udp::congestion`).
    congestion: UdpCongestion,
}

impl Demux {
    /// A demux with no sessions on `sock` (its reap queue included).
    fn new(
        sock: Arc<UdpSocket>,
        end_tx: Sender<Queued>,
        cookie: CookieKey,
        inbox_cap: usize,
        outbox_cap: usize,
        max_datagram: usize,
        idle: Option<Duration>,
    ) -> Self {
        let (reaper, reap_rx) = Reaper::new(sock.clone());
        Self {
            sock,
            end_tx,
            cookie,
            clock: CookieClock::new(),
            inbox_cap,
            outbox_cap,
            max_datagram,
            idle,
            sessions: Sessions::default(),
            migration: false,
            deadlines: BTreeSet::new(),
            reaper,
            reap_rx,
            buf: vec![0u8; max_datagram + 64],
            established: 0,
            challenges: 0,
            proofs_reanswered: 0,
            bad_cookie: 0,
            endpoints_dropped: 0,
            accept_gone: 0,
            swept_idle: 0,
            removed_actor_gone: 0,
            reaped: 0,
            reap_wakes: 0,
            acks_piggybacked: 0,
            ack_piggyback_failed: 0,
            reports_not_forwarded: 0,
            oversized_in: 0,
            bad_datagrams: 0,
            frag_refused: 0,
            full_requests: 0,
            full_actions: 0,
            full_controls: 0,
            closed_requests: 0,
            closed_actions: 0,
            closed_controls: 0,
            no_session: 0,
            acks_send_failed: 0,
            challenges_send_failed: 0,
            mig: migrate::Counts::default(),
            flusher: crate::metrics::Flusher::new(None),
            metrics: None,
            congestion: UdpCongestion::Off,
        }
    }

    /// Remove a session: its CURRENT deadline entry (exact match; stale
    /// ones are lazily discarded by the sweep), both index entries, and
    /// its pending path validation's end in the ledger.
    fn remove_session(&mut self, key: SessionKey) -> Option<UdpSession> {
        let s = self.sessions.remove(key)?;
        if let Some(idle) = self.idle {
            self.deadlines.remove(&(s.last_seen + idle, key));
        }
        if let Some(p) = s.path {
            self.mig.validation_over(&p, Instant::now());
        }
        Some(s)
    }

    /// The session's idle window restarts: it was heard at `now`.
    fn heard(&mut self, key: SessionKey, now: Instant) {
        let idle = self.idle;
        if let Some(s) = self.sessions.get_mut(key) {
            s.last_seen = now;
            if let Some(idle) = idle {
                self.deadlines.insert((now + idle, key));
            }
        }
    }
}

/// The shared read path: one task, one awaited source (the socket read,
/// optionally wrapped in the min-idle-deadline — the same single-future
/// idiom as the TCP reader pump's idle timeout), one local state map.
pub(super) async fn demux(
    sock: Arc<UdpSocket>,
    end_tx: Sender<Queued>,
    cookie: CookieKey,
    cfg: UdpTransportConfig,
) {
    let mut d = Demux::new(
        sock,
        end_tx,
        cookie,
        cfg.inbox_capacity,
        cfg.outbox_capacity,
        cfg.max_datagram_bytes,
        cfg.idle_timeout,
    );
    d.flusher = crate::metrics::Flusher::new(cfg.metrics.clone());
    d.metrics = cfg.metrics;
    d.congestion = cfg.congestion;
    d.migration = cfg.migration;
    loop {
        // Arm the read: if any session has an idle deadline pending, the
        // read is bounded by the EARLIEST one (the deadline fires only
        // while the read stays pending; a ready datagram always wins —
        // this wraps one future, it does not multiplex two sources).
        let wait = d
            .idle
            .and_then(|_| d.deadlines.first().map(|(deadline, _)| *deadline))
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let item = match wait {
            Some(w) => match tokio::time::timeout(w, d.sock.recv_from(&mut d.buf)).await {
                Ok(Ok(x)) => Some(Ok(x)),
                Ok(Err(e)) => Some(Err(e)),
                Err(_) => None, // the earliest deadline passed: sweep
            },
            None => match d.sock.recv_from(&mut d.buf).await {
                Ok(x) => Some(Ok(x)),
                Err(e) => Some(Err(e)),
            },
        };
        // Every wake — a datagram, a writer's reap wake, the idle
        // deadline — first frees the sessions whose actors are gone, so a
        // datagram from a returning peer finds its slot already free.
        d.reap();
        match item {
            Some(Ok((_, peer))) if d.is_wake(peer) => d.reap_wakes += 1,
            Some(Ok((n, peer))) => d.handle(n, peer),
            Some(Err(e)) => {
                if e.kind() == std::io::ErrorKind::NotConnected {
                    break; // the listener closed the socket
                }
                warn!(%e, "rUDP demux: recv error; backing off");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            None => d.sweep(),
        }
        d.flush_metrics(false);
    }
    info!(
        established = d.established,
        challenges = d.challenges,
        proofs_reanswered = d.proofs_reanswered,
        bad_cookie = d.bad_cookie,
        endpoints_dropped = d.endpoints_dropped,
        accept_gone = d.accept_gone,
        swept_idle = d.swept_idle,
        removed_actor_gone = d.removed_actor_gone,
        reaped = d.reaped,
        reap_wakes = d.reap_wakes,
        acks_piggybacked = d.acks_piggybacked,
        ack_piggyback_failed = d.ack_piggyback_failed,
        oversized_in = d.oversized_in,
        bad_datagrams = d.bad_datagrams,
        frag_refused = d.frag_refused,
        migrations = d.mig.migrations,
        cid_unknown = d.mig.cid_unknown,
        "rUDP demux stopped"
    );
}
