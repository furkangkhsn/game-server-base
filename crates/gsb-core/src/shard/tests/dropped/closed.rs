//! The room's B32 split on the shard: a batch meeting a CLOSED outbound
//! channel (the connection gone before the shard processed its end) is
//! `sends_closed`; only a FULL channel is a drop (`dropped_frames`).

use super::*;

/// Player 1's channel closed, player 2's holding one batch: two steps.
async fn two_steps_one_closed_one_full(
    a: &mut ShardActor<TWorld, (), TState, TStrip>,
) -> mpsc::Receiver<FrameBatch> {
    let mut outs = Vec::new();
    for (conn, cap) in [(1, 64), (2, 1)] {
        let (out, out_rx) = mpsc::channel::<FrameBatch>(cap);
        let (reply, replied) = oneshot::channel();
        assert!(a.handle_msg(
            ShardMsg::Join {
                conn: ConnectionId(conn),
                epoch: 1,
                identity: String::new(),
                out,
                reply,
            },
            1,
        ));
        replied.await.expect("join reply").expect("join ok");
        outs.push(out_rx);
    }
    // Player 1's connection is gone; the shard has not been told.
    drop(outs.remove(0));
    let slow = outs.remove(0);
    for t in 1..=2u64 {
        assert!(a.step(&tinfo(t)));
    }
    slow
}

#[tokio::test]
async fn a_closed_outbound_channel_is_counted_apart_from_drops_on_the_shard() {
    let (mut a, _seen) = shard();
    let mut slow = two_steps_one_closed_one_full(&mut a).await;

    assert_eq!(a.m.dropped_frames, 1, "only the full queue is a drop");
    assert_eq!(a.m.sends_closed, 2, "one per step on the closed channel");
    assert_eq!(a.sample().sends_closed, 2, "the sample carries it");
    assert!(slow.try_recv().is_ok(), "step 1 reached player 2");
    assert!(slow.try_recv().is_err(), "step 2 did not");
}

/// `shipped_*` counts only the batch the channel took (the room's B57
/// rule): player 2's step-1 batch, nothing of the closed or full sends.
#[tokio::test]
async fn only_a_batch_the_channel_took_is_counted_as_shipped_on_the_shard() {
    let (mut a, _seen) = shard();
    let mut slow = two_steps_one_closed_one_full(&mut a).await;
    let batch = slow.try_recv().expect("step 1 reached player 2");
    assert!(slow.try_recv().is_err(), "nothing else did");
    let bytes: u64 = batch.iter().map(|f| f.payload.len() as u64).sum();
    let private = batch.iter().filter(|f| f.op == 0x7191).count() as u64;
    assert_eq!(a.m.shipped_frames, batch.len() as u64);
    assert_eq!(a.m.private_frames, private);
    assert_eq!(a.m.shipped_bytes, bytes);
}
