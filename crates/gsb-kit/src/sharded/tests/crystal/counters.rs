//! Crystallization's counters (F9): a room that opted in reports its six
//! `crystal_*` counters through the core's logic-counter seam — the
//! movers it pinned, the holds it ended by cause, the contacts the
//! table's cap refused and the table's peak — and a room that did not
//! reports none.

use gsb_core::metrics::LogicCounters;
use gsb_core::room::GameLogic;

use super::hold::{hand_over, quiet, walk};
use super::*;

/// The room's counters, as `(name, value)` in the order it put them.
fn counted(world: &World, room: &Duel) -> Vec<(String, u64)> {
    let mut out = LogicCounters::new();
    room.logic_counters(world, &mut out);
    out.slots()
        .iter()
        .map(|s| (s.counter.name().to_owned(), s.value))
        .collect()
}

/// One counter's value (`None`: the room does not report it).
fn count(world: &World, room: &Duel, name: &str) -> Option<u64> {
    counted(world, room)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
}

#[test]
fn only_a_room_that_opted_in_reports_the_six() {
    let w = World::new();
    assert_eq!(counted(&w, &duel(0, 2, None)), [], "no crystallization");
    let names: Vec<String> = counted(&w, &duel(0, 2, Some(POLICY)))
        .into_iter()
        .map(|(n, v)| {
            assert_eq!(v, 0, "{n} starts at zero");
            n
        })
        .collect();
    assert_eq!(
        names,
        [
            "crystal_moves",
            "crystal_release_quiet",
            "crystal_release_band",
            "crystal_release_partner",
            "crystal_untracked",
            "crystal_fights_peak",
        ]
    );
}

/// The mover's shard counts the move; the holding shard counts the two
/// holds (the pair) ending when the fight goes quiet.
#[test]
fn a_move_and_a_quiet_release_are_counted() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = duel(0, 2, Some(POLICY));
    let mut s1 = duel(1, 2, Some(POLICY));
    let a = spawn(&mut w0, &mut s0, 1, -2.0, 0.0);
    let b = spawn(&mut w1, &mut s1, 2, 2.0, 0.0);
    let mut t = (1..=40)
        .find(|&t| !duel_tick(&mut w1, &mut s1, t, (b, a, 0, -2)).is_empty())
        .expect("the fight crystallizes");
    assert_eq!(count(&w1, &s1, "crystal_moves"), Some(1));
    assert_eq!(count(&w1, &s1, "crystal_fights_peak"), Some(1));
    hand_over((&mut w1, &mut s1), (&mut w0, &mut s0), b);
    assert_eq!(
        count(&w0, &s0, "crystal_moves"),
        Some(0),
        "not the anchor's"
    );
    for _ in 0..=POLICY.release {
        t += 1;
        quiet(&mut w0, &mut s0, t, &[]);
    }
    assert_eq!(count(&w0, &s0, "crystal_release_quiet"), Some(2), "A and B");
    assert_eq!(count(&w0, &s0, "crystal_release_band"), Some(0));
    assert_eq!(count(&w0, &s0, "crystal_release_partner"), Some(0));
    assert_eq!(count(&w1, &s1, "crystal_release_quiet"), Some(0));
}

/// B strays out of the band: its hold ends (BAND); once B has gone
/// home, A's reason to stay is gone too (PARTNER).
#[test]
fn a_band_and_a_partner_release_are_counted() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = duel(0, 2, Some(POLICY));
    let mut s1 = duel(1, 2, Some(POLICY));
    let a = spawn(&mut w0, &mut s0, 1, -2.0, 0.0);
    let b = spawn(&mut w1, &mut s1, 2, 4.0, 0.0);
    let t = (1..=40)
        .find(|&t| !duel_tick(&mut w1, &mut s1, t, (b, a, 0, -2)).is_empty())
        .expect("the fight crystallizes");
    hand_over((&mut w1, &mut s1), (&mut w0, &mut s0), b);
    walk(&mut w0, &s0, b, 10.5, 0.0);
    assert_eq!(quiet(&mut w0, &mut s0, t + 1, &[(a, b)]), [(1, b, None)]);
    assert_eq!(count(&w0, &s0, "crystal_release_band"), Some(1));
    hand_over((&mut w0, &mut s0), (&mut w1, &mut s1), b);
    quiet(&mut w0, &mut s0, t + 2, &[]);
    assert_eq!(count(&w0, &s0, "crystal_release_partner"), Some(1));
    assert_eq!(count(&w0, &s0, "crystal_release_quiet"), Some(0));
}

/// The table at its cap: every refused contact counted, the peak at
/// the cap — and still there once the table has emptied (a high-water
/// mark).
#[test]
fn the_cap_and_the_peak_are_counted() {
    let mut w1 = World::new();
    let mut s1 = duel(1, 2, Some(POLICY));
    let b = spawn(&mut w1, &mut s1, 2, 2.0, 0.0);
    let cap = crate::sharded::crystal::FIGHT_CAP as u64;
    let mut stage = stage(1, 1, &[]);
    for n in 1..=cap + 7 {
        let foe = interleaved_id(0, 2, n);
        s1.apply_remote_effect(&mut w1, 1, &hit(b, foe, 0, n), &mut stage.seam());
    }
    assert_eq!(count(&w1, &s1, "crystal_untracked"), Some(7));
    assert_eq!(count(&w1, &s1, "crystal_fights_peak"), Some(cap));
    s1.update_seam(&mut w1, &ctx(cap + 8 + POLICY.window), &mut stage.seam());
    assert!(s1.crystal.as_ref().unwrap().book.fights.is_empty());
    assert_eq!(count(&w1, &s1, "crystal_fights_peak"), Some(cap));
}
