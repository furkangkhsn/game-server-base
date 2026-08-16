//! Regression tests for the broadcast phase.
//!
//! 1. A connection that joins a room where others are already present must
//!    receive the **entire world** (full snapshot) on the next broadcast —
//!    not only what changes afterwards. (Previously `last_sent` was global
//!    per room, so a late joiner saw nothing until each entity moved.)
//! 2. A *stale* leave (its connection re-joined before the leave was
//!    processed) must not despawn the new entity.

use std::time::Duration;

use bevy_ecs::prelude::World;
use bytes::Bytes;
use gsb_core::channel::{FrameBatch, channel};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::room::{RoomActor, RoomConfig, RoomMsg};
use gsb_protocol::FrameBody;

use gsb_game::op;
use prost::Message;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);

async fn reply<T>(rx: tokio::sync::oneshot::Receiver<T>) -> T {
    tokio::time::timeout(WAIT, rx)
        .await
        .expect("timed out waiting for reply")
        .expect("reply dropped")
}

async fn next_batch(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<FrameBody> {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("timed out waiting for a batch")
        .expect("out channel closed")
}

fn states(batch: &[FrameBody]) -> Vec<gsb_game::game::EntityState> {
    batch
        .iter()
        .filter(|f| f.op == op::ENTITY_STATE)
        .map(|f| {
            gsb_game::game::EntityState::decode(f.payload.as_ref())
                .expect("bad ENTITY_STATE payload")
        })
        .collect()
}

fn make_room() -> (
    gsb_core::channel::Mailbox<RoomMsg>,
    tokio::task::JoinHandle<()>,
) {
    let config = RoomConfig {
        id: RoomId(1),
        ..Default::default()
    };
    let (tx, rx) = channel(config.mailbox_capacity);
    let actor = RoomActor::new(
        config,
        World::new(),
        Box::new(gsb_game::room::DemoRoom::new()),
        rx,
    );
    (tx, tokio::spawn(async move { actor.run().await }))
}

async fn join(
    tx: &gsb_core::channel::Mailbox<RoomMsg>,
    conn: ConnectionId,
) -> (EntityId, mpsc::Receiver<FrameBatch>) {
    let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<EntityId>();
    tx.send(RoomMsg::PlayerJoined {
        conn,
        out: out_tx,
        reply: reply_tx,
    })
    .await
    .expect("room mailbox closed");
    let entity = reply(reply_rx).await;
    (entity, out_rx)
}

#[tokio::test]
async fn late_joiner_receives_full_world_snapshot() {
    let (tx, handle) = make_room();

    // A joins and then stays perfectly still.
    let c_a = ConnectionId(1);
    let (a_entity, mut a_rx) = join(&tx, c_a).await;
    tx.send(RoomMsg::Tick).await.unwrap();
    let batch = next_batch(&mut a_rx).await;
    assert!(
        states(&batch).iter().any(|s| s.entity == a_entity),
        "A must see its own entity"
    );

    // B joins late, while A is still.
    let c_b = ConnectionId(2);
    let (b_entity, mut b_rx) = join(&tx, c_b).await;
    assert_ne!(a_entity, b_entity);

    // One tick: B must see both A (existing, still) and itself.
    tx.send(RoomMsg::Tick).await.unwrap();
    let batch = next_batch(&mut b_rx).await;
    let seen: Vec<u64> = states(&batch).iter().map(|s| s.entity).collect();
    assert!(
        seen.contains(&a_entity),
        "late joiner did not see the existing still entity: {seen:?}"
    );
    assert!(
        seen.contains(&b_entity),
        "late joiner must see its own entity"
    );

    // A moves; B must observe the version bump.
    let move_to = gsb_game::game::MoveTo { x: 10, y: 10 };
    tx.send(RoomMsg::Action {
        conn: c_a,
        op: op::MOVE_TO,
        payload: Bytes::from(move_to.encode_to_vec()),
    })
    .await
    .unwrap();
    tx.send(RoomMsg::Tick).await.unwrap();
    let batch = next_batch(&mut b_rx).await;
    let a_state = states(&batch)
        .into_iter()
        .find(|s| s.entity == a_entity)
        .expect("B must receive A's updated state");
    assert!(
        a_state.version > 0,
        "moved entity must carry a bumped version"
    );

    tx.send(RoomMsg::Shutdown).await.unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn stale_leave_cannot_kill_rejoined_entity() {
    let (tx, handle) = make_room();

    // A joins → entity E1.
    let c_a = ConnectionId(3);
    let (e1, mut a_rx) = join(&tx, c_a).await;
    tx.send(RoomMsg::Tick).await.unwrap();
    let _ = next_batch(&mut a_rx).await;

    // A leaves, then re-joins: the join replaces the stale state and
    // creates E2.
    tx.send(RoomMsg::PlayerLeft {
        conn: c_a,
        entity: e1,
    })
    .await
    .unwrap();
    // The rejoin gets a fresh out channel (the room's old one is dropped).
    let (e2, mut a_rx) = join(&tx, c_a).await;
    assert_ne!(e1, e2, "rejoin must create a fresh entity");

    // A *stale* leave for E1 arrives late: it must be ignored.
    tx.send(RoomMsg::PlayerLeft {
        conn: c_a,
        entity: e1,
    })
    .await
    .unwrap();

    // If E2 had been killed by the stale leave, A's MOVE_TO below would be
    // dropped (no live entity) and this batch would never arrive.
    let move_to = gsb_game::game::MoveTo { x: -20, y: 20 };
    tx.send(RoomMsg::Action {
        conn: c_a,
        op: op::MOVE_TO,
        payload: Bytes::from(move_to.encode_to_vec()),
    })
    .await
    .unwrap();
    tx.send(RoomMsg::Tick).await.unwrap();
    let batch = next_batch(&mut a_rx).await;
    let state = states(&batch)
        .into_iter()
        .find(|s| s.entity == e2)
        .expect("rejoined entity E2 must survive the stale leave");
    assert!(state.version > 0);

    tx.send(RoomMsg::Shutdown).await.unwrap();
    handle.await.unwrap();
}
