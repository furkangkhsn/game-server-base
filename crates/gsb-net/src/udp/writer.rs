//! The per-session outbound pump: one writer task per session (the
//! reader is the shared demux), carrying the reliable band's
//! retransmit state and the datagram budget.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    Box::new(move |conn: ConnectionId,
          _in_tx: Mailbox<ConnIn>,
          out_rx: Inbox<FrameBatch>,
          _idle: Option<Duration>| {
        // `in_tx` is already registered in the demux (at handshake) and
        // `idle` is its deadline heap's concern — neither belongs to the
        // per-connection part.
        let writer = tokio::spawn(UdpWriter {
            conn,
            sock,
            peer,
            out_rx,
            max_datagram,
            seq: 0,
            acked: 1,
            retransmit: VecDeque::new(),
            dropped_oversized: 0,
            retransmits: 0,
            gave_up: 0,
            oversized_warned: false,
        }
        .run());
        (None, writer)
    })
}

/// The per-session writer: outbound batches → datagrams, with the
/// reliable control band (retransmit + cumulative-ack state) and the
/// MTU drop+count rule (feature 3). Local state only.
pub(super) struct UdpWriter {
    conn: ConnectionId,
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    out_rx: Inbox<FrameBatch>,
    max_datagram: usize,
    /// Next outbound control seq (the client's first is 1, so the server
    /// hands out from 1 as well).
    seq: u32,
    /// Highest cumulative ACK received (the next expected seq).
    acked: u32,
    /// Un-ACKed outbound control frames: (seq, encoded datagram, sent_at).
    retransmit: VecDeque<(u32, Bytes, Instant)>,
    dropped_oversized: u64,
    retransmits: u64,
    gave_up: u64,
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
                Err(_) => None, // RTO: retransmit pass only
            };
            if let Some(batch) = batch {
                for frame in batch {
                    if frame.op == op::base::UDP_ACK {
                        // The demux's piggybacked inbound ACK (see
                        // `Demux::handle`): advance the cumulative state
                        // and prune the retransmit buffer.
                        let ok = frame.payload.len() >= 4;
                        if ok {
                            let ack =
                                u32::from_le_bytes(frame.payload[..4].try_into().unwrap());
                            self.acked = self.acked.max(ack);
                            while let Some(&(s, _, _)) = self.retransmit.front() {
                                if s < self.acked {
                                    self.retransmit.pop_front();
                                } else {
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    let control = is_control(frame.op);
                    let datagram = if control {
                        self.seq = self.seq.wrapping_add(1);
                        Bytes::from(encode_rel(self.seq, &frame))
                    } else {
                        Bytes::from(encode_raw(&frame))
                    };
                    // Feature 3: the datagram budget. Drop + count (the
                    // snapshot band is self-healing; the room-side
                    // max_snapshot_bytes warning is the standing signal).
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
                        // The socket is shut (listener close) or the peer
                        // is gone: keep draining the channel so the
                        // actor's exit cascade is not delayed; the next
                        // send keeps failing until the channel closes.
                        debug!(conn = %self.conn, peer = %self.peer, %e, "rUDP: send failed");
                    }
                }
            }
            self.retransmit_pass();
        }
        if self.dropped_oversized > 0 || self.retransmits > 0 || self.gave_up > 0 {
            info!(
                conn = %self.conn,
                peer = %self.peer,
                dropped_oversized = self.dropped_oversized,
                retransmits = self.retransmits,
                gave_up = self.gave_up,
                "rUDP writer session counters"
            );
        }
        debug!(conn = %self.conn, "rUDP writer stopped");
    }

    /// Retransmit the oldest un-ACKed control frame whose RTO has passed;
    /// give up (counted) on frames older than RETRANSIT_MAX.
    fn retransmit_pass(&mut self) {
        let now = Instant::now();
        while let Some((_, datagram, sent)) = self.retransmit.front_mut() {
            if *sent + RETRANSIT_MAX <= now {
                self.retransmit.pop_front();
                self.gave_up += 1;
                continue;
            }
            if *sent + RETRANSIT_RTO <= now {
                match self.sock.try_send_to(datagram, self.peer) {
                    Ok(_) => {
                        *sent = now;
                        self.retransmits += 1;
                    }
                    Err(_) => break, // socket busy/closed: retry next pass
                }
            } else {
                break;
            }
        }
    }
}
