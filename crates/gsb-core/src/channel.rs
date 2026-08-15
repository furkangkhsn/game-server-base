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
