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
    let _ = post_where(tx, msg);
}

/// Where [`post`] put a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    /// In the mailbox: ahead of anything sent to it later.
    InPlace,
    /// The mailbox was full: a spawned sender waits for a slot. It may
    /// land behind a later in-place message, or be refused when the
    /// receiver ends first — what a caller that cares does about that is
    /// the caller's (the registry's verdicts, BACKLOG F60).
    Spawned,
    /// The receiver was already gone: the message is dropped.
    Refused,
}

/// [`post`], telling where the message went.
pub fn post_where<T: Send + 'static>(tx: &Mailbox<T>, msg: T) -> Posted {
    match tx.try_send(msg) {
        Ok(()) => Posted::InPlace,
        Err(mpsc::error::TrySendError::Closed(_)) => Posted::Refused,
        Err(mpsc::error::TrySendError::Full(msg)) => {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(msg).await;
            });
            Posted::Spawned
        }
    }
}
