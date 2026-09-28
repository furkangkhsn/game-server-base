//! What a team's content admits beyond its own shard's units: another
//! shard's records for THAT team only (isolation at the merge), and a
//! neighbour's lent records its units see — by the room radius or a
//! unit's own (A8).

use super::*;
use crate::team::SightRadius;

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

/// A unit's own sight radius (A8) on a shard: the hero (45) sees an own
/// enemy on its boundary and a lent record two cells away — past the
/// room radius and the 3×3 block — and exports both; an own enemy 46
/// away stays hidden, and a ward (5) misses an enemy the room radius
/// would see.
#[test]
fn a_units_own_radius_reaches_own_enemies_and_the_strip() {
    let mut world = World::new();
    let mut room = shard0();
    let hero = member(&mut world, &mut room, 1, 0, -5.0, -50.0);
    let ward = member(&mut world, &mut room, 2, 0, -80.0, -10.0);
    let edge = member(&mut world, &mut room, 3, 1, -50.0, -50.0);
    let beyond = member(&mut world, &mut room, 4, 1, -5.0, -96.0);
    let shy = member(&mut world, &mut room, 5, 1, -80.0, -25.0);
    for (wire, r) in [(hero, 45.0), (ward, 5.0)] {
        let e = room.inner.wire_entity[&wire];
        world.entity_mut(e).insert(SightRadius(r));
    }
    let lent = BorderRecord {
        wire: interleaved_id(1, 4, 20),
        state: WirePos { x: 35, y: -50 },
    };
    let strip = [lent.clone()];
    let export = exchange(&mut world, &mut room, 1, &strip, &TeamImports::default());
    assert_eq!(exported(&export, 0), [hero, ward, edge, lent.wire]);
    let seen: Vec<u64> = view(&mut world, &mut room, 1, 0)
        .iter()
        .map(|r| r.0)
        .collect();
    assert!(seen.contains(&edge) && seen.contains(&lent.wire));
    assert!(!seen.contains(&beyond) && !seen.contains(&shy));
}
