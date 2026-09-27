//! The fan-out's two send failures, counted apart (BACKLOG B32): a FULL
//! outbound channel is the slow-client signal (`dropped_frames`); a
//! CLOSED one is a connection already gone — the client closed its
//! socket and the room has not processed the leave or detach yet — and
//! goes to `sends_closed`. Nothing the client wanted is lost there, so it
//! must not read as a slow client.

use super::*;

/// Player 1's connection is gone before the room learns it (its receiver
/// dropped); player 2's channel holds one batch and is never read. Step 1:
/// player 1's batch meets the closed channel, player 2's is delivered.
/// Step 2: player 1's again, and player 2's meets the full channel.
#[test]
fn a_closed_outbound_channel_is_counted_apart_from_drops() {
    let (mut actor, _seen, _control) = room();
    drop(join(&mut actor, 1, 64));
    let mut slow = join(&mut actor, 2, 1);
    step(&mut actor, 1);
    step(&mut actor, 2);

    let s = actor.sample();
    assert_eq!(s.dropped_frames, 1, "only the full queue is a drop");
    assert_eq!(s.sends_closed, 2, "one per step on the closed channel");
    assert!(slow.try_recv().is_ok(), "step 1 reached player 2");
    assert!(slow.try_recv().is_err(), "step 2 did not");
}
