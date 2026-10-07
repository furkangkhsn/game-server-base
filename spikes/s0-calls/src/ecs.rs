//! The ECS-free room flavour (the core has no ECS dependency at all;
//! bevy_ecs and hecs adapters are separate optional crates:
//! `adapters/bevy-ecs`, `adapters/hecs`).

use crate::calls::Calls;
use crate::contract::Owner;
use crate::room::{Control, RoomLogic, StateHasher, TickCtx};

/// No ECS: the game is a struct; it says itself which owners are alive.
pub mod plain {
    use super::*;

    pub trait PlainGame {
        fn alive(&self, owner: Owner) -> bool;
        /// Join hook: add the player and `calls.bind(conn, Owner(id))`.
        fn control(&mut self, calls: &mut Calls, c: &Control);
        fn systems(&mut self, calls: &mut Calls, ctx: &mut TickCtx);
        fn state_hash(&self, h: &mut StateHasher);
    }

    pub struct PlainRoom<G> {
        pub calls: Calls,
        pub game: G,
    }

    impl<G: PlainGame> RoomLogic for PlainRoom<G> {
        fn with_calls<T>(&mut self, f: impl FnOnce(&mut Calls, &dyn Fn(Owner) -> bool) -> T) -> T {
            let game = &self.game;
            f(&mut self.calls, &|o| game.alive(o))
        }
        fn control(&mut self, c: &Control, _ctx: &mut TickCtx) {
            self.game.control(&mut self.calls, c);
        }
        fn systems(&mut self, ctx: &mut TickCtx) {
            self.game.systems(&mut self.calls, ctx);
        }
        fn state_hash(&mut self, h: &mut StateHasher) {
            self.game.state_hash(h);
        }
    }
}
