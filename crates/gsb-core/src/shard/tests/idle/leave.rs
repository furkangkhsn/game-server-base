//! The shard's half of the default action (BACKLOG B40): the room
//! actor's rule, mirrored — a leave request per expired member, no
//! transport-death report beside a despawn, and a park re-keyed to the
//! connection's park key together with its session-epoch entry, out of
//! reach of the still-open connection's own leave.

use super::*;
use crate::registry::{LeaveRequest, RegistryMsg};
use crate::room::AfkAction;

fn step(a: &mut ShardActor<TWorld, (), TState, TStrip>, t0: Instant, k: u64, secs: u64) {
    a.step_phases(&TickInfo {
        tick: k,
        at: t0 + Duration::from_secs(secs),
    });
}

fn leave_room() -> RoomConfig {
    RoomConfig {
        afk_action: AfkAction::LeaveRoom,
        ..config(Some(5))
    }
}

/// A despawn: one leave request, nothing else.
#[tokio::test]
async fn a_sharded_despawn_asks_for_the_leave_only() {
    let (a, _obs, _disc) = rig_with(leave_room(), Detach::Despawn);
    let (tx, mut reg) = channel::<RegistryMsg>(64);
    let mut a = a.with_registry(tx);
    let _o1 = join(&mut a, ConnectionId(1), "ana");
    let t0 = Instant::now();
    for k in 1..=3u64 {
        step(&mut a, t0, k, 10 + k);
    }
    let got = drain(&mut reg);
    assert!(
        matches!(
            got.as_slice(),
            [RegistryMsg::LeaveConn(LeaveRequest {
                conn: ConnectionId(1),
                room: RoomId(61),
                entity: 1,
                park: None,
            })]
        ),
        "{got:?}"
    );
}

/// A park: re-keyed (binding, session epoch, back-reference), its
/// outbound half released, and the live connection's leave no longer
/// reaches it.
#[tokio::test]
async fn a_sharded_park_is_left_behind_under_the_park_key() {
    let hold = Detach::Hold {
        grace: Some(Duration::from_secs(600)),
        to: ExpireTo::Despawn,
    };
    let (a, _obs, _disc) = rig_with(leave_room(), hold);
    let (tx, mut reg) = channel::<RegistryMsg>(64);
    let mut a = a.with_registry(tx);
    let _o1 = join(&mut a, ConnectionId(1), "ana");
    let entity = a.conns[&PlayerId(1)].entity;
    let t0 = Instant::now();
    step(&mut a, t0, 1, 30);
    let key = ConnectionId(1).park_key();
    let got = drain(&mut reg);
    assert!(
        matches!(
            got.as_slice(),
            [RegistryMsg::LeaveConn(LeaveRequest { park: Some(k), .. })] if *k == key
        ),
        "{got:?}"
    );
    let row = &a.conns[&PlayerId(1)];
    assert!(row.detached && row.conn == key && row.out.is_closed());
    assert_eq!(a.binding.get(&key), Some(&PlayerId(1)));
    assert_eq!(a.conn_epoch.get(&key), Some(&1), "the epoch moved along");
    assert!(!a.binding.contains_key(&ConnectionId(1)));
    assert!(!a.conn_epoch.contains_key(&ConnectionId(1)));

    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn: ConnectionId(1),
            entity,
            epoch: 0,
        },
        2,
    ));
    assert!(a.conns.contains_key(&PlayerId(1)), "the park is untouched");
}

/// A queued request goes when the same connection joins the shard again.
#[tokio::test]
async fn a_sharded_rejoin_drops_the_queued_request() {
    let (a, _obs, _disc) = rig_with(leave_room(), Detach::Despawn);
    let (tx, mut reg) = channel::<RegistryMsg>(1);
    tx.try_send(RegistryMsg::Shutdown).expect("the one slot");
    let mut a = a.with_registry(tx);
    let _o1 = join(&mut a, ConnectionId(1), "ana");
    let t0 = Instant::now();
    step(&mut a, t0, 1, 30);
    assert_eq!(a.leave_requests.len(), 1, "the request waits");
    let _o2 = join(&mut a, ConnectionId(1), "ana");
    assert!(a.leave_requests.is_empty(), "the rejoin dropped it");
    assert!(matches!(reg.try_recv(), Ok(RegistryMsg::Shutdown)));
}

/// A park that ends while its request still waits: the report goes
/// first and the request settles a despawn (the room actor's rule).
#[tokio::test]
async fn a_sharded_park_that_ended_first_settles_as_a_despawn() {
    let zero = Detach::Hold {
        grace: Some(Duration::ZERO),
        to: ExpireTo::Despawn,
    };
    let (a, _obs, _disc) = rig_with(leave_room(), zero);
    let (tx, mut reg) = channel::<RegistryMsg>(1);
    tx.try_send(RegistryMsg::Shutdown).expect("the one slot");
    let mut a = a.with_registry(tx);
    let _o1 = join(&mut a, ConnectionId(1), "ana");
    let t0 = Instant::now();
    step(&mut a, t0, 1, 30);
    let key = ConnectionId(1).park_key();
    assert_eq!(a.leave_requests[0].park, Some(key));
    step(&mut a, t0, 2, 31);
    assert!(a.conns.is_empty(), "the zero hold ended in a despawn");
    let mut order = drain(&mut reg);
    for k in 3..=6u64 {
        step(&mut a, t0, k, 30 + k);
        order.extend(drain(&mut reg));
    }
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
