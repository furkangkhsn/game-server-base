//! The per-team export budget (KIT-ARCHITECTURE §10 "A29"): the default
//! cut is pinned over a seeded crowd; a game's rank picks what a cut
//! keeps (members first still), ties by wire id.

use super::*;

/// A seeded generator (the crowd is scripted, not random).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// A coordinate in `[lo, lo + span)`.
    fn coord(&mut self, lo: f32, span: u64) -> f32 {
        lo + (self.next() % span) as f32
    }
}

/// Fold `bytes` into an FNV-1a digest.
fn fold(digest: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *digest = (*digest ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
    }
}

/// The seeded crowd on `room` (shard 0): twelve players of three teams
/// and six wards packed in a 40 × 40 square near the x = 0 seam, four
/// neutrals, five lent records across the seam; thirty ticks, every
/// unit and lent record jittering. Returns the digest of every export
/// (views, then each record's team, wire and body, in export order)
/// and the records the budget cut.
fn crowd(mut room: TeamShard, seed: u64) -> (u64, u64) {
    let mut rng = Lcg(seed);
    let mut world = World::new();
    let mut units = Vec::new();
    for i in 0..12u64 {
        let (x, y) = (rng.coord(-45.0, 40), rng.coord(-60.0, 40));
        let wire = member(&mut world, &mut room, i + 1, (i % 3) as u8, x, y);
        units.push(room.inner.wire_entity[&wire]);
    }
    for i in 0..10u8 {
        let team = (i < 6).then_some(i % 3);
        let (x, y) = (rng.coord(-45.0, 40), rng.coord(-60.0, 40));
        units.push(npc(&mut world, team, x, y));
    }
    let mut strip: Vec<BorderRecord<WirePos>> = (0..5)
        .map(|k| BorderRecord {
            wire: interleaved_id(1, 4, 40 + k),
            state: WirePos { x: 2, y: -50 },
        })
        .collect();
    let mut digest = 0xCBF2_9CE4_8422_2325;
    for tick in 1..=30 {
        for &e in &units {
            let p = *world.get::<Position>(e).expect("placed");
            let x = (p.x + rng.coord(-3.0, 7)).clamp(-60.0, -5.0);
            let y = (p.y + rng.coord(-3.0, 7)).clamp(-70.0, -10.0);
            world.entity_mut(e).insert(Position { x, y });
        }
        for r in &mut strip {
            r.state = WirePos {
                x: rng.coord(0.0, 20) as i32,
                y: rng.coord(-70.0, 60) as i32,
            };
        }
        let export = exchange(&mut world, &mut room, tick, &strip, &TeamImports::default());
        for v in &export.views {
            fold(&mut digest, &v.to_le_bytes());
        }
        for r in &export.records {
            fold(&mut digest, &r.team.to_le_bytes());
            fold(&mut digest, &r.wire.to_le_bytes());
            fold(&mut digest, &r.bytes);
        }
    }
    (digest, room.over_budget())
}

/// A game without a rank cuts exactly as before A29: members first,
/// then what they see, each in the order the kit knows them (own by
/// wire id, then the strip) — the whole export pinned over a seeded
/// crowd that the budget cuts every tick, once inside the members (4 of
/// a team's six) and once inside what they see (9). (Measured at
/// `28be644`.)
#[test]
fn the_default_cut_is_pinned() {
    let runs = [4, 9].map(|budget| crowd(shard0().with_team_budget(budget), 0xA29_0001));
    assert_eq!(
        runs,
        [(0x4317_9a2f_bf10_8d64, 1747), (0x4174_beed_7239_f919, 1297)],
        "the default export changed: {runs:#x?}"
    );
}

/// A rank changes nothing while the budget holds: the same crowd, uncut,
/// exports the same bytes in the same order with or without one.
#[test]
fn a_rank_changes_nothing_under_the_budget() {
    let plain = crowd(shard0(), 0xA29_0002);
    let ranked = crowd(shard0().with_export_rank(east), 0xA29_0002);
    assert_eq!(plain.1, 0, "the default budget holds");
    assert_eq!(plain, ranked);
}

/// The fixtures' rank: the easternmost first.
fn east(w: &WirePos) -> u32 {
    (w.x + 1_000) as u32
}

/// Over budget, a ranked game keeps its highest-ranked records — among
/// the members (team 1: four wards, three kept — the westernmost, the
/// first minted, is cut) and among what the members see (team 0: its
/// player and the two easternmost of the four wards it sees). What is
/// kept goes out in the export's own order; the cut is counted, per
/// export and in total.
#[test]
fn a_ranked_cut_keeps_the_highest_ranked_records() {
    let mut world = World::new();
    let mut room = shard0().with_team_budget(3).with_export_rank(east);
    let a = member(&mut world, &mut room, 1, 0, -80.0, -80.0);
    let wards = [-90.0, -70.0, -85.0, -75.0].map(|x| npc(&mut world, Some(1), x, -80.0));
    let export = exchange(&mut world, &mut room, 1, &[], &TeamImports::default());
    let w = wards.map(|e| wire_of(&world, e));
    assert!(w.windows(2).all(|p| p[0] < p[1]), "minted in spawn order");
    assert_eq!(exported(&export, 0), [a, w[1], w[3]], "a, then -70 and -75");
    assert_eq!(
        exported(&export, 1),
        [w[1], w[2], w[3]],
        "-90 is cut, a too"
    );
    assert_eq!(export.over_budget, 4, "the export reports its cut (A29)");
    assert_eq!(room.over_budget(), 4);
}

/// Equal ranks fall back to the wire id — whatever order the strip
/// arrived in: the kept records are the best rank, then the smallest
/// wire ids among the next rank (here the lent records, minted
/// interleaved below this shard's own).
#[test]
fn ties_keep_the_smaller_wire_ids_whatever_the_strip_order() {
    let lent = |serial: u64, x: i32| BorderRecord {
        wire: interleaved_id(1, 4, serial),
        state: WirePos { x, y: -80 },
    };
    let (l1, l2) = (lent(1, 5), lent(2, 8));
    let run = |strip: &[BorderRecord<WirePos>]| {
        let mut world = World::new();
        let mut room = shard0()
            .with_team_budget(3)
            .with_export_rank(|w| if w.x == -10 { 2 } else { 1 });
        let a = member(&mut world, &mut room, 1, 0, -5.0, -80.0);
        let e = [-10.0, -15.0, -20.0].map(|x| npc(&mut world, Some(1), x, -80.0));
        let export = exchange(&mut world, &mut room, 1, strip, &TeamImports::default());
        (a, e.map(|e| wire_of(&world, e)), exported(&export, 0))
    };
    let (a, e, kept) = run(&[l1.clone(), l2.clone()]);
    assert!(l1.wire < l2.wire && l2.wire < e[1], "{l1:?} {l2:?} {e:?}");
    assert_eq!(kept, [a, e[0], l1.wire], "rank 2, then the smallest wire");
    assert_eq!(run(&[l2, l1]).2, kept, "the strip's order does not matter");
}

/// The rank refines, it does not replace, "members first": a team's
/// members are kept before any record it sees, however high the game
/// ranks the latter (two enemies east of both members); under a budget
/// of one, the better-ranked member (the ward, minted after the player).
#[test]
fn members_stay_first_under_a_rank() {
    for budget in [2, 1] {
        let mut world = World::new();
        let mut room = shard0().with_team_budget(budget).with_export_rank(east);
        let a = member(&mut world, &mut room, 1, 0, -80.0, -80.0);
        let ward = npc(&mut world, Some(0), -78.0, -80.0);
        for x in [-75.0, -70.0] {
            npc(&mut world, Some(1), x, -80.0);
        }
        let export = exchange(&mut world, &mut room, 1, &[], &TeamImports::default());
        let ward = wire_of(&world, ward);
        let want = if budget == 2 {
            vec![a, ward]
        } else {
            vec![ward]
        };
        assert_eq!(exported(&export, 0), want, "budget {budget}");
    }
}
