//! Prompt removal of a session whose connection actor is gone (BACKLOG
//! B6). The demux awaits exactly one thing — its socket read — so it
//! cannot also wait for "an actor died". Nothing new is awaited here:
//!
//! - the session's WRITER notices (it already wakes every RTO, and it
//!   holds a clone of the actor's mailbox): once the mailbox is closed
//!   and its own reliable band has nothing outstanding — the actor's
//!   final notice is delivered and ACKed, or the REL liveness bound gave
//!   up on it — it hands the session's key to the demux over a bounded
//!   in-process queue ([`Reaper::signal`]) — its key, not its address,
//!   which a migration may change (BACKLOG B3);
//! - and it wakes the demux through the one thing the demux awaits: a
//!   one-byte datagram to the demux's own socket, from that same socket.
//!   The datagram carries nothing — the demux knows it by its source
//!   (its own address, [`wake_addr`]) and drops it;
//! - the demux drains the queue on EVERY wake (a datagram or the idle
//!   deadline), before it handles the datagram — so a lost wake costs
//!   nothing on a busy socket, and a returning client's handshake from
//!   the same address finds its slot already free.
//!
//! The queue grants no authority: the demux removes a named session
//! only if it is really dead — its actor's mailbox or its writer's
//! channel is closed — so a stale signal or a forged wake does nothing
//! but an O(1) check (a key is never reused, so a signal can name no
//! session but its own).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use tokio::net::UdpSocket;
use tracing::debug;

use super::SessionKey;

/// Sessions queued for the reap pass, at most this many at once (each
/// is a session whose writer finished; a full queue falls back to the
/// older nets — the next datagram for the session or the idle sweep).
pub(in crate::udp) const REAP_QUEUE: usize = 1024;

/// The wake datagram's one byte (its content is never read: the demux
/// recognizes a wake by its source address alone).
const WAKE: [u8; 1] = [0xFF];

/// The demux's own address as its writers reach it: the bound address,
/// with an unspecified IP replaced by the loopback of the same family (a
/// datagram to it comes back FROM it, so the demux can tell a wake from
/// a peer by the source).
pub(in crate::udp) fn wake_addr(bound: SocketAddr) -> SocketAddr {
    let ip = match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, bound.port())
}

/// A writer's handle on the reap pass: the queue's sender, the socket,
/// where the wake goes, and the session it speaks for. Cloned into each
/// session's writer ([`Reaper::session`]).
#[derive(Debug, Clone)]
pub(in crate::udp) struct Reaper {
    queue: Sender<SessionKey>,
    sock: Arc<UdpSocket>,
    wake: SocketAddr,
    /// The session this handle signals for (`SessionKey(0)`, which names
    /// no session, on the demux's own handle).
    key: SessionKey,
}

impl Reaper {
    /// A reaper for the demux on `sock` and the queue's receiving end.
    pub(in crate::udp) fn new(sock: Arc<UdpSocket>) -> (Self, Receiver<SessionKey>) {
        let (queue, rx) = crossbeam_channel::bounded(REAP_QUEUE);
        let wake = sock
            .local_addr()
            .map(wake_addr)
            .unwrap_or_else(|_| SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
        let key = SessionKey(0);
        (
            Self {
                queue,
                sock,
                wake,
                key,
            },
            rx,
        )
    }

    /// The handle for session `key`'s writer.
    pub(in crate::udp) fn session(&self, key: SessionKey) -> Self {
        Self {
            key,
            ..self.clone()
        }
    }

    /// Where the wakes go (the demux's own address, as the writers reach it).
    pub(in crate::udp) fn wake(&self) -> SocketAddr {
        self.wake
    }

    /// Hand this handle's session to the demux's reap pass and wake it.
    /// The queue is never waited on: a full one leaves the session to the
    /// older nets (returns `false` so the caller can say so). The wake is
    /// one datagram on the writer's own socket, sent like its others.
    pub(in crate::udp) async fn signal(&self) -> bool {
        let key = self.key;
        match self.queue.try_send(key) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => return false,
        }
        if let Err(e) = self.sock.send_to(&WAKE, self.wake).await {
            // The queue still holds the key: the demux's next wake
            // of any kind drains it.
            debug!(?key, %e, "rUDP: reap wake not sent (queued for the next wake)");
        }
        true
    }
}

impl super::Demux {
    /// The reap pass: remove every queued session that is really dead
    /// (its actor's mailbox or its writer's channel closed). Bounded by
    /// the queue's size; O(1) per key.
    pub(super) fn reap(&mut self) {
        while let Ok(key) = self.reap_rx.try_recv() {
            let dead = self
                .sessions
                .get(key)
                .is_some_and(|s| s.in_tx.is_closed() || s.out_tx.is_closed());
            if dead {
                self.reaped += 1;
                if let Some(s) = self.remove_session(key) {
                    debug!(peer = %s.addr, "rUDP: session reaped (its actor is gone)");
                }
            }
        }
    }

    /// Whether a datagram from `peer` is a writer's wake (from this
    /// socket's own address — never a peer's).
    pub(super) fn is_wake(&self, peer: SocketAddr) -> bool {
        peer == self.reaper.wake()
    }
}
