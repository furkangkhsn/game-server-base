//! Where a join into a stopping room is counted (BACKLOG B75): exactly
//! once. A send the room refused (its inbox already closed) is the
//! dispatcher's `Refused`; a reply the room dropped after taking the op
//! is `Gone` (the room's stop counted it); a resume whose identity is
//! parked on a stopping shard is answered `RoomGone` by that shard
//! ("counted here"), and no fallback fresh join may count it again.

use super::*;
use crate::error::CoreError;
use crate::room::RoomControl;

/// A single room whose control inbox the test holds.
fn single() -> (RoomHandle<(), ()>, Inbox<RoomControl>) {
    let (control, inbox) = channel(8);
    (RoomHandle::Single(control), inbox)
}

/// The dispatcher's plain join, home shard 0 when sharded.
fn join(handle: RoomHandle<(), ()>) -> JoinHandle<OpOutcome> {
    tokio::spawn(async move {
        let shard = matches!(handle, RoomHandle::Sharded(_)).then_some(0);
        let (out, _gone) = channel(8);
        Reg::dispatch_plain_join(CONN, ROOM, &handle, shard, 1, String::new(), out).await
    })
}

/// A single room's resume (the room falls back to a fresh join itself).
fn resume_single(handle: RoomHandle<(), ()>) -> JoinHandle<OpOutcome> {
    tokio::spawn(async move {
        let (out, _gone) = channel(8);
        Reg::dispatch_resume(CONN, ROOM, &handle, None, 1, "ada".into(), out).await
    })
}

/// A room (or the join's shard) that had stopped before the op: the send
/// is refused — `Refused`, whichever shape and op.
#[tokio::test]
async fn a_join_a_closed_room_refuses_is_refused_not_gone() {
    let (handle, inbox) = single();
    drop(inbox);
    let outcome = join(handle).await.expect("the plain join");
    assert!(matches!(outcome, OpOutcome::Refused), "a single room's");

    let (handle, inbox) = single();
    drop(inbox);
    let outcome = resume_single(handle).await.expect("the resume");
    assert!(
        matches!(outcome, OpOutcome::Refused),
        "a single room's resume"
    );

    let (handle, mut inboxes) = shards(2);
    stop(inboxes.remove(0));
    let outcome = join(handle).await.expect("the sharded join");
    assert!(matches!(outcome, OpOutcome::Refused), "the join's shard's");
}

/// The room took the op and stopped with it queued: its stop counts it
/// (`joins_unprocessed` / `resumes_unprocessed`), so the answer is
/// `Gone` — not a refusal the dispatcher would count a second time.
#[tokio::test]
async fn a_join_taken_and_dropped_at_the_stop_is_gone_not_refused() {
    let (handle, mut inbox) = single();
    let task = join(handle);
    let queued = inbox.recv().await.expect("the join reached the room");
    assert!(matches!(queued, RoomControl::Join { .. }));
    drop(queued);
    let outcome = task.await.expect("the plain join");
    assert!(matches!(outcome, OpOutcome::Gone), "a single room's join");

    let (handle, mut inbox) = single();
    let task = resume_single(handle);
    let queued = inbox.recv().await.expect("the resume reached the room");
    assert!(matches!(queued, RoomControl::Resume { .. }));
    drop(queued);
    let outcome = task.await.expect("the resume");
    assert!(matches!(outcome, OpOutcome::Gone), "a single room's resume");

    let (handle, mut inboxes) = shards(1);
    let task = join(handle);
    match inboxes[0].recv().await {
        Some(ShardMsg::Join { .. }) => {}
        other => panic!("expected the join, got {other:?}"),
    }
    let outcome = task.await.expect("the sharded join");
    assert!(matches!(outcome, OpOutcome::Gone), "the shard's join");
}

/// The identity is parked on shard 1, which stops with the resume
/// queued: it counts the resume and answers `RoomGone`, the others drop
/// theirs. The fan-out answers `RoomGone` without the fallback fresh
/// join — which would meet the home shard's closed inbox and be counted
/// again as a refusal.
#[tokio::test(start_paused = true)]
async fn a_resume_parked_on_a_stopping_shard_is_counted_there_only() {
    let (handle, mut inboxes) = shards(3);
    let t0 = Instant::now();
    let task = resume(handle);
    let mut held = Vec::new();
    for inbox in &mut inboxes {
        held.push(queued(inbox).await);
    }
    // The stop's answer for the park it holds (the shard's
    // `count_leftovers`): "counted here".
    match held.remove(1) {
        ShardMsg::Resume { reply, .. } => {
            let _ = reply.send(Err(CoreError::RoomGone));
        }
        _ => unreachable!("queued returns a resume"),
    }
    drop(held);
    for inbox in inboxes {
        stop(inbox);
    }
    let outcome = task.await.expect("the dispatcher's resume");
    assert!(
        matches!(outcome, OpOutcome::Rejected(CoreError::RoomGone)),
        "answered RoomGone as the parked shard's own answer, not refused"
    );
    let waited = t0.elapsed();
    assert!(waited < PER_ANSWER, "the gone shards cost {waited:?}");
}
