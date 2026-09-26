//! The end of a session, as the writer sees it (BACKLOG B6): when the
//! transport may let the session go, and the one signal that tells the
//! demux so. A CHILD of [`super`], so the writer's state stays private.

use tracing::debug;

impl super::UdpWriter {
    /// The session is over for the transport: the connection actor has
    /// exited (its mailbox is closed) and the reliable band owes nothing
    /// — the actor's final notice, if any, is delivered and ACKed. The
    /// demux may then free the session's address; this writer keeps
    /// running until its channel closes (the room still holds a sender
    /// until it has processed the detach), so nothing it is sent fails.
    pub(super) fn session_over(&self) -> bool {
        !self.reap_signalled && self.in_tx.is_closed() && self.retransmit.is_empty()
    }

    /// Tell the demux, once, that this session can go (the demux's reap pass).
    pub(super) async fn signal_reap(&mut self) {
        if self.reap_signalled {
            return;
        }
        self.reap_signalled = true;
        if !self.reaper.signal(self.peer).await {
            debug!(conn = %self.conn, peer = %self.peer, "rUDP: reap queue full; the idle sweep frees the session");
        }
    }
}
