//! The server-wide families: the registry scope (control-plane gauges
//! and cumulative counters) and the net scope (wire traffic over all
//! connections).

use super::{Kind, Scalar};
use crate::metrics::{NetReport, RegistryReport};

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
pub(in crate::metrics::export) const REGISTRY: [Scalar<RegistryReport>; 9] = [
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
];

/// A net family: every one is a cumulative counter over all connections.
const fn net(
    name: &'static str,
    help: &'static str,
    get: fn(&NetReport) -> u64,
) -> Scalar<NetReport> {
    Scalar {
        name,
        kind: Kind::Counter,
        help,
        get,
    }
}

/// The net scope: wire traffic aggregated over all connections (the
/// per-connection attribution lives in `actions_dropped_top` and the
/// per-actor log lines, not in this aggregate).
pub(in crate::metrics::export) const NET: [Scalar<NetReport>; 11] = [
    net(
        "gsb_net_bytes_in_total",
        "Wire bytes received over all connections (frame bodies), cumulative.",
        |n| n.bytes_in,
    ),
    net(
        "gsb_net_bytes_out_room_total",
        "Snapshot+private payload bytes shipped by rooms, cumulative.",
        |n| n.bytes_out_room,
    ),
    net(
        "gsb_net_bytes_out_control_total",
        "Control-frame bytes sent by connection actors, cumulative.",
        |n| n.bytes_out_control,
    ),
    net(
        "gsb_net_bytes_out_total",
        "Total wire bytes sent, cumulative.",
        |n| n.bytes_out_total,
    ),
    net(
        "gsb_net_frames_in_total",
        "Frames received over all connections, cumulative.",
        |n| n.frames_in,
    ),
    net(
        "gsb_net_frames_out_total",
        "Control frames sent by connection actors, cumulative.",
        |n| n.frames_out,
    ),
    net(
        "gsb_net_actions_dropped_total",
        "Input actions dropped on full per-connection action channels, cumulative.",
        |n| n.actions_dropped,
    ),
    net(
        "gsb_net_violations_total",
        "Protocol-violation events counted by violation budgets, cumulative.",
        |n| n.violations,
    ),
    net(
        "gsb_net_input_rate_limited_total",
        "Valid game-band input refused over a room's input rate limit (dropped, not a violation), cumulative.",
        |n| n.input_rate_limited,
    ),
    // B51: the forwards a connection dropped into a closed action channel
    // (the room had ended the membership; its notice was still on the way).
    net(
        "gsb_net_actions_dropped_closed_total",
        "Game-band actions a connection forwarded after the room had ended its membership (closed action channel; never seen by the room), cumulative.",
        |n| n.actions_dropped_closed,
    ),
    net(
        "gsb_net_requests_dropped_closed_total",
        "RPC requests a connection forwarded after the room had ended its membership (closed action channel; never processed, never answered), cumulative.",
        |n| n.requests_dropped_closed,
    ),
];
