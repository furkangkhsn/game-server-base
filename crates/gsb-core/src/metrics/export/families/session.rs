//! The per-room families of the session and the room's traffic with
//! others: the session lifecycle, the detach-hold ceiling, remote
//! effects, migrations, the team exchange, RPC, and the room's own
//! metrics-channel drops.

use super::{RoomFamily, counter, gauge};

/// Session lifecycle through the room's metric drops, in exposition
/// order. The effect, migration and team families move on shard rows
/// only (0 on a single room).
pub(super) const SESSION: [RoomFamily; 36] = [
    counter(
        "gsb_room_joins_total",
        "Joins processed, cumulative.",
        |r| r.joins,
    ),
    counter(
        "gsb_room_leaves_total",
        "Leaves processed, cumulative.",
        |r| r.leaves,
    ),
    counter(
        "gsb_room_resumes_total",
        "Resumes accepted onto parked sessions, cumulative.",
        |r| r.resumes,
    ),
    counter(
        "gsb_room_resume_rejected_stale_total",
        "Resume attempts rejected as stale, cumulative.",
        |r| r.resume_rejected_stale,
    ),
    counter(
        "gsb_room_detach_expired_despawn_total",
        "Detach holds expired toward despawn, cumulative.",
        |r| r.detach_expired_despawn,
    ),
    counter(
        "gsb_room_detach_expired_ai_total",
        "Detach holds expired toward AI handover, cumulative.",
        |r| r.detach_expired_ai,
    ),
    counter(
        "gsb_room_detach_forced_total",
        "Detach holds forced to end by max_detach_hold over a standing veto, cumulative.",
        |r| r.detach_forced,
    ),
    counter(
        "gsb_room_effects_applied_total",
        "Remote effects this shard's game applied as their authority, cumulative.",
        |r| r.effects_applied,
    ),
    counter(
        "gsb_room_effects_forwarded_total",
        "Remote effects handed on to a migrated target's new owner, cumulative.",
        |r| r.effects_forwarded,
    ),
    counter(
        "gsb_room_effects_orphaned_total",
        "Remote effects whose target is gone, cumulative.",
        |r| r.effects_orphaned,
    ),
    counter(
        "gsb_room_effects_dropped_total",
        "Remote effects lost to a full retry buffer, a closed link, the hop or the age bound, cumulative.",
        |r| r.effects_dropped,
    ),
    counter(
        "gsb_room_effects_refused_total",
        "Remote-effect emits refused at the source (budget spent, target not lent), cumulative.",
        |r| r.effects_refused,
    ),
    counter(
        "gsb_room_migrations_out_total",
        "Entities handed to a neighbour shard (committed sends), cumulative.",
        |r| r.migrations_out,
    ),
    counter(
        "gsb_room_migrations_in_total",
        "Entities installed from a neighbour shard, cumulative.",
        |r| r.migrations_in,
    ),
    counter(
        "gsb_room_migrations_failed_total",
        "Migration sends a full neighbour inbox refused (retried next tick), cumulative.",
        |r| r.migrations_failed,
    ),
    counter(
        "gsb_room_team_exports_total",
        "Team exports queued on the registry's mailbox (the team hub), cumulative.",
        |r| r.team_exports,
    ),
    counter(
        "gsb_room_team_export_drops_total",
        "Team exports a full or closed registry mailbox refused, cumulative.",
        |r| r.team_export_drops,
    ),
    counter(
        "gsb_room_team_export_records_total",
        "Records in the queued team exports, cumulative.",
        |r| r.team_export_records,
    ),
    counter(
        "gsb_room_team_over_cap_total",
        "Team records (and viewed teams) cut by the core's per-message caps, cumulative.",
        |r| r.team_over_cap,
    ),
    counter(
        "gsb_room_team_over_budget_total",
        "Team records the game's per-team export budget cut before the export reached the core, cumulative.",
        |r| r.team_over_budget,
    ),
    counter(
        "gsb_room_team_imports_total",
        "Team imports (the hub's relays) applied, cumulative.",
        |r| r.team_imports,
    ),
    counter(
        "gsb_room_team_import_records_total",
        "Records in the applied team imports, cumulative.",
        |r| r.team_import_records,
    ),
    counter(
        "gsb_room_team_expired_total",
        "Team import slots dropped by the TTL (a silent source), cumulative.",
        |r| r.team_expired,
    ),
    // RPC.
    counter(
        "gsb_room_requests_local_total",
        "RPC requests answered room-local, cumulative.",
        |r| r.requests_local,
    ),
    counter(
        "gsb_room_requests_external_total",
        "RPC requests delegated to workers, cumulative.",
        |r| r.requests_external,
    ),
    counter(
        "gsb_room_requests_rejected_malformed_total",
        "RPC rejections: malformed envelope/id=0, cumulative.",
        |r| r.requests_rejected_malformed,
    ),
    counter(
        "gsb_room_requests_rejected_dup_total",
        "RPC rejections: duplicate in-flight id, cumulative.",
        |r| r.requests_rejected_dup,
    ),
    counter(
        "gsb_room_requests_rejected_no_handler_total",
        "RPC rejections: no handler for the op, cumulative.",
        |r| r.requests_rejected_no_handler,
    ),
    counter(
        "gsb_room_requests_rejected_logic_total",
        "RPC rejections: the logic's own reject decision, cumulative.",
        |r| r.requests_rejected_logic,
    ),
    counter(
        "gsb_room_requests_rejected_conn_cap_total",
        "RPC rejections: per-connection pending cap, cumulative.",
        |r| r.requests_rejected_conn_cap,
    ),
    counter(
        "gsb_room_requests_rejected_room_cap_total",
        "RPC rejections: room-wide pending cap, cumulative.",
        |r| r.requests_rejected_room_cap,
    ),
    counter(
        "gsb_room_requests_refused_congested_total",
        "RPC requests refused unanswered on a congested connection (the storm bound), cumulative.",
        |r| r.requests_refused_congested,
    ),
    counter(
        "gsb_room_requests_timed_out_total",
        "RPC pending requests swept as timed out, cumulative.",
        |r| r.requests_timed_out,
    ),
    counter(
        "gsb_room_requests_late_total",
        "RPC worker reports dropped as late, cumulative.",
        |r| r.requests_late,
    ),
    gauge(
        "gsb_room_pending_requests",
        "External RPC requests currently in flight.",
        |r| f64::from(r.pending_requests),
    ),
    counter(
        "gsb_room_metrics_dropped_total",
        "Metric samples this room dropped on a full metrics channel, cumulative.",
        |r| r.metrics_dropped,
    ),
];
