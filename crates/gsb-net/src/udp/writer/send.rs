//! The writer's send path: an outbound batch frame by frame (the
//! control band's fatal checks included), one datagram onto the socket,
//! and what the writer never sends (BACKLOG B66). A CHILD of [`super`],
//! so the writer's state stays private.

use std::time::Instant;

use bytes::Bytes;
use gsb_core::channel::FrameBatch;
use gsb_protocol::{FrameBody, op};
use tracing::debug;

use crate::udp::*;

impl super::UdpWriter {
    /// Encode and send one outbound batch. Returns the fatal reason when
    /// the reliable band cannot carry a control frame: its memory bound
    /// ([`RETRANSIT_CAP`]) is crossed, or the frame exceeds the datagram
    /// budget (the control band is never fragmented — module docs,
    /// "MTU (feature 3)").
    pub(super) async fn send_batch(&mut self, batch: FrameBatch) -> Option<String> {
        let mut frames = batch.into_iter();
        while let Some(frame) = frames.next() {
            let fatal = self.send_frame(frame).await;
            if fatal.is_some() {
                // The undeliverable frame and the rest of its batch are
                // never sent (B66).
                self.unsent += 1 + frames.filter(is_session_frame).count() as u64;
                return fatal;
            }
        }
        None
    }

    /// Send one frame of a batch; the fatal reason when the reliable band
    /// cannot carry it (see [`Self::send_batch`]).
    async fn send_frame(&mut self, frame: FrameBody) -> Option<String> {
        if frame.op == op::base::UDP_ACK {
            // The demux's piggybacked inbound ACK (see `Demux::handle`).
            self.apply_ack(&frame);
            return None;
        }
        if frame.op == op::base::UDP_REPORT {
            // The demux's piggybacked game-band report (`feedback`).
            self.apply_report(&frame);
            return None;
        }
        if frame.op == op::base::UDP_PATH {
            // The demux's notice that the session migrated (`path`).
            self.apply_path(&frame);
            return None;
        }
        if !is_control(frame.op) {
            // The game band: RAW, or FRAG when over the budget.
            self.send_game(&frame).await;
            return None;
        }
        if self.rel.len() >= RETRANSIT_CAP {
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
        self.rel.push(self.seq, datagram.clone(), Instant::now());
        self.send(&datagram, true).await;
        None
    }

    /// Put one datagram on the socket (`control`: the reliable band's).
    pub(super) async fn send(&mut self, datagram: &[u8], control: bool) {
        match self.sock.send_to(datagram, self.peer).await {
            // A game datagram on the wire: the feedback's sent count.
            Ok(_) if !control => self.feedback.game_sent(datagram.len()),
            // A control one: the paced game band yields its bytes.
            Ok(_) => self.pace_charge(datagram.len()),
            Err(e) => self.send_failed(&e, control),
        }
    }

    /// The socket is shut (listener close) or the peer is gone: keep
    /// draining the channel so the actor's exit cascade is not delayed;
    /// the next send keeps failing until the channel closes. Counted by
    /// band (B66): a game datagram is lost, a control one is
    /// retransmitted.
    fn send_failed(&mut self, e: &std::io::Error, control: bool) {
        match control {
            true => self.control_send_failed += 1,
            false => self.game_send_failed += 1,
        }
        debug!(conn = %self.conn, peer = %self.peer, %e, "rUDP: send failed");
    }

    /// Close the outbound channel and count what it still holds as never
    /// sent (the demux's piggybacked ACKs and reports are not frames of
    /// the session).
    pub(super) fn drain_unsent(&mut self) {
        self.out_rx.close();
        while let Ok(batch) = self.out_rx.try_recv() {
            self.unsent += session_frames(&batch);
        }
    }
}

/// Whether an outbound frame is one of the SESSION's (a game or control
/// frame the room or the connection sent), not the demux's piggybacked
/// inbound ACK, game-band report or path change — transport messages for
/// this writer, which the loss counters leave out (B66, B73).
pub(super) fn is_session_frame(frame: &FrameBody) -> bool {
    !matches!(
        frame.op,
        op::base::UDP_ACK | op::base::UDP_REPORT | op::base::UDP_PATH
    )
}

/// The session's frames in a batch (see [`is_session_frame`]).
pub(super) fn session_frames(batch: &FrameBatch) -> u64 {
    batch.iter().filter(|f| is_session_frame(f)).count() as u64
}
