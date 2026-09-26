//! The door every in-tree listener closes (BACKLOG B16): `close` ends
//! the pending `accept` — and every later one — with a recognizable
//! "listener closed" error, so an accept loop ENDS on it instead of being
//! aborted from outside.
//!
//! The accept loop still awaits one thing, `accept()`. The door is part
//! of that one future, the way a deadline is part of `timeout(d, read)`
//! in the pump idiom: it can only END the pending accept (with the
//! closed error), never hand the loop a second stream of work. What it
//! ends is at most a connection that was not yet a session — the socket
//! accept itself, or a TLS/WebSocket/QUIC handshake still in flight (in
//! its own task since B31: `super::intake` runs every such task under
//! this door) — which is exactly what a closed door refuses; the live
//! sessions are not the door's (they end through the actor cascade,
//! `Listener::close`).

use std::future::Future;
use std::io;

use tokio_util::sync::CancellationToken;

/// A listener's door: open until [`Self::close`], then every
/// [`Self::admit`] — the pending one included — ends with
/// [`listener_closed`].
#[derive(Debug, Default)]
pub struct Door(CancellationToken);

impl Door {
    /// An open door.
    pub fn new() -> Self {
        Self::default()
    }

    /// Close it: the pending accept (if any) and every later one end
    /// with [`listener_closed`]. Idempotent, never waits.
    pub fn close(&self) {
        self.0.cancel();
    }

    /// Whether [`Self::close`] ran.
    pub fn is_closed(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Run one accept (the socket accept and whatever handshake the door
    /// adds) unless the door is closed first, in which case the accept
    /// is dropped — its half-open connection with it — and the answer is
    /// [`listener_closed`]. A connection that completes in the same poll
    /// as the close is still returned (the next accept then reports the
    /// close).
    pub async fn admit<T>(&self, accept: impl Future<Output = io::Result<T>>) -> io::Result<T> {
        match self.0.run_until_cancelled(accept).await {
            Some(result) => result,
            None => Err(listener_closed()),
        }
    }
}

/// The marker inside a [`listener_closed`] error.
#[derive(Debug)]
struct ListenerClosed;

impl std::fmt::Display for ListenerClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("listener closed")
    }
}

impl std::error::Error for ListenerClosed {}

/// The error an `accept` returns once its listener is closed: the one
/// an accept loop ends on (see [`is_listener_closed`]).
pub fn listener_closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, ListenerClosed)
}

/// Whether `e` is [`listener_closed`] (by its marker, not its kind — a
/// real socket error of the same kind is not a close).
pub fn is_listener_closed(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.is::<ListenerClosed>())
}

#[cfg(test)]
pub(crate) mod tests;
