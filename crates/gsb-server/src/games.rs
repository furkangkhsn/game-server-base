//! The games this build hosts, one cargo feature each
//! (docs/GAME-MODULE.md §4.2), and the `game` config key's lookup.

use crate::{GameError, GameModule};

pub mod demo;

/// The `game` config key's default: the 2D demo, the game every
/// pre-module config and test has always run.
pub const DEFAULT_GAME: &str = "demo";

/// One compiled-in game: its name and a constructor for a fresh module.
type Entry = (&'static str, fn() -> Box<dyn GameModule>);

/// The catalog. A game compiled out of the build is simply absent: the
/// lookup then reports it as unknown, with this list.
const GAMES: &[Entry] = &[(demo::DemoModule::NAME, demo::DemoModule::boxed)];

/// The names of the games compiled into this build.
pub fn compiled_in() -> Vec<&'static str> {
    GAMES.iter().map(|(name, _)| *name).collect()
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
