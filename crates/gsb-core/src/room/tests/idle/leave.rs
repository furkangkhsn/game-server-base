//! The default action's room half (BACKLOG B40): under
//! `afk_action = LeaveRoom` the ceiling asks the registry to settle the
//! row as a leave (`RegistryMsg::LeaveConn`), and a PARK is left behind
//! under the connection's park key with both channel halves it shared
//! with the (still open) connection released — so nothing that
//! connection does next can reach it.

use super::*;
use crate::registry::{LeaveRequest, RegistryMsg};
use crate::room::AfkAction;

fn leave_room(id: u64) -> RoomConfig {
    RoomConfig {
        afk_action: AfkAction::LeaveRoom,
        ..cfg(id, Some(5))
    }
}

fn registry(r: &mut Rig, cap: usize) -> mpsc::Receiver<RegistryMsg> {
    let (tx, rx) = channel(cap);
    r.actor.registry = Some(tx);
    rx
}

/// Everything the registry received so far, in order.
fn received(rx: &mut mpsc::Receiver<RegistryMsg>) -> Vec<RegistryMsg> {
    let mut v = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        v.push(msg);
    }
    v
}

fn leaves(msgs: &[RegistryMsg]) -> Vec<LeaveRequest> {
    msgs.iter()
        .filter_map(|m| match m {
            RegistryMsg::LeaveConn(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

const PARK: Detach = Detach::Hold {
    grace: Some(Duration::from_secs(600)),
    to: ExpireTo::Despawn,
};

/// A despawn: one leave request for the ended membership, no park — and
/// no transport-death report beside it (the request IS the settlement).
#[test]
fn a_despawn_asks_for_the_leave_and_nothing_else() {
    let mut r = Rig::new(leave_room(70), Detach::Despawn);
    let mut reg = registry(&mut r, 64);
    let (entity, act) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    r.step_at(2, 31);
    let msgs = received(&mut reg);
    assert_eq!(
        leaves(&msgs),
        vec![LeaveRequest {
            conn: ConnectionId(1),
            room: RoomId(70),
            entity,
            park: None,
        }]
    );
    assert_eq!(msgs.len(), 1, "no report, no close: {msgs:?}");
    assert!(act.is_closed(), "the connection's forwards now fail");
}

/// A park: the row is re-keyed to the park key (binding and session
/// back-reference), both halves it shared with the connection are
/// closed, and the request names the key.
#[test]
fn a_park_is_left_behind_under_the_park_key() {
    let mut r = Rig::new(leave_room(71), PARK);
    let mut reg = registry(&mut r, 64);
    let (entity, act) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    let key = ConnectionId(1).park_key();
    assert_eq!(
        leaves(&received(&mut reg)),
        vec![LeaveRequest {
            conn: ConnectionId(1),
            room: RoomId(71),
            entity,
            park: Some(key),
        }]
    );
    let row = &r.actor.conns[&PlayerId(1)];
    assert!(row.detached && row.conn == key);
    assert!(row.out.is_closed(), "the socket's writer is not held open");
    assert!(
        act.is_closed(),
        "the connection's frames no longer land here"
    );
    assert_eq!(r.actor.binding.get(&key), Some(&PlayerId(1)));
    assert!(!r.actor.binding.contains_key(&ConnectionId(1)));
}

/// The live connection's own session can no longer touch the park: a
/// leave or a detach it sends for the old membership is a no-op.
#[test]
fn the_old_session_cannot_reach_the_park() {
    let mut r = Rig::new(leave_room(72), PARK);
    let _reg = registry(&mut r, 64);
    let (entity, _act) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    assert_eq!(r.disconnects().len(), 1);
    r.actor.handle_control(RoomControl::Leave {
        conn: ConnectionId(1),
        entity,
    });
    r.actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity,
        identity: "ana".into(),
    });
    assert!(r.actor.conns[&PlayerId(1)].detached, "still parked");
    assert!(r.disconnects().is_empty(), "the policy was not asked again");
}

/// A full mailbox keeps the request; a join of the same connection in
/// the meantime drops it (the membership it would settle is that
/// connection's again).
#[test]
fn a_queued_request_goes_when_the_connection_joins_again() {
    let mut r = Rig::new(leave_room(73), Detach::Despawn);
    let mut reg = registry(&mut r, 1);
    let tx = r.actor.registry.clone().expect("attached");
    tx.try_send(RegistryMsg::Shutdown).expect("the one slot");
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    assert_eq!(r.actor.leave_requests.len(), 1, "the request waits");
    let (_e2, _a2) = r.join(ConnectionId(1), "ana");
    assert!(r.actor.leave_requests.is_empty(), "the rejoin dropped it");
    assert!(matches!(reg.try_recv(), Ok(RegistryMsg::Shutdown)));
    // (A step inside the new membership's first seconds: nothing is due.)
    r.step_at(2, 1);
    assert!(leaves(&received(&mut reg)).is_empty());
}

/// A park that ENDS while its request still waits: the despawn report
/// goes first, and the request then settles a despawn — there is no
/// park left to move the row to.
#[test]
fn a_park_that_ended_first_settles_as_a_despawn() {
    let zero = Detach::Hold {
        grace: Some(Duration::ZERO),
        to: ExpireTo::Despawn,
    };
    let mut r = Rig::new(leave_room(74), zero);
    let mut reg = registry(&mut r, 1);
    let tx = r.actor.registry.clone().expect("attached");
    tx.try_send(RegistryMsg::Shutdown).expect("the one slot");
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    assert_eq!(
        r.actor.leave_requests[0].park,
        Some(ConnectionId(1).park_key())
    );
    r.step_at(2, 31);
    assert!(r.actor.conns.is_empty(), "the zero hold ended in a despawn");
    let mut order = received(&mut reg);
    for k in 3..=6u64 {
        r.step_at(k, 30 + k);
        order.extend(received(&mut reg));
    }
    let key = ConnectionId(1).park_key();
    let kinds: Vec<String> = order
        .iter()
        .map(|m| match m {
            RegistryMsg::Shutdown => "shutdown".to_string(),
            RegistryMsg::DetachDespawned { conn, .. } if *conn == key => "report".into(),
            RegistryMsg::LeaveConn(req) => format!("leave park={:?}", req.park),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(kinds, vec!["shutdown", "report", "leave park=None"]);
}

mod resume;
mod unread;
