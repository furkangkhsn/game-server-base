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
use tracing::{debug, info};

use crate::transport::PumpSpawner;
use crate::udp::*;

/// The per-session outbound pump spawner: ONLY a writer task (the reader
/// is the shared demux, owned by the listener).
pub(super) fn udp_pump_spawner(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    max_datagram: usize,
    reaper: Reaper,
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
                    frag_id: 0,
                    frag_messages: 0,
                    frag_datagrams: 0,
                    retransmits: 0,
                    abandoned: 0,
                    oversized_warned: false,
                    reaper,
                    reap_signalled: false,
                    drained: 0,
                }
                .run(),
            );
            (None, writer)
        },
    )
}

/// The per-session writer: outbound batches → datagrams, with the
/// reliable control band (retransmit + cumulative-ack state + the
/// liveness bound) and the game band's fragmentation (feature 3). Local
/// state only.
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
    /// Game-band frames over the fragment ceiling (dropped and counted).
    dropped_oversized: u64,
    /// The next FRAG message id (per session, wrapping).
    frag_id: u16,
    /// Game-band messages sent fragmented, and the FRAG datagrams they
    /// took.
    frag_messages: u64,
    frag_datagrams: u64,
    retransmits: u64,
    /// Control frames still outstanding when the band was declared dead
    /// (reported once, with the close).
    abandoned: u64,
    oversized_warned: bool,
    /// The demux's reap pass (BACKLOG B6): told once, when this session
    /// is over — see [`Self::session_over`].
    reaper: Reaper,
    reap_signalled: bool,
    /// Frames taken off the channel after the session was over (never
    /// sent: see the loop).
    drained: u64,
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
                if self.reap_signalled {
                    // The session is over and its address may already
                    // carry a NEW session: what the room still sends
                    // until it processes the detach is taken off the
                    // channel (so the room sees no dead outbound path)
                    // and never put on the wire.
                    self.drained += batch.len() as u64;
                    continue;
                }
                fatal = self.send_batch(batch).await;
            }
            if fatal.is_none() {
                fatal = self.retransmit_pass();
            }
            if let Some(reason) = fatal {
                self.die(reason);
                break;
            }
            if self.session_over() {
                self.signal_reap().await;
            }
        }
        // Whatever ended the writer (the REL band died; every sender is
        // gone), the session has no writer any more: the demux may free it.
        self.signal_reap().await;
        if self.dropped_oversized > 0
            || self.frag_messages > 0
            || self.retransmits > 0
            || self.abandoned > 0
            || self.drained > 0
        {
            info!(
                conn = %self.conn,
                peer = %self.peer,
                dropped_oversized = self.dropped_oversized,
                frag_messages = self.frag_messages,
                frag_datagrams = self.frag_datagrams,
                retransmits = self.retransmits,
                abandoned = self.abandoned,
                drained = self.drained,
                "rUDP writer session counters"
            );
        }
        debug!(conn = %self.conn, "rUDP writer stopped");
    }

    /// Encode and send one outbound batch. Returns the fatal reason when
    /// the reliable band cannot carry a control frame: its memory bound
    /// ([`RETRANSIT_CAP`]) is crossed, or the frame exceeds the datagram
    /// budget (the control band is never fragmented — module docs,
    /// "MTU (feature 3)").
    async fn send_batch(&mut self, batch: FrameBatch) -> Option<String> {
        for frame in batch {
            if frame.op == op::base::UDP_ACK {
                // The demux's piggybacked inbound ACK (see `Demux::handle`).
                self.apply_ack(&frame);
                continue;
            }
            if !is_control(frame.op) {
                // The game band: RAW, or FRAG when over the budget.
                self.send_game(&frame).await;
                continue;
            }
            if self.retransmit.len() >= RETRANSIT_CAP {
                return Some(format!(
                    "rUDP reliable control band: {RETRANSIT_CAP} frames outstanding, \
                     the peer has confirmed none of them"
                ));
            }
            // Checked BEFORE a seq is spent: a control frame that cannot
            // ride one datagram is undeliverable, and dropping it after
            // taking its seq would wedge the peer's cumulative stream.
            let size = 5 + 2 + frame.payload.len();
            if size > self.max_datagram {
                return Some(format!(
                    "rUDP reliable control band: op {} needs a {size}-byte datagram, \
                     over the {}-byte budget (control frames are never fragmented)",
                    frame.op, self.max_datagram
                ));
            }
            self.seq = self.seq.wrapping_add(1);
            let datagram = Bytes::from(encode_rel(self.seq, &frame));
            self.retransmit
                .push_back((self.seq, datagram.clone(), Instant::now()));
            self.send(&datagram).await;
        }
        None
    }

    /// Put one datagram on the socket.
    async fn send(&self, datagram: &[u8]) {
        if let Err(e) = self.sock.send_to(datagram, self.peer).await {
            // The socket is shut (listener close) or the peer is gone:
            // keep draining the channel so the actor's exit cascade is
            // not delayed; the next send keeps failing until the channel
            // closes.
            debug!(conn = %self.conn, peer = %self.peer, %e, "rUDP: send failed");
        }
    }
}

/// The reliable band's own half (ACK bookkeeping, the retransmit pass,
/// the liveness bound and the close it triggers). A CHILD module, so it
/// reaches this writer's private state directly.
mod reliable;

/// The game band's half: RAW, or FRAG for a frame over the budget (and
/// the drop+count rule past the fragment ceiling). A CHILD module too.
mod split;

/// The session's end: when the demux may free it, and the signal
/// (BACKLOG B6). A CHILD module too.
mod end;
