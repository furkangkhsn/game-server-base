//! The server's teardown close (1001 "Going Away", B24) on its way into
//! the socket writer's queue (BACKLOG B80). A child of [`super`].
//!
//! The queue is shared by the pump's game frames and the reader's control
//! replies, and at the teardown it may well be FULL: the actor's last
//! batch is still going out to a slow reader. Before B80 the close was a
//! `try_send` there, dropped uncounted on a full queue — the client got
//! no close frame at all, and the connection lingered until it hung up
//! itself. Now the close WAITS for a slot, exactly as a game frame does:
//! `poll_close` stays pending, and the pump awaits its close under the
//! write-stall window, so a socket that drains delivers the close behind
//! the frames queued ahead of it (the same bytes as ever, only no longer
//! lost), and a wedged one gives up at the window — the bound the other
//! doors' close already has while it flushes their socket.
//!
//! What still cannot be delivered is counted in the transport scope:
//!
//! - `ws_going_away_unsent_closed`: the queue was closed — the socket
//!   writer had already stopped on a failed socket write (the only way
//!   it stops while this sender lives, other than the peer's close
//!   handshake below);
//! - `ws_going_away_unsent_stalled`: the close was dropped still waiting
//!   for a slot — the pump's stall window ran out with no byte written.
//!
//! Neither counts when the read path queued a close of its own
//! meanwhile (the echo of the client's close, a failure close; the
//! peer's close handshake is the other way the writer stops): at most one
//! close frame leaves a connection (RFC 6455 §5.5.1), and that one is the
//! reader's — its loss is the reader's `ws_close_frames_dropped`. The
//! shared `closing` flag is claimed only once a slot is held, so the
//! reader's close, queued first, always wins.

use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};

use gsb_core::metrics::TransportCounters;
use tokio_util::sync::PollSender;

use super::WsOut;
use crate::TransportMetrics;
use crate::metrics::Flusher;
use crate::ws::{CLOSE_GOING_AWAY, OP_CLOSE};

/// Where the teardown close stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// `poll_close` has not run.
    Idle,
    /// Claimed (no close of the reader's was queued) and waiting for a
    /// queue slot.
    Waiting,
    /// Queued, counted, or left to the reader's close.
    Done,
}

/// The teardown close of one connection (see the module docs).
#[derive(Debug)]
pub(super) struct Teardown {
    state: State,
    metrics: TransportMetrics,
}

impl Teardown {
    pub(super) fn new(metrics: TransportMetrics) -> Self {
        Self {
            state: State::Idle,
            metrics,
        }
    }

    /// Drive the close: pending while the queue is full, ready once the
    /// frame is queued — or known undeliverable, and counted.
    pub(super) fn poll(
        &mut self,
        tx: &mut PollSender<WsOut>,
        closing: &AtomicBool,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        match self.state {
            State::Done => return Poll::Ready(()),
            State::Idle if closing.load(Ordering::SeqCst) => {
                self.state = State::Done;
                return Poll::Ready(());
            }
            State::Idle | State::Waiting => self.state = State::Waiting,
        }
        let reserved = ready!(tx.poll_reserve(cx));
        self.state = State::Done;
        match reserved {
            // The reader queued its close while this one waited: that
            // one is the connection's close.
            Ok(()) if closing.swap(true, Ordering::SeqCst) => {
                tx.abort_send();
            }
            Ok(()) => {
                // A reserved slot: the send cannot be refused. The frame
                // is then the socket writer's to write or count.
                let close = CLOSE_GOING_AWAY.to_be_bytes().to_vec();
                let _ = tx.send_item(WsOut::Control(OP_CLOSE, close));
            }
            Err(_) if closing.load(Ordering::SeqCst) => {}
            Err(_) => self.count(TransportCounters {
                ws_going_away_unsent_closed: 1,
                ..Default::default()
            }),
        }
        Poll::Ready(())
    }

    /// The writer is going (its `Drop`): a close still waiting for a slot
    /// is abandoned — the pump gave up at its stall window.
    pub(super) fn abandon(&mut self, closing: &AtomicBool) {
        if self.state == State::Waiting && !closing.load(Ordering::SeqCst) {
            self.count(TransportCounters {
                ws_going_away_unsent_stalled: 1,
                ..Default::default()
            });
        }
        self.state = State::Done;
    }

    fn count(&self, lost: TransportCounters) {
        Flusher::new(self.metrics.clone()).flush(lost, true);
    }
}
