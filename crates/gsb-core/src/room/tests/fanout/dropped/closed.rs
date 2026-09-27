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

/// `shipped_*` counts what left for a connection (B57): of the four
/// batches above, only player 2's step-1 batch was taken by its channel
/// — the closed and the full sends are failures, not traffic.
#[test]
fn only_a_batch_the_channel_took_is_counted_as_shipped() {
    let (mut actor, _seen, _control) = room();
    drop(join(&mut actor, 1, 64));
    let mut slow = join(&mut actor, 2, 1);
    step(&mut actor, 1);
    step(&mut actor, 2);

    let batch = slow.try_recv().expect("step 1 reached player 2");
    assert!(slow.try_recv().is_err(), "nothing else did");
    let bytes: u64 = batch.iter().map(|f| f.payload.len() as u64).sum();
    let private = batch.iter().filter(|f| f.op == 0x7041).count() as u64;
    let s = actor.sample();
    assert_eq!(s.shipped_frames, batch.len() as u64);
    assert_eq!(s.private_frames, private);
    assert_eq!(s.shipped_bytes, bytes);
}
