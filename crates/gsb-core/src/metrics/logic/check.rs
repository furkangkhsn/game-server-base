//! The declaration checks, as `const fn`s: a bad name or help line in
//! a `const` counter is a compile error.

/// A help line the exposition can carry as it is: one line, no
/// backslash (Prometheus would need them escaped).
pub(super) const fn valid_help(help: &str) -> bool {
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

/// The one name a logic may not declare: the core's own overflow key
/// (`logic_counters_dropped=` on the line, `gsb_room_logic_counters_dropped`
/// in the exposition — F17) lives in the logic's namespace, so the name
/// is taken.
pub(super) const RESERVED: &[u8] = b"counters_dropped";

/// `b` is [`RESERVED`].
pub(super) const fn is_reserved(b: &[u8]) -> bool {
    if b.len() != RESERVED.len() {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        if b[i] != RESERVED[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// The name ends in `_total` (the exposition's own counter suffix).
pub(super) const fn ends_with_total(b: &[u8]) -> bool {
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
