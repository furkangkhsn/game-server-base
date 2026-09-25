//! `post_stop`: never parks the caller, and never loses the message while
//! the receiver lives.

use std::time::Duration;

use super::post_stop;
use crate::channel::channel;

const WAIT: Duration = Duration::from_secs(5);

/// A mailbox with room takes the message in place (no task, no await).
#[tokio::test]
async fn delivers_in_place_when_there_is_room() {
    let (tx, mut rx) = channel::<u32>(1);
    post_stop(&tx, 7);
    assert_eq!(rx.try_recv().ok(), Some(7));
}

/// A FULL mailbox: the call returns at once, and the message still
/// arrives — behind what was queued — once the receiver drains. A mutation
/// that drops the message on `Full` (a plain `try_send`) fails here: with
/// the ticker still running, that room would never stop.
#[tokio::test]
async fn full_mailbox_still_gets_the_message_in_order() {
    let (tx, mut rx) = channel::<u32>(1);
    tx.try_send(1).expect("room for the first");
    post_stop(&tx, 2);
    let first = tokio::time::timeout(WAIT, rx.recv()).await.expect("first");
    let second = tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("the stop message was lost on a full mailbox");
    assert_eq!((first, second), (Some(1), Some(2)));
}
