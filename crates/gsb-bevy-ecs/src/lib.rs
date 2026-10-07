//! The [`bevy_ecs`] binding of gsb — the `bevy_ecs` crate only, never the
//! Bevy engine (a game server needs no game engine).
//!
//! The engine is ECS-agnostic: the call table ([`gsb_core::calls::Calls`])
//! is a plain struct the room or shard actor owns and lends to the game
//! (`GameLogic::update_with_calls`), and an [`Owner`](gsb_core::Owner) is
//! an opaque `u64`. This crate is the thin glue a bevy_ecs game uses
//! (`docs/SERVICES.md` §5.5):
//!
//! - [`calls`]: the table as a resource for the game's systems
//!   ([`GsbCalls`], lent for the schedule's run by [`lend`]), service
//!   clients as resources ([`Svc`]), `Owner ⇄ Entity` ([`OwnerExt`],
//!   [`EntityExt`]) and the owner-alive test ([`owner_alive`]).
//!
//! One binding crate per ECS (`docs/KIT-ECS.md` K10 (b)): the kit's
//! bevy_ecs binding joins this crate as a `kit` module once the kit is
//! ECS-agnostic, so a game picks its ECS once.

pub mod calls;

pub use calls::{EntityExt, GsbCalls, OwnerExt, Svc, lend, owner_alive};
