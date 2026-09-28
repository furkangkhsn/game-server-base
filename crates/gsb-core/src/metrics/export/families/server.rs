//! The net-scope families (wire traffic over all connections). The
//! registry scope is the sibling `registry` module.

use super::{Kind, Scalar};
use crate::metrics::NetReport;

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
