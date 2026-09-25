//! The hold: a crystallized pair stays on its shard however long the
//! fight and wherever it steps inside the band, goes back to its region
//! once the fight is over (or it leaves the band) — once, no
//! oscillation — and a held entity is never moved by another fight.

use super::*;

/// B moves from shard 1 to shard 0 the way the core does it: collected
/// on shard 1, installed on shard 0, despawned on shard 1.
fn hand_over(from: (&mut World, &mut Duel), to: (&mut World, &mut Duel), wire: u64) {
    let (wf, sf) = from;
    let (wt, st) = to;
    let target = st.index();
    let m = sf
        .collect_migrations(wf, target)
        .into_iter()
        .find(|m| m.wire == wire)
        .expect("the move is due");
    st.on_migrate_in(wt, m.wire, m.state, m.player);
    sf.on_migrate_out(wf, wire);
}

/// Put `wire`'s entity at `(x, y)` on `room`.
fn walk(world: &mut World, room: &Duel, wire: u64, x: f32, y: f32) {
    let entity = room.wire_entity[&wire];
    world.entity_mut(entity).insert(Position { x, y });
}

/// A quiet tick on `room`, plus `local` hits: the moves it produces.
fn quiet(
    world: &mut World,
    room: &mut Duel,
    t: u64,
    local: &[(u64, u64)],
) -> Vec<(usize, u64, Option<ShardPin>)> {
    room.game_mut().local.extend_from_slice(local);
    let index = room.index();
    room.update_seam(world, &ctx(t), &mut stage(index, t, &[]).seam());
    moves(world, room)
}

/// Crystallized onto shard 0, A and B fight on for 300 ticks with B in
/// shard 1's region, B wandering back and forth across the seam inside
/// the band: nothing moves. The fight ends; `release` ticks later B —
/// and only B — goes back to its region, once; nothing moves after.
#[test]
fn a_held_fight_does_not_flip_and_returns_to_the_region_once_it_is_over() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = duel(0, 2, Some(POLICY));
    let mut s1 = duel(1, 2, Some(POLICY));
    let a = spawn(&mut w0, &mut s0, 1, -2.0, 0.0);
    let b = spawn(&mut w1, &mut s1, 2, 2.0, 0.0);
    let mut t = (1..=40)
        .find(|&t| !duel_tick(&mut w1, &mut s1, t, (b, a, 0, -2)).is_empty())
        .expect("the fight crystallizes");
    hand_over((&mut w1, &mut s1), (&mut w0, &mut s0), b);
    let pins = &s0.crystal.as_ref().unwrap().pins;
    assert_eq!(
        (pins[&a].anchor, pins[&b].anchor),
        (0, 0),
        "the pair is held"
    );

    let mut last = t;
    for i in 0..300u64 {
        t += 1;
        walk(
            &mut w0,
            &s0,
            b,
            [2.0, -3.0, 7.0, 4.0][(i / 40 % 4) as usize],
            0.0,
        );
        let local: &[(u64, u64)] = if i % 3 == 0 { &[(a, b), (b, a)] } else { &[] };
        if !local.is_empty() {
            last = t;
        }
        assert_eq!(quiet(&mut w0, &mut s0, t, local), [], "held (tick {t})");
        assert_eq!(quiet(&mut w1, &mut s1, t, &[]), []);
    }
    walk(&mut w0, &s0, b, 4.0, 0.0);
    let mut back = None;
    for _ in 0..3 * POLICY.release {
        t += 1;
        let mv = quiet(&mut w0, &mut s0, t, &[]);
        if back.is_none() && !mv.is_empty() {
            assert_eq!(mv, [(1, b, None)], "B only, no pin");
            back = Some(t);
            hand_over((&mut w0, &mut s0), (&mut w1, &mut s1), b);
        } else {
            assert_eq!(mv, [], "once (tick {t})");
        }
        assert_eq!(quiet(&mut w1, &mut s1, t, &[]), [], "no bounce (tick {t})");
    }
    assert_eq!(back, Some(last + POLICY.release + 1), "released when quiet");
    assert!(
        s0.crystal.as_ref().unwrap().pins.is_empty(),
        "A released too"
    );
}

/// The band: a mover must stand within HALF the margin of the partner's
/// shard; a held entity is released at the margin — and then, fighting
/// on beyond half of it, is not pinned again (no oscillation at the
/// band's edge).
#[test]
fn the_band_gates_the_move_and_ends_the_hold() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = duel(0, 2, Some(POLICY));
    let mut s1 = duel(1, 2, Some(POLICY));
    let a = spawn(&mut w0, &mut s0, 1, -2.0, 0.0);
    let b = spawn(&mut w1, &mut s1, 2, 6.0, 0.0); // beyond margin / 2
    for t in 1..=40 {
        assert_eq!(
            duel_tick(&mut w1, &mut s1, t, (b, a, 0, -2)),
            [],
            "tick {t}"
        );
    }
    walk(&mut w1, &s1, b, 4.0, 0.0);
    assert_eq!(
        duel_tick(&mut w1, &mut s1, 41, (b, a, 0, -2)).len(),
        1,
        "inside: moves"
    );
    hand_over((&mut w1, &mut s1), (&mut w0, &mut s0), b);

    walk(&mut w0, &s0, b, 9.9, 0.0);
    assert_eq!(
        quiet(&mut w0, &mut s0, 42, &[(a, b)]),
        [],
        "inside the margin"
    );
    walk(&mut w0, &s0, b, 10.5, 0.0);
    assert_eq!(
        quiet(&mut w0, &mut s0, 43, &[(a, b)]),
        [(1, b, None)],
        "out: released"
    );
    hand_over((&mut w0, &mut s0), (&mut w1, &mut s1), b);
    for t in 44..=120 {
        assert_eq!(
            duel_tick(&mut w1, &mut s1, t, (b, a, 0, -2)),
            [],
            "not re-pinned ({t})"
        );
    }
}

/// Three fighters at a corner (the 2×2 grid with its diagonals): C
/// (shard 3, the highest wire) fights A (shard 0) and B (shard 1) and
/// moves to A's shard — the LOWEST partner's, where B's own move goes
/// too. Held there, C fights Z (shard 2, a lower wire than C's) across
/// the other seam for a long time and is not moved by it.
#[test]
fn a_corner_fight_converges_on_the_lowest_wire_and_a_held_entity_stays() {
    let (mut w0, mut w3) = (World::new(), World::new());
    let mut s0 = duel(0, 4, Some(POLICY));
    let mut s3 = duel(3, 4, Some(POLICY));
    let a = spawn(&mut w0, &mut s0, 1, -2.0, -2.0);
    let b = interleaved_id(1, 4, 2); // shard 1's first entity: a < b < c
    let c = spawn(&mut w3, &mut s3, 3, 2.0, 2.0);
    let mut moved = None;
    for t in 1..=40u64 {
        let mut stage = stage(3, t, &[(0, a, -2, -2), (1, b, 2, -2)]);
        if t % 2 == 1 {
            for foe in [a, b] {
                s3.apply_remote_effect(&mut w3, t, &hit(c, foe, 0, t), &mut stage.seam());
            }
        } else {
            s3.game_mut().strike.extend([(c, a), (c, b)]);
        }
        s3.update_seam(&mut w3, &ctx(t), &mut stage.seam());
        let mv = moves(&mut w3, &mut s3);
        if moved.is_none() && !mv.is_empty() {
            moved = Some(mv);
        }
    }
    let pin = Some(ShardPin {
        partner: a,
        last: 1 + POLICY.after,
    });
    assert_eq!(
        moved,
        Some(vec![(0, c, pin)]),
        "to the lowest partner's shard"
    );
    hand_over((&mut w3, &mut s3), (&mut w0, &mut s0), c);

    let z = interleaved_id(2, 4, 1); // shard 2's: z < c
    for t in 41..=200u64 {
        let mut stage = stage(0, t, &[(2, z, -2, 2)]);
        if t % 2 == 1 {
            s0.apply_remote_effect(&mut w0, t, &hit(c, z, 2, t), &mut stage.seam());
        } else {
            s0.game_mut().strike.push((c, z));
        }
        s0.game_mut().local.push((a, c));
        s0.update_seam(&mut w0, &ctx(t), &mut stage.seam());
        assert_eq!(moves(&mut w0, &mut s0), [], "held: not a mover (tick {t})");
    }
}

/// The partner leaves the holding shard mid-fight (here: logs out): the
/// held entity's reason to stay is gone — on the next pass its region
/// owns it again, however recent the fight.
#[test]
fn a_hold_ends_when_the_partner_is_gone() {
    let (mut w0, mut w1) = (World::new(), World::new());
    let mut s0 = duel(0, 2, Some(POLICY));
    let mut s1 = duel(1, 2, Some(POLICY));
    let a = spawn(&mut w0, &mut s0, 1, -2.0, 0.0);
    let b = spawn(&mut w1, &mut s1, 2, 2.0, 0.0);
    let t = (1..=40)
        .find(|&t| !duel_tick(&mut w1, &mut s1, t, (b, a, 0, -2)).is_empty())
        .expect("the fight crystallizes");
    hand_over((&mut w1, &mut s1), (&mut w0, &mut s0), b);
    assert_eq!(
        quiet(&mut w0, &mut s0, t + 1, &[(a, b), (b, a)]),
        [],
        "held"
    );
    let player = s0.entity_player[&s0.wire_entity[&a]];
    s0.on_leave(&mut w0, player);
    assert_eq!(
        quiet(&mut w0, &mut s0, t + 2, &[]),
        [(1, b, None)],
        "released"
    );
}
