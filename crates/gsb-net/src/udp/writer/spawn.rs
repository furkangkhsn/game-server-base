//! The writer's birth: the pump spawner the demux hands each endpoint,
//! and the writer it builds (one place, so a test can build one too). A
//! CHILD of [`super`], so the writer's state stays private.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use tokio::net::UdpSocket;

use super::UdpWriter;
use crate::transport::PumpSpawner;
use crate::udp::*;

/// What a session's writer is spawned with besides its channels: the
/// shared socket, the peer, the datagram budget, the demux's reap pass,
/// the metrics channel and — on a sealed door — the session's record
/// sealer.
pub(in crate::udp) struct Link {
    pub(in crate::udp) sock: Arc<UdpSocket>,
    pub(in crate::udp) peer: SocketAddr,
    pub(in crate::udp) max_datagram: usize,
    pub(in crate::udp) reaper: Reaper,
    pub(in crate::udp) metrics: crate::TransportMetrics,
    /// The door's congestion response (module `crate::udp::congestion`).
    pub(in crate::udp) congestion: UdpCongestion,
    /// The session's server → client record sealer on a sealed door (B5a,
    /// module `crate::udp::sealed`), with its key-phase policy (B5b):
    /// every datagram this writer sends is sealed by it. `None`: a
    /// plaintext door.
    pub(in crate::udp) sealer: Option<crate::udp::sealed::SendHalf>,
}

/// The per-session outbound pump spawner: ONLY a writer task (the reader
/// is the shared demux, owned by the listener).
#[cfg(test)]
pub(in crate::udp) fn udp_pump_spawner(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    max_datagram: usize,
    reaper: Reaper,
    metrics: crate::TransportMetrics,
    congestion: UdpCongestion,
) -> PumpSpawner {
    udp_link_spawner(Link {
        sock,
        peer,
        max_datagram,
        reaper,
        metrics,
        congestion,
        sealer: None,
    })
}

/// [`udp_pump_spawner`] from a whole [`Link`] (the demux's: a sealed
/// session's writer gets its sealer).
pub(in crate::udp) fn udp_link_spawner(link: Link) -> PumpSpawner {
    Box::new(
        move |conn: ConnectionId,
              in_tx: Mailbox<ConnIn>,
              out_rx: Inbox<FrameBatch>,
              _timeouts: crate::pump::PumpTimeouts| {
            // `in_tx` is already registered in the demux (at handshake);
            // the copy handed here is the writer's ONE way to end the
            // session when the reliable band dies (see `die`). Neither
            // pump deadline applies here: inbound silence is the demux
            // deadline heap's concern, and this writer's own liveness
            // bound is the REL band's ACK-progress clock (see `reliable`),
            // which is the datagram equivalent of the stream pumps' write
            // stall — a datagram `try_send_to` never parks.
            let writer = tokio::spawn(UdpWriter::new(link, conn, in_tx, out_rx).run());
            (None, writer)
        },
    )
}

impl UdpWriter {
    /// A session's writer, not yet running. The band's death verdict
    /// gets a mailbox slot reserved NOW, before the task runs (B66; see
    /// `crate::pump::verdict`).
    pub(in crate::udp) fn new(
        link: Link,
        conn: ConnectionId,
        in_tx: Mailbox<ConnIn>,
        out_rx: Inbox<FrameBatch>,
    ) -> Self {
        let verdict = Some(crate::pump::verdict::Verdict::reserve(in_tx.clone(), true));
        // A sealed writer's budget is the INNER datagram's: the record's
        // header and tag ride on top, within the door's budget.
        let max_datagram = match link.sealer {
            Some(_) => link.max_datagram - crate::seal::wire::OVERHEAD_S2C,
            None => link.max_datagram,
        };
        Self {
            conn,
            sock: link.sock,
            peer: link.peer,
            in_tx,
            out_rx,
            max_datagram,
            seq: 0,
            // No sample yet: the server's handshake is stateless, so its
            // first sample is its first control frame's ACK (or, for a
            // reporting client, its first probe's).
            rel: RelSend::new(Instant::now(), Rto::default()),
            dropped_oversized: 0,
            frag_id: 0,
            frag_messages: 0,
            frag_datagrams: 0,
            retransmits: 0,
            abandoned: 0,
            oversized_warned: false,
            reaper: link.reaper,
            reap_signalled: false,
            drained: 0,
            verdict,
            deferred_verdict: None,
            game_send_failed: 0,
            control_send_failed: 0,
            unsent: 0,
            verdicts_deferred: 0,
            flusher: crate::metrics::Flusher::new(link.metrics),
            feedback: Feedback::new(Instant::now()),
            pace: super::pace::Pace::new(link.congestion, max_datagram, Instant::now()),
            path_changes: 0,
            path_resets: 0,
            sealer: link.sealer,
            seal_exhausted: false,
            ended_seal_limit: 0,
            sends_ack_failed: 0,
            sends_challenge_failed: 0,
        }
    }
}
