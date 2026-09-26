//! The server-close family: ONE metric with a `reason` label, not one
//! metric per reason — the same label-over-name rule the per-room
//! families follow (see the parent's docs), so a consumer sums or
//! filters the family instead of enumerating names.

use std::fmt::Write as _;

use crate::metrics::ServerCloses;

/// Append `gsb_net_server_closes_total{reason="…"}`, one sample per
/// reason — zeros included: the label set is closed and known, and a
/// series that only appears once it is non-zero cannot be rated or
/// alerted on from its first increment.
pub(super) fn render(out: &mut String, closes: &ServerCloses) {
    out.push_str(
        "# HELP gsb_net_server_closes_total Sessions the server ended on its own \
         initiative, by reason (client-initiated closes are not counted), cumulative.\n",
    );
    out.push_str("# TYPE gsb_net_server_closes_total counter\n");
    for (reason, n) in closes.iter() {
        let _ = writeln!(
            out,
            "gsb_net_server_closes_total{{reason=\"{}\"}} {n}",
            reason.label()
        );
    }
}
