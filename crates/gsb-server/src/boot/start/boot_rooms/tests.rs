//! The boot rooms are in the registry mailbox, in id order, when
//! `create_boot_rooms` returns — before the caller can yield to any
//! other task (B44) — and the ones past the mailbox's free capacity
//! still follow, in order, from the spawned task.

use std::time::Duration;

use gsb_core::channel::{Inbox, channel};
use gsb_core::registry::{RegistryMsg, RoomStatus};

use super::create_boot_rooms;
use crate::Config;

/// The next message must be room `id`'s `CreateRoom`; answer it.
fn created(msg: Option<RegistryMsg>, id: u64) {
    match msg {
        Some(RegistryMsg::CreateRoom { config, reply }) => {
            assert_eq!(config.id.0, id, "the boot rooms go in id order");
            let _ = reply.send(Ok(RoomStatus::Running { members: 0 }));
        }
        other => panic!("expected room {id}'s CreateRoom, got {other:?}"),
    }
}

fn now(inbox: &mut Inbox<RegistryMsg>) -> Option<RegistryMsg> {
    inbox.try_recv().ok()
}

/// The next message, waiting for it (a hang guard only, not the claim).
async fn next(inbox: &mut Inbox<RegistryMsg>) -> Option<RegistryMsg> {
    tokio::time::timeout(Duration::from_secs(10), inbox.recv())
        .await
        .expect("the spawned task sends the rest")
}

/// Current-thread runtime and no await between the call and the reads:
/// no spawned task has run, so what is in the mailbox was put there by
/// the call itself.
#[tokio::test(flavor = "current_thread")]
async fn every_boot_room_is_enqueued_before_the_call_returns() {
    let (tx, mut inbox) = channel::<RegistryMsg>(16);
    create_boot_rooms(&tx, &Config::default().room_template(), 3);
    for id in 1..=3 {
        created(now(&mut inbox), id);
    }
    assert!(now(&mut inbox).is_none(), "three rooms, three creates");
}

/// A mailbox with room for two: rooms 1 and 2 are in it at once, the
/// rest arrive from the spawned task, still in order after them.
#[tokio::test(flavor = "current_thread")]
async fn rooms_past_the_free_capacity_follow_in_order() {
    let (tx, mut inbox) = channel::<RegistryMsg>(2);
    create_boot_rooms(&tx, &Config::default().room_template(), 5);
    created(now(&mut inbox), 1);
    created(now(&mut inbox), 2);
    assert!(now(&mut inbox).is_none(), "room 3 did not fit");
    for id in 3..=5 {
        created(next(&mut inbox).await, id);
    }
    drop(tx);
    assert!(next(&mut inbox).await.is_none(), "five rooms, five creates");
}
