//! ECS layer for gsb servers.
//!
//! Binds [`bevy_ecs`] and adds the small scaffolding this architecture needs:
//!
//! - [`System`]: a single game-logic pass over the world. Systems are owned by
//!   the room (one `World` per room actor) and run sequentially, single
//!   threaded — the room actor is the only owner of the world, so no
//!   synchronization is ever needed.
//! - [`SystemRunner`]: an ordered list of systems, run once per tick.
//!
//! Everything game-specific (components, systems, rooms) lives in the game
//! crate; this crate stays game-agnostic. Change detection for the
//! broadcast phase is deliberately *not* here: the room asks the game
//! logic whether the wire content changed (`RoomLogic::snapshot`), and the
//! logic compares what the snapshot actually carries.

pub mod prelude;

use bevy_ecs::world::World;

/// Context passed to every system within a tick.
#[derive(Debug, Clone, Copy)]
pub struct SystemCtx {
    /// Current room tick (starts at 0).
    pub tick: u64,
    /// Seconds since the previous tick (drift-corrected by the pacer).
    pub dt: f32,
}

/// A single game-logic pass over the world.
///
/// Implementations are plain data + a `run` method; the game crate decides
/// what a system is. Keeping this a hand-rolled trait (instead of the full
/// bevy `SystemParam` machinery) keeps the hot path as a plain function
/// call with zero per-call overhead and no scheduling bookkeeping.
pub trait System: Send {
    fn run(&mut self, world: &mut World, ctx: &SystemCtx);
}

/// Ordered collection of systems, run once per tick.
#[derive(Default)]
pub struct SystemRunner {
    systems: Vec<Box<dyn System>>,
}

impl SystemRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a system; execution order = insertion order.
    pub fn add<S: System + 'static>(&mut self, system: S) {
        self.systems.push(Box::new(system));
    }

    /// Run all systems in order.
    pub fn run_all(&mut self, world: &mut World, ctx: &SystemCtx) {
        for system in &mut self.systems {
            system.run(world, ctx);
        }
    }
}
