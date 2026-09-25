//! The map's geometry keeps the promises the module docs make.

use super::*;

fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

fn towers() -> Vec<(Team, usize, [f32; 2])> {
    (0..FACTIONS)
        .flat_map(|f| (0..SHARDS).map(move |r| (Team(f), r, tower(Team(f), r))))
        .collect()
}

/// Every tower stands in its region, outside the border strip (more
/// than 220 m from both seams), more than 110 m from every base, out of
/// sight of every other tower and of both capture points.
#[test]
fn towers_stand_clear_of_seams_bases_points_and_each_other() {
    let all = towers();
    assert_eq!(all.len(), 12);
    for &(f, r, at) in &all {
        assert_eq!(home_shard(&Pos3::ground(at[0], at[1])), r, "{f:?} {r}");
        assert!(at[0].abs() > 220.0 && at[1].abs() > 220.0, "{at:?}");
        for b in BASES {
            assert!(dist(at, b) > 110.0, "{at:?} by base {b:?}");
        }
        for p in POINTS {
            assert!(dist(at, p) > VISION_RADIUS, "{at:?} sees point {p:?}");
        }
        for &(g, s, other) in &all {
            if (g, s) != (f, r) {
                assert!(dist(at, other) > 300.0, "{at:?} and {other:?}");
            }
        }
    }
}

/// Faction `f`'s base lies in region `f`, and region 3 has none; both
/// capture points lie in region 3, deeper inside it than the capture
/// radius (a capture is decided on one shard).
#[test]
fn bases_and_points_lie_where_the_docs_say() {
    for (f, [x, z]) in BASES.into_iter().enumerate() {
        assert_eq!(home_shard(&Pos3::ground(x, z)), f);
        assert_eq!(base(Team(f as u8)), Pos3::ground(x, z));
    }
    for [x, z] in POINTS {
        assert_eq!(home_shard(&Pos3::ground(x, z)), 3);
        assert!(x.min(z) > CAPTURE_RADIUS, "({x}, {z})");
    }
}

/// The shard grid is the preset's 8-neighbourhood: every region borders
/// the other three (the middle point sits by the corner they share).
#[test]
fn every_region_borders_the_other_three() {
    use gsb_kit::space::Partition;
    let grid = partition();
    for r in 0..SHARDS {
        let mut n = Partition::<WarWire>::neighbors(&grid, r);
        n.sort_unstable();
        let others: Vec<usize> = (0..SHARDS).filter(|&o| o != r).collect();
        assert_eq!(n, others, "region {r}");
    }
}
