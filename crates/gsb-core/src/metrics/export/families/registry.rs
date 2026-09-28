//! The registry-scope families: control-plane gauges and cumulative
//! counters (present when the report has a registry slice).

use super::{Kind, Scalar};
use crate::metrics::RegistryReport;

/// A registry family (present when the report has a registry slice).
const fn reg(
    name: &'static str,
    kind: Kind,
    help: &'static str,
    get: fn(&RegistryReport) -> u64,
) -> Scalar<RegistryReport> {
    Scalar {
        name,
        kind,
        help,
        get,
    }
}

/// The registry scope: control-plane gauges and cumulative counters.
pub(in crate::metrics::export) const REGISTRY: [Scalar<RegistryReport>; 19] = [
    reg(
        "gsb_registry_rooms",
        Kind::Gauge,
        "Current room count.",
        |r| u64::from(r.rooms),
    ),
    reg(
        "gsb_registry_conns",
        Kind::Gauge,
        "Current registered connection count.",
        |r| u64::from(r.conns),
    ),
    reg(
        "gsb_registry_rooms_created_total",
        Kind::Counter,
        "Rooms created, cumulative.",
        |r| r.rooms_created,
    ),
    reg(
        "gsb_registry_rooms_destroyed_total",
        Kind::Counter,
        "Rooms destroyed, cumulative.",
        |r| r.rooms_destroyed,
    ),
    reg(
        "gsb_registry_rooms_died_total",
        Kind::Counter,
        "Rooms/shards died unexpectedly (panicked), cumulative.",
        |r| r.rooms_died,
    ),
    reg(
        "gsb_registry_joins_total",
        Kind::Counter,
        "Joins completed, cumulative.",
        |r| r.joins,
    ),
    reg(
        "gsb_registry_leaves_total",
        Kind::Counter,
        "Leaves completed, cumulative.",
        |r| r.leaves,
    ),
    reg(
        "gsb_registry_opens_total",
        Kind::Counter,
        "Connections opened, cumulative.",
        |r| r.opens,
    ),
    reg(
        "gsb_registry_closes_total",
        Kind::Counter,
        "Connections closed, cumulative.",
        |r| r.closes,
    ),
    // B57: control-plane losses.
    reg(
        "gsb_registry_join_ops_dropped_total",
        Kind::Counter,
        "Joins the registry could not hand to the connection's op dispatcher (queue full or gone; the client got an ERROR), cumulative.",
        |r| r.join_ops_dropped,
    ),
    reg(
        "gsb_registry_close_ops_dropped_total",
        Kind::Counter,
        "Close ops (a closing connection's detach) the registry could not hand to its op dispatcher (queue full or gone), cumulative.",
        |r| r.close_ops_dropped,
    ),
    reg(
        "gsb_registry_match_results_dropped_full_total",
        Kind::Counter,
        "Match results a stopping room could not hand to the result sink because it was full (the consumer is not reading), cumulative.",
        |r| r.match_results_dropped_full,
    ),
    reg(
        "gsb_registry_match_results_dropped_closed_total",
        Kind::Counter,
        "Match results a stopping room could not hand to the result sink because it was closed (the consumer dropped its receiver), cumulative.",
        |r| r.match_results_dropped_closed,
    ),
    // B67: a panicked room/shard's lost final count.
    reg(
        "gsb_registry_rooms_ended_uncounted_total",
        Kind::Counter,
        "Room/shard tasks that ended without their final count (a panic): their last window and what they held at the end are counted nowhere, cumulative.",
        |r| r.rooms_ended_uncounted,
    ),
    // B72: the team hubs' refused relays, by cause.
    reg(
        "gsb_registry_team_relays_dropped_full_total",
        Kind::Counter,
        "Team imports a sharded room's team hub could not queue on a target shard because its mailbox was full (the shard is not keeping up; the source's next export carries the whole set again), cumulative.",
        |r| r.team_relays_dropped_full,
    ),
    reg(
        "gsb_registry_team_relays_dropped_closed_total",
        Kind::Counter,
        "Team imports a sharded room's team hub could not queue on a target shard because its mailbox was closed (the shard has stopped or died), cumulative.",
        |r| r.team_relays_dropped_closed,
    ),
    // B75: joins a stopped (or dead) room refused.
    reg(
        "gsb_registry_joins_refused_closed_total",
        Kind::Counter,
        "Joins (resume attempts included) answered RoomGone because the room refused the send: its inbox was already closed (the room or the join's shard had stopped or died), so no room counted them, cumulative.",
        |r| r.joins_refused_closed,
    ),
    // F53: what the registry left unread in its mailbox at its stop.
    reg(
        "gsb_registry_joins_unread_total",
        Kind::Counter,
        "Joins (resume attempts included) still in the registry's mailbox behind its stop: never handled, the client got an ERROR, cumulative.",
        |r| r.joins_unread,
    ),
    reg(
        "gsb_registry_team_exports_unread_total",
        Kind::Counter,
        "Team exports of a live sharded room still in the registry's mailbox behind its stop: the shard counted them queued, the team hub never relayed them, cumulative.",
        |r| r.team_exports_unread,
    ),
];
