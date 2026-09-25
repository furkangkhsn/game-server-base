//! `max_detach_hold_secs`: the detach-hold ceiling as the config file
//! spells it — seconds, or `"off"` for none
//! ([`Config::max_detach_hold`](crate::config::Config::max_detach_hold)).
//!
//! A string sentinel because TOML has no null, and the three meanings
//! must stay apart: omitted = the core's default, a number = that many
//! seconds (`0` = no extension, literally), `"off"` = no ceiling. Rejected
//! spellings: `0` = off (the idle ceiling's convention) — zero has a safe
//! meaning here and a typo must not become the unbounded lock; a negative
//! number = off — the same typo risk; a separate boolean key — two keys
//! for one setting, and they could disagree.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Deserializer, Visitor};

/// Deserialize the ceiling: a non-negative number of seconds, or `"off"`.
pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    d.deserialize_any(Ceiling)
}

struct Ceiling;

impl Visitor<'_> for Ceiling {
    type Value = Option<Duration>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number of seconds >= 0, or \"off\" (no ceiling)")
    }

    fn visit_u64<E: de::Error>(self, secs: u64) -> Result<Self::Value, E> {
        Ok(Some(Duration::from_secs(secs)))
    }

    fn visit_i64<E: de::Error>(self, secs: i64) -> Result<Self::Value, E> {
        u64::try_from(secs)
            .map(|s| Some(Duration::from_secs(s)))
            .map_err(|_| E::invalid_value(de::Unexpected::Signed(secs), &self))
    }

    fn visit_f64<E: de::Error>(self, secs: f64) -> Result<Self::Value, E> {
        Duration::try_from_secs_f64(secs)
            .map(Some)
            .map_err(|_| E::invalid_value(de::Unexpected::Float(secs), &self))
    }

    fn visit_str<E: de::Error>(self, word: &str) -> Result<Self::Value, E> {
        match word {
            "off" => Ok(None),
            _ => Err(E::invalid_value(de::Unexpected::Str(word), &self)),
        }
    }
}
