//! The seam queries on the REAL shard actors: four shards on a 2×2 grid
//! with the 8-neighbourhood, every shard's systems asking for the disc
//! of radius 20 around the map's centre, where the four regions meet.
//! X crosses the centre from shard 1 to shard 0; Y stands on shard 1
//! exactly 20 from the centre, Z just beyond, W on shard 2 inside.

use super::rig::Rig;
use super::sweep::{RADIUS, Sample};
use crate::sharded::Holder;

/// Where `wire` was in `sample`: `(holder, spot)`.
fn at(sample: &Sample, wire: u64) -> Option<(Holder, [f32; 2])> {
    let hit = sample.found.iter().find(|f| f.0 == wire);
    hit.map(|&(_, holder, spot)| (holder, spot))
}

/// `wire`'s holders on `shard`, tick by tick (absent ticks skipped,
/// repeats collapsed).
fn holders(samples: &[Sample], shard: usize, wire: u64) -> Vec<Holder> {
    let mut seq: Vec<Holder> = samples
        .iter()
        .filter(|s| s.shard == shard)
        .filter_map(|s| at(s, wire).map(|(h, _)| h))
        .collect();
    seq.dedup();
    seq
}

const fn lent(lender: usize) -> Holder {
    Holder::Lent { lender }
}

/// Through the handover every shard counts X once in every tick — local
/// on exactly one shard, lent on the others; its old shard sees the copy
/// it is about to despawn as lent by the new owner in the tick after
/// the move. The disc keeps Y, exactly on its boundary, on every shard,
/// and Z, just beyond, on none.
#[tokio::test(start_paused = true)]
async fn a_crossing_entity_is_counted_once_on_every_shard_through_the_handover() {
    let mut rig = Rig::new().await;
    let x = rig.join(1, "5:-5").await;
    let y = rig.join(2, "12:-16").await;
    let z = rig.join(3, "13:-16").await;
    let w = rig.join(4, "-8:6").await;
    rig.steps(3).await;
    let settled = rig.tick;
    rig.teleport(0, -5.0, -5.0);
    rig.steps(6).await;

    let samples: Vec<&Sample> = rig.samples.iter().filter(|s| s.tick > settled).collect();
    assert_eq!(samples.len(), 4 * 6, "every shard, every tick");
    for s in &samples {
        let wires: Vec<u64> = s.found.iter().map(|f| f.0).collect();
        assert!(
            wires.is_sorted_by(|a, b| a < b),
            "once, in wire order: {s:?}"
        );
        let (holder, spot) = at(s, y).expect("Y on the boundary is inside");
        assert_eq!(spot, [12.0, -16.0], "{s:?}");
        let own = matches!(holder, Holder::Local(_));
        assert_eq!(own, s.shard == 1, "{s:?}");
        assert!(own || holder == lent(1), "{s:?}");
        assert!(at(s, z).is_none(), "Z is beyond {RADIUS}: {s:?}");
        assert!(at(s, w).is_some(), "W is inside: {s:?}");
    }
    for tick in settled + 1..=rig.tick {
        let local: Vec<usize> = samples
            .iter()
            .filter(|s| s.tick == tick)
            .filter(|s| matches!(at(s, x), Some((Holder::Local(_), _))))
            .map(|s| s.shard)
            .collect();
        assert_eq!(
            local.len(),
            1,
            "X local on one shard in tick {tick}: {local:?}"
        );
    }

    let local = |seq: &[Holder]| -> Vec<bool> {
        seq.iter().map(|h| matches!(h, Holder::Local(_))).collect()
    };
    let on1 = holders(&rig.samples, 1, x);
    assert_eq!(local(&on1), vec![true, false], "{on1:?}");
    assert_eq!(on1[1], lent(0), "the old shard: lent by the new owner");
    let on0 = holders(&rig.samples, 0, x);
    assert_eq!(local(&on0), vec![false, true], "{on0:?}");
    assert_eq!(on0[0], lent(1), "the new shard: lent by the old one first");
    for shard in [2, 3] {
        assert_eq!(holders(&rig.samples, shard, x).last(), Some(&lent(0)));
    }

    // The tick after the move: shard 1 still holds the copy (the core
    // despawns it at the end of that tick) and shows it lent, with the
    // record it left with, while shard 0 has it.
    let first_lent = samples
        .iter()
        .find(|s| s.shard == 1 && matches!(at(s, x), Some((Holder::Lent { .. }, _))))
        .expect("X leaves shard 1");
    assert_eq!(at(first_lent, x), Some((lent(0), [-5.0, -5.0])));
    let there = samples
        .iter()
        .find(|s| s.shard == 0 && s.tick == first_lent.tick)
        .expect("shard 0's sample");
    assert!(
        matches!(at(there, x), Some((Holder::Local(_), _))),
        "{there:?}"
    );
}
