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
pub(in crate::metrics::export) const REGISTRY: [Scalar<RegistryReport>; 17] = [
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
pub(in crate::metrics::export) const NET: [Scalar<NetReport>; 20] = [
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
        "Game-band input actions dropped on full per-connection action channels (RPC requests are counted apart), cumulative.",
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
    // B55: the RPC ledger's two connection-side edges, alone.
    net(
        "gsb_net_requests_dropped_full_total",
        "RPC requests a connection dropped on its full action channel (never processed, never answered), cumulative.",
        |n| n.requests_dropped_full,
    ),
    net(
        "gsb_net_requests_no_room_total",
        "RPC requests received by a connection in no room (not forwarded; answered ERROR 6 within the violation answer limit; also counted as violations), cumulative.",
        |n| n.requests_no_room,
    ),
    // B56: the heartbeat throttle's surplus (SECURITY §3.2), by phase.
    net(
        "gsb_net_heartbeats_throttled_preauth_total",
        "Heartbeats over the 1/s ACK rate before authentication (counted, not answered, not a violation; the pre-auth probing signal), cumulative.",
        |n| n.heartbeats_throttled_preauth,
    ),
    net(
        "gsb_net_heartbeats_throttled_authed_total",
        "Heartbeats over the 1/s ACK rate after authentication (counted, not answered, not a violation; a client's heartbeat timer set too fast), cumulative.",
        |n| n.heartbeats_throttled_authed,
    ),
    // B57: the connection actors' own outbound losses.
    net(
        "gsb_net_frames_out_closed_total",
        "Control frames a connection could not queue because its writer was already gone (closed outbound channel; not sent, not in frames_out), cumulative.",
        |n| n.frames_out_closed,
    ),
    net(
        "gsb_net_close_notices_dropped_total",
        "Best-effort close notices (ERROR 9/14 of the ends that never wait) dropped on a full outbound channel (the client was not reading), cumulative.",
        |n| n.close_notices_dropped,
    ),
    // B60: what a server-decided end left unprocessed, by kind.
    net(
        "gsb_net_requests_unprocessed_total",
        "RPC requests a connection received but never processed because the server ended its session first (left in its inbox, or the frame that crossed the pre-auth budget; never answered), cumulative.",
        |n| n.requests_unprocessed,
    ),
    net(
        "gsb_net_actions_unprocessed_total",
        "Game-band frames a connection received but never processed because the server ended its session first (left in its inbox, or the frame that crossed the pre-auth budget), cumulative.",
        |n| n.actions_unprocessed,
    ),
    net(
        "gsb_net_control_frames_unprocessed_total",
        "Base-band frames other than RPC requests (auth, join, leave, heartbeat, undefined) a connection received but never processed because the server ended its session first, cumulative.",
        |n| n.control_frames_unprocessed,
    ),
];
