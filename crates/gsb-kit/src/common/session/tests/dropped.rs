//! A fan-out drop (F11), through EVERY kit room: the dropped frame's
//! one-shot content — the session payload, the delta rooms' one-shot
//! full — rides the player's next frame; a drop that carried none of it
//! changes nothing; a lost GROUP frame costs a delta room's member a
//! one-shot full and a full room's member nothing.

use super::*;
use crate::sharded::ShardedTeamRoom;
use crate::testing::fix_lent_pos;

/// Tick `n` over `players` (in that order, like one fan-out); then, if
/// `drop` names one of them, report ITS batch dropped — as the core does,
/// right after that player's `private` (so it must be the last one).
fn tick_dropping<R: GameLogic<World>>(
    room: &mut R,
    world: &mut World,
    n: u64,
    players: &[PlayerId],
    drop: Option<(PlayerId, bool)>,
) -> Vec<Option<BytesMut>> {
    let frames = tick(room, world, n, players);
    if let Some((player, snapshot)) = drop {
        assert_eq!(players.last(), Some(&player), "reported after its private");
        room.on_batch_dropped(world, player, snapshot);
    }
    frames
}

fn check<R: GameLogic<World>>(name: &str, mut room: R, delta: bool) {
    let mut world = World::new();
    let a = room.on_join(&mut world, ConnectionId(1));
    tick(&mut room, &mut world, 1, &[a.player]);
    let b = room.on_join(&mut world, ConnectionId(3));
    let (ap, bp) = (a.player, b.player);
    let greeting = Some(vec![0xA5, b.entity as u8]);

    // b's first batch — its greeting, and in a delta room its one-shot
    // full — is dropped (the private frame alone: no group frame lost).
    let frames = tick_dropping(&mut room, &mut world, 2, &[ap, bp], Some((bp, false)));
    assert_eq!(game_of(&frames[1]), greeting, "{name}");
    assert_eq!(is_one_shot_full(&frames[1]), delta, "{name}");

    let frames = tick(&mut room, &mut world, 3, &[ap, bp]);
    assert!(frames[0].is_none(), "{name}: a is not re-told");
    assert_eq!(game_of(&frames[1]), greeting, "{name}: the greeting again");
    assert_eq!(
        is_one_shot_full(&frames[1]),
        delta,
        "{name}: a delta room re-sends the one-shot full"
    );
    let frames = tick(&mut room, &mut world, 4, &[ap, bp]);
    assert!(frames.iter().all(Option::is_none), "{name}: delivered");

    // A dropped batch without one-shot content and without the group
    // frame: nothing to re-arm.
    tick_dropping(&mut room, &mut world, 5, &[ap, bp], Some((bp, false)));
    let frames = tick(&mut room, &mut world, 6, &[ap, bp]);
    assert!(frames.iter().all(Option::is_none), "{name}: nothing owed");

    // The group frame was lost: a delta room's member may hold a stale
    // view — a one-shot full (and nothing else) heals it; a full room's
    // next group frame is self-contained, so nothing.
    tick_dropping(&mut room, &mut world, 7, &[bp, ap], Some((ap, true)));
    let frames = tick(&mut room, &mut world, 8, &[ap, bp]);
    assert_eq!(is_one_shot_full(&frames[0]), delta, "{name}");
    assert_eq!(game_of(&frames[0]), None, "{name}: no greeting");
    assert!(frames[1].is_none(), "{name}: b holds its view");

    // A player that leaves between the drop and its next frame: nothing
    // is owed to anyone, and the next joiner is told exactly once.
    let c = room.on_join(&mut world, ConnectionId(5));
    tick_dropping(
        &mut room,
        &mut world,
        9,
        &[ap, bp, c.player],
        Some((c.player, false)),
    );
    room.on_leave(&mut world, c.player);
    let frames = tick(&mut room, &mut world, 10, &[ap, bp]);
    assert!(
        frames.iter().all(Option::is_none),
        "{name}: after the leave"
    );
    let d = room.on_join(&mut world, ConnectionId(7));
    let frames = tick(&mut room, &mut world, 11, &[ap, bp, d.player]);
    assert_eq!(game_of(&frames[2]), Some(vec![0xA5, d.entity as u8]));
    let frames = tick(&mut room, &mut world, 12, &[ap, bp, d.player]);
    assert!(frames.iter().all(Option::is_none), "{name}: once");
}

/// Every room re-arms what the dropped frame carried, and only that.
#[test]
fn every_room_rearms_what_a_dropped_frame_carried() {
    let g = Greeting::default;
    check("open", OpenRoom::with_game(g()), false);
    check("aoi", AoiRoom::with_game(g(), Grid2::new(20.0)), true);
    check(
        "team",
        TeamRoom::with_game(g(), VisionGrid2::<Position>::new(10.0)),
        false,
    );
    check(
        "team (delta)",
        TeamRoom::with_game(g(), VisionGrid2::<Position>::new(10.0)).with_delta(),
        true,
    );
    check("pvs", SectorRoom::with_game(g(), fixture_map()), false);
    check("sharded", sharded(g()), false);
    check(
        "sharded × spatial",
        ShardedSpatialRoom::with_shard(sharded(g()), Grid2::new(20.0)),
        true,
    );
    let team = |delta: bool| {
        let room = ShardedTeamRoom::with_shard(sharded(g()), VisionGrid2::new(10.0), fix_lent_pos);
        if delta { room.with_delta() } else { room }
    };
    check("sharded × team", team(false), false);
    check("sharded × team (delta)", team(true), true);
}
