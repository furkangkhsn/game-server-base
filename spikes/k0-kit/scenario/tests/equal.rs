//! The proof: the same scripted world, played through today's bevy-native
//! kit path and through the ECS-free core under each binding, ends every
//! tick in the same book state — and the agnostic drivers emit the same
//! packet bytes for every member cell.

use k0_kit_scenario::native::NativeBevy;
use k0_kit_scenario::{Agnostic, BevySide, Driver, HecsDiffSide, HecsTrackedSide, script};

fn run(n: usize, ticks: usize, move_pct: usize, churn: usize, seed: u64) {
    let s = script(n, ticks, move_pct, churn, seed);
    let mut drivers: Vec<(&str, Box<dyn Driver>)> = vec![
        ("native-bevy", Box::new(NativeBevy::new(s.slots))),
        (
            "core+bevy_ecs",
            Box::new(Agnostic::<BevySide>::new(s.slots)),
        ),
        (
            "core+hecs-tracked",
            Box::new(Agnostic::<HecsTrackedSide>::new(s.slots)),
        ),
        (
            "core+hecs-diff",
            Box::new(Agnostic::<HecsDiffSide>::new(s.slots)),
        ),
    ];
    let steps = std::iter::once(&s.init).chain(s.ticks.iter());
    let (mut moved, mut born, mut exits) = (0usize, 0usize, 0usize);
    for (tick, ops) in steps.enumerate() {
        for (_, d) in &mut drivers {
            d.apply(ops);
            d.update();
        }
        let (name0, d0) = &drivers[0];
        let reference = d0.canon();
        moved += reference.changes.values().map(|c| c.0.len()).sum::<usize>();
        exits += reference.changes.values().map(|c| c.1.len()).sum::<usize>();
        born += reference.born.len();
        for (name, d) in &drivers[1..] {
            assert_eq!(
                d.canon(),
                reference,
                "tick {tick}: {name} differs from {name0}"
            );
        }
        let packets = drivers[1].1.packets(tick as u64).expect("agnostic");
        for (name, d) in &drivers[2..] {
            assert_eq!(
                d.packets(tick as u64).expect("agnostic"),
                packets,
                "tick {tick}: {name}'s packets differ from core+bevy_ecs"
            );
        }
    }
    // The scenario must actually exercise the paths it claims to.
    assert!(moved > n, "updates recorded");
    assert!(exits > 0, "exits (crossings, despawns) recorded");
    assert!(born > 0, "group births recorded");
}

#[test]
fn small_world_every_tick_equal() {
    run(300, 120, 10, 5, 7);
}

#[test]
fn busy_world_every_tick_equal() {
    // Everyone moves, heavy churn: crossings, births, cell exits, recycled
    // entity slots.
    run(1000, 80, 100, 30, 99);
}

#[test]
fn sparse_world_every_tick_equal() {
    run(60, 300, 50, 50, 3);
}
