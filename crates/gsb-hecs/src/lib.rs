//! The [`hecs`] binding of gsb.
//!
//! The engine is ECS-agnostic: the call table is a plain struct the actor
//! lends to the game as `&mut Calls`, an [`Owner`](gsb_core::Owner) an
//! opaque `u64`. A hecs game's systems are plain functions
//! (`fn sys(world: &mut hecs::World, calls: &mut Calls, bank: &BankClient)`),
//! so the binding is only the owner model (`docs/SERVICES.md` §5.5):
//!
//! - [`calls`]: `Owner ⇄ Entity` ([`OwnerExt`], [`EntityExt`]) and the
//!   owner-alive test ([`owner_alive`]).
//!
//! One binding crate per ECS (`docs/KIT-ECS.md` K10 (b)): the kit's hecs
//! binding joins this crate as a `kit` module once the kit is
//! ECS-agnostic.

pub mod calls;

pub use calls::{EntityExt, OwnerExt, owner_alive};
