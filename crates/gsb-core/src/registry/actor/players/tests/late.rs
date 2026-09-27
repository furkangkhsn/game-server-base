//! A replaced dispatcher's LATE `OpsClosed` (BACKLOG B65): the report of
//! a dispatcher that ended while its slot was still held reaches the
//! registry only after B63 has replaced it. It must end its own slot, not
//! the fresh dispatcher's — removing the fresh one's only sender would
//! close its queue, and it would detach the live membership.

use super::*;
use crate::registry::RoomOp;

/// A dispatcher installed in the slot and then ended while the slot still
/// holds it (its `Close` op run): returns the slot's serial. Its
/// `OpsClosed` is queued on the registry's inbox, not yet handled.
async fn ended_in_slot(reg: &mut Reg) -> u64 {
    let op_tx = reg.install_conn_ops(CONN, None);
    assert!(op_tx.try_send(RoomOp::Close).is_ok());
    tokio::time::timeout(WAIT, op_tx.closed())
        .await
        .expect("the dispatcher ended");
    reg.conn_ops[&CONN].0
}

/// The next registry message, which must be this dispatcher's report.
async fn report(reg: &mut Reg, serial: u64) {
    match tokio::time::timeout(WAIT, reg.inbox.recv()).await {
        Ok(Some(RegistryMsg::OpsClosed { conn, serial: s })) => {
            assert_eq!((conn, s), (CONN, serial));
            reg.on_ops_closed(conn, s);
        }
        other => panic!("expected the dispatcher's report, got {other:?}"),
    }
}

#[tokio::test]
async fn a_replaced_dispatcher_s_late_close_keeps_the_fresh_one() {
    let (mut reg, mut events) = setup().await;
    let stale = ended_in_slot(&mut reg).await;

    // The join finds the slot's queue closed: B63 replaces the dispatcher.
    let first = join(&mut reg).await;
    let fresh = reg.conn_ops[&CONN].0;
    assert_ne!(fresh, stale, "a replacement has its own serial");
    // The old dispatcher's report, late: behind the replacement.
    report(&mut reg, stale).await;
    let (serial, op_tx) = reg
        .conn_ops
        .get(&CONN)
        .expect("the late report removed the fresh dispatcher");
    assert_eq!(*serial, fresh);
    assert!(!op_tx.is_closed(), "the fresh dispatcher is alive");
    settle(&mut reg, first).await;
    assert_eq!(events.recv().await, Some(Ev::Joined(1)));

    // A later leave and join go through the same dispatcher, in order
    // (the leave as the `DespawnPlayer` arm sends it).
    let leave = RoomOp::Leave { room: ROOM };
    assert!(reg.conn_ops[&CONN].1.try_send(leave).is_ok());
    let second = join(&mut reg).await;
    match tokio::time::timeout(WAIT, reg.inbox.recv()).await {
        Ok(Some(RegistryMsg::LeaveDone { conn, room })) => assert_eq!((conn, room), (CONN, ROOM)),
        other => panic!("expected the leave first, got {other:?}"),
    }
    settle(&mut reg, second).await;
    assert_eq!(events.recv().await, Some(Ev::Left(1)), "no detach");
    assert_eq!(events.recv().await, Some(Ev::Joined(2)));
    assert_eq!(reg.conn_ops[&CONN].0, fresh, "no third dispatcher");
    assert_eq!(reg.reg_join_ops_dropped, 0);
}

#[tokio::test]
async fn a_dispatcher_s_own_close_frees_its_slot() {
    let (mut reg, _events) = setup().await;
    let serial = ended_in_slot(&mut reg).await;
    report(&mut reg, serial).await;
    assert!(!reg.conn_ops.contains_key(&CONN), "its own report ends it");
}
