//! The shard's half of the ceiling's ACTION (E6): the room actor's rule,
//! mirrored — `Disconnect` asks the registry to close each expired
//! member's connection after the policy ran (parked or not), the default
//! asks nothing, and a row parked by a transport death is never closed.

use super::*;
use crate::conn::ServerClose;
use crate::registry::{CloseRequest, RegistryMsg};
use crate::room::AfkAction;

fn closes(rx: &mut mpsc::Receiver<RegistryMsg>) -> Vec<CloseRequest> {
    drain(rx)
        .into_iter()
        .filter_map(|m| match m {
            RegistryMsg::CloseConn(r) => Some(r),
            _ => None,
        })
        .collect()
}

fn step(a: &mut ShardActor<TWorld, (), TState, TStrip>, t0: Instant, k: u64, secs: u64) {
    a.step_phases(&TickInfo {
        tick: k,
        at: t0 + Duration::from_secs(secs),
    });
}

/// `Disconnect` on a shard: one request per expired member, with its
/// entity and the policy's outcome; `LeaveRoom` (the default) none.
#[tokio::test]
async fn sharded_disconnect_asks_for_the_close_and_the_default_does_not() {
    for (action, want) in [(AfkAction::LeaveRoom, 0), (AfkAction::Disconnect, 2)] {
        let cfg = RoomConfig {
            afk_action: action,
            ..config(Some(5))
        };
        let (a, _obs, mut disc) = rig_with(cfg, Detach::Despawn);
        let (tx, mut reg) = channel::<RegistryMsg>(64);
        let mut a = a.with_registry(tx);
        let _o1 = join(&mut a, ConnectionId(1), "ana");
        let _o2 = join(&mut a, ConnectionId(2), "bora");
        let t0 = Instant::now();
        for k in 1..=4u64 {
            step(&mut a, t0, k, 10 + k);
        }
        assert_eq!(
            drain(&mut disc).len(),
            2,
            "{action:?}: the policy ran for both"
        );
        let mut got = closes(&mut reg);
        assert_eq!(got.len(), want, "{action:?}: {got:?}");
        got.sort_by_key(|r| r.conn.0);
        for (i, r) in got.iter().enumerate() {
            assert_eq!(r.conn, ConnectionId(i as u64 + 1));
            assert_eq!((r.room, r.entity), (RoomId(61), i as u64 + 1));
            assert!(!r.parked, "the policy despawned");
            assert_eq!(r.cause, ServerClose::IdleInput);
        }
    }
}

/// A park by the policy is reported as parked; a park by a transport
/// death is off the clock and never closed.
#[tokio::test]
async fn sharded_parks_are_reported_or_left_alone() {
    let hold = Detach::Hold {
        grace: Some(Duration::from_secs(600)),
        to: ExpireTo::Despawn,
    };
    let cfg = RoomConfig {
        afk_action: AfkAction::Disconnect,
        ..config(Some(5))
    };
    let (a, _obs, _disc) = rig_with(cfg, hold);
    let (tx, mut reg) = channel::<RegistryMsg>(64);
    let mut a = a.with_registry(tx);
    let _o1 = join(&mut a, ConnectionId(1), "ana");
    let _o2 = join(&mut a, ConnectionId(2), "bora");
    let entity = a.conns[&PlayerId(2)].entity;
    assert!(a.handle_msg(
        ShardMsg::Detach {
            conn: ConnectionId(2),
            entity,
            identity: "bora".into(),
        },
        1,
    ));
    let t0 = Instant::now();
    for k in 1..=10u64 {
        step(&mut a, t0, k, 10 * k);
    }
    let got = closes(&mut reg);
    assert_eq!(got.len(), 1, "only the idle member: {got:?}");
    assert_eq!(got[0].conn, ConnectionId(1));
    assert!(got[0].parked, "the policy parked it");
    assert!(a.conns[&PlayerId(1)].out.is_closed(), "its queue let go");
    assert!(
        !a.conns[&PlayerId(2)].out.is_closed(),
        "the transport death's park is not the ceiling's to touch"
    );
}
