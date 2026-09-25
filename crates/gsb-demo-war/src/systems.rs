//! The war game's systems (its `Game::systems` — the kit has no movement
//! or AI trait; it sees only the resulting spawns and component writes):
//! the map's static units come up, players run to their targets, and
//! capture points change hands.

use bevy_ecs::prelude::{Entity, World};
use bevy_ecs::query::QueryState;
use gsb_kit::team::{Team, TeamMember};

use crate::components::{Capture, Kind, MoveTarget, Pos3, Unit};
use crate::world::{
    CAPTURE_RADIUS, CAPTURE_TICKS, FACTIONS, POINTS, RUN_SPEED, TOWER_HEIGHT, home_shard, tower,
};

type Runners = QueryState<(&'static mut Pos3, &'static MoveTarget)>;

/// The systems of ONE shard (query states cached across ticks; built on
/// first use — the shard's world is created by the actor, after the
/// game).
pub(crate) struct Systems {
    /// This shard's region index.
    index: usize,
    /// Whether this shard's towers and points stand yet.
    raised: bool,
    runners: Option<Runners>,
    /// Scratch: the capture points and the players near each.
    points: Vec<(Entity, Pos3, Capture, Option<Team>)>,
    near: Vec<(Pos3, Team)>,
}

impl Systems {
    pub(crate) fn new(index: usize) -> Self {
        Self {
            index,
            raised: false,
            runners: None,
            points: Vec::new(),
            near: Vec::new(),
        }
    }

    pub(crate) fn run(&mut self, world: &mut World, dt: f32) {
        if !self.raised {
            self.raised = true;
            raise(world, self.index);
        }
        self.players_run(world, dt);
        self.capture(world);
    }

    /// Players run toward their target on the ground and stop on it
    /// (a standing player is never written: it costs no record).
    fn players_run(&mut self, world: &mut World, dt: f32) {
        let runners = self.runners.get_or_insert_with(|| world.query());
        let step = RUN_SPEED * dt;
        for (mut pos, target) in runners.iter_mut(world) {
            let (dx, dz) = (target.x - pos.x, target.z - pos.z);
            let dist = (dx * dx + dz * dz).sqrt();
            if dist == 0.0 {
                continue;
            }
            if step >= dist {
                pos.x = target.x;
                pos.z = target.z;
            } else {
                pos.x += dx * step / dist;
                pos.z += dz * step / dist;
            }
        }
    }

    /// A point one faction holds ALONE (its standing players within
    /// [`CAPTURE_RADIUS`], nobody else's) for [`CAPTURE_TICKS`] ticks in
    /// a row becomes that faction's: its `Unit::faction` and the kit's
    /// `TeamMember` — from then on a unit of that faction (seen map-wide
    /// by it, a vision source for it). Anyone else there, or nobody,
    /// resets the count. Every point lies deeper than the radius inside
    /// its region: all the players that count are this shard's own.
    fn capture(&mut self, world: &mut World) {
        let mut q = world.query::<(Entity, &Pos3, &Capture, &Unit)>();
        self.points
            .extend(q.iter(world).map(|(e, p, c, u)| (e, *p, *c, u.faction)));
        if self.points.is_empty() {
            return;
        }
        let mut players = world.query::<(&Pos3, &Unit)>();
        self.near.extend(players.iter(world).filter_map(|(p, u)| {
            (u.kind == Kind::Player && u.hp > 0)
                .then_some(u.faction)
                .flatten()
                .map(|f| (*p, f))
        }));
        for (point, at, capture, owner) in self.points.drain(..) {
            let mut sides = self
                .near
                .iter()
                .filter(|(p, _)| p.ground_dist(&at) <= CAPTURE_RADIUS)
                .map(|&(_, f)| f);
            let first = sides.next();
            let alone = first.filter(|f| sides.all(|g| g == *f));
            let claim = match (alone, capture.claim) {
                (Some(f), _) if Some(f) == owner => None,
                (Some(f), Some((c, n))) if c == f => Some((f, n + 1)),
                (Some(f), _) => Some((f, 1)),
                (None, _) => None,
            };
            match claim {
                Some((f, n)) if n >= CAPTURE_TICKS => {
                    let mut e = world.entity_mut(point);
                    e.insert((Capture { claim: None }, TeamMember(f)));
                    if let Some(mut u) = e.get_mut::<Unit>() {
                        u.faction = Some(f);
                    }
                }
                claim if claim != capture.claim => {
                    world.entity_mut(point).insert(Capture { claim });
                }
                _ => {}
            }
        }
        self.near.clear();
    }
}

/// Spawn the static units standing in region `index`: every faction's
/// tower there (a `TeamMember` of its faction — a vision source) and the
/// capture points there (neutral: no `TeamMember`). The kit stamps their
/// wire identities in the same tick (orphans: no player behind them).
fn raise(world: &mut World, index: usize) {
    for f in 0..FACTIONS {
        let [x, z] = tower(Team(f), index);
        let unit = Unit {
            kind: Kind::Tower,
            faction: Some(Team(f)),
            hp: 0,
        };
        world.spawn((Pos3::new(x, TOWER_HEIGHT, z), unit, TeamMember(Team(f))));
    }
    for [x, z] in POINTS {
        let at = Pos3::ground(x, z);
        if home_shard(&at) != index {
            continue;
        }
        let unit = Unit {
            kind: Kind::Point,
            faction: None,
            hp: 0,
        };
        world.spawn((at, unit, Capture::default()));
    }
}
