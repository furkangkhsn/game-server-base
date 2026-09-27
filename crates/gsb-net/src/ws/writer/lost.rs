//! What the WebSocket socket writer never puts on the wire, counted
//! (BACKLOG B66) into the transport scope. A child of [`super`].
//!
//! The door's single socket-writer task sits one queue BELOW the writer
//! pump: a frame the pump handed over (its sink completes when the queue
//! takes it) is still only queued here. Three ways one never reaches the
//! wire:
//!
//! - a socket write failed: the frame being written and everything
//!   still queued — game frames join the stream doors'
//!   `stream_frames_unwritten` (the same loss one queue lower), control
//!   frames (pongs, closes) are `ws_control_frames_unwritten`;
//! - a close frame has already gone out (RFC 6455 §5.5.1: no data after
//!   a close): game frames behind it are `ws_frames_dropped_after_close`
//!   (control frames there — a second close, a late pong — are the
//!   protocol's rule, not a loss, and stay uncounted);
//! - the peer's close handshake shut the socket down (`WsOut::Shutdown`)
//!   with frames still queued behind it: counted by the same two rules.

use gsb_core::metrics::TransportCounters;
use tokio::sync::mpsc;

use super::WsOut;
use crate::TransportMetrics;
use crate::metrics::Flusher;

/// The socket writer's losses (see the module docs).
#[derive(Debug, Default)]
pub(super) struct Lost {
    game_unwritten: u64,
    control_unwritten: u64,
    after_close: u64,
}

impl Lost {
    /// A game frame skipped because a close frame went out before it.
    pub(super) fn after_close(&mut self) {
        self.after_close += 1;
    }

    /// The frame whose socket write failed.
    pub(super) fn failed(&mut self, game: bool) {
        match game {
            true => self.game_unwritten += 1,
            false => self.control_unwritten += 1,
        }
    }

    /// The writer is leaving the loop early (a failed write, or the
    /// shutdown request): close the queue and count what it holds.
    /// `close_sent`: a close frame is already on the wire, so the game
    /// frames behind it were never to be written.
    pub(super) fn drain(&mut self, rx: &mut mpsc::Receiver<WsOut>, close_sent: bool) {
        rx.close();
        while let Ok(out) = rx.try_recv() {
            match out {
                WsOut::Game(_) if close_sent => self.after_close += 1,
                WsOut::Game(_) => self.game_unwritten += 1,
                WsOut::Control(..) if close_sent => {}
                WsOut::Control(..) => self.control_unwritten += 1,
                WsOut::Shutdown => {}
            }
        }
    }

    /// Send the count (nothing when nothing was lost).
    pub(super) fn report(self, metrics: TransportMetrics) {
        Flusher::new(metrics).flush(
            TransportCounters {
                stream_frames_unwritten: self.game_unwritten,
                ws_control_frames_unwritten: self.control_unwritten,
                ws_frames_dropped_after_close: self.after_close,
                ..Default::default()
            },
            true,
        );
    }
}
