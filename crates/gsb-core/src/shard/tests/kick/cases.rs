//! The shard kick's edges: the crossing tick, the park that crosses,
//! the kick asked after the crossing, the broadcast-phase kick of a
//! member that stays, and the no-op of a parked or unknown player.

use super::*;

/// A member kicked by a systems hook in the tick it would cross the seam
/// is kicked HERE, before MIGRATE: it never reaches the neighbour.
#[tokio::test]
async fn a_member_kicked_in_its_crossing_tick_never_crosses() {
    let mut r = Rig::new(Detach::Despawn, vec![kick(2, At::Update, 10, "x")]);
    let _e = r.join(ConnectionId(10), "cem");
    r.step(2);
    assert_eq!(r.disconnects().len(), 1, "kicked on this shard");
    assert!(r.crossed().is_empty(), "the kicked member did not cross");
    r.step(3);
    assert_eq!(r.closes().len(), 1);
}

/// A PARKED kicked member is still an entity: it crosses as a park (its
/// detach flags ride along, RECONNECT §14.2) and its close is sent.
#[tokio::test]
async fn a_member_parked_by_its_kick_crosses_as_a_park() {
    let hold = Detach::Hold {
        grace: Some(Duration::from_secs(600)),
        to: ExpireTo::Despawn,
    };
    let mut r = Rig::new(hold, vec![kick(2, At::Update, 10, "x")]);
    let _e = r.join(ConnectionId(10), "cem");
    r.step(2);
    assert_eq!(r.crossed(), vec![(PlayerId(10), true)], "crossed parked");
    r.step(3);
    assert_eq!(r.closes().len(), 1, "and its connection is closed");
}

/// A kick asked AFTER MIGRATE (a broadcast-phase hook) of a member that
/// crossed in that MIGRATE is a no-op: this shard no longer owned it
/// when the hook asked.
#[tokio::test]
async fn a_kick_asked_after_the_member_crossed_is_a_no_op() {
    let mut r = Rig::new(Detach::Despawn, vec![kick(2, At::Snapshot, 10, "late")]);
    let _stay = r.join(ConnectionId(1), "ana");
    let _e = r.join(ConnectionId(10), "cem");
    r.step(2);
    assert_eq!(r.crossed(), vec![(PlayerId(10), false)], "it crossed");
    r.step(3);
    assert!(r.disconnects().is_empty(), "no policy call here");
    assert!(r.closes().is_empty(), "and no close");
}

/// A row a transport death parked (and an unknown player) is not a live
/// member: the kick reaches no policy and asks for no close.
#[tokio::test]
async fn a_shard_kick_of_a_parked_or_unknown_player_is_a_no_op() {
    let hold = Detach::Hold {
        grace: Some(Duration::from_secs(600)),
        to: ExpireTo::Despawn,
    };
    let plan = vec![
        kick(2, At::Update, 1, "parked"),
        kick(2, At::Update, 99, "?"),
    ];
    let mut r = Rig::new(hold, plan);
    let entity = r.join(ConnectionId(1), "ana");
    assert!(r.a.handle_msg(
        ShardMsg::Detach {
            conn: ConnectionId(1),
            entity,
            identity: "ana".into(),
        },
        1,
    ));
    assert_eq!(r.disconnects().len(), 1, "the transport death parked it");
    r.step(2);
    r.step(3);
    assert!(r.disconnects().is_empty(), "no kick reached the policy");
    assert!(r.a.conns[&PlayerId(1)].detached, "the park is untouched");
    assert!(r.closes().is_empty());
}

/// A kick asked in a broadcast-phase hook of a member that is still this
/// shard's is applied at the end of the tick.
#[tokio::test]
async fn a_broadcast_phase_kick_is_applied_at_the_end_of_the_tick() {
    let mut r = Rig::new(Detach::Despawn, vec![kick(2, At::Snapshot, 1, "late")]);
    let _e = r.join(ConnectionId(1), "ana");
    r.step(2);
    assert_eq!(r.disconnects(), vec![(PlayerId(1), "ana".to_string())]);
    r.step(3);
    let got = r.closes();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].reason, "kicked: late");
}
