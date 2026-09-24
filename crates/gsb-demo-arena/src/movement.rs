//! The arena's movement system: 3D kinematic move-to-target (straight
//! line, constant speed, snap on arrival). Movement is the game's
//! (KIT-ARCHITECTURE §6 — the kit has no movement trait by design); the
//! kit only sees the resulting `Pos3` writes.
//!
//! Vertical movement is ordinary movement: a target above or below the
//! unit is reached along the 3D segment, so climbing onto a platform
//! (and out of an enemy's vision sphere) takes time like walking does.

use bevy_ecs::prelude::World;
use bevy_ecs::query::QueryState;

use crate::components::{MoveTarget3, Pos3, Speed};

/// The movers' query: position (written), target and speed (read).
type Movers = QueryState<(&'static mut Pos3, &'static MoveTarget3, &'static Speed)>;

/// The movement system. Holds its query state across ticks (built on
/// first use: the room's world is created by the room actor, after the
/// game).
#[derive(Default)]
pub struct Movement {
    movers: Option<Movers>,
}

impl Movement {
    /// Advance every unit with a [`MoveTarget3`] by `speed · dt` along
    /// the straight 3D segment toward it; a unit within one step of its
    /// target lands exactly on it. A unit already at its target is not
    /// written (no spurious change for the codec's `Changed<Pos3>`).
    pub fn run(&mut self, world: &mut World, dt: f32) {
        let movers = self.movers.get_or_insert_with(|| world.query());
        for (mut pos, &MoveTarget3(target), &Speed(speed)) in movers.iter_mut(world) {
            let d = [target.x - pos.x, target.y - pos.y, target.z - pos.z];
            let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            if dist == 0.0 {
                continue;
            }
            let step = speed * dt;
            if step >= dist {
                *pos = target;
            } else {
                let k = step / dist;
                pos.x += d[0] * k;
                pos.y += d[1] * k;
                pos.z += d[2] * k;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy_ecs::change_detection::DetectChanges;

    use super::*;

    /// A unit climbs diagonally: after one second at 5 m/s it has covered
    /// 5 m of the 3D segment (height included), and it lands exactly on
    /// the target without overshooting.
    #[test]
    fn moves_along_the_3d_segment_and_lands_on_the_target() {
        let mut world = World::new();
        let target = Pos3::new(3.0, 4.0, 12.0); // |d| = 13
        let unit = world
            .spawn((Pos3::default(), MoveTarget3(target), Speed(5.0)))
            .id();
        let mut movement = Movement::default();
        movement.run(&mut world, 1.0);
        let p = *world.get::<Pos3>(unit).expect("pos");
        let travelled = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
        assert!((travelled - 5.0).abs() < 1e-4, "{p:?}");
        assert!((p.y - 5.0 * 4.0 / 13.0).abs() < 1e-4, "height moved: {p:?}");
        for _ in 0..3 {
            movement.run(&mut world, 1.0);
        }
        assert_eq!(*world.get::<Pos3>(unit).expect("pos"), target);
    }

    /// A unit at its target is left untouched: no change tick.
    #[test]
    fn a_unit_at_rest_is_not_written() {
        let mut world = World::new();
        let at = Pos3::new(1.0, 2.0, 3.0);
        let unit = world.spawn((at, MoveTarget3(at), Speed(5.0))).id();
        world.clear_trackers();
        Movement::default().run(&mut world, 1.0);
        let changed = world
            .entity(unit)
            .get_ref::<Pos3>()
            .expect("pos")
            .is_changed();
        assert!(!changed, "a resting unit must not look moved");
    }
}
