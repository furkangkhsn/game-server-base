//! A member's path on a sharded room (BACKLOG B103): the shard's READ
//! diverts the marker like the room's, and a crossing hands the path to
//! the receiving shard (`PlayerMigration::path`) — the connection actor
//! sends only news, so a dropped path would stay unknown until the next
//! change.

use super::*;
use crate::path::{PathPhase, PathState, path_action};

fn paced(rate: u32) -> PathState {
    PathState {
        phase: PathPhase::Paced,
        rate: Some(rate),
        loss_permille: Some(120),
        ..Default::default()
    }
}

/// Conn 10 spawns at x = 0 — shard 1's region — so it crosses on the
/// first step, right after the READ that took its marker.
#[tokio::test]
async fn a_crossing_member_carries_its_path_to_the_next_shard() {
    let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (n1, mut n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let mut a = rig_actor(0, vec![n0, n1]);
    let p = PlayerId(10);
    let (reply, joined) = oneshot::channel();
    let (out, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(10),
            epoch: 1,
            identity: String::new(),
            out,
            reply,
            claims: None,
        },
        1,
    ));
    let (_entity, actions) = joined.await.expect("reply").expect("joined");
    let stamped = a.idle.last(p).expect("a join starts the clock");
    actions
        .try_send(path_action(ConnectionId(10), &paced(40_000)))
        .expect("room for the marker");
    a.step_phases(&tinfo(2));
    let msg = n1_rx.try_recv().expect("the member crossed to shard 1");
    let ShardMsg::Migrate {
        player: Some(moved),
        ..
    } = &msg
    else {
        panic!("a player migration");
    };
    assert_eq!(moved.player, p);
    assert_eq!(moved.path, Some(paced(40_000)), "the path rides along");
    assert_eq!(
        moved.last_input,
        Some(stamped),
        "the marker is not input: the idle stamp is the join's"
    );
    assert_eq!(a.paths.get(p), None, "and leaves this shard's table");
    let (m0, _m0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (m1, _m1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let mut b = rig_actor(1, vec![m0, m1]);
    // Installed one tick after the crossing was sampled (the gate).
    assert!(b.handle_msg(msg, 3));
    assert_eq!(b.paths.get(p), Some(paced(40_000)), "the receiver knows it");
}

/// The shard's fan-out asks the same gate: a member whose frame is over
/// its budget gets no group frame (counted); a member with no path gets
/// it as always; a budget the frame fits ships.
#[tokio::test]
async fn the_shard_s_fan_out_withholds_what_the_logic_withholds() {
    let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (n1, _n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let mut a = rig_actor(0, vec![n0, n1]);
    let mut join = |conn: u64| {
        let (reply, mut joined) = oneshot::channel();
        let (out, out_rx) = mpsc::channel::<FrameBatch>(8);
        assert!(a.handle_msg(
            ShardMsg::Join {
                conn: ConnectionId(conn),
                epoch: 1,
                identity: String::new(),
                out,
                reply,
                claims: None,
            },
            1,
        ));
        let (_e, actions) = joined.try_recv().expect("sync").expect("joined");
        (actions, out_rx)
    };
    // Conns 1..=3 spawn at x = -9..-7: shard 0's own region.
    let (tight, mut tight_out) = join(1);
    let (_none, mut none_out) = join(2);
    let (roomy, mut roomy_out) = join(3);
    tight
        .try_send(path_action(ConnectionId(1), &paced(30)))
        .expect("room");
    roomy
        .try_send(path_action(ConnectionId(3), &paced(30_000)))
        .expect("room");
    a.step_phases(&tinfo(2));
    assert!(
        tight_out.try_recv().is_err(),
        "1 B/tick: the 48 B frame is withheld"
    );
    assert!(none_out.try_recv().is_ok(), "no path: as always");
    assert!(roomy_out.try_recv().is_ok(), "999 B/tick: the frame fits");
    assert_eq!(a.m.snapshots_withheld, 1);
}
