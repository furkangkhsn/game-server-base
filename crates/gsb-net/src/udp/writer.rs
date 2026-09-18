//! The per-session outbound pump: one writer task per session (the
//! reader is the shared demux), carrying the reliable band's
//! retransmit state and the datagram budget.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::op;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::transport::PumpSpawner;
use crate::udp::*;

/// The per-session outbound pump spawner: ONLY a writer task (the reader
/// is the shared demux, owned by the listener).
pub(super) fn udp_pump_spawner(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    max_datagram: usize,
) -> PumpSpawner {
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
            let writer = tokio::spawn(
                UdpWriter {
                    conn,
                    sock,
                    peer,
                    in_tx,
                    out_rx,
                    max_datagram,
                    seq: 0,
                    acked: 1,
                    retransmit: VecDeque::new(),
                    ack_progress: Instant::now(),
                    dropped_oversized: 0,
                    retransmits: 0,
                    abandoned: 0,
                    oversized_warned: false,
                }
                .run(),
            );
            (None, writer)
        },
    )
}

/// The per-session writer: outbound batches → datagrams, with the
/// reliable control band (retransmit + cumulative-ack state + the
/// liveness bound) and the MTU drop+count rule (feature 3). Local state
/// only.
pub(super) struct UdpWriter {
    conn: ConnectionId,
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    /// The connection actor's mailbox. The writer never talks to the
    /// actor on the happy path; this is the teardown path of the REL
    /// liveness bound, and it is in-process on purpose — the socket is
    /// exactly what is in doubt when it is used.
    in_tx: Mailbox<ConnIn>,
    out_rx: Inbox<FrameBatch>,
    max_datagram: usize,
    /// Next outbound control seq (the client's first is 1, so the server
    /// hands out from 1 as well).
    seq: u32,
    /// Highest cumulative ACK received (the next expected seq).
    acked: u32,
    /// Un-ACKed outbound control frames: (seq, encoded datagram, sent_at).
    /// Bounded by [`RETRANSIT_CAP`].
    retransmit: VecDeque<(u32, Bytes, Instant)>,
    /// When the cumulative ACK last actually advanced — or, while nothing
    /// is outstanding, simply "now" (an idle band has nothing to prove).
    /// The liveness bound of the module docs is measured from here, NOT
    /// from an individual frame's age.
    ack_progress: Instant,
    dropped_oversized: u64,
    retransmits: u64,
    /// Control frames still outstanding when the band was declared dead
    /// (reported once, with the close).
    abandoned: u64,
    oversized_warned: bool,
}

impl UdpWriter {
    async fn run(mut self) {
        loop {
            // One awaited source: the outbound channel, optionally bounded
            // by the retransmit interval (the deadline fires only while
            // the recv stays pending — a ready batch always wins).
            let batch = match tokio::time::timeout(RETRANSIT_RTO, self.out_rx.recv()).await {
                Ok(Some(b)) => Some(b),
                Ok(None) => break, // the actor (and the room) are gone
                Err(_) => None,    // RTO: retransmit pass only
            };
            let mut fatal = None;
            if let Some(batch) = batch {
                fatal = self.send_batch(batch).await;
            }
            if fatal.is_none() {
                fatal = self.retransmit_pass();
            }
            if let Some(reason) = fatal {
                self.die(reason);
                break;
            }
        }
        if self.dropped_oversized > 0 || self.retransmits > 0 || self.abandoned > 0 {
            info!(
                conn = %self.conn,
                peer = %self.peer,
                dropped_oversized = self.dropped_oversized,
                retransmits = self.retransmits,
                abandoned = self.abandoned,
                "rUDP writer session counters"
            );
        }
        debug!(conn = %self.conn, "rUDP writer stopped");
    }

    /// Encode and send one outbound batch. Returns the fatal reason when
    /// the reliable band's memory bound ([`RETRANSIT_CAP`]) is crossed.
    async fn send_batch(&mut self, batch: FrameBatch) -> Option<String> {
        for frame in batch {
            if frame.op == op::base::UDP_ACK {
                // The demux's piggybacked inbound ACK (see `Demux::handle`).
                self.apply_ack(&frame);
                continue;
            }
            let control = is_control(frame.op);
            if control && self.retransmit.len() >= RETRANSIT_CAP {
                return Some(format!(
                    "rUDP reliable control band: {RETRANSIT_CAP} frames outstanding, \
                     the peer has confirmed none of them"
                ));
            }
            let datagram = if control {
                self.seq = self.seq.wrapping_add(1);
                Bytes::from(encode_rel(self.seq, &frame))
            } else {
                Bytes::from(encode_raw(&frame))
            };
            // Feature 3: the datagram budget. Drop + count (the snapshot
            // band is self-healing; the room-side max_snapshot_bytes
            // warning is the standing signal).
            if datagram.len() > self.max_datagram {
                self.dropped_oversized += 1;
                if !self.oversized_warned {
                    self.oversized_warned = true;
                    warn!(
                        conn = %self.conn,
                        peer = %self.peer,
                        op = frame.op,
                        size = datagram.len(),
                        budget = self.max_datagram,
                        "rUDP: frame exceeds the datagram budget; oversized \
                         frames are dropped and counted (split the snapshot \
                         group or lower its emission rate — the room-side \
                         max_snapshot_bytes warning is the signal)"
                    );
                }
                continue;
            }
            if control {
                self.retransmit
                    .push_back((self.seq, datagram.clone(), Instant::now()));
            }
            if let Err(e) = self.sock.send_to(&datagram, self.peer).await {
                // The socket is shut (listener close) or the peer is gone:
                // keep draining the channel so the actor's exit cascade is
                // not delayed; the next send keeps failing until the
                // channel closes.
                debug!(conn = %self.conn, peer = %self.peer, %e, "rUDP: send failed");
            }
        }
        None
    }
}

/// The reliable band's own half (ACK bookkeeping, the retransmit pass,
/// the liveness bound and the close it triggers). A CHILD module, so it
/// reaches this writer's private state directly.
mod reliable;
