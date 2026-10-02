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
/// shared socket, the peer, the datagram budget, the demux's reap pass
/// and the metrics channel.
pub(in crate::udp) struct Link {
    pub(in crate::udp) sock: Arc<UdpSocket>,
    pub(in crate::udp) peer: SocketAddr,
    pub(in crate::udp) max_datagram: usize,
    pub(in crate::udp) reaper: Reaper,
    pub(in crate::udp) metrics: crate::TransportMetrics,
}

/// The per-session outbound pump spawner: ONLY a writer task (the reader
/// is the shared demux, owned by the listener).
pub(in crate::udp) fn udp_pump_spawner(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    max_datagram: usize,
    reaper: Reaper,
    metrics: crate::TransportMetrics,
) -> PumpSpawner {
    let link = Link {
        sock,
        peer,
        max_datagram,
        reaper,
        metrics,
    };
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
        Self {
            conn,
            sock: link.sock,
            peer: link.peer,
            in_tx,
            out_rx,
            max_datagram: link.max_datagram,
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
        }
    }
}
