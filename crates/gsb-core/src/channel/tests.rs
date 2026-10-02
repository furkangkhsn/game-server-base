//! The counted, tokio-free send (BACKLOG F65): the outcome names the
//! cause, and the loss counts split full from closed.

use super::*;

/// Taken, then full, then closed: each outcome says which.
#[tokio::test]
async fn the_outcome_names_the_cause() {
    let (tx, mut rx) = channel::<u8>(1);
    assert_eq!(try_send(&tx, 1), TrySend::Sent);
    assert_eq!(try_send(&tx, 2), TrySend::Full);
    assert_eq!(rx.recv().await, Some(1), "the full one was not kept");
    drop(rx);
    assert_eq!(try_send(&tx, 3), TrySend::Closed);
}

/// The counting form: every message not taken is counted once, by why;
/// a taken one counts nothing.
#[tokio::test]
async fn the_losses_are_counted_by_cause() {
    let (tx, mut rx) = channel::<u8>(2);
    let mut lost = SendLosses::default();
    for m in 0..5 {
        lost.try_send(&tx, m);
    }
    assert_eq!((lost.full, lost.closed, lost.total()), (3, 0, 3));
    assert_eq!(rx.recv().await, Some(0));
    assert_eq!(lost.try_send(&tx, 5), TrySend::Sent);
    assert_eq!(lost.total(), 3, "a taken message counts nothing");
    drop(rx);
    assert_eq!(lost.try_send(&tx, 6), TrySend::Closed);
    assert_eq!((lost.full, lost.closed, lost.total()), (3, 1, 4));
}

/// No runtime needed: the send never spawns and never awaits (a room's
/// tick body, a game's logic, a plain thread).
#[test]
fn it_needs_no_runtime() {
    let (tx, _rx) = channel::<u8>(1);
    let mut lost = SendLosses::default();
    assert_eq!(lost.try_send(&tx, 1), TrySend::Sent);
    assert_eq!(lost.try_send(&tx, 2), TrySend::Full);
    assert_eq!(lost.full, 1);
}
