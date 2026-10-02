//! Telling the door how the session ended (BACKLOG B30,
//! [`crate::conn::SessionEnd`]).

use crate::conn::{EndNotice, SessionEnd};

impl super::ConnectionActor {
    /// Tell `notice` how the session ended, once, at the end of
    /// [`Self::run`] — before the outbound sender drops, so a door that
    /// waits for the outbound channel to close finds the reason there
    /// (BACKLOG B30: the WebSocket door's close code).
    pub fn with_end_notice(mut self, notice: EndNotice) -> Self {
        self.end_notice = Some(notice);
        self
    }

    /// The run loop is over (`stopped`: the server's stop ended it): send
    /// the door its [`SessionEnd`], if it asked — the stop, else the
    /// verdict on record, else the client's end.
    pub(super) fn tell_end(&mut self, stopped: bool) {
        let Some(notice) = self.end_notice.take() else {
            return;
        };
        let end = match (stopped, self.server_close) {
            (true, _) => SessionEnd::Stopped,
            (false, Some(verdict)) => SessionEnd::Verdict(verdict),
            (false, None) => SessionEnd::Client,
        };
        let _ = notice.send(end);
    }
}
