//! The logic's own counters (BACKLOG F9): named cumulative numbers a
//! room logic — a kit composite, a game — reports through the same
//! metrics path as the core's counters, without the core knowing what
//! they mean.
//!
//! **Declared once, statically.** A counter is a `const`
//! ([`LogicCounter::sum`] / [`LogicCounter::max`]): a name, a help line
//! and a fold rule. The name is checked when the constant is evaluated
//! (a bad name is a compile error in a `const` item), so nothing is
//! registered at run time and nothing can collide with the core's own
//! keys (every exposure prefixes it: `logic_<name>=` on the log line,
//! `gsb_room_logic_<name>…` in the exposition).
//!
//! **Counted by the logic, read at the sample.** The logic keeps its
//! values in its own plain fields (`self.kills += 1` — no allocation,
//! no lock, no message on the tick path) and hands them to the core in
//! `GameLogic::logic_counters`, which the actor calls once per metrics
//! sample (the report cadence, not the tick) into a fixed-size
//! [`LogicCounters`] that rides the sample by value.
//!
//! **Bounded.** At most [`LOGIC_COUNTERS_MAX`] distinct names per room;
//! a value put beyond the bound is dropped and counted
//! ([`LogicCounters::dropped`]; the actor warns once). The set of names
//! is static per game, so the bound is a design limit hit in the first
//! test run, not a load condition.
//!
//! **One name, one counter.** A name put twice in one set (a composite
//! and its game both reporting it) is folded by its rule — the same
//! fold the load generator applies across a sharded room's shards
//! ([`LogicCounters::merge`]).

/// The most logic counters one room (or shard) reports. The kit's
/// crystallization uses six; the rest is the game's.
pub const LOGIC_COUNTERS_MAX: usize = 16;

/// The longest counter name, in bytes.
pub const LOGIC_NAME_MAX: usize = 32;

/// How two values of one counter combine — two shards of a room, or
/// two reports of the same name in one set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicFold {
    /// A cumulative count over disjoint work: the values add. Exposed
    /// as a Prometheus `counter` (`…_total`).
    Sum,
    /// A high-water mark: the larger value wins. Exposed as a
    /// Prometheus `gauge` (a peak is not a rate's numerator).
    Max,
}

/// One declared counter: its name (stored inline, so a counter decoded
/// off a wire is the same `Copy` value as a declared one), its help
/// line and its fold rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicCounter {
    name: [u8; LOGIC_NAME_MAX],
    len: u8,
    help: &'static str,
    fold: LogicFold,
}

impl LogicCounter {
    /// A cumulative count ([`LogicFold::Sum`]). Panics — at compile time
    /// in a `const` item — unless `name` is 1–[`LOGIC_NAME_MAX`] bytes of
    /// `[a-z0-9_]`, starts with a letter and does not end in `_total`
    /// (the exposition adds that suffix), and `help` is one line without
    /// backslashes.
    pub const fn sum(name: &str, help: &'static str) -> Self {
        Self::declare(name, help, LogicFold::Sum)
    }

    /// A high-water mark ([`LogicFold::Max`]); the same rules as
    /// [`Self::sum`].
    pub const fn max(name: &str, help: &'static str) -> Self {
        Self::declare(name, help, LogicFold::Max)
    }

    const fn declare(name: &str, help: &'static str, fold: LogicFold) -> Self {
        assert!(
            valid_help(help),
            "a logic counter's help is one line without backslashes"
        );
        match Self::parse(name, help, fold) {
            Some(c) => c,
            None => panic!(
                "a logic counter's name is 1-32 bytes of [a-z0-9_], starts with a letter and does not end in _total"
            ),
        }
    }

    /// A counter from a name read off a wire (`None`: not a valid name).
    /// The help is whatever the reader has; the load generator's codec
    /// does not carry it.
    pub const fn parse(name: &str, help: &'static str, fold: LogicFold) -> Option<Self> {
        let b = name.as_bytes();
        if b.is_empty() || b.len() > LOGIC_NAME_MAX || !b[0].is_ascii_lowercase() {
            return None;
        }
        let mut out = [0u8; LOGIC_NAME_MAX];
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_') {
                return None;
            }
            out[i] = c;
            i += 1;
        }
        if ends_with_total(b) {
            return None;
        }
        Some(Self {
            name: out,
            len: b.len() as u8,
            help,
            fold,
        })
    }

    /// The counter's name.
    pub fn name(&self) -> &str {
        // Validated ASCII at construction.
        std::str::from_utf8(&self.name[..usize::from(self.len)]).unwrap_or("")
    }

    /// The counter's help line (empty for one decoded off a wire).
    pub fn help(&self) -> &'static str {
        self.help
    }

    /// The counter's fold rule.
    pub fn fold(&self) -> LogicFold {
        self.fold
    }
}

const fn valid_help(help: &str) -> bool {
    let b = help.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\n' || b[i] == b'\\' {
            return false;
        }
        i += 1;
    }
    true
}

const fn ends_with_total(b: &[u8]) -> bool {
    let suffix = b"_total";
    if b.len() < suffix.len() {
        return false;
    }
    let off = b.len() - suffix.len();
    let mut i = 0;
    while i < suffix.len() {
        if b[off + i] != suffix[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// One counter and its current value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicSlot {
    pub counter: LogicCounter,
    pub value: u64,
}

/// A room's logic counters as one sample carries them: at most
/// [`LOGIC_COUNTERS_MAX`] slots, by value (`Copy`, no allocation).
/// Empty for a logic that declares nothing — and then every exposure
/// is exactly what it was before the seam existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicCounters {
    len: u8,
    dropped: u32,
    slots: [LogicSlot; LOGIC_COUNTERS_MAX],
}

const EMPTY_SLOT: LogicSlot = LogicSlot {
    counter: LogicCounter {
        name: [0; LOGIC_NAME_MAX],
        len: 0,
        help: "",
        fold: LogicFold::Sum,
    },
    value: 0,
};

impl Default for LogicCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl LogicCounters {
    /// No counters.
    pub const fn new() -> Self {
        Self {
            len: 0,
            dropped: 0,
            slots: [EMPTY_SLOT; LOGIC_COUNTERS_MAX],
        }
    }

    /// Report `counter` at `value`. A name already in the set folds by
    /// its rule; a new name beyond [`LOGIC_COUNTERS_MAX`] is dropped
    /// and counted.
    pub fn put(&mut self, counter: &LogicCounter, value: u64) {
        let n = usize::from(self.len);
        if let Some(slot) = self.slots[..n]
            .iter_mut()
            .find(|s| s.counter.name() == counter.name())
        {
            slot.value = match slot.counter.fold {
                LogicFold::Sum => slot.value.saturating_add(value),
                LogicFold::Max => slot.value.max(value),
            };
            return;
        }
        if n == LOGIC_COUNTERS_MAX {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.slots[n] = LogicSlot {
            counter: *counter,
            value,
        };
        self.len += 1;
    }

    /// Fold `other` into this set, name by name, each by its rule (the
    /// drops add up).
    pub fn merge(&mut self, other: &LogicCounters) {
        for s in other.slots() {
            self.put(&s.counter, s.value);
        }
        self.dropped = self.dropped.saturating_add(other.dropped);
    }

    /// The counters, in the order they were first put.
    pub fn slots(&self) -> &[LogicSlot] {
        &self.slots[..usize::from(self.len)]
    }

    /// The value of the counter named `name`, if the set has it.
    pub fn get(&self, name: &str) -> Option<u64> {
        self.slots()
            .iter()
            .find(|s| s.counter.name() == name)
            .map(|s| s.value)
    }

    /// No counters (a logic that declares none).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Values put beyond [`LOGIC_COUNTERS_MAX`] distinct names.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Record `n` drops (a decoder restoring a set off a wire).
    pub fn add_dropped(&mut self, n: u32) {
        self.dropped = self.dropped.saturating_add(n);
    }
}

#[cfg(test)]
mod tests;
