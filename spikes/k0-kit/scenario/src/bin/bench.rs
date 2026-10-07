//! The k0 benchmark: the kit phase of the AOI path (`AoiRoom::update`
//! after the game's systems) per tick, today's bevy-native path vs the
//! ECS-free core under each binding. Plain `Instant` timing; each driver
//! runs `ROUNDS` times interleaved, the reported figure per driver is the
//! median over rounds of the per-round median tick (p90 likewise).
//!
//! `cargo run --release --bin bench [-- quick]` — `quick`: the 10k
//! sizes only. `CHURN=<per mille>` (default 1): the NPC despawn/spawn
//! rate per tick (with 0, no player leaves or joins either).

use std::time::{Duration, Instant};

use k0_kit_scenario::native::NativeBevy;
use k0_kit_scenario::{
    Agnostic, BevySide, Driver, HecsDiffSide, HecsLoggedSide, HecsTrackedSide, script,
};

const ROUNDS: usize = 5;

fn make(name: &str, slots: usize) -> Box<dyn Driver> {
    match name {
        "native-bevy" => Box::new(NativeBevy::new(slots)),
        "core+bevy_ecs" => Box::new(Agnostic::<BevySide>::new(slots)),
        "core+hecs-tracked" => Box::new(Agnostic::<HecsTrackedSide>::new(slots)),
        "core+hecs-logged" => Box::new(Agnostic::<HecsLoggedSide>::new(slots)),
        "core+hecs-diff" => Box::new(Agnostic::<HecsDiffSide>::new(slots)),
        _ => unreachable!(),
    }
}

fn pct(v: &mut [Duration], p: f64) -> Duration {
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p) as usize]
}

fn main() {
    let quick = std::env::args().any(|a| a == "quick");
    let names = [
        "native-bevy",
        "core+bevy_ecs",
        "core+hecs-tracked",
        "core+hecs-logged",
        "core+hecs-diff",
    ];
    let sizes: &[(usize, usize, usize)] = if quick {
        &[(10_000, 10, 300), (10_000, 1, 300)]
    } else {
        &[
            (1_000, 10, 600),
            (10_000, 10, 400),
            (10_000, 1, 400),
            (100_000, 10, 100),
        ]
    };
    println!("size,move_pct,driver,update_median_us,update_p90_us,apply_median_us");
    for &(n, move_pct, ticks) in sizes {
        let s = script(
            n,
            ticks + 20,
            move_pct,
            std::env::var("CHURN").map_or(1, |v| v.parse().unwrap()),
            42,
        );
        let mut med: Vec<Vec<Duration>> = vec![Vec::new(); names.len()];
        let mut p90: Vec<Vec<Duration>> = vec![Vec::new(); names.len()];
        let mut app: Vec<Vec<Duration>> = vec![Vec::new(); names.len()];
        for _ in 0..ROUNDS {
            for (i, name) in names.iter().enumerate() {
                let mut d = make(name, s.slots);
                d.apply(&s.init);
                d.update();
                let mut upd = Vec::with_capacity(ticks);
                let mut apl = Vec::with_capacity(ticks);
                for (t, ops) in s.ticks.iter().enumerate() {
                    let a = Instant::now();
                    d.apply(ops);
                    let b = Instant::now();
                    d.update();
                    let c = Instant::now();
                    if t >= 20 {
                        apl.push(b - a);
                        upd.push(c - b);
                    }
                }
                med[i].push(pct(&mut upd, 0.5));
                p90[i].push(pct(&mut upd, 0.9));
                app[i].push(pct(&mut apl, 0.5));
            }
        }
        for (i, name) in names.iter().enumerate() {
            let us = |d: Duration| d.as_secs_f64() * 1e6;
            println!(
                "{n},{move_pct},{name},{:.1},{:.1},{:.1}",
                us(pct(&mut med[i], 0.5)),
                us(pct(&mut p90[i], 0.5)),
                us(pct(&mut app[i], 0.5)),
            );
        }
    }
}
