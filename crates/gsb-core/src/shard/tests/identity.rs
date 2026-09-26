//! The wire-identity exhaustion guard (module docs, "Wire identity"):
//! the join that would draw past the logic's serial capacity is refused
//! as `RoomFull`, and nothing is minted or bound for it.

use super::*;
use crate::error::CoreError;

/// Send one Join straight through `handle_msg` and return its reply.
async fn join_reply(
    a: &mut ShardActor<TWorld, (), TState, TStrip>,
    conn: ConnectionId,
) -> Result<EntityId, CoreError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(
        a.handle_msg(
            ShardMsg::Join {
                conn,
                epoch: 1,
                identity: String::new(),
                out: out_tx,
                reply: reply_tx
            },
            1
        ),
        "a join must never stop the actor"
    );
    reply_rx
        .await
        .expect("join reply delivered")
        .map(|(entity, _actions)| entity)
}

/// Capacity 4, one draw per join (the test logic): the guard admits a
/// join while `used + 1 < capacity` — three joins, serials 1..=3 of
/// shard 1's class — and refuses the fourth as `RoomFull` without
/// binding it. The shard keeps serving: a leave frees nothing (serials
/// are never re-drawn), so the next join is refused too.
#[tokio::test]
async fn a_shard_refuses_joins_past_its_serial_capacity() {
    let mut a = bare_shard_capped(1, 4);
    let mut minted = Vec::new();
    for c in 1..=3 {
        let entity = join_reply(&mut a, ConnectionId(c)).await.expect("admitted");
        minted.push(entity);
    }
    assert_eq!(
        minted,
        [2, 4, 6],
        "shard 1 of 2 draws the even values, in order"
    );
    match join_reply(&mut a, ConnectionId(4)).await {
        Err(CoreError::RoomFull(9)) => {}
        other => panic!("the fourth join must be refused as RoomFull: {other:?}"),
    }
    assert!(
        !a.binding.contains_key(&ConnectionId(4)) && !a.conn_epoch.contains_key(&ConnectionId(4)),
        "a refused join binds nothing"
    );
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn: ConnectionId(1),
            entity: minted[0],
            epoch: 1
        },
        2
    ));
    assert!(
        matches!(
            join_reply(&mut a, ConnectionId(5)).await,
            Err(CoreError::RoomFull(9))
        ),
        "a leave returns no serial to the shard"
    );
}

/// F21: a zero action capacity (`conn_action = 0`, flat or per room)
/// reached `mpsc::channel(0)` on the shard's join and resume paths and
/// panicked the shard at its first join. A zero-capacity action channel
/// has no meaning; it is one slot, as the room control channel always
/// was (`crate::channel::channel`).
#[tokio::test]
async fn a_zero_action_capacity_is_one_slot_not_a_panic() {
    let mut a = bare_shard(0);
    a.config.action_capacity = 0;
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: String::new(),
            out: out_tx.clone(),
            reply: reply_tx
        },
        1
    ));
    let (_entity, actions) = reply_rx.await.expect("delivered").expect("admitted");
    assert_eq!(actions.max_capacity(), 1, "the join's channel holds one");
    let player = a.binding[&ConnectionId(1)];
    let resumed = a.rebind_session(player, ConnectionId(2), 2, "", out_tx);
    assert_eq!(resumed.max_capacity(), 1, "the resume's channel holds one");
}
