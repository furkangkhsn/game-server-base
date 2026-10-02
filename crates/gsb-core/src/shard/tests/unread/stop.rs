//! What a stopping shard still holds (BACKLOG B62), the room's rule per
//! shard: the unread input of its rows, the answers it owes, the
//! requests in flight — counted and handed to the collector in the
//! shard's final sample, under the shard's own sample id.

use std::collections::VecDeque;
use std::time::Instant;

use super::*;
use crate::rpc::{PendingRequest, RpcReply};

#[tokio::test]
async fn a_stopping_shard_counts_what_it_holds_in_its_final_sample() {
    let mut a = bare_shard(1);
    let (metrics, mut samples) = mpsc::channel(8);
    a.metrics = metrics;
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: String::new(),
            out: out_tx,
            reply: reply_tx,
            claims: None,
        },
        1
    ));
    let (_entity, actions) = reply_rx.await.expect("delivered").expect("admitted");
    two_requests_and_an_action(&actions, ConnectionId(1));
    a.queued.insert(
        ConnectionId(1),
        vec![RpcReply {
            id: 3,
            ok: true,
            op: 1,
            reason: String::new(),
            payload: bytes::Bytes::new(),
        }],
    );
    a.pending.insert(
        ConnectionId(1),
        VecDeque::from([PendingRequest {
            id: 4,
            op: 1,
            due: Instant::now(),
        }]),
    );
    a.pending_total = 1;

    a.finish();

    let mut last = None;
    while let Ok(ev) = samples.try_recv() {
        last = Some(ev);
    }
    let Some(MetricsEvent::RoomFinal(s)) = last else {
        panic!("the final sample is the shard's last event: {last:?}");
    };
    assert_eq!(s.room, RoomId((9 << 16) + 1), "the shard's own sample id");
    assert_eq!(s.requests_dropped_unread, 2);
    assert_eq!(s.actions_dropped_unread, 1);
    assert_eq!(s.requests_undelivered, 1);
    assert_eq!(s.requests_abandoned, 1);
    assert_eq!(s.pending_requests, 0);
}

/// What else a stopping shard holds (B68): the ops and cross-shard
/// messages left in its inbox and deferred queue, and the effects in
/// flight. Broadcast ops count only where they would have acted; a
/// migrating player's unread input is counted with it; the inbox is
/// closed after the count.
#[tokio::test]
async fn a_stopping_shard_counts_what_its_inbox_and_effects_hold() {
    use crate::shard::link::InProcLink;
    use crate::shard::{EffectId, PlayerMigration, RemoteEffect, TeamImport};

    let mut a = bare_shard(1);
    let (metrics, mut samples) = mpsc::channel(8);
    a.metrics = metrics;
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: String::new(),
            out: out_tx.clone(),
            reply: reply_tx,
            claims: None,
        },
        1
    ));
    let (entity, _actions) = reply_rx.await.expect("delivered").expect("admitted");
    let (inbox_tx, inbox_rx) = channel::<ShardMsg<TState, TStrip>>(32);
    a.inbox = Box::new(InProcLink::inbound(inbox_rx));
    let effect = || RemoteEffect {
        target: 77,
        source: 0,
        id: EffectId {
            origin: 0,
            epoch: 0,
            seq: 1,
        },
        at_tick: 1,
        hops: 0,
        payload: bytes::Bytes::new(),
    };
    // A migrating player with a request and an action unread.
    let (act_tx, act_rx) = mpsc::channel::<Action>(8);
    for op in [crate::rpc::RPC_REQ_OP, 0x2001] {
        act_tx
            .try_send(Action {
                conn: ConnectionId(4),
                player: PlayerId(4),
                op,
                payload: bytes::Bytes::new(),
            })
            .expect("room");
    }
    let migrate = ShardMsg::Migrate {
        from: 0,
        at_tick: 1,
        wire: 44,
        state: TState {
            x: 1.0,
            y: 0.0,
            mode: 0,
        },
        player: Some(Box::new(PlayerMigration {
            player: PlayerId(4),
            conn: ConnectionId(4),
            epoch: 1,
            entity: 44,
            out: out_tx.clone(),
            actions: act_rx,
            detached: false,
            detach_deadline: None,
            detach_ceiling: None,
            expire_to: crate::room::ExpireTo::Despawn,
            bot_fed: false,
            session_epoch: 0,
            identity: String::new(),
            last_input: None,
            path: None,
        })),
    };
    let (join_reply, joined) = oneshot::channel();
    let (resume_reply, resumed) = oneshot::channel();
    let msgs = vec![
        ShardMsg::Join {
            conn: ConnectionId(5),
            epoch: 1,
            identity: String::new(),
            out: out_tx.clone(),
            reply: join_reply,
            claims: None,
        },
        // Nobody parked under "ghost" here: another shard's answer.
        ShardMsg::Resume {
            conn: ConnectionId(6),
            epoch: 1,
            identity: "ghost".to_string(),
            out: out_tx,
            reply: resume_reply,
        },
        ShardMsg::Leave {
            conn: ConnectionId(1),
            entity,
            epoch: 1,
        },
        // Another shard's member: a no-op here.
        ShardMsg::Detach {
            conn: ConnectionId(8),
            entity: 8,
            identity: String::new(),
        },
        ShardMsg::Detach {
            conn: ConnectionId(1),
            entity,
            identity: String::new(),
        },
        migrate,
        ShardMsg::RemoteEffect(effect()),
        ShardMsg::TeamImport(TeamImport {
            from: 0,
            tick: 1,
            records: Vec::new(),
        }),
        ShardMsg::ResyncRequest { from: 0 },
        ShardMsg::Shutdown,
    ];
    for m in msgs {
        inbox_tx.try_send(m).expect("room in the inbox");
    }
    // One more border update deferred behind a Shutdown the CONTROL
    // phase met; an effect awaiting its tick, two held to send.
    a.deferred.push_back(ShardMsg::ResyncRequest { from: 0 });
    a.effects.pending.push(effect());
    a.effects.retry.push_back((0, effect()));
    a.effects.out.queue.push((0, effect()));

    a.finish();

    let mut last = None;
    while let Ok(ev) = samples.try_recv() {
        last = Some(ev);
    }
    let Some(MetricsEvent::RoomFinal(s)) = last else {
        panic!("the final sample: {last:?}");
    };
    let st = s.stop;
    assert_eq!(st.joins_unprocessed, 1);
    assert_eq!(st.resumes_unprocessed, 0, "not parked here");
    assert_eq!(st.leaves_unprocessed, 1);
    assert_eq!(st.detaches_unprocessed, 1, "only this shard's member");
    assert_eq!(st.migrations_in_dropped, 1);
    assert_eq!(st.effects_unapplied, 2, "one in the inbox, one pending");
    assert_eq!(st.effects_unsent, 2, "the retry buffer and the outbox");
    assert_eq!(st.team_imports_unapplied, 1);
    assert_eq!(
        st.border_updates_unapplied, 2,
        "the inbox's and the deferred"
    );
    // The migrating player's input, counted as a session end counts it
    // (the member's own channel, unread, adds nothing).
    assert_eq!(s.requests_dropped_unread, 1);
    assert_eq!(s.actions_dropped_unread, 1);
    assert!(joined.await.is_err(), "the join's reply was dropped");
    // Dropped unanswered: the dispatcher's resume fan-out stops waiting
    // for this shard the moment it is dropped (B71).
    assert!(resumed.await.is_err(), "the resume's reply was dropped");
    assert!(
        inbox_tx.try_send(ShardMsg::Shutdown).is_err(),
        "the inbox is closed"
    );
}

/// The CONTROL phase that meets a Shutdown leaves the rest of its drain
/// to the stop's count (before B68 the rest of the drained batch died
/// with it, uncounted).
#[tokio::test]
async fn messages_behind_a_shutdown_in_one_drain_are_counted() {
    use crate::shard::link::InProcLink;

    let mut a = bare_shard(1);
    let (metrics, mut samples) = mpsc::channel(8);
    a.metrics = metrics;
    let (inbox_tx, inbox_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    a.inbox = Box::new(InProcLink::inbound(inbox_rx));
    inbox_tx.try_send(ShardMsg::Shutdown).expect("room");
    inbox_tx
        .try_send(ShardMsg::ResyncRequest { from: 0 })
        .expect("room");
    assert!(!a.step(&tinfo(1)), "the Shutdown stops the shard");
    a.finish();
    let mut last = None;
    while let Ok(ev) = samples.try_recv() {
        last = Some(ev);
    }
    let Some(MetricsEvent::RoomFinal(s)) = last else {
        panic!("the final sample: {last:?}");
    };
    assert_eq!(s.stop.border_updates_unapplied, 1);
}
