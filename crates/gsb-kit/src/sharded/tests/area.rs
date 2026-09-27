//! The seam's merged queries (BACKLOG F6 — `Seam::{find, area,
//! within}`): each wire id once, with the seam's precedence — own over
//! its lent copy, a departing copy lent by its new owner with the record
//! it left with, the lower lender of a wire two neighbours lend — the
//! boundary of a disc included, and a room nobody lends to answering
//! what a world query does. Driven over a `SeamStage` (the core's
//! actor-free seam); the real actors are in `actors`.

use std::collections::HashMap;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::{Entity, World};
use bevy_ecs::world::EntityRef;
use gsb_core::shard::{BorderRecord, SeamStage};

use super::*;
use crate::sharded::departing::Departures;
use crate::sharded::{Found, Holder, Seam, SeamView};
use crate::space::Planar;
use crate::testing::WirePos;

mod actors;
mod rig;
mod sweep;

/// The tests' view: where an entity stands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Spot(pub(super) [f32; 2]);

impl SeamView<WirePos> for Spot {
    fn local(entity: EntityRef<'_>) -> Option<Self> {
        entity.get::<Position>().map(|p| Spot([p.x, p.y]))
    }
    fn lent(record: &WirePos) -> Option<Self> {
        Some(Spot([record.x as f32, record.y as f32]))
    }
}

impl Planar for Spot {
    type Coord = f32;
    fn planar(&self) -> [f32; 2] {
        self.0
    }
}

/// Shard 0's side of a seam: its world, its owned-wire table, its
/// departures and the neighbours' records.
struct Scene {
    world: World,
    own: HashMap<u64, Entity>,
    departures: Departures<WirePos>,
    stage: SeamStage<WirePos>,
}

impl Scene {
    fn new() -> Self {
        Self {
            world: World::new(),
            own: HashMap::new(),
            departures: Departures::default(),
            stage: SeamStage::new(0, 5),
        }
    }

    /// An own entity `wire` at `(x, y)`.
    fn own(&mut self, wire: u64, x: f32, y: f32) -> Entity {
        let e = self.world.spawn(Position { x, y }).id();
        self.own.insert(wire, e);
        e
    }

    /// Neighbour `lender` lends `wire` at `(x, y)`.
    fn lend(&mut self, lender: usize, wire: u64, x: i32, y: i32) {
        let state = WirePos { x, y };
        self.stage.lend(lender, BorderRecord { wire, state });
    }

    /// Run `f` with the seam the game's hooks would get.
    fn with<R>(&mut self, f: impl FnOnce(&Seam<'_, '_, WirePos>, &World) -> R) -> R {
        let mut cross = self.stage.seam();
        let seam = Seam::new(&mut cross, &self.own, &self.departures, None);
        f(&seam, &self.world)
    }

    /// `within(center, radius)` as `(wire, holder, spot)`.
    fn within(&mut self, center: [f32; 2], radius: f32) -> Vec<(u64, Holder, [f32; 2])> {
        let mut out = Vec::new();
        self.with(|seam, world| seam.within::<Spot>(world, center, radius, &mut out));
        out.iter().map(|f| (f.wire, f.holder, f.view.0)).collect()
    }

    fn find(&mut self, wire: u64) -> Option<(Holder, [f32; 2])> {
        self.with(|seam, world| seam.find::<Spot>(world, wire))
            .map(|f: Found<Spot>| (f.holder, f.view.0))
    }
}

const fn lent(lender: usize) -> Holder {
    Holder::Lent { lender }
}

/// A disc includes its boundary — local (f32) and lent (integer wire)
/// alike — and nothing beyond; the centre is where the caller says; the
/// answer is in wire order, local and lent interleaved; the buffer is
/// cleared and reused.
#[test]
fn within_counts_the_boundary_and_nothing_beyond() {
    let mut s = Scene::new();
    let a = s.own(7, 3.0, 4.0);
    s.own(3, 0.0, 5.01);
    s.lend(1, 2, -5, 0);
    s.lend(1, 9, 4, 4);
    s.lend(2, 11, 0, -5);
    let want = vec![
        (2, lent(1), [-5.0, 0.0]),
        (7, Holder::Local(a), [3.0, 4.0]),
        (11, lent(2), [0.0, -5.0]),
    ];
    assert_eq!(s.within([0.0, 0.0], 5.0), want);
    assert_eq!(s.within([-2.0, 0.0], 3.0), [(2, lent(1), [-5.0, 0.0])]);
    assert_eq!(s.within([0.0, -2.0], 3.0), [(11, lent(2), [0.0, -5.0])]);

    let mut out = Vec::with_capacity(8);
    s.with(|seam, world| {
        seam.within::<Spot>(world, [0.0, 0.0], 5.0, &mut out);
        seam.within::<Spot>(world, [0.0, 0.0], 5.0, &mut out);
    });
    assert_eq!(out.len(), 3, "cleared first");
    assert_eq!(out.capacity(), 8, "the caller's buffer, reused");
}

/// Any shape: `area` applies the game's predicate to both halves.
#[test]
fn area_applies_the_predicate_to_local_and_lent() {
    let mut s = Scene::new();
    let a = s.own(4, 1.0, 0.0);
    s.own(5, -1.0, 0.0);
    s.lend(1, 6, 2, 9);
    s.lend(1, 8, -2, 9);
    let mut out = Vec::new();
    s.with(|seam, world| seam.area::<Spot>(world, |p| p.0[0] >= 0.0, &mut out));
    let got: Vec<(u64, Holder)> = out.iter().map(|f| (f.wire, f.holder)).collect();
    assert_eq!(got, [(4, Holder::Local(a)), (6, lent(1))]);
}

/// An own entity is answered by the world, never by its lent copy (it
/// just migrated in): once, with its own position — and not at all when
/// only the copy is in range, or when the game's view leaves it out.
#[test]
fn own_wins_over_its_lent_copy_even_out_of_range() {
    let mut s = Scene::new();
    let a = s.own(4, 1.0, 0.0);
    let b = s.own(6, 40.0, 0.0);
    let c = s.world.spawn_empty().id();
    s.own.insert(8, c);
    for wire in [4, 6, 8] {
        s.lend(1, wire, 2, 0);
    }
    assert_eq!(
        s.within([0.0, 0.0], 5.0),
        [(4, Holder::Local(a), [1.0, 0.0])]
    );
    assert_eq!(s.find(6), Some((Holder::Local(b), [40.0, 0.0])));
    assert_eq!(s.find(8), None, "an own entity the view leaves out");
}

/// The migration tick: the copy of an entity handed on last tick is
/// lent by its new owner with the record it left with — once, whether
/// or not the kit has hidden the copy yet and whether or not the new
/// owner's own export is in. A refused send leaves it local.
#[test]
fn a_departing_copy_is_lent_by_its_new_owner_as_it_left() {
    let mut s = Scene::new();
    let q = s.own(4, 1.0, 0.0);
    s.departures.record(4, WirePos { x: 1, y: 0 });
    s.stage.depart(4, 1);
    s.lend(1, 4, 3, 0);
    let want = [(4, lent(1), [1.0, 0.0])];
    assert_eq!(s.within([0.0, 0.0], 5.0), want, "not yet hidden");
    s.departures.hide(&mut s.world, &s.own, &s.stage.seam());
    assert!(s.world.entity(q).contains::<Disabled>());
    assert_eq!(s.within([0.0, 0.0], 5.0), want, "hidden");
    assert_eq!(s.find(4), Some((lent(1), [1.0, 0.0])));

    let mut refused = Scene::new();
    let q = refused.own(4, 1.0, 0.0);
    refused.departures.record(4, WirePos { x: 1, y: 0 });
    let want = [(4, Holder::Local(q), [1.0, 0.0])];
    assert_eq!(refused.within([0.0, 0.0], 5.0), want);
}

/// An entity handed between two neighbours is lent by both for a tick:
/// it is found once, from the lower lender (where `lent` and `emit`
/// answer) — and not through the other copy when the lower one is out
/// of range. `lent_iter` yields it once too.
#[test]
fn a_wire_two_neighbours_lend_is_found_once_from_the_lower_lender() {
    let mut s = Scene::new();
    s.lend(2, 4, 1, 0);
    s.lend(1, 4, 3, 0);
    s.lend(1, 6, 30, 0);
    s.lend(2, 6, 1, 1);
    assert_eq!(s.within([0.0, 0.0], 5.0), [(4, lent(1), [3.0, 0.0])]);
    assert_eq!(s.find(6), Some((lent(1), [30.0, 0.0])));
    let mut iter: Vec<(u64, usize)> =
        s.with(|seam, _| seam.lent_iter().map(|l| (l.wire, l.lender)).collect());
    iter.sort_unstable();
    assert_eq!(iter, [(4, 1), (6, 1)]);
}

/// Nobody lends: the answer is what a world query finds — not an entity
/// the game despawned or disabled, not one without the view's
/// component; an entity spawned in this hook has no wire id yet.
#[test]
fn a_room_nobody_lends_to_answers_as_a_world_query() {
    let mut s = Scene::new();
    let a = s.own(1, 1.0, 1.0);
    let b = s.own(2, 2.0, 2.0);
    let c = s.own(3, 3.0, 3.0);
    let d = s.world.spawn_empty().id();
    s.own.insert(4, d);
    s.own(5, 100.0, 100.0);
    s.world.spawn(Position { x: 0.0, y: 0.0 });
    s.world.despawn(b);
    s.world.entity_mut(c).insert(Disabled);

    let mut q = s.world.query::<(Entity, &Position)>();
    let queried: Vec<Entity> = q
        .iter(&s.world)
        .filter(|(e, p)| p.x.hypot(p.y) <= 10.0 && s.own.values().any(|o| o == e))
        .map(|(e, _)| e)
        .collect();
    assert_eq!(queried, [a]);
    assert_eq!(
        s.within([0.0, 0.0], 10.0),
        [(1, Holder::Local(a), [1.0, 1.0])]
    );
    for wire in [2, 3, 4] {
        assert_eq!(s.find(wire), None, "wire {wire}");
    }
}
