//! The games this build hosts, one cargo feature each
//! (docs/GAME-MODULE.md §4.2), and the `game` config key's lookup.
//!
//! Built with `--no-default-features` the catalog is empty: the library
//! still compiles (the structural proof that the server core is
//! game-agnostic), and `start_server` refuses every `game` name while a
//! caller's own module still runs through `start_game_server`.

use crate::{GameError, GameModule};

#[cfg(feature = "game-arena")]
pub mod arena;
#[cfg(feature = "game-demo")]
pub mod demo;
#[cfg(feature = "game-mmo")]
pub mod mmo;
#[cfg(any(feature = "game-arena", feature = "game-mmo", feature = "game-war"))]
pub mod settings;
#[cfg(feature = "game-war")]
pub mod war;

/// The `game` config key's default: the 2D demo, the game every
/// pre-module config and test has always run.
pub const DEFAULT_GAME: &str = "demo";

/// One compiled-in game: its name and a constructor for a fresh module.
type Entry = (&'static str, fn() -> Box<dyn GameModule>);

/// The catalog. A game compiled out of the build is simply absent: the
/// lookup then reports it as unknown, with this list.
const GAMES: &[Entry] = &[
    #[cfg(feature = "game-demo")]
    (demo::DemoModule::NAME, demo::DemoModule::boxed),
    #[cfg(feature = "game-arena")]
    (arena::ArenaModule::NAME, arena::ArenaModule::boxed),
    #[cfg(feature = "game-mmo")]
    (mmo::MmoModule::NAME, mmo::MmoModule::boxed),
    #[cfg(feature = "game-war")]
    (war::WarModule::NAME, war::WarModule::boxed),
];

/// The names of the games compiled into this build.
pub fn compiled_in() -> Vec<&'static str> {
    GAMES.iter().map(|(name, _)| *name).collect()
}

/// The top-level config keys each compiled-in game owns
/// ([`GameModule::owned_keys`], BACKLOG F62): a config file may carry any
/// of them, whichever game it hosts.
pub fn owned_keys() -> Vec<(&'static str, Vec<&'static str>)> {
    GAMES
        .iter()
        .map(|(name, make)| (*name, make().owned_keys()))
        .collect()
}

/// A fresh, unconfigured module for the game named `name`.
pub fn by_name(name: &str) -> Result<Box<dyn GameModule>, GameError> {
    GAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, make)| make())
        .ok_or_else(|| GameError::Unknown {
            name: name.to_string(),
            compiled_in: compiled_in(),
        })
}
