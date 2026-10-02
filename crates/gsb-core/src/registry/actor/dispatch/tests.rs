//! The resume fan-out against a stopping sharded room (BACKLOG B71). The
//! shards are played by the test: a stopping shard's `finish` closes its
//! inbox and drains it, dropping every queued reply unanswered — so does
//! [`stop`] here. The client's answer is the `RoomGone` it always was; it
//! must not wait for shards known to be gone (before B71 each cost the
//! per-answer timeout; since B82 there is none — see [`late`]).
//!
//! B75 made the answer's cause exact: a send the room refused is
//! `Refused` (the dispatcher counts it), a reply dropped after the room
//! took the op is `Gone` (the room's stop counts it) — see [`refused`].

use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::channel::{Inbox, channel};
use crate::id::{ConnectionId, RoomId};
use crate::registry::actor::Registry;
use crate::registry::{OpOutcome, RoomHandle};
use crate::shard::ShardMsg;

mod late;
mod refused;

type Reg = Registry<(), (), (), ()>;
type Shard = Inbox<ShardMsg<(), ()>>;

const CONN: ConnectionId = ConnectionId(3);
const ROOM: RoomId = RoomId(1);
/// The fan-out's old bound on one shard's answer (gone since B82): a gone
/// shard must not cost it, a slow live one is waited for past it.
const PER_ANSWER: Duration = Duration::from_secs(5);

/// A sharded room of `n` test-played shards.
fn shards(n: usize) -> (RoomHandle<(), ()>, Vec<Shard>) {
    let (mailboxes, inboxes) = (0..n).map(|_| channel(8)).unzip();
    (RoomHandle::Sharded(mailboxes), inboxes)
}

/// The dispatcher's resume of a parked-nowhere identity, home shard 0.
fn resume(handle: RoomHandle<(), ()>) -> JoinHandle<OpOutcome> {
    tokio::spawn(async move {
        let (out, _gone) = channel(8);
        Reg::dispatch_resume(CONN, ROOM, &handle, Some(0), 1, "ada".into(), None, out).await
    })
}

/// A shard's stop, as `finish` runs it on the inbox: close, drain, drop.
fn stop(mut inbox: Shard) {
    inbox.close();
    while inbox.try_recv().is_ok() {}
}

/// The resume a shard has been handed (it sits in the shard's queue).
async fn queued(inbox: &mut Shard) -> ShardMsg<(), ()> {
    match inbox.recv().await {
        Some(m @ ShardMsg::Resume { .. }) => m,
        other => panic!("expected the resume, got {other:?}"),
    }
}

/// Every shard stops with the resume queued (the stop's leftovers — the
/// identity is parked on none of them): the answer is `RoomGone` at once,
/// not after one timeout per shard (15 s here before B71). No shard
/// counted it, so the fallback fresh join meets the home shard's closed
/// inbox: `Refused`, counted once by the dispatcher (B75).
#[tokio::test(start_paused = true)]
async fn a_resume_queued_at_a_sharded_room_s_stop_is_answered_at_once() {
    let (handle, mut inboxes) = shards(3);
    let t0 = Instant::now();
    let task = resume(handle);
    let mut held = Vec::new();
    for inbox in &mut inboxes {
        held.push(queued(inbox).await);
    }
    drop(held);
    for inbox in inboxes {
        stop(inbox);
    }
    let outcome = task.await.expect("the dispatcher's resume");
    assert!(
        matches!(outcome, OpOutcome::Refused),
        "answered RoomGone, refused"
    );
    let waited = t0.elapsed();
    assert!(waited < PER_ANSWER, "the gone shards cost {waited:?}");
}

/// Mid-stop — the shards end on different ticks: one had already stopped
/// (the send is refused), one answers "not here" on its last tick, one
/// stops with the resume queued. The all-miss falls through to the fresh
/// join on the home shard, whose inbox is closed: `RoomGone`, at once —
/// a refusal (B75).
#[tokio::test(start_paused = true)]
async fn a_resume_meeting_a_room_mid_stop_does_not_wait_for_its_gone_shards() {
    let (handle, mut inboxes) = shards(3);
    stop(inboxes.remove(0));
    let t0 = Instant::now();
    let task = resume(handle);
    match queued(&mut inboxes[0]).await {
        ShardMsg::Resume { reply, .. } => {
            let _ = reply.send(Ok(None));
        }
        _ => unreachable!("queued returns a resume"),
    }
    drop(queued(&mut inboxes[1]).await);
    stop(inboxes.remove(1));
    let outcome = task.await.expect("the dispatcher's resume");
    assert!(
        matches!(outcome, OpOutcome::Refused),
        "answered RoomGone, refused"
    );
    let waited = t0.elapsed();
    assert!(waited < PER_ANSWER, "the gone shards cost {waited:?}");
    assert!(inboxes[0].try_recv().is_err(), "no join reached a shard");
}
