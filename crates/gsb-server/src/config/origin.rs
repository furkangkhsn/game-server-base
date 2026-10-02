//! Where a config file wrote each of its top-level keys (BACKLOG F64),
//! so a refusal of one (`ServerError::UnknownKey`) can name `path:line`.
//!
//! The top-level check runs at startup on [`Config::raw`], a plain table
//! with no positions, long after the file's text is gone. So
//! [`Config::from_file`] records the positions while it still has the
//! text, in [`Config::origin`]: the file's path and, per top-level key,
//! the line that first writes it (`key = …`, `[key]`, `[key.sub]`,
//! `[[key]]`, `key.sub = …`).
//!
//! WHY a field of its own, and an opaque one: the positions must travel
//! with the config from the loader to the start, and every public way in
//! between takes the `Config` alone. An opaque type keeps the struct
//! literal `Config { tick_hz: 60.0, ..Config::default() }` compiling (a
//! private field would refuse it) and gives a caller nothing to forge:
//! only `from_file` fills it. A config built in code has none, and its
//! refusals read as they always did.
//!
//! [`Config`]: crate::Config
//! [`Config::raw`]: crate::Config::raw
//! [`Config::from_file`]: crate::Config::from_file
//! [`Config::origin`]: crate::Config::origin

use std::collections::BTreeMap;

/// Where a config file wrote its top-level keys (see the module docs).
/// Filled by [`Config::from_file`](crate::Config::from_file); the
/// default — a config built in code — knows no place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOrigin {
    /// The file, as its errors spell it (`Path::display`).
    path: String,
    /// Each top-level key's first line (1-based).
    lines: BTreeMap<String, usize>,
}

impl ConfigOrigin {
    /// The positions of `text`'s top-level keys, read from `path`. Text
    /// that does not parse records no line (the loader refuses it
    /// anyway, with the parser's own position).
    pub(crate) fn of_file(path: String, text: &str) -> Self {
        let mut lines = BTreeMap::new();
        if let Ok(table) = toml::de::DeTable::parse(text) {
            for key in table.get_ref().keys() {
                let at = key.span().start.min(text.len());
                let line = text.as_bytes()[..at]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count();
                lines.insert(key.get_ref().to_string(), line + 1);
            }
        }
        Self { path, lines }
    }

    /// Where the file wrote `key` — `path:line` — or `None` when it did
    /// not (a config built in code, or a key put into `raw` afterwards).
    pub(crate) fn locate(&self, key: &str) -> Option<String> {
        let line = self.lines.get(key)?;
        Some(format!("{}:{line}", self.path))
    }
}

#[cfg(test)]
mod tests;
