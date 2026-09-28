//! Bounded channel aliases.
//!
//! Everything between actors flows through bounded `mpsc` channels. Bounded
//! capacity is the backpressure mechanism: a slow consumer makes the sender
//! park (or, for fire-and-forget fan-out, drop via `try_send`).

use tokio::sync::mpsc;

/// One batch of frames destined for a single connection in one tick. The
/// room's broadcast phase produces at most one batch per connection per
/// tick; the connection's writer pump consumes them and writes to the
/// socket.
pub type FrameBatch = Vec<gsb_protocol::FrameBody>;

/// Sender half of an actor mailbox. Cloneable; cheap to hand to other actors.
pub type Mailbox<T> = mpsc::Sender<T>;

/// Receiver half of an actor mailbox. Owned by exactly one actor.
pub type Inbox<T> = mpsc::Receiver<T>;

/// Create a bounded channel.
#[inline]
pub fn channel<T>(capacity: usize) -> (Mailbox<T>, Inbox<T>) {
    mpsc::channel(capacity.max(1))
}

/// Deliver `msg` without awaiting: in place when the mailbox has room,
/// from a spawned sender when it is full, not at all when the receiver is
/// already gone. The stop-message idiom (DESIGN §9.1): a stop path must
/// never park its caller on another actor's mailbox, and must not lose
/// the message while that actor is still draining. Must be called inside
/// a Tokio runtime (the full-mailbox fallback spawns).
pub fn post<T: Send + 'static>(tx: &Mailbox<T>, msg: T) {
    post_or(tx, msg, drop);
}

/// [`post`], told of the one way it loses a message: `refused` gets the
/// message back when the receiver refused it — at once when it is
/// already gone, or from the spawned sender when it closed before a
/// slot freed (BACKLOG F58: the registry's verdict to a connection whose
/// inbox was full). Nothing else changes: a message the mailbox takes is
/// never handed back.
pub fn post_or<T, F>(tx: &Mailbox<T>, msg: T, refused: F)
where
    T: Send + 'static,
    F: FnOnce(T) + Send + 'static,
{
    match tx.try_send(msg) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Closed(msg)) => refused(msg),
        Err(mpsc::error::TrySendError::Full(msg)) => {
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(mpsc::error::SendError(msg)) = tx.send(msg).await {
                    refused(msg);
                }
            });
        }
    }
}
