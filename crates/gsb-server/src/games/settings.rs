//! Reading a hosted game's settings from the parsed config file
//! (GAME-MODULE §4.3): a game other than the 2D demo reads ONLY the table
//! named after itself (`[arena]`, `[mmo]`), refuses an unknown key in
//! it, and refuses every flat key it fixes when that key is written
//! EXPLICITLY (§6 decision 2: never silently ignored). A config built in
//! code has an empty [`Config::raw`](crate::Config::raw), so nothing in
//! it counts as written and every setting takes the game's default.
//!
//! The readers are public: a third-party game hosted through
//! `start_game_server` can read its own table the same way.

use std::time::Duration;

use crate::{GameError, ServerError};

/// Why a hosted game refused its settings (carried as the source of
/// [`GameError::Module`]).
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// A key the game fixes was written explicitly.
    #[error("config key `{key}` is fixed by this game and must not be set: {why}")]
    Fixed {
        /// The key as written (`visibility`, or `mmo.shard_count` inside
        /// the game's own table).
        key: String,
        /// What the game fixes it to, and where to look instead.
        why: &'static str,
    },
    /// A key the game's table does not know (a typo would otherwise be
    /// silently ignored).
    #[error("unknown key `{table}.{key}` (the `[{table}]` table knows: {known})")]
    Unknown {
        /// The game's table.
        table: &'static str,
        /// The unknown key.
        key: String,
        /// The keys the table knows, comma-separated.
        known: String,
    },
    /// A known key with a value of the wrong type or out of range.
    #[error("config key `{key}`: expected {expected}")]
    BadValue {
        /// The key (`arena.teams`).
        key: String,
        /// What the game accepts there.
        expected: &'static str,
    },
}

impl SettingsError {
    /// This error as the server's startup error for `game`.
    pub fn into_server(self, game: &'static str) -> ServerError {
        GameError::Module {
            game,
            source: Box::new(self),
        }
        .into()
    }
}

/// A flat key a game fixes, with the reason its error message gives.
pub type FixedKey = (&'static str, &'static str);

/// The game's own table, checked: every `fixed` key written at the top
/// level (or inside the table) is refused, and every key of the table
/// must be in `known`. An absent table reads as empty.
pub fn own_table<'a>(
    raw: &'a toml::Table,
    table: &'static str,
    fixed: &[FixedKey],
    known: &[&'static str],
) -> Result<Option<&'a toml::Table>, SettingsError> {
    for &(key, why) in fixed {
        if raw.contains_key(key) {
            return Err(SettingsError::Fixed {
                key: key.into(),
                why,
            });
        }
    }
    let own = match raw.get(table) {
        None => return Ok(None),
        Some(toml::Value::Table(t)) => t,
        Some(_) => {
            return Err(SettingsError::BadValue {
                key: table.into(),
                expected: "a table (`[<game>]` section)",
            });
        }
    };
    for key in own.keys() {
        // A table key may share a flat key's name (the arena's own
        // `disconnect_grace_secs`): known wins.
        if known.contains(&key.as_str()) {
            continue;
        }
        if let Some(&(_, why)) = fixed.iter().find(|(k, _)| k == key) {
            return Err(SettingsError::Fixed {
                key: format!("{table}.{key}"),
                why,
            });
        }
        return Err(SettingsError::Unknown {
            table,
            key: key.clone(),
            known: known.join(", "),
        });
    }
    Ok(Some(own))
}

/// An integer setting in `range`, or `None` when not written.
pub fn integer(
    own: Option<&toml::Table>,
    table: &'static str,
    key: &'static str,
    range: std::ops::RangeInclusive<i64>,
    expected: &'static str,
) -> Result<Option<i64>, SettingsError> {
    let Some(value) = own.and_then(|t| t.get(key)) else {
        return Ok(None);
    };
    match value.as_integer() {
        Some(n) if range.contains(&n) => Ok(Some(n)),
        _ => Err(SettingsError::BadValue {
            key: format!("{table}.{key}"),
            expected,
        }),
    }
}

/// A duration in seconds (an integer or a float, finite and not
/// negative), or `None` when not written.
pub fn seconds(
    own: Option<&toml::Table>,
    table: &'static str,
    key: &'static str,
) -> Result<Option<Duration>, SettingsError> {
    let Some(value) = own.and_then(|t| t.get(key)) else {
        return Ok(None);
    };
    let secs = match value {
        toml::Value::Integer(n) => *n as f64,
        toml::Value::Float(f) => *f,
        _ => f64::NAN,
    };
    if secs.is_finite() && secs >= 0.0 {
        Ok(Some(Duration::from_secs_f64(secs)))
    } else {
        Err(SettingsError::BadValue {
            key: format!("{table}.{key}"),
            expected: "a number of seconds, 0 or more",
        })
    }
}

/// One of `choices` (a string), or `None` when not written.
pub fn choice(
    own: Option<&toml::Table>,
    table: &'static str,
    key: &'static str,
    choices: &[&'static str],
    expected: &'static str,
) -> Result<Option<&'static str>, SettingsError> {
    let Some(value) = own.and_then(|t| t.get(key)) else {
        return Ok(None);
    };
    value
        .as_str()
        .and_then(|s| choices.iter().find(|c| **c == s).copied())
        .map(Some)
        .ok_or_else(|| SettingsError::BadValue {
            key: format!("{table}.{key}"),
            expected,
        })
}

/// A duration as a config line prints it (`30 s`, `0.5 s`).
pub(crate) fn secs_label(d: Duration) -> String {
    format!("{} s", d.as_secs_f64())
}
