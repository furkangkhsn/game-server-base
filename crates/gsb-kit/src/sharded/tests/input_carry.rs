//! A player's input session crosses a seam with it (GAME-MODULE §5, G2
//! findings K1–K3): the high-water mark and the pending ack travel in
//! the migration state, the destination acks what the source processed
//! but never reported, the sequence rule keeps its mark, and the source
//! forgets the session once the move commits.

use super::*;
use crate::testing::private::Payload;

/// Join a player on `room` at an exact position: `(player, wire)`.
fn join_at(
    world: &mut World,
    room: &mut ShardedRoom,
    conn: ConnectionId,
    x: f32,
) -> (PlayerId, u64) {
    let admission = room.on_join(world, conn);
    let entity = room.player_entity[&admission.player];
    world.entity_mut(entity).insert(Position { x, y: -10.0 });
    (admission.player, admission.entity)
}

/// Move `player`'s entity on `room` to `x` (same row).
fn move_to(world: &mut World, room: &ShardedRoom, player: PlayerId, x: f32) {
    let entity = room.player_entity[&player];
    world.entity_mut(entity).insert(Position { x, y: -10.0 });
}

/// The ack a private frame carries (`None`: no frame, or no ack in it).
fn ack_of(emitted: bool, out: &bytes::BytesMut) -> Option<u64> {
    if !emitted {
        return None;
    }
    match crate::testing::Private::decode(out.as_ref())
        .expect("private frame")
        .payload
    {
        Some(Payload::Ack(a)) => Some(a.processed_up_to),
        _ => None,
    }
}

/// `player`'s private frame on `room` this tick: its ack, if any.
fn private_ack(world: &mut World, room: &mut ShardedRoom, player: PlayerId) -> Option<u64> {
    let mut out = bytes::BytesMut::new();
    let emitted = room.private(world, player, &(), &[], &mut out);
    ack_of(emitted, &out)
}

/// Shard 0 → shard 1 (2×2 grid, the x = 0 seam): a player whose input
/// numbered 3 was acked on shard 0 processes input 5 (the one that moves
/// it) in the tick it crosses. Returns both worlds and shards after the
/// move, the source NOT yet told the move committed.
fn crossed_with_pending_5() -> (World, ShardedRoom, World, ShardedRoom, PlayerId, u64) {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = ShardedRoom::new(0, 4, 50.0);
    let mut s1 = ShardedRoom::new(1, 4, 50.0);
    let (p, wire) = join_at(&mut w0, &mut s0, ConnectionId(1), -1.0);
    assert!(s0.input.admit(p, 3));
    assert_eq!(private_ack(&mut w0, &mut s0, p), Some(3), "3 acked at home");
    assert!(s0.input.admit(p, 5), "the input that moves it");
    move_to(&mut w0, &s0, p, 1.0);
    let m = s0.collect_migrations(&mut w0, 1).pop().expect("crossing");
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    (w0, s0, w1, s1, p, wire)
}

/// K1: the source hands the session off before its broadcast would ack
/// input 5 — the destination sends that ack, once.
#[test]
fn k1_the_input_that_moves_a_player_is_acked_by_the_destination() {
    let (_, _, mut w1, mut s1, p, _) = crossed_with_pending_5();
    assert_eq!(private_ack(&mut w1, &mut s1, p), Some(5), "the move's ack");
    assert_eq!(private_ack(&mut w1, &mut s1, p), None, "acked once");
}

/// K1's other half: an input the source already acked is not acked again
/// on arrival (the carried `acked` is honoured, not just the mark).
#[test]
fn an_input_acked_before_the_crossing_is_not_acked_again() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = ShardedRoom::new(0, 4, 50.0);
    let mut s1 = ShardedRoom::new(1, 4, 50.0);
    let (p, _) = join_at(&mut w0, &mut s0, ConnectionId(1), -1.0);
    assert!(s0.input.admit(p, 4));
    assert_eq!(private_ack(&mut w0, &mut s0, p), Some(4));
    move_to(&mut w0, &s0, p, 1.0);
    let m = s0.collect_migrations(&mut w0, 1).pop().expect("crossing");
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    assert_eq!(private_ack(&mut w1, &mut s1, p), None, "nothing pending");
    assert!(!s1.input.admit(p, 4), "the mark came along");
}

/// K2: the sequence rule keeps its mark across the seam — an input
/// numbered at or below the move's is a duplicate/late datagram there
/// too; a newer one is processed.
#[test]
fn k2_the_sequence_rule_keeps_its_mark_across_a_migration() {
    let (_, _, _, mut s1, p, _) = crossed_with_pending_5();
    assert!(!s1.input.admit(p, 4), "older than the move: dropped");
    assert!(
        !s1.input.admit(p, 5),
        "the move itself, duplicated: dropped"
    );
    assert!(s1.input.admit(p, 6), "newer: processed");
}

/// K3: once the move commits (`on_migrate_out`), the source forgets the
/// player's input session — no entry leaks per migration.
#[test]
fn k3_the_source_forgets_the_input_session_when_the_move_commits() {
    let (mut w0, mut s0, _, _, p, wire) = crossed_with_pending_5();
    assert_eq!(
        s0.input.mark(p),
        Some((5, 3, false)),
        "kept until the commit"
    );
    s0.on_migrate_out(&mut w0, wire);
    assert_eq!(s0.input.mark(p), None, "gone with the player");
}

/// A refused send (the neighbour's channel full: the core rolls the
/// session back and re-collects next tick) keeps the session on the
/// source intact — the rule still holds there, and the retry carries
/// the same mark and pending ack.
#[test]
fn a_refused_send_keeps_the_session_for_the_retry() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = ShardedRoom::new(0, 4, 50.0);
    let mut s1 = ShardedRoom::new(1, 4, 50.0);
    let (p, _) = join_at(&mut w0, &mut s0, ConnectionId(1), -1.0);
    assert!(s0.input.admit(p, 5));
    move_to(&mut w0, &s0, p, 1.0);
    drop(s0.collect_migrations(&mut w0, 1)); // the send was refused
    assert!(!s0.input.admit(p, 4), "the rule still holds on the source");
    let m = s0.collect_migrations(&mut w0, 1).pop().expect("retried");
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    assert_eq!(private_ack(&mut w1, &mut s1, p), Some(5));
}

/// A crossing into the diagonal region travels hop by hop (§8.4 — the
/// MMO's `Travel` to the far waystone): the intermediate shard installs
/// the session and hands it on unchanged.
#[test]
fn the_session_rides_every_hop_of_a_diagonal_crossing() {
    let (mut w0, mut w1, mut w3) = (World::new(), World::new(), World::new());
    let mut s0 = ShardedRoom::new(0, 4, 50.0);
    let mut s1 = ShardedRoom::new(1, 4, 50.0);
    let mut s3 = ShardedRoom::new(3, 4, 50.0);
    let (p, wire) = join_at(&mut w0, &mut s0, ConnectionId(1), -1.0);
    assert!(s0.input.admit(p, 7));
    let entity = s0.player_entity[&p];
    w0.entity_mut(entity).insert(Position { x: 1.0, y: 1.0 }); // region 3
    let m = s0.collect_migrations(&mut w0, 1).pop().expect("first hop");
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    let m = s1.collect_migrations(&mut w1, 3).pop().expect("second hop");
    s1.on_migrate_out(&mut w1, wire);
    assert_eq!(s1.input.mark(p), None, "the intermediate forgets it too");
    s3.on_migrate_in(&mut w3, m.wire, m.state, m.player);
    assert_eq!(private_ack(&mut w3, &mut s3, p), Some(7));
    assert!(!s3.input.admit(p, 6));
}

/// The spatial composite runs the same kit path: the move's ack reaches
/// the arrival once (a tick late when its first private frame is the
/// one-shot full — the ack cannot share the `Private` payload oneof with
/// it); the mark holds and the source forgets the session.
#[test]
fn the_spatial_composite_carries_the_session_too() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = ShardedSpatialRoom::new(0, 4, 50.0, 20.0);
    let mut s1 = ShardedSpatialRoom::new(1, 4, 50.0, 20.0);
    let wire = place_spatial(&mut w0, &mut s0, ConnectionId(1), -1.0, -10.0);
    let p = s0.inner.entity_player[&s0.inner.wire_entity[&wire]];
    assert!(s0.inner.input.admit(p, 5));
    let entity = s0.inner.player_entity[&p];
    w0.entity_mut(entity).insert(Position { x: 1.0, y: -10.0 });
    let m = s0.collect_migrations(&mut w0, 1).pop().expect("crossing");
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    s0.on_migrate_out(&mut w0, wire);
    assert_eq!(s0.inner.input.mark(p), None, "K3 on the composite");

    let mut arrival = |tick: u64| {
        s1.update(&mut w1, &ctx(tick));
        let mut out = bytes::BytesMut::new();
        s1.snapshot(&mut w1, &ctx(tick), &Cell(0, -1), &[], &mut out);
        let mut out = bytes::BytesMut::new();
        let emitted = s1.private(&mut w1, p, &Cell(0, -1), &[], &mut out);
        (emitted, out)
    };
    // The first frame is the arrival's full (the group's own, or the
    // one-shot); the ack rides it or the next one — exactly once.
    let acks: Vec<u64> = (1..=3)
        .filter_map(|tick| {
            let (emitted, out) = arrival(tick);
            ack_of(emitted, &out)
        })
        .collect();
    assert_eq!(acks, [5], "K1 on the composite");
    assert!(!s1.inner.input.admit(p, 4), "K2 on the composite");
}

/// F11: what a dropped batch owed — the ack its frame carried, the
/// session payload it carried — crosses the seam with the session: the
/// destination sends both, once.
#[test]
fn what_a_dropped_frame_owed_is_sent_by_the_destination() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = ShardedRoom::new(0, 4, 50.0);
    let mut s1 = ShardedRoom::new(1, 4, 50.0);
    let (p, _) = join_at(&mut w0, &mut s0, ConnectionId(1), -1.0);
    assert!(s0.input.admit(p, 3));
    assert_eq!(private_ack(&mut w0, &mut s0, p), Some(3));
    // The fixture has no session payload: mark the greeting as riding
    // the same frame, as a game's would.
    s0.input.carries_greeting();
    s0.on_batch_dropped(&mut w0, p, true);
    assert_eq!(s0.input.mark(p), Some((3, 0, true)), "both owed again");

    move_to(&mut w0, &s0, p, 1.0);
    let m = s0.collect_migrations(&mut w0, 1).pop().expect("crossing");
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    assert!(
        s1.input.take_greeting(p),
        "the greeting is the destination's"
    );
    assert_eq!(private_ack(&mut w1, &mut s1, p), Some(3), "the dropped ack");
    assert_eq!(private_ack(&mut w1, &mut s1, p), None, "once");
    assert!(!s1.input.take_greeting(p));
}
