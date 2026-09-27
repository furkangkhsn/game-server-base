//! The demo's `MovementSystem` in isolation (BACKLOG F1): spawn an entity,
//! give it a target, run the system, read back its position and whether
//! it arrived. The room-level tests cover it only indirectly (through
//! the snapshots it causes).
//!
//! Every distance below is a 3-4-5 triangle, so the expected values are
//! exact in `f32` and the assertions can compare with `==` where the
//! system promises an exact landing.

use bevy_ecs::change_detection::DetectChanges;

use super::*;

/// A world with one entity at `at`, heading for `to`, with an optional
/// explicit speed. Change trackers are cleared, so a later `is_changed`
/// means "the system wrote the position".
fn one_mover(at: (f32, f32), to: (f32, f32), speed: Option<f32>) -> (World, Entity) {
    let mut world = World::new();
    let mut e = world.spawn((
        Position { x: at.0, y: at.1 },
        MoveTarget { x: to.0, y: to.1 },
    ));
    if let Some(s) = speed {
        e.insert(Speed(s));
    }
    let entity = e.id();
    world.clear_trackers();
    (world, entity)
}

fn run(world: &mut World, dt: f32) {
    MovementSystem.run(world, &SystemCtx { tick: 1, dt });
}

fn pos(world: &World, e: Entity) -> Position {
    *world.get::<Position>(e).expect("position")
}

fn has_target(world: &World, e: Entity) -> bool {
    world.get::<MoveTarget>(e).is_some()
}

fn pos_written(world: &World, e: Entity) -> bool {
    world
        .entity(e)
        .get_ref::<Position>()
        .expect("position")
        .is_changed()
}

/// One tick covers `speed · dt` along the straight line toward the
/// target — direction and distance both — and the target stays until
/// the entity gets there.
#[test]
fn a_tick_moves_speed_times_dt_along_the_line_to_the_target() {
    // |d| = 50 toward (+30, -40); speed 10, dt 0.5 → 5 units.
    let (mut world, e) = one_mover((1.0, 2.0), (31.0, -38.0), Some(10.0));
    run(&mut world, 0.5);
    let p = pos(&world, e);
    assert!((p.x - 4.0).abs() < 1e-5, "x: 1 + 5·3/5: {p:?}");
    assert!((p.y - (-2.0)).abs() < 1e-5, "y: 2 − 5·4/5: {p:?}");
    assert!(has_target(&world, e), "not arrived: the target stays");
}

/// An entity without a `Speed` moves at `DEFAULT_SPEED`; one with a
/// `Speed` moves at its own.
#[test]
fn speed_comes_from_the_component_or_the_default() {
    let (mut world, e) = one_mover((0.0, 0.0), (300.0, 400.0), None);
    run(&mut world, 0.1);
    let p = pos(&world, e);
    let travelled = (p.x * p.x + p.y * p.y).sqrt();
    assert!(
        (travelled - DEFAULT_SPEED * 0.1).abs() < 1e-5,
        "default speed: {p:?}"
    );

    let (mut world, e) = one_mover((0.0, 0.0), (300.0, 400.0), Some(2.5));
    run(&mut world, 2.0);
    let p = pos(&world, e);
    assert!(
        (p.x - 3.0).abs() < 1e-5 && (p.y - 4.0).abs() < 1e-5,
        "{p:?}"
    );
}

/// Within one step the entity lands EXACTLY on the target (no
/// overshoot) and its `MoveTarget` is removed: it has arrived.
#[test]
fn within_one_step_it_lands_on_the_target_and_arrives() {
    // |d| = 5, step = 10 · 1 = 10.
    let (mut world, e) = one_mover((0.0, 0.0), (3.0, -4.0), Some(10.0));
    run(&mut world, 1.0);
    assert_eq!(pos(&world, e), Position { x: 3.0, y: -4.0 });
    assert!(!has_target(&world, e), "arrived: the target is removed");
}

/// A step of exactly the remaining distance is an arrival too (not a
/// partial move that leaves the target behind forever).
#[test]
fn a_step_of_exactly_the_distance_arrives() {
    // |d| = 5, step = 10 · 0.5 = 5.
    let (mut world, e) = one_mover((0.0, 0.0), (3.0, 4.0), Some(10.0));
    run(&mut world, 0.5);
    assert_eq!(pos(&world, e), Position { x: 3.0, y: 4.0 });
    assert!(!has_target(&world, e), "exact step: arrived");
}

/// Spawn → target → run until it stops: it takes `ceil(dist / step)`
/// ticks, it never passes the target, and afterwards further ticks leave
/// it alone. Runs through the demo's system stack, the path every demo
/// room takes.
#[test]
fn run_to_arrival_through_the_demo_stack() {
    // |d| = 25, step = 10 · 0.1 = 1 → 25 ticks.
    let (mut world, e) = one_mover((0.0, 0.0), (15.0, 20.0), Some(10.0));
    let mut runner = movement_runner();
    let mut ticks = 0;
    while has_target(&world, e) {
        ticks += 1;
        assert!(
            ticks <= 25,
            "should have arrived by now: {:?}",
            pos(&world, e)
        );
        runner.run_all(
            &mut world,
            &SystemCtx {
                tick: ticks,
                dt: 0.1,
            },
        );
        let p = pos(&world, e);
        assert!(
            p.x <= 15.0 && p.y <= 20.0,
            "overshot at tick {ticks}: {p:?}"
        );
    }
    assert_eq!(ticks, 25);
    assert_eq!(pos(&world, e), Position { x: 15.0, y: 20.0 });
    world.clear_trackers();
    runner.run_all(&mut world, &SystemCtx { tick: 26, dt: 0.1 });
    assert!(!pos_written(&world, e), "an arrived entity is not written");
}

/// An entity already on its target is not written (no move, no spurious
/// change).
#[test]
fn an_entity_on_its_target_is_not_written() {
    let (mut world, e) = one_mover((7.0, -7.0), (7.0, -7.0), Some(10.0));
    run(&mut world, 1.0);
    assert_eq!(pos(&world, e), Position { x: 7.0, y: -7.0 });
    assert!(!pos_written(&world, e));
}

/// A non-positive `dt` moves nothing and writes nothing (a zero step is
/// not a move; a negative one would walk backwards).
#[test]
fn a_non_positive_dt_moves_nothing() {
    for dt in [0.0, -0.5] {
        let (mut world, e) = one_mover((0.0, 0.0), (3.0, 4.0), Some(10.0));
        run(&mut world, dt);
        assert_eq!(pos(&world, e), Position { x: 0.0, y: 0.0 }, "dt {dt}");
        assert!(!pos_written(&world, e), "dt {dt}: written");
        assert!(has_target(&world, e), "dt {dt}: target kept");
    }
}

/// Only entities with a target move, each toward its own target: a
/// target-less entity is left alone, and one arrival does not end
/// another's move.
#[test]
fn each_entity_moves_toward_its_own_target() {
    let (mut world, near) = one_mover((0.0, 0.0), (-3.0, 4.0), Some(10.0));
    let far = world
        .spawn((Position { x: 0.0, y: 0.0 }, MoveTarget { x: 0.0, y: -50.0 }))
        .id();
    let idle = world.spawn(Position { x: 9.0, y: 9.0 }).id();
    world.clear_trackers();
    run(&mut world, 1.0);
    assert_eq!(pos(&world, near), Position { x: -3.0, y: 4.0 });
    assert!(!has_target(&world, near));
    assert_eq!(
        pos(&world, far),
        Position {
            x: 0.0,
            y: -DEFAULT_SPEED
        }
    );
    assert!(has_target(&world, far));
    assert_eq!(pos(&world, idle), Position { x: 9.0, y: 9.0 });
    assert!(!pos_written(&world, idle));
}
