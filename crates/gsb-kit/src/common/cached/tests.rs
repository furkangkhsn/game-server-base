//! The kept query sees what a fresh query sees, in the order a fresh
//! query sees it (the frames' byte identity rests on the order).

use bevy_ecs::prelude::{Component, Entity, World};

use super::Cached;

#[derive(Component)]
struct A(u32);

/// Eight tags: their subsets give the world many archetypes that all
/// hold `A`, so the component index's order and creation order part.
#[derive(Component)]
struct T0;
#[derive(Component)]
struct T1;
#[derive(Component)]
struct T2;
#[derive(Component)]
struct T3;
#[derive(Component)]
struct T4;
#[derive(Component)]
struct T5;
#[derive(Component)]
struct T6;
#[derive(Component)]
struct T7;

/// Spawn an `A(n)` carrying the tags whose bits are set in `mask`.
fn spawn(world: &mut World, n: u32, mask: u8) -> Entity {
    let mut e = world.spawn(A(n));
    macro_rules! tag {
        ($bit:expr, $t:expr) => {
            if mask & (1 << $bit) != 0 {
                e.insert($t);
            }
        };
    }
    tag!(0, T0);
    tag!(1, T1);
    tag!(2, T2);
    tag!(3, T3);
    tag!(4, T4);
    tag!(5, T5);
    tag!(6, T6);
    tag!(7, T7);
    e.id()
}

type Kept = Cached<(Entity, &'static A)>;

fn kept_order(kept: &mut Kept, world: &mut World) -> Vec<u32> {
    let state = kept.state(world);
    state.iter(world).map(|(_, a)| a.0).collect()
}

fn fresh_order(world: &mut World) -> Vec<u32> {
    world.query::<&A>().iter(world).map(|a| a.0).collect()
}

/// An entity spawned after the first pass — into an archetype the kept
/// state already knows, and into a brand-new one — is seen by the next.
#[test]
fn an_entity_spawned_after_the_first_pass_is_seen() {
    let mut world = World::new();
    let mut kept = Kept::default();
    spawn(&mut world, 1, 0);
    assert_eq!(kept_order(&mut kept, &mut world), vec![1]);
    spawn(&mut world, 2, 0);
    let mut seen = kept_order(&mut kept, &mut world);
    seen.sort_unstable();
    assert_eq!(seen, vec![1, 2], "same archetype");
    spawn(&mut world, 3, 0b101);
    let mut seen = kept_order(&mut kept, &mut world);
    seen.sort_unstable();
    assert_eq!(seen, vec![1, 2, 3], "new archetype");
}

/// Archetypes created in stages between passes: after every stage the
/// kept state iterates in exactly a fresh query's order (a state that
/// only appended the new archetypes would not — the fresh order is the
/// component index's, not creation order).
#[test]
fn the_kept_order_is_a_fresh_querys_order_after_new_archetypes() {
    let mut world = World::new();
    let mut kept = Kept::default();
    let mut n = 0;
    for stage in 0..4u8 {
        for mask in (0..=u8::MAX).filter(|m| m % 4 == stage) {
            n += 1;
            spawn(&mut world, n, mask);
        }
        assert_eq!(
            kept_order(&mut kept, &mut world),
            fresh_order(&mut world),
            "stage {stage}"
        );
    }
    // A pass with no new archetype: the same order again.
    assert_eq!(kept_order(&mut kept, &mut world), fresh_order(&mut world));
}

/// A state is only valid for the world it was built from: a pass over
/// another world rebuilds it (no mismatched-world panic).
#[test]
fn another_world_gets_its_own_state() {
    let mut first = World::new();
    let mut second = World::new();
    let mut kept = Kept::default();
    spawn(&mut first, 1, 0);
    spawn(&mut second, 2, 0);
    assert_eq!(kept_order(&mut kept, &mut first), vec![1]);
    assert_eq!(kept_order(&mut kept, &mut second), vec![2]);
    assert_eq!(kept_order(&mut kept, &mut first), vec![1]);
}
