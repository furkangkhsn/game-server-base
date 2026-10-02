//! The per-session outbound pump: one writer task per session (the
//! reader is the shared demux), carrying the reliable band's
//! retransmit state and the datagram budget.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use tokio::net::UdpSocket;
use tracing::{debug, info};

use crate::udp::*;

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
    /// The reliable band's sending half: the un-ACKed frames (bounded by
    /// [`RETRANSIT_CAP`]), the cumulative ACK, the liveness clock and the
    /// retransmit timer (module docs, "Retransmit timer").
    rel: RelSend,
    /// Game-band frames over the fragment ceiling (dropped and counted).
    dropped_oversized: u64,
    /// The next FRAG message id (per session, wrapping).
    frag_id: u16,
    /// Game-band messages sent fragmented, and the FRAG datagrams they
    /// took.
    frag_messages: u64,
    frag_datagrams: u64,
    /// Control frames re-sent because their retransmit timer expired.
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
    /// The game band's feedback: the probes, the client's reports and the
    /// session's estimate (module `feedback`).
    feedback: Feedback,
    /// The congestion response: its controller and pacing queue (module
    /// `crate::udp::congestion`; inert unless the door's is on).
    pace: pace::Pace,
    /// Migrations applied (module `crate::udp::path`), and of those the
    /// ones to a new IP, which reset the path estimate (RFC 9000 §9.4).
    path_changes: u64,
    path_resets: u64,
    /// The record layer's send half on a sealed door (B5a, module
    /// `seal`; its key phases, B5b): every datagram to the peer is sealed
    /// by it. `None`: a plaintext door.
    sealer: Option<crate::udp::sealed::SendHalf>,
    /// The sealer's counter ran out (2^62): the session ends at the next
    /// turn of the loop (`udp_sessions_ended_seal_limit`).
    seal_exhausted: bool,
    ended_seal_limit: u64,
    /// The demux's `UDP_SEND` requests the socket refused, by what they
    /// carried: a cumulative ACK or a path challenge.
    sends_ack_failed: u64,
    sends_challenge_failed: u64,
}

impl UdpWriter {
    async fn run(mut self) {
        loop {
            // One awaited source: the outbound channel, bounded by the
            // oldest frame's retransmit timer (at most the housekeeping
            // tick; the deadline fires only while the recv stays pending
            // — a ready batch always wins), and by the pacer's next
            // release while the session is paced.
            let wait = self.wake(Instant::now());
            let batch = match tokio::time::timeout(wait, self.out_rx.recv()).await {
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
            if fatal.is_none() && !self.reap_signalled {
                self.pace_pass().await;
            }
            if fatal.is_none() {
                fatal = self.retransmit_pass();
            }
            if fatal.is_none() && !self.reap_signalled {
                self.probe_pass();
            }
            if fatal.is_none() && self.seal_exhausted {
                self.die_sealed();
                break;
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
        self.pace_abandon();
        self.feedback.end();
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
            || self.pace.queue.counts.queued > 0
            || self.path_changes > 0
        {
            info!(
                conn = %self.conn,
                peer = %self.peer,
                dropped_oversized = self.dropped_oversized,
                frag_messages = self.frag_messages,
                frag_datagrams = self.frag_datagrams,
                retransmits = self.retransmits,
                srtt_us = self.rel.rto().srtt().map(|d| d.as_micros() as u64),
                rto_ms = self.rel.rto().current().as_millis() as u64,
                abandoned = self.abandoned,
                drained = self.drained,
                game_reports = self.feedback.counts.reports,
                game_loss = self.game_estimate().map(|e| e.loss),
                game_min_rtt_us = self.game_estimate().map(|e| e.min_rtt.as_micros() as u64),
                path = ?self.path_state(),
                paced_queued = self.pace.queue.counts.queued,
                paced_dropped = self.pace.queue.counts.dropped,
                paced_unsent = self.pace.queue.counts.unsent,
                path_changes = self.path_changes,
                path_resets = self.path_resets,
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

/// The writer's birth: the spawner the demux hands each endpoint, and
/// the writer it builds. A CHILD module too.
mod spawn;
#[cfg(test)]
pub(super) use spawn::udp_pump_spawner;
pub(super) use spawn::{Link, udp_link_spawner};

/// The game band's feedback: the probe pass and the client's reports
/// (module `crate::udp::feedback`). A CHILD module too.
mod feedback;

/// The congestion response: the controller, the pacing queue and the
/// pass that releases it. A CHILD module too.
mod pace;

/// Connection migration, writer side: the path change the demux
/// announces (module `crate::udp::path`). A CHILD module too.
mod path;

/// The record layer, writer side (B5a, module `crate::udp::sealed`):
/// sealing every outgoing datagram, and the demux's `UDP_SEND`
/// requests. A CHILD module too.
mod seal;

#[cfg(test)]
mod tests;
