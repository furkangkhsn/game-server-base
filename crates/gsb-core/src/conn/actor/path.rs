//! The actor's seat of the path signal (BACKLOG B103; the rule is
//! `crate::conn::path`): the transport's news in, the room's channel out.

use crate::path::PathState;

impl super::ConnectionActor {
    /// `ConnIn::Path`: the transport's newest state, handed on when it is
    /// news for the room this connection plays in.
    pub(super) fn on_path(&mut self, state: PathState) {
        self.path.offer(state);
        self.deliver_path();
    }

    /// Hand the owed state, if any, to the room (never waits; a full
    /// channel keeps it owed for the next message this actor reads). Not
    /// in a room: kept for the next join.
    pub(super) fn deliver_path(&mut self) {
        if let Some(actions) = &self.actions {
            let _ = crate::conn::path::deliver(&mut self.path, self.conn, actions);
        }
    }
}
