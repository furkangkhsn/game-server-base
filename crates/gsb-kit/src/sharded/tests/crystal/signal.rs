//! The signal: when a fight is ripe (K ticks, both directions), who
//! moves (the higher wire id), what does not count (one direction, a
//! brief exchange), what a room that did not opt in does (nothing new),
//! and how much state it keeps (bounded).

use super::*;

/// A (shard 0, the lower wire) and B (shard 1) trade hits across x = 0
/// from tick 1 on. B's shard reports B's move to shard 0 at tick
/// 1 + K exactly — not a tick earlier — carrying its pin; A's shard never
/// moves A (the lower wire stays).
#[test]
fn a_fight_crystallizes_after_k_ticks_and_only_the_higher_wire_moves() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = duel(0, 2, Some(POLICY));
    let mut s1 = duel(1, 2, Some(POLICY));
    let a = spawn(&mut w0, &mut s0, 1, -2.0, 0.0);
    let b = spawn(&mut w1, &mut s1, 2, 2.0, 0.0);
    assert!(a < b, "shard ranges: {a} < {b}");

    let mut first = None;
    for t in 1..=40 {
        let mv = duel_tick(&mut w1, &mut s1, t, (b, a, 0, -2));
        if first.is_none() && !mv.is_empty() {
            assert_eq!(
                mv,
                [(
                    0,
                    b,
                    Some(ShardPin {
                        partner: a,
                        last: t
                    })
                )]
            );
            first = Some(t);
        }
        // The mirror: A's shard sees the same fight, never moves A.
        let mv = duel_tick(&mut w0, &mut s0, t, (a, b, 1, 2));
        assert_eq!(mv, [], "the lower wire stays (tick {t})");
    }
    assert_eq!(
        first,
        Some(1 + POLICY.after),
        "K ticks after the first contact"
    );
}

/// One direction only (a sniper, a mob that does not strike back) and a
/// brief exchange shorter than K: nothing moves.
#[test]
fn one_sided_or_brief_contact_does_not_crystallize() {
    let mut w1 = World::new();
    let mut s1 = duel(1, 2, Some(POLICY));
    let a = interleaved_id(0, 2, 1); // a wire of shard 0's: a < b
    let b = spawn(&mut w1, &mut s1, 2, 2.0, 0.0);
    for t in 1..=60 {
        let mut stage = stage(1, t, &[(0, a, -2, 0)]);
        s1.game_mut().strike.push((b, a)); // B only
        s1.update_seam(&mut w1, &ctx(t), &mut stage.seam());
        assert_eq!(moves(&mut w1, &mut s1), [], "one-sided (tick {t})");
    }
    for t in 100..=100 + POLICY.after + 20 {
        let brief = t < 100 + POLICY.after - 2;
        let mv = if brief {
            duel_tick(&mut w1, &mut s1, t, (b, a, 0, -2))
        } else {
            let mut stage = stage(1, t, &[(0, a, -2, 0)]);
            s1.update_seam(&mut w1, &ctx(t), &mut stage.seam());
            moves(&mut w1, &mut s1)
        };
        assert_eq!(mv, [], "a brief exchange (tick {t})");
    }
}

/// A room that did not opt in: the same sustained fight moves nothing,
/// carries no pin and keeps no state — and a region crossing is
/// reported exactly as before.
#[test]
fn a_room_that_did_not_opt_in_behaves_as_before() {
    let mut w1 = World::new();
    let mut s1 = duel(1, 2, None);
    let a = interleaved_id(0, 2, 1);
    let b = spawn(&mut w1, &mut s1, 2, 2.0, 0.0);
    for t in 1..=60 {
        assert_eq!(duel_tick(&mut w1, &mut s1, t, (b, a, 0, -2)), []);
    }
    assert!(s1.crystal.is_none(), "no fight table, no pins");
    let entity = s1.wire_entity[&b];
    w1.entity_mut(entity).insert(Position { x: -1.0, y: 0.0 });
    assert_eq!(moves(&mut w1, &mut s1), [(0, b, None)], "the region's move");
}

/// The fight table never outgrows its cap, whatever the traffic, and
/// empties once the fights are over; a despawned held entity drops its
/// pin.
#[test]
fn the_state_stays_bounded() {
    let mut w1 = World::new();
    let mut s1 = duel(1, 2, Some(POLICY));
    let b = spawn(&mut w1, &mut s1, 2, 2.0, 0.0);
    // Shard 0's wires (the odd values of a 2-shard room): never B's.
    let foes: Vec<u64> = (1..=3 * crate::sharded::crystal::FIGHT_CAP as u64)
        .map(|n| interleaved_id(0, 2, n))
        .collect();
    let mut stage = stage(1, 1, &[]);
    for (at, &foe) in (1..).zip(&foes) {
        let got = s1.apply_remote_effect(&mut w1, 1, &hit(b, foe, 0, at), &mut stage.seam());
        assert_eq!(got, EffectOutcome::Applied);
    }
    let book = &s1.crystal.as_ref().expect("opted in").book;
    assert_eq!(book.fights.len(), crate::sharded::crystal::FIGHT_CAP);
    assert_eq!(
        book.untracked,
        2 * crate::sharded::crystal::FIGHT_CAP as u64
    );

    let t = 2 + POLICY.window;
    s1.update_seam(&mut w1, &ctx(t), &mut stage.seam());
    assert!(
        s1.crystal.as_ref().unwrap().book.fights.is_empty(),
        "expired"
    );

    s1.crystal.as_mut().unwrap().pins.insert(
        b,
        crate::sharded::crystal::Pin {
            anchor: 1,
            partner: 7,
            last: t,
        },
    );
    let entity = s1.wire_entity[&b];
    s1.on_leave(&mut w1, s1.entity_player[&entity]);
    s1.update_seam(&mut w1, &ctx(t + 1), &mut stage.seam());
    assert!(
        s1.crystal.as_ref().unwrap().pins.is_empty(),
        "a gone entity's pin"
    );
}
