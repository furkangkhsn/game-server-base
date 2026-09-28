//! The server-close families: ONE metric with a `reason` label, not one
//! metric per reason — the same label-over-name rule the per-room
//! families follow (see the parent's docs), so a consumer sums or
//! filters the family instead of enumerating names. Two families use
//! it: the net scope's server closes and the registry scope's close
//! verdicts the stop kept from their connections (F56).

use std::fmt::Write as _;

use super::header;
use crate::metrics::ServerCloses;

/// Append `<name>{reason="…"}` for the family `(name, help)`, one sample
/// per reason — zeros included: the label set is closed and known, and a
/// series that only appears once it is non-zero cannot be rated or
/// alerted on from its first increment.
pub(super) fn render(out: &mut String, (name, help): (&str, &str), closes: &ServerCloses) {
    header(out, name, "counter", help);
    for (reason, n) in closes.iter() {
        let _ = writeln!(out, "{name}{{reason=\"{}\"}} {n}", reason.label());
    }
}
