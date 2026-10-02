//! A member's path on a shard goes with its session (BACKLOG B103): a
//! park or a despawn empties it, as on the single room — the shard's own
//! funnels, not the room's.

use super::*;
use crate::path::{PathPhase, PathState, path_action};

fn paced(rate: u32) -> PathState {
    PathState {
        phase: PathPhase::Paced,
        rate: Some(rate),
        ..Default::default()
    }
}

/// Join conn 1 (x = -9: shard 0's own region, no crossing) and deliver
/// one path marker through a step.
fn joined_with_path(
    a: &mut ShardActor<TWorld, (), TState, TStrip>,
) -> (EntityId, mpsc::Receiver<FrameBatch>, Mailbox<Action>) {
    let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply_tx, mut reply_rx) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: "ana".into(),
            out: out_tx,
            reply: reply_tx,
            claims: None,
        },
        1,
    ));
    let (entity, actions) = reply_rx.try_recv().expect("sync").expect("joined");
    actions
        .try_send(path_action(ConnectionId(1), &paced(25_000)))
        .expect("room");
    a.step_phases(&tinfo(2));
    assert_eq!(a.paths.get(PlayerId(1)), Some(paced(25_000)));
    (entity, out_rx, actions)
}

#[tokio::test]
async fn a_parked_member_on_a_shard_has_no_path() {
    let (mut a, _obs, _disc) = rig(
        None,
        Detach::Hold {
            grace: Some(Duration::from_secs(60)),
            to: ExpireTo::Despawn,
        },
    );
    let (entity, _out, _actions) = joined_with_path(&mut a);
    assert!(a.handle_msg(
        ShardMsg::Detach {
            conn: ConnectionId(1),
            entity,
            identity: "ana".into(),
        },
        3,
    ));
    assert!(a.conns[&PlayerId(1)].detached, "parked");
    assert_eq!(a.paths.get(PlayerId(1)), None);
}

#[tokio::test]
async fn a_member_that_leaves_a_shard_takes_its_path() {
    let (mut a, _obs, _disc) = rig(None, Detach::Despawn);
    let (entity, _out, _actions) = joined_with_path(&mut a);
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn: ConnectionId(1),
            entity,
            epoch: 1,
        },
        3,
    ));
    assert!(!a.conns.contains_key(&PlayerId(1)));
    assert!(a.paths.is_empty());
}
