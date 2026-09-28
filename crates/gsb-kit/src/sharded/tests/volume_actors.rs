//! The volumetric partition (`GridPartition3`, BACKLOG A5) on the REAL
//! shard actors: eight shards on a 2×2×2 grid over `[-100, 100]³`
//! (border margin 25; the third axis is height), a live registry and a
//! global ticker on a paused clock. Every shard's game reports, every
//! tick, every entity its seam holds — own or lent — so a test sees
//! where each entity lives on every shard through a migration.

use bevy_ecs::world::EntityRef;

use crate::sharded::{Holder, SeamView};
use crate::testing::{Position3, WirePos3};

mod climb;
mod rig;

use climb::Sample;
use rig::Rig;

/// What the seam reports of an entity: where it stands.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Spot3([f32; 3]);

impl SeamView<WirePos3> for Spot3 {
    fn local(entity: EntityRef<'_>) -> Option<Self> {
        entity.get::<Position3>().map(|p| Spot3([p.x, p.y, p.z]))
    }
    fn lent(w: &WirePos3) -> Option<Self> {
        Some(Spot3([w.x as f32, w.y as f32, w.z as f32]))
    }
}

/// `wire`'s holder on `shard`, tick by tick (absent ticks skipped,
/// repeats collapsed).
fn holders(samples: &[Sample], shard: usize, wire: u64) -> Vec<Holder> {
    let mut seq: Vec<Holder> = samples
        .iter()
        .filter(|s| s.shard == shard)
        .filter_map(|s| s.found.iter().find(|f| f.0 == wire).map(|f| f.1))
        .collect();
    seq.dedup();
    seq
}

/// The shards holding `wire` locally, tick by tick from `after` on (one
/// entry per tick: exactly one owner every tick), repeats collapsed.
fn owners(rig: &Rig, after: u64, wire: u64) -> Vec<usize> {
    let mut seq = Vec::new();
    for tick in after + 1..=rig.tick {
        let local: Vec<usize> = rig
            .samples
            .iter()
            .filter(|s| s.tick == tick)
            .filter(|s| {
                let hit = s.found.iter().find(|f| f.0 == wire);
                matches!(hit, Some((_, Holder::Local(_), _)))
            })
            .map(|s| s.shard)
            .collect();
        assert_eq!(
            local.len(),
            1,
            "one owner of {wire} in tick {tick}: {local:?}"
        );
        seq.push(local[0]);
    }
    seq.dedup();
    seq
}

const fn lent(lender: usize) -> Holder {
    Holder::Lent { lender }
}

/// The 6-neighbourhood: a climb through z = 0 hands the climber from
/// shard 0 to the shard above (4) in one hop; a jump through the centre
/// corner relays face by face (0 → 1 → 3 → 7). Each has exactly one
/// owner every tick, and an entity deep in shard 0 is never lent.
#[tokio::test(start_paused = true)]
async fn a_climb_hands_over_to_the_shard_above_and_a_corner_jump_relays() {
    let mut rig = Rig::new(false).await;
    let climber = rig.join(1, "-50:-50:-5").await;
    let jumper = rig.join(2, "-5:-5:-5").await;
    let deep = rig.join(3, "-60:-60:-60").await;
    rig.steps(3).await;
    let settled = rig.tick;
    rig.teleport(0, [-50.0, -50.0, 5.0]);
    rig.teleport(1, [5.0, 5.0, 5.0]);
    rig.steps(8).await;

    assert_eq!(owners(&rig, settled, climber), vec![0, 4]);
    assert_eq!(owners(&rig, settled, jumper), vec![0, 1, 3, 7]);
    assert_eq!(owners(&rig, settled, deep), vec![0]);
    let on4 = holders(&rig.samples, 4, climber);
    assert_eq!(
        on4.first(),
        Some(&lent(0)),
        "lent across the floor: {on4:?}"
    );
    assert!(matches!(on4.last(), Some(Holder::Local(_))), "{on4:?}");
    let on0 = holders(&rig.samples, 0, climber);
    assert_eq!(on0.last(), Some(&lent(4)), "lent back down: {on0:?}");
    for shard in 1..8 {
        assert!(
            holders(&rig.samples, shard, deep).is_empty(),
            "shard {shard}"
        );
    }
}

/// The 26-neighbourhood: the same corner jump goes straight to shard 7
/// in one hop. Shard 0 then sees the jumper lent by 7, shard 7 saw it
/// lent by 0 before it arrived, and every other shard — each a
/// neighbour of both — ends up seeing it lent by 7.
#[tokio::test(start_paused = true)]
async fn with_the_diagonals_a_corner_jump_takes_one_hop() {
    let mut rig = Rig::new(true).await;
    let jumper = rig.join(1, "-5:-5:-5").await;
    rig.steps(3).await;
    let settled = rig.tick;
    rig.teleport(0, [5.0, 5.0, 5.0]);
    rig.steps(6).await;

    assert_eq!(owners(&rig, settled, jumper), vec![0, 7]);
    let on0 = holders(&rig.samples, 0, jumper);
    assert_eq!(on0.last(), Some(&lent(7)), "{on0:?}");
    let on7 = holders(&rig.samples, 7, jumper);
    assert_eq!(on7.first(), Some(&lent(0)), "{on7:?}");
    for shard in 1..7 {
        let seq = holders(&rig.samples, shard, jumper);
        assert_eq!(seq.first(), Some(&lent(0)), "shard {shard}: {seq:?}");
        assert_eq!(seq.last(), Some(&lent(7)), "shard {shard}: {seq:?}");
    }
}
