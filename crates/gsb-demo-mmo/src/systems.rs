//! The MMO's systems (its `Game::systems`, KIT-ARCHITECTURE §6 — the kit
//! has no movement or AI trait; it only sees the resulting component
//! writes, spawns and despawns): the camps spawning mobs, the mobs
//! walking their routes, the mobs' end of life, the players running
//! to their targets, and their combat state running out. All movement is on the ground plane; a flyer keeps
//! its altitude.

use bevy_ecs::prelude::{Entity, World};
use bevy_ecs::query::QueryState;

use crate::components::{InCombat, Mob, MoveTarget, Pos3, RunSpeed, Vitals};
use crate::realm::MobSpawn;

/// Step `pos` toward `(tx, tz)` on the ground by at most `step` metres;
/// `true` when it arrived (snapped onto the target). Never writes a
/// position that does not change.
fn walk(pos: &mut bevy_ecs::prelude::Mut<Pos3>, tx: f32, tz: f32, step: f32) -> bool {
    let (dx, dz) = (tx - pos.x, tz - pos.z);
    let dist = (dx * dx + dz * dz).sqrt();
    if dist == 0.0 {
        return true;
    }
    if step >= dist {
        pos.x = tx;
        pos.z = tz;
        return true;
    }
    let k = step / dist;
    pos.x += dx * k;
    pos.z += dz * k;
    false
}

/// The camps of ONE shard: its rows of the spawn table and when each
/// spawns next.
pub(crate) struct Camps {
    rows: Vec<(MobSpawn, u64)>,
}

impl Camps {
    pub(crate) fn new(rows: impl Iterator<Item = MobSpawn>) -> Self {
        Self {
            rows: rows.map(|s| (s.clone(), s.first)).collect(),
        }
    }

    /// Spawn every due mob. The spawned entity carries the codec's
    /// marker ([`Pos3`]) but no wire identity: the kit stamps it from
    /// this shard's counter in the same tick (`Marker` = broadcast).
    pub(crate) fn run(&mut self, world: &mut World, tick: u64) {
        for (spawn, due) in &mut self.rows {
            if tick < *due {
                continue;
            }
            world.spawn((
                spawn.at.clamped(),
                Vitals {
                    kind: spawn.kind,
                    hp: spawn.hp,
                },
                Mob {
                    route: spawn.route.clone(),
                    leg: 0,
                    pace: spawn.pace,
                    patrol: spawn.patrol,
                    dies_at: tick.saturating_add(spawn.lifetime),
                },
            ));
            *due = spawn
                .every
                .map_or(u64::MAX, |e| tick.saturating_add(e.max(1)));
        }
    }
}

type Walkers = QueryState<(&'static mut Pos3, &'static mut Mob)>;
type Runners = QueryState<(&'static mut Pos3, &'static MoveTarget, &'static RunSpeed)>;

/// The movement and lifecycle systems (query states cached across
/// ticks; built on first use — the shard's world is created by the
/// actor, after the game).
#[derive(Default)]
pub(crate) struct Systems {
    walkers: Option<Walkers>,
    runners: Option<Runners>,
    dying: Vec<Entity>,
    cooled: Vec<Entity>,
}

impl Systems {
    pub(crate) fn run(&mut self, world: &mut World, tick: u64, dt: f32) {
        self.mobs_walk(world, dt);
        self.mobs_die(world, tick);
        self.players_run(world, dt);
        self.combat_cools(world, tick);
    }

    /// A player whose last landed hit is [`crate::world::COMBAT_TICKS`]
    /// old leaves combat (a parked one may now log out: the core asks
    /// the veto again on the next tick).
    fn combat_cools(&mut self, world: &mut World, tick: u64) {
        let mut q = world.query::<(Entity, &InCombat)>();
        self.cooled.extend(
            q.iter(world)
                .filter(|(_, c)| c.until <= tick)
                .map(|(e, _)| e),
        );
        for e in self.cooled.drain(..) {
            world.entity_mut(e).remove::<InCombat>();
        }
    }

    /// Every mob walks its route at its own pace (the pace is the
    /// mob's brain, not a component the kit could lean on).
    fn mobs_walk(&mut self, world: &mut World, dt: f32) {
        let walkers = self.walkers.get_or_insert_with(|| world.query());
        for (mut pos, mut mob) in walkers.iter_mut(world) {
            let Some(&[tx, tz]) = mob.route.get(mob.leg) else {
                continue; // no route, or held at its end
            };
            let step = mob.pace * dt;
            if walk(&mut pos, tx, tz, step) {
                mob.leg += 1;
                if mob.patrol && mob.leg == mob.route.len() {
                    mob.leg = 0;
                }
            }
        }
    }

    /// Game-code despawn: a mob whose time is up leaves the world. The
    /// kit learns it from the world's removed-component buffers (§8.2):
    /// no leave, no hook.
    fn mobs_die(&mut self, world: &mut World, tick: u64) {
        let mut q = world.query::<(Entity, &Mob)>();
        self.dying.extend(
            q.iter(world)
                .filter(|(_, m)| m.dies_at <= tick)
                .map(|(e, _)| e),
        );
        for e in self.dying.drain(..) {
            world.despawn(e);
        }
    }

    /// Players run toward their target on the ground and stop on it.
    fn players_run(&mut self, world: &mut World, dt: f32) {
        let runners = self.runners.get_or_insert_with(|| world.query());
        for (mut pos, target, &RunSpeed(speed)) in runners.iter_mut(world) {
            if pos.x != target.x || pos.z != target.z {
                walk(&mut pos, target.x, target.z, speed * dt);
            }
        }
    }
}
