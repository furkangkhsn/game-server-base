//! The shared read path. ONE task owns the socket and every session's
//! transport state; per-connection there is only a writer. The inbound
//! handlers live in child modules ([`inbound`], [`handshake`]) so they
//! still reach this struct's private fields.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use gsb_core::channel::{FrameBatch, Mailbox};
use gsb_core::conn::ConnIn;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::transport::Endpoint;
use crate::udp::*;

mod handshake;
mod inbound;
#[cfg(test)]
mod tests;

/// One established session's transport state (all of it local to the
/// demux task — the demux is the actor; its map is its counter).
#[derive(Debug)]
pub(super) struct UdpSession {
    in_tx: Mailbox<ConnIn>,
    /// Kept for ACK piggyback (the demux hands inbound ACKs to the
    /// session's writer through the outbound channel).
    out_tx: Mailbox<FrameBatch>,
    last_seen: Instant,
    /// Inbound reliable state (client→server): the next expected seq,
    /// plus the small out-of-order window awaiting the gap.
    in_expected: u32,
    in_oob: HashMap<u32, Vec<u8>>,
    oob_dropped: u64,
    dup_in: u64,
    /// Inbound frames dropped on a full (bounded) session mailbox.
    inbox_full: u64,
    inbox_full_warned: bool,
}

#[derive(Debug)]
pub(super) struct Demux {
    sock: Arc<UdpSocket>,
    end_tx: Sender<Endpoint>,
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
    sessions: HashMap<SocketAddr, UdpSession>,
    /// Idle deadlines (feature 4): (deadline, peer) with lazy
    /// invalidation — an entry is *current* only while it equals
    /// `session.last_seen + idle`; datagrams supersede it (a new entry
    /// is pushed); the sweep discards stale tops.
    deadlines: BTreeSet<(Instant, SocketAddr)>,
    buf: Vec<u8>,
    // lifetime counters (reported once at demux exit):
    established: u64,
    challenges: u64,
    bad_cookie: u64,
    endpoints_dropped: u64,
    swept_idle: u64,
    removed_actor_gone: u64,
    acks_piggybacked: u64,
    ack_piggyback_failed: u64,
    oversized_in: u64,
    bad_datagrams: u64,
}

impl Demux {
    fn remove_session(&mut self, peer: SocketAddr) {
        // Drop the CURRENT deadline entry (exact match); stale ones are
        // lazily discarded by the sweep.
        if let Some(idle) = self.idle
            && let Some(s) = self.sessions.get(&peer)
        {
            self.deadlines.remove(&(s.last_seen + idle, peer));
        }
        self.sessions.remove(&peer);
    }
    /// The idle sweep (feature 4): pop overdue entries; a LIVE one
    /// (still equal to `last_seen + idle`) means the session has been
    /// silent for the whole window: notify the actor (best-effort) and
    /// remove it. Stale entries (superseded by a datagram) are discarded.
    fn sweep(&mut self) {
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

/// The shared read path: one task, one awaited source (the socket read,
/// optionally wrapped in the min-idle-deadline — the same single-future
/// idiom as the TCP reader pump's idle timeout), one local state map.
pub(super) async fn demux(
    sock: Arc<UdpSocket>,
    end_tx: Sender<Endpoint>,
    cookie: CookieKey,
    inbox_cap: usize,
    outbox_cap: usize,
    max_datagram: usize,
    idle: Option<Duration>,
) {
    let mut d = Demux {
        sock,
        end_tx,
        cookie,
        clock: CookieClock::new(),
        inbox_cap,
        outbox_cap,
        max_datagram,
        idle,
        sessions: HashMap::new(),
        deadlines: BTreeSet::new(),
        buf: vec![0u8; max_datagram + 64],
        established: 0,
        challenges: 0,
        bad_cookie: 0,
        endpoints_dropped: 0,
        swept_idle: 0,
        removed_actor_gone: 0,
        acks_piggybacked: 0,
        ack_piggyback_failed: 0,
        oversized_in: 0,
        bad_datagrams: 0,
    };
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
        match item {
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
    }
    info!(
        established = d.established,
        challenges = d.challenges,
        bad_cookie = d.bad_cookie,
        endpoints_dropped = d.endpoints_dropped,
        swept_idle = d.swept_idle,
        removed_actor_gone = d.removed_actor_gone,
        acks_piggybacked = d.acks_piggybacked,
        ack_piggyback_failed = d.ack_piggyback_failed,
        oversized_in = d.oversized_in,
        bad_datagrams = d.bad_datagrams,
        "rUDP demux stopped"
    );
}
