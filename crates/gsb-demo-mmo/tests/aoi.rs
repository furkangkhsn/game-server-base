//! Ground-plane AOI through the real shard actors: a player receives
//! EXACTLY the entities in the 3×3 block of 64 m ground cells around it —
//! including what a neighbouring shard lends through the border strip —
//! and height does not matter: a flyer high above a neighbouring cell is
//! visible, a mob on the ground two cells away is not, although the
//! flyer is the farther of the two in 3D.

mod common;

use std::collections::BTreeSet;

use common::{Client, Mmo};
use gsb_demo_mmo::codec::to_dm;
use gsb_demo_mmo::components::Kind;
use gsb_demo_mmo::world::client_cell;
use gsb_demo_mmo::{MobSpawn, Pos3, Realm};

/// A mob standing at `(x, y, z)` from tick 1 for the whole test.
fn standing(kind: Kind, x: f32, y: f32, z: f32) -> MobSpawn {
    MobSpawn::once(kind, Pos3::new(x, y, z), 1, 100_000, 100)
}

/// `(x, y, z)` decimetres of a position.
fn dm(p: &Pos3) -> (i32, i32, i32) {
    (to_dm(p.x), to_dm(p.y), to_dm(p.z))
}

/// The positions `viewer` must see: every entity whose ground cell is in
/// the 3×3 block around the viewer's.
fn expected(viewer: &Pos3, everything: &[Pos3]) -> BTreeSet<(i32, i32, i32)> {
    let (vx, vz) = client_cell(to_dm(viewer.x), to_dm(viewer.z));
    everything
        .iter()
        .filter(|p| {
            let (cx, cz) = client_cell(to_dm(p.x), to_dm(p.z));
            (cx - vx).abs() <= 1 && (cz - vz).abs() <= 1
        })
        .map(dm)
        .collect()
}

fn seen(c: &Client) -> BTreeSet<(i32, i32, i32)> {
    c.view.values().map(|r| (r.x, r.y, r.z)).collect()
}

#[tokio::test]
async fn a_player_sees_exactly_its_ground_cell_block_whatever_the_height() {
    // V deep in shard 3 (cell (3,3)); W at shard 3's west seam (cell
    // (0,3)), whose block reaches one cell into shard 2.
    let v = Pos3::new(224.0, 0.0, 224.0);
    let w = Pos3::new(32.0, 0.0, 224.0);
    let high_neighbour = Pos3::new(300.0, 150.0, 230.0); // cell (4,3), 150 m up
    let ground_two_away = Pos3::new(330.0, 0.0, 224.0); // cell (5,3), on the ground
    let mobs = [
        (Kind::Flyer, high_neighbour),
        (Kind::Mob, ground_two_away),
        (Kind::Mob, Pos3::new(140.0, 0.0, 140.0)), // V's diagonal cell (2,2)
        (Kind::Flyer, Pos3::new(224.0, 200.0, 224.0)), // straight above V
        (Kind::Flyer, Pos3::new(-20.0, 60.0, 224.0)), // shard 2, cell (-1,3)
        (Kind::Mob, Pos3::new(-100.0, 0.0, 224.0)), // shard 2, cell (-2,3)
        (Kind::Mob, Pos3::new(-300.0, 0.0, -300.0)), // far away, shard 0
    ];
    let mut realm = Realm::empty().with_login("c1", v).with_login("c2", w);
    for (kind, at) in mobs {
        realm = realm.with_spawn(standing(kind, at.x, at.y, at.z));
    }
    let mut everything = vec![v, w];
    everything.extend(mobs.iter().map(|(_, p)| *p));

    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "c1", &mut []).await];
    let joined = room.join(2, "c2", &mut cs).await;
    cs.push(joined);
    // Past two keep-alive fulls (every 15 ticks): the view stays exact
    // through fulls and deltas alike.
    for tick in 0..40 {
        room.step(&mut cs).await;
        if tick < 3 {
            continue; // the first frames are in flight
        }
        for (c, at) in cs.iter().zip([v, w]) {
            assert_eq!(seen(c), expected(&at, &everything), "tick {}", room.tick);
        }
    }

    // The two named cases, spelled out.
    let far_3d = |p: &Pos3| p.dist(&v);
    assert!(far_3d(&high_neighbour) > far_3d(&ground_two_away));
    assert!(
        seen(&cs[0]).contains(&dm(&high_neighbour)),
        "high flyer, next cell: visible"
    );
    assert!(
        !seen(&cs[0]).contains(&dm(&ground_two_away)),
        "ground mob, 2 cells: hidden"
    );
    assert!(
        seen(&cs[1]).contains(&dm(&Pos3::new(-20.0, 60.0, 224.0))),
        "lent by shard 2"
    );
    assert_eq!(cs[0].of_kind(gsb_demo_mmo::mmo::Kind::Flyer).len(), 2);
    assert!(cs[0].me().is_some() && cs[1].me().is_some());
}
