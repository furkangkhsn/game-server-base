//! Convenience re-exports for the game crate.

pub use bevy_ecs::entity::Entity;
pub use bevy_ecs::prelude::{Component, Query, Resource, World};
pub use bevy_ecs::world::World as BevyWorld;

pub use crate::{System, SystemCtx, SystemRunner};
