//! `afk_action`: what the input-idle ceiling does to the member it
//! expires, as the config file spells it — `"leave_room"` or
//! `"disconnect"` ([`Config::afk_action`](crate::config::Config::afk_action),
//! BACKLOG E6). The spellings are the core's
//! (`gsb_core::room::AfkAction::label`), so the file and the engine
//! cannot drift. Omitted = the game's default (`GameModule::afk_action`),
//! `leave_room` when the game has none.

use std::fmt;

use gsb_core::room::AfkAction;
use serde::de::{self, Deserializer, Visitor};

/// Deserialize a written `afk_action` (as `Some`: written).
pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<AfkAction>, D::Error> {
    d.deserialize_str(Action).map(Some)
}

struct Action;

impl Visitor<'_> for Action {
    type Value = AfkAction;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words: Vec<String> = AfkAction::ALL
            .iter()
            .map(|a| format!("\"{}\"", a.label()))
            .collect();
        write!(f, "{}", words.join(" or "))
    }

    fn visit_str<E: de::Error>(self, word: &str) -> Result<Self::Value, E> {
        AfkAction::parse(word).ok_or_else(|| E::invalid_value(de::Unexpected::Str(word), &self))
    }
}

#[cfg(test)]
mod tests;
