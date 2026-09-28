//! A live shard slower than five seconds (BACKLOG B82). Its answer is
//! the resume's answer: the fan-out waits for every live shard, as the
//! plain join waits for its one. Before, the fan-out gave up after 5 s
//! per answer and fell back to a fresh join on the home shard while the
//! slow shard's resume was still queued — if that shard held the park,
//! it then rebound the parked row to this connection's outbound: the
//! connection bound on two shards, its late acceptance (and the action
//! channel it carried) dropped uncounted by the forwarder.
//!
//! A shard is slow for two reasons: a stalled runtime, or a room whose
//! tick period is longer than the bound (`tick_hz` below 0.2 — every
//! shard drains its inbox once per step).

use super::*;
use crate::channel::Mailbox;
use crate::error::CoreError;
use crate::room::Action;
use crate::shard::ResumeReply;
use tokio::sync::mpsc::error::TryRecvError;

/// Past the old per-answer bound.
const SLOW: Duration = Duration::from_secs(6);

/// The resume fanned out to two shards, home shard 0: shard 0 answers
/// "not here" at once, shard 1 answers `late` after [`SLOW`]. Returns the
/// dispatcher's task and shard 0's inbox — where a fresh join would land.
async fn slow_second(late: ResumeReply) -> (JoinHandle<OpOutcome>, Shard) {
    let (handle, mut inboxes) = shards(2);
    let task = resume(handle);
    match queued(&mut inboxes[0]).await {
        ShardMsg::Resume { reply, .. } => {
            let _ = reply.send(Ok(None));
        }
        _ => unreachable!("queued returns a resume"),
    }
    let ShardMsg::Resume { reply, .. } = queued(&mut inboxes[1]).await else {
        unreachable!("queued returns a resume")
    };
    let t0 = Instant::now();
    tokio::time::sleep(SLOW).await;
    let mut home = inboxes.remove(0);
    assert!(
        home.try_recv().is_err(),
        "no fresh join while shard 1's answer is outstanding"
    );
    let _ = reply.send(late);
    assert!(t0.elapsed() >= PER_ANSWER, "answered past the old bound");
    (task, home)
}

/// The slow shard held the park and accepted: that IS the join — the
/// entity and the action channel it rebound come back, and no fresh join
/// reaches the home shard (before: a second binding there, and the
/// acceptance's channel dropped).
#[tokio::test(start_paused = true)]
async fn a_slow_shard_s_acceptance_is_the_join_not_a_fresh_one() {
    let (actions, mut act_rx) = channel::<Action>(8);
    let accepted: (u64, Mailbox<Action>) = (7, actions);
    let (task, mut home) = slow_second(Ok(Some(accepted))).await;
    // A fresh join from here on would be refused, not wait forever.
    home.close();
    let outcome = task.await.expect("the dispatcher's resume");
    let OpOutcome::Joined(entity, actions) = outcome else {
        panic!("the slow shard's acceptance is the join");
    };
    assert_eq!(entity, 7, "the parked entity, rebound");
    assert!(
        matches!(act_rx.try_recv(), Err(TryRecvError::Empty)),
        "the rebound row's action channel reached the connection"
    );
    drop(actions);
    assert!(home.try_recv().is_err(), "no fresh join on the home shard");
}

/// The slow shard's epoch guard tripped: the rejection is the answer, as
/// from a fast shard — no fresh join behind it.
#[tokio::test(start_paused = true)]
async fn a_slow_shard_s_stale_answer_is_the_answer() {
    let (task, mut home) = slow_second(Err(CoreError::ResumeStale)).await;
    home.close();
    let outcome = task.await.expect("the dispatcher's resume");
    assert!(
        matches!(outcome, OpOutcome::Rejected(CoreError::ResumeStale)),
        "the slow shard's rejection"
    );
    assert!(home.try_recv().is_err(), "no fresh join on the home shard");
}

/// The slow shard answers "not here" too: the all-miss falls back to the
/// fresh join on the home shard — only now, after the last answer.
#[tokio::test(start_paused = true)]
async fn a_slow_miss_falls_back_after_the_last_answer() {
    let (task, mut home) = slow_second(Ok(None)).await;
    match home.recv().await {
        Some(ShardMsg::Join { reply, .. }) => {
            let (actions, _act_rx) = channel::<Action>(8);
            let _ = reply.send(Ok((9, actions)));
        }
        other => panic!("expected the fallback join, got {other:?}"),
    }
    let outcome = task.await.expect("the dispatcher's resume");
    assert!(
        matches!(outcome, OpOutcome::Joined(9, _)),
        "the fresh join on the home shard"
    );
}
