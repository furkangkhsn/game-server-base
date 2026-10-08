//! Determinism at the room boundary (round q4, `docs/SERVICES.md` §10):
//! TEST mode (`sim`) and record → replay (`record`, feature `record`),
//! both over one ECS-free game ([`game`]): a wallet per player, an
//! inventory load per player (in-process call), a `Buy` client request
//! answered by a bank call (`.owner().with().answers()`), and a dice roll
//! from the room's generator every tick.

mod events;
mod game;
#[cfg(feature = "record")]
mod record;
mod sim;
