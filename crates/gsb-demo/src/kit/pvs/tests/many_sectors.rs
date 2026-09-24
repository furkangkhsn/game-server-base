//! More than sixteen sectors (KIT-ARCHITECTURE §8.4): the visibility
//! table's capacity is the map's, not a bitmask's.

use super::*;
use crate::kit::seam::DemoGame;
use crate::kit::space::{ConvexSectors2, SectorMap};

/// A strip of `n` unit squares along x (sector `i` = `[i, i+1] × [0, 1]`):
/// each sector sees itself and its two neighbours, and the two ends see
/// each other (a long sightline).
fn strip_map(n: u8) -> ConvexSectors2<Position> {
    let polygons = (0..n)
        .map(|i| {
            let x = f32::from(i);
            vec![(x, 0.0), (x + 1.0, 0.0), (x + 1.0, 1.0), (x, 1.0)]
        })
        .collect();
    let visible = (0..n)
        .map(|i| {
            let mut v = vec![Sector(i)];
            if i > 0 {
                v.push(Sector(i - 1));
            }
            if i + 1 < n {
                v.push(Sector(i + 1));
            }
            if i == 0 {
                v.push(Sector(n - 1));
            }
            if i == n - 1 {
                v.push(Sector(0));
            }
            v
        })
        .collect();
    ConvexSectors2::new(polygons, visible)
}

/// Twenty sectors: lookups past index 15 land in the right sector, the
/// far end's sightline to sector 0 holds in both directions, a sector
/// two steps away stays invisible, and the containment sector (index
/// 20) still sees only itself.
#[test]
fn twenty_sector_map_keeps_every_sightline() {
    let map = strip_map(20);
    assert_eq!(map.sector_of(&Position { x: 19.5, y: 0.5 }), Sector(19));
    assert_eq!(map.outside(), Sector(20));

    let mut world = World::new();
    let mut room = super::super::SectorRoom::with_game(DemoGame::default(), map);
    let mut place = |world: &mut World, conn: u64, x: f32| {
        let admission = room.on_join(world, ConnectionId(conn));
        let entity = room.player_entity[&admission.player];
        world.entity_mut(entity).insert(Position { x, y: 0.5 });
        admission.entity
    };
    let e0 = place(&mut world, 1, 0.5);
    let e1 = place(&mut world, 2, 1.5);
    let e17 = place(&mut world, 3, 17.5);
    let e18 = place(&mut world, 4, 18.5);
    let e19 = place(&mut world, 5, 19.5);
    let runaway = place(&mut world, 6, 100.0);
    room.update(&mut world, &ctx(1));

    let mut package = |sector: u8| {
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Sector(sector), &[], &mut out));
        snap_ids(&out)
    };
    assert_eq!(package(0), BTreeSet::from([e0, e1, e19]), "sector 0");
    assert_eq!(package(18), BTreeSet::from([e17, e18, e19]), "sector 18");
    assert_eq!(package(19), BTreeSet::from([e0, e18, e19]), "sector 19");
    assert_eq!(package(20), BTreeSet::from([runaway]), "containment");
}
