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
    metrics: crate::TransportMetrics,
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
            // stall — a datagram `try_send_to` never parks. The band's
            // death verdict gets a mailbox slot reserved NOW, before the
            // task runs (B66; see `crate::pump::verdict`).
            let verdict = Some(crate::pump::verdict::Verdict::reserve(in_tx.clone(), true));
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
                    verdict,
                    deferred_verdict: None,
                    game_send_failed: 0,
                    control_send_failed: 0,
                    unsent: 0,
                    verdicts_deferred: 0,
                    flusher: crate::metrics::Flusher::new(metrics),
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
    /// The session's frames (game and control; not the demux's
    /// piggybacked ACKs — B73) taken off the channel after the session
    /// was over (never sent: see the loop).
    drained: u64,
    /// The band's death verdict: its mailbox slot, reserved at birth
    /// (taken by `die`), and — only when no slot could be reserved and
    /// the mailbox was full — the notice still to deliver after the
    /// outbound channel is closed.
    verdict: Option<crate::pump::verdict::Verdict>,
    deferred_verdict: Option<(Mailbox<ConnIn>, ConnIn)>,
    /// Datagrams the socket refused, by band (B66).
    game_send_failed: u64,
    control_send_failed: u64,
    /// Frames never sent because the band died (B66, `die`).
    unsent: u64,
    /// Death verdicts that could not use a reserved slot (B66).
    verdicts_deferred: u64,
    /// The loss counters' path to the collector (B58).
    flusher: crate::metrics::Flusher,
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
                    // and never put on the wire. Only the session's own
                    // frames are counted: the demux's piggybacked ACKs
                    // are not frames of the session (B73).
                    self.drained += send::session_frames(&batch);
                    self.flush_metrics(false);
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
            self.flush_metrics(false);
        }
        // What the channel still holds is never sent (B66): after the
        // band's death, the batches queued behind the fatal one; after an
        // ordinary end, nothing. Closed first, so a later send fails at
        // its sender (which counts it).
        self.drain_unsent();
        // Whatever ended the writer (the REL band died; every sender is
        // gone), the session has no writer any more: the demux may free it.
        self.signal_reap().await;
        self.flush_metrics(true);
        if let Some((in_tx, msg)) = self.deferred_verdict.take() {
            // No slot was reserved at birth and the mailbox was full: the
            // notice goes after the close (counted in `die`), last — it
            // may wait for the actor to make room.
            let _ = in_tx.send(msg).await;
        }
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

/// The loss counters on their way to the collector (BACKLOG B58). A
/// CHILD module too.
mod flush;

/// The send path: a batch frame by frame, one datagram, and what is
/// never sent (B66). A CHILD module too.
mod send;
