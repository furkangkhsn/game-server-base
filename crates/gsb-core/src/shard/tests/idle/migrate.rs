//! The input-idle stamp crosses a shard seam with its member
//! (`PlayerMigration::last_input`, `docs/RECONNECT.md` §16 "Shard"): the
//! MIGRATE phase must see the shard's REAL clock. It runs inside the tick
//! body, which lends the clock to the game's hooks through the tick
//! context — so the body hands the clock back before MIGRATE runs.

use super::*;

/// A member that crosses on the first tick (conn 10 spawns at x = 0,
/// shard 1's region) carries its stamp to the neighbour and leaves this
/// shard's clock. Read against the lent-out (empty) clock, the stamp was
/// `None` — the receiving shard then started no clock at all, so a
/// crossing took an idle player off the ceiling for good — and the
/// sender kept a stale slot.
#[tokio::test]
async fn a_migrating_member_carries_its_input_stamp() {
    let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (n1, mut n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let mut a = rig_actor(0, vec![n0, n1]);
    let p = PlayerId(10);
    let _entity = join_direct(&mut a, ConnectionId(10), 1, 1).await;
    let stamped = a.idle.last(p).expect("a join starts the clock");
    a.step_phases(&tinfo(2));
    let Ok(ShardMsg::Migrate {
        player: Some(moved),
        ..
    }) = n1_rx.try_recv()
    else {
        panic!("the member crossed to shard 1");
    };
    assert_eq!(moved.player, p);
    assert_eq!(moved.last_input, Some(stamped), "the stamp rides along");
    assert_eq!(a.idle.last(p), None, "and leaves this shard's clock");
}
