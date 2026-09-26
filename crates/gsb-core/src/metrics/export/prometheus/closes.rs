//! The server-close family: ONE metric with a `reason` label, not one
//! metric per reason — the same label-over-name rule the per-room
//! families follow (see the parent's docs), so a consumer sums or
//! filters the family instead of enumerating names.

use std::fmt::Write as _;

use super::header;
use crate::metrics::ServerCloses;
use crate::metrics::export::families::SERVER_CLOSES;

/// Append `gsb_net_server_closes_total{reason="…"}`, one sample per
/// reason — zeros included: the label set is closed and known, and a
/// series that only appears once it is non-zero cannot be rated or
/// alerted on from its first increment.
pub(super) fn render(out: &mut String, closes: &ServerCloses) {
    let (name, help) = SERVER_CLOSES;
    header(out, name, "counter", help);
    for (reason, n) in closes.iter() {
        let _ = writeln!(out, "{name}{{reason=\"{}\"}} {n}", reason.label());
    }
}
