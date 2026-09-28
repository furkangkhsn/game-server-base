//! The top level of the config file (BACKLOG F62): the one table the
//! engine shares with the hosted game. A top-level key is the engine's
//! (a field of [`Config`]) or a game's (`GameModule::owned_keys`: the
//! hosted game's, or another compiled-in game's — a file may carry a
//! sibling game's table). Any other key refuses startup, before anything
//! binds, instead of being silently ignored.
//!
//! The engine's keys are read from `Config`'s own derived deserializer
//! (the field list serde hands a struct's deserializer), so the list
//! cannot drift from the struct. The demo's flat keys, which live on
//! `Config` for compatibility (GAME-MODULE §6 decision 1), are taken out
//! of it: they are the demo's, accepted only where the demo is compiled
//! in.

use std::fmt;

use serde::de::{self, Visitor};

use crate::{Config, GameModule, ServerError};

/// The demo's flat keys (GAME-MODULE §6 decision 1): fields of [`Config`]
/// that only the demo module reads — its `GameModule::owned_keys`, and
/// not the engine's. Each must be a field of `Config` (a test holds it).
pub(crate) const DEMO_KEYS: &[&str] = &[
    "visibility",
    "topology",
    "communication",
    "shard_count",
    "aoi_cell_size",
    "team_vision_radius",
    "spawn_half_size",
    "disconnect_grace_secs",
];

/// Each game's owned top-level keys, by game name.
pub(crate) type Owners = [(&'static str, Vec<&'static str>)];

impl Config {
    /// Refuse a top-level key nobody reads (BACKLOG F62): every key of
    /// [`Self::raw`] must be one of the engine's (a field of `Config`) or
    /// owned by a game — `hosted` or any game compiled into this build
    /// (`gsb_server::games`), whose [`GameModule::owned_keys`] name them.
    /// A table for a game this build does not host is refused too: typo
    /// protection wins over a file shared between builds.
    ///
    /// Every start runs it first, before anything binds; public so a
    /// caller can check a file against a module without starting. A
    /// config built in code (empty `raw`) always passes.
    pub fn check_top_level_keys(&self, hosted: &dyn GameModule) -> Result<(), ServerError> {
        let mut owners = vec![(hosted.name(), hosted.owned_keys())];
        owners.extend(crate::games::owned_keys());
        check(&self.raw, &owners)
    }
}

/// The first key of `raw` that is neither the engine's nor in `owners`,
/// as the startup error.
pub(crate) fn check(raw: &toml::Table, owners: &Owners) -> Result<(), ServerError> {
    let owned = |key: &str| owners.iter().any(|(_, keys)| keys.contains(&key));
    let unknown = raw.iter().find(|(k, _)| !is_engine_key(k) && !owned(k));
    let Some((key, value)) = unknown else {
        return Ok(());
    };
    Err(ServerError::UnknownKey {
        key: key.clone(),
        written: written(key, value),
        suggestion: closest(key, owners),
        owners: listing(owners),
    })
}

/// The engine's own top-level keys: `Config`'s fields, the demo's aside.
pub(crate) fn engine_keys() -> impl Iterator<Item = &'static str> {
    config_fields()
        .iter()
        .copied()
        .filter(|k| !DEMO_KEYS.contains(k))
}

fn is_engine_key(key: &str) -> bool {
    engine_keys().any(|k| k == key)
}

/// Every top-level key `Config` deserializes, spelled as the file writes
/// it (renames applied, `#[serde(skip)]` fields absent): the list its
/// derived `Deserialize` hands to `deserialize_struct`.
pub(crate) fn config_fields() -> &'static [&'static str] {
    let mut fields = None;
    let _ = <Config as serde::Deserialize>::deserialize(FieldProbe(&mut fields));
    fields.expect("`Config` deserializes as a plain struct (no `flatten`)")
}

/// How the file writes `key`: a value (`tik_hz`), a table (`[metric]`,
/// or `[metric.otlp]` when it holds only sub-tables), or an array of
/// tables (`[[listener]]`).
fn written(key: &str, value: &toml::Value) -> String {
    match value {
        toml::Value::Table(t) => match t.iter().next() {
            Some((sub, _)) if t.values().all(toml::Value::is_table) => format!("`[{key}.{sub}]`"),
            _ => format!("`[{key}]`"),
        },
        toml::Value::Array(a) if !a.is_empty() && a.iter().all(toml::Value::is_table) => {
            format!("`[[{key}]]`")
        }
        _ => format!("`{key}`"),
    }
}

/// The known key `key` most resembles (one or two edits away), if any.
fn closest(key: &str, owners: &Owners) -> Option<String> {
    let owned = owners.iter().flat_map(|(_, keys)| keys.iter().copied());
    engine_keys()
        .chain(owned)
        .map(|k| (distance(key, k), k))
        .filter(|&(d, _)| d <= 2 && d < key.len())
        .min_by_key(|&(d, _)| d)
        .map(|(_, k)| k.to_string())
}

/// The games' keys as the error lists them, one game after another
/// (the hosted game first; a game listed twice, once).
fn listing(owners: &Owners) -> String {
    let mut seen: Vec<&(&str, Vec<&str>)> = Vec::new();
    for owner in owners {
        if !seen.contains(&owner) {
            seen.push(owner);
        }
    }
    let game = |(name, keys): &&(&str, Vec<&str>)| {
        if keys.is_empty() {
            return format!("{name}: none");
        }
        let keys: Vec<String> = keys.iter().map(|k| format!("`{k}`")).collect();
        format!("{name}: {}", keys.join(", "))
    };
    seen.iter().map(game).collect::<Vec<_>>().join("; ")
}

/// Levenshtein distance (characters).
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(above + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

/// A deserializer that answers nothing: it records the field list a
/// struct's derived `Deserialize` hands it, then stops.
struct FieldProbe<'a>(&'a mut Option<&'static [&'static str]>);

/// The probe's one error: every deserialization it sees stops.
#[derive(Debug)]
struct Probed;

impl fmt::Display for Probed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("field probe")
    }
}

impl std::error::Error for Probed {}

impl de::Error for Probed {
    fn custom<T: fmt::Display>(_: T) -> Self {
        Probed
    }
}

impl<'de> de::Deserializer<'de> for FieldProbe<'_> {
    type Error = Probed;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Probed> {
        Err(Probed)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Probed> {
        *self.0 = Some(fields);
        Err(Probed)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}

#[cfg(test)]
mod tests;
