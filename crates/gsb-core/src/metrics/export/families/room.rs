//! The per-room families of the tick and the broadcast: tick health,
//! the two step-duration distributions, drops and fan-out, and the
//! group state.

use super::{RoomFamily, RoomValue, counter, gauge};

/// Tick health through group state, in exposition order.
pub(super) const TICK: [RoomFamily; 29] = [
    gauge(
        "gsb_room_hz",
        "Measured room step rate (Δsteps/s over the last sample interval).",
        |r| r.hz,
    ),
    gauge(
        "gsb_room_budget_us",
        "The room's tick budget in microseconds (one period).",
        |r| r.budget_us as f64,
    ),
    gauge(
        "gsb_room_step_min_us",
        "Minimum step body duration, µs.",
        |r| r.step_min_us as f64,
    ),
    gauge(
        "gsb_room_step_mean_us",
        "Mean step body duration, µs.",
        |r| r.step_mean_us,
    ),
    gauge(
        "gsb_room_step_max_us",
        "Maximum step body duration, µs.",
        |r| r.step_max_us as f64,
    ),
    gauge(
        "gsb_room_late_min_us",
        "Minimum tick processing latency (step start − ticker timestamp), µs.",
        |r| r.late_min_us as f64,
    ),
    gauge(
        "gsb_room_late_mean_us",
        "Mean tick processing latency, µs.",
        |r| r.late_mean_us,
    ),
    gauge(
        "gsb_room_late_max_us",
        "Maximum tick processing latency, µs.",
        |r| r.late_max_us as f64,
    ),
    // Step-duration distribution #1: budget-relative log2 histogram
    // (edges are fractions of each room's budget, exported in µs).
    RoomFamily {
        name: "gsb_room_step_hist",
        help: "Step body duration histogram; buckets are fractions of the room's tick budget (le in µs); bins at/above the budget edge are budget overflow.",
        value: RoomValue::StepHist,
    },
    // Step-duration distribution #2: the fine fixed-bin histogram (its
    // design purpose: sub-budget resolution). The help is the
    // exposition's, where it is a p50/p99 summary.
    RoomFamily {
        name: "gsb_room_step_duration_us",
        help: "Step body duration, µs (summary quantiles from the fine fixed-bin histogram; steps at/above its cap are not quantiled here).",
        value: RoomValue::StepFine,
    },
    // Drops and broadcast fan-out.
    counter("gsb_room_steps_total", "Steps run, cumulative.", |r| {
        r.steps
    }),
    counter(
        "gsb_room_lagged_events_total",
        "Broadcast Lagged occurrences, cumulative.",
        |r| r.lagged_events,
    ),
    counter(
        "gsb_room_lagged_ticks_total",
        "Missed tick indices caught up, cumulative.",
        |r| r.lagged_ticks,
    ),
    counter(
        "gsb_room_dropped_total",
        "Outbound batches dropped at the fan-out (slow client), cumulative.",
        |r| r.dropped,
    ),
    gauge(
        "gsb_room_dropped_s",
        "Batch drop rate (Δ/s over the last sample interval).",
        |r| r.dropped_s,
    ),
    counter(
        "gsb_room_keepalive_resends_total",
        "Keep-alive re-sends of unchanged groups, cumulative.",
        |r| r.keepalive_resends,
    ),
    counter(
        "gsb_room_snapshots_total",
        "Group snapshots encoded, cumulative.",
        |r| r.snapshots,
    ),
    gauge(
        "gsb_room_snap_bytes_s",
        "Encoded snapshot byte rate (Δ/s).",
        |r| r.snap_bytes_s,
    ),
    gauge(
        "gsb_room_snap_bytes_max",
        "Largest single group payload seen, bytes.",
        |r| f64::from(r.snap_bytes_max),
    ),
    counter(
        "gsb_room_snap_overflows_total",
        "Snapshots exceeding max_snapshot_bytes, cumulative.",
        |r| r.snap_overflows,
    ),
    counter(
        "gsb_room_snap_records_total",
        "Entity records encoded (overlap-metric numerator), cumulative.",
        |r| r.snap_records,
    ),
    counter(
        "gsb_room_shipped_bytes_total",
        "Bytes shipped to connections (fan-out copies), cumulative.",
        |r| r.shipped_bytes,
    ),
    gauge("gsb_room_shipped_s", "Shipped-byte rate (Δ/s).", |r| {
        r.shipped_s
    }),
    // The same fan-out in FRAMES (B39): a datagram transport is bounded
    // by packets as well as bytes (`shipped_bytes / shipped_frames` is
    // the mean frame size), and the private split separates the
    // per-connection traffic from the broadcast half. Cumulative, like
    // the bytes (the loadgen fold SUMs them over shards).
    counter(
        "gsb_room_shipped_frames_total",
        "Frames shipped to connections (snapshot + private, fan-out copies), cumulative.",
        |r| r.shipped_frames,
    ),
    counter(
        "gsb_room_private_frames_total",
        "Private per-connection frames shipped (RPC answers, acks, one-shot fulls; a subset of the shipped frames), cumulative.",
        |r| r.private_frames,
    ),
    // Group/membership state.
    gauge("gsb_room_groups", "Current snapshot group count.", |r| {
        f64::from(r.groups)
    }),
    gauge("gsb_room_members", "Current member count.", |r| {
        f64::from(r.members)
    }),
    gauge(
        "gsb_room_max_group",
        "Largest snapshot group right now.",
        |r| f64::from(r.max_group),
    ),
    gauge(
        "gsb_room_detached",
        "Detached-but-parked connections right now (their cap slots are held).",
        |r| f64::from(r.detached),
    ),
];
