//! What a server-decided end leaves unprocessed (BACKLOG B60): the
//! frames still in the inbox when the actor stops reading it, and the
//! frame that crossed the pre-auth budget — counted by kind.

use crate::conn::*;

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
    /// Everything else in the inbox (a second verdict, a room notice) is
    /// moot once the session is over.
    pub(super) fn abandon_inbox(&mut self) {
        self.inbox.close();
        while let Ok(msg) = self.inbox.try_recv() {
            if let ConnIn::Frame(frame) = msg {
                self.count_unprocessed(frame.op);
            }
        }
    }
}
