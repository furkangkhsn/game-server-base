//! How a session ended, told to the transport door (BACKLOG B30).
//!
//! The connection actor is the only place that knows why a session
//! ended; a door that speaks its own goodbye — the WebSocket close
//! frame's status code — needs that reason, and nothing else carried it
//! there (the door saw only the outbound channel closing). A door that
//! wants it hands the actor a [`EndNotice`] (a oneshot, through the
//! endpoint — `gsb_net::transport::Endpoint::take_end_notice`); the
//! actor sends its [`SessionEnd`] once, right after its run loop and
//! before it drops its outbound sender — so the reason is there before
//! the door can see the outbound channel close. Nothing on the wire
//! changes here: the `ERROR` frame ahead of the close is the actor's, as
//! ever; what the door does with the reason is the door's.

use tokio::sync::oneshot;

use crate::conn::ServerClose;

/// How a session ended (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// The client ended it: an EOF, a reset, a WebSocket close, a failed
    /// write. No server verdict.
    Client,
    /// The server's stop (`ConnIn::Shutdown`, `ERROR` 14). Not a verdict
    /// on the session (see [`ServerClose`]).
    Stopped,
    /// A server verdict — what the session booked in `server_closes`.
    Verdict(ServerClose),
}

/// Where the actor tells its [`SessionEnd`]: the sending half of a
/// oneshot whose receiver the door holds.
pub type EndNotice = oneshot::Sender<SessionEnd>;
