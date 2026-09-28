//! What a server-decided end leaves unprocessed (BACKLOG B60): the
//! frames still in the inbox when the actor stops reading it, and the
//! frame that crossed the pre-auth budget — counted by kind. Behind the
//! server's stop, also the verdict the stop overtook (F56).

use crate::conn::*;
use crate::metrics::VerdictsLost;

impl super::ConnectionActor {
    /// One inbound frame this actor will never process, counted by its
    /// kind: an RPC request (a term of the RPC ledger), a game-band
    /// frame, or another base-band frame.
    pub(super) fn count_unprocessed(&mut self, op: u16) {
        let n = match FrameKind::of(op) {
            FrameKind::Request => &mut self.m_requests_unprocessed,
            FrameKind::Action => &mut self.m_actions_unprocessed,
            FrameKind::Control => &mut self.m_control_frames_unprocessed,
        };
        *n += 1;
    }

    /// The run loop is over: close the inbox (no sender can add to it
    /// from here on, so the drain below is bounded by its capacity) and
    /// count every frame still in it. Only a server-decided end can leave
    /// any — the reader's own `Closed` is its last message — but the
    /// drain runs on every end: it costs one `try_recv` when empty.
    ///
    /// The other messages are moot once the session is over — with one
    /// exception (F56): when the SERVER'S STOP ended it (`stopped`), a
    /// verdict behind the stop's notice (a room's kick or idle close
    /// relayed by the registry, a pump's, a destroyed room's) is one the
    /// session would have booked in `server_closes` and its client would
    /// have been told, and got the stop's `ERROR` 14 instead: a lost
    /// verdict, sent as `MetricsEvent::VerdictsLost`. Only the first — a
    /// session books one reason. Behind a verdict or a client's end a
    /// second verdict loses nothing: the session already had its end.
    pub(super) fn abandon_inbox(&mut self, stopped: bool) {
        self.inbox.close();
        let mut lost = VerdictsLost::default();
        while let Ok(msg) = self.inbox.try_recv() {
            if let ConnIn::Frame(frame) = &msg {
                self.count_unprocessed(frame.op);
                continue;
            }
            let Some(verdict) = msg.verdict() else {
                continue;
            };
            if stopped && lost.closes.total() == 0 {
                lost.close(verdict);
            }
        }
        crate::room::send_verdicts_lost(&self.metrics, &lost);
    }
}
