//! What a team's content admits beyond its own shard's units: another
//! shard's records for THAT team only (isolation at the merge), and a
//! neighbour's lent records its units see.

use super::*;

/// Isolation at the merge: team 0's viewers never see the records the
/// other shards exported for team 1, and the other way round.
#[test]
fn a_team_never_sees_another_teams_imports() {
    let mut world = World::new();
    let mut room = shard0();
    let a = member(&mut world, &mut room, 1, 0, -80.0, -80.0);
    let b = member(&mut world, &mut room, 2, 1, -10.0, -10.0);
    let (x, y) = (interleaved_id(3, 4, 20), interleaved_id(3, 4, 21));
    let im = imports(3, 1, &[(0, x, body(x, 50, 50)), (1, y, body(y, 60, 60))]);
    exchange(&mut world, &mut room, 1, &[], &im);
    assert_eq!(
        view(&mut world, &mut room, 1, 0),
        [(a, -80, -80), (x, 50, 50)]
    );
    assert_eq!(
        view(&mut world, &mut room, 1, 1),
        [(b, -10, -10), (y, 60, 60)]
    );
}

/// A lent record (a neighbour's border strip) in an own unit's vision is
/// seen and exported; out of it, neither. With no lent position the
/// strip takes no part in vision at all.
#[test]
fn a_lent_record_in_an_own_units_vision_is_seen() {
    let mut world = World::new();
    let mut room = shard0();
    member(&mut world, &mut room, 1, 0, -5.0, -50.0);
    let near = BorderRecord {
        wire: interleaved_id(1, 4, 20),
        state: WirePos { x: 5, y: -50 },
    };
    let far = BorderRecord {
        wire: interleaved_id(1, 4, 21),
        state: WirePos { x: 40, y: -50 },
    };
    let strip = [near.clone(), far.clone()];
    let export = exchange(&mut world, &mut room, 1, &strip, &TeamImports::default());
    assert!(exported(&export, 0).contains(&near.wire));
    assert!(!exported(&export, 0).contains(&far.wire));
    let seen: Vec<u64> = view(&mut world, &mut room, 1, 0)
        .iter()
        .map(|r| r.0)
        .collect();
    assert!(seen.contains(&near.wire) && !seen.contains(&far.wire));

    let mut blind = TeamShard::with_shard(
        ShardedRoom::new(0, 4, 100.0),
        crate::space::VisionGrid2::new(R),
        |_| None,
    );
    let mut world = World::new();
    member(&mut world, &mut blind, 1, 0, -5.0, -50.0);
    let export = exchange(&mut world, &mut blind, 1, &strip, &TeamImports::default());
    assert!(!exported(&export, 0).contains(&near.wire));
}
