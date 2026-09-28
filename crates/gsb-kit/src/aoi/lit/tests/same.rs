//! A game that lights nobody ships exactly the AOI room's frames: the
//! same scripted session through `AoiRoom<Fixture, Grid2>` and
//! `LitAoiRoom<Lamp, Grid2>` (the same codec) — joins, moves, crossings,
//! keep-alives, a dropped batch, a leave — gives every player the same
//! bytes on every tick. One record per cell throughout, so a frame's
//! record order does not depend on a hash map's.

use crate::aoi::AoiRoom;
use crate::aoi::lit::tests::*;
use crate::testing::Fixture;

/// The script: seat `i` sits in its own cell, `x = 10 + 20·i`, and the
/// seats move along y (each stays alone in its cell).
fn play<R: GameLogic<World>>(room: R) -> Vec<Vec<Vec<(bool, Bytes)>>>
where
    R::GroupKey: Hash + Eq + Clone,
{
    let mut sim = Sim::new(room);
    for i in 0..4 {
        sim.join(i + 1, 10.0 + 20.0 * i as f32, 10.0);
    }
    let mut ticks = Vec::new();
    for t in 0..30u64 {
        if t == 12 {
            let late = sim.join(9, 130.0, 10.0);
            assert_eq!(late, 4);
        }
        if t == 20 {
            let player = sim.seats[1].player;
            sim.room.on_leave(&mut sim.world, player);
            sim.seats.remove(1);
        }
        for i in 0..sim.seats.len() {
            let y = 10.0 + ((t + i as u64) % 7) as f32 * 7.0;
            let x = sim.seats[i].wire as f32 * 20.0 - 10.0;
            sim.at(i, x, y);
        }
        let dropped: &[usize] = if t == 8 { &[0] } else { &[] };
        sim.step_with(t % 10 == 9, dropped);
        ticks.push(sim.seats.iter().map(|s| s.frames.clone()).collect());
    }
    ticks
}

#[test]
fn a_game_that_lights_nobody_ships_the_aoi_rooms_bytes() {
    let aoi = play(AoiRoom::with_game(Fixture::default(), Grid2::new(EDGE)));
    let lit = play(lit_room());
    assert!(
        aoi.iter().flatten().flatten().count() > 60,
        "a busy session"
    );
    assert_eq!(aoi, lit);
}
