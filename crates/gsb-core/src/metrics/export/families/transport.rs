//! The transport scope (B58): the network layer's own losses, every door
//! together (see `crate::metrics::TransportCounters`).

use super::{Kind, Scalar};
use crate::metrics::TransportCounters;

/// A transport family: every one is a cumulative counter.
const fn tr(
    name: &'static str,
    help: &'static str,
    get: fn(&TransportCounters) -> u64,
) -> Scalar<TransportCounters> {
    Scalar {
        name,
        kind: Kind::Counter,
        help,
        get,
    }
}

/// The transport scope, in exposition order (the counters' order).
pub(in crate::metrics::export) const TRANSPORT: [Scalar<TransportCounters>; 35] = [
    tr(
        "gsb_transport_udp_requests_dropped_full_total",
        "RPC requests the rUDP demux dropped on a session's full inbox (acknowledged on the reliable band, never reached the connection, never answered), cumulative.",
        |t| t.udp_requests_dropped_full,
    ),
    tr(
        "gsb_transport_udp_actions_dropped_full_total",
        "Game-band frames the rUDP demux dropped on a session's full inbox, cumulative.",
        |t| t.udp_actions_dropped_full,
    ),
    tr(
        "gsb_transport_udp_control_frames_dropped_full_total",
        "Base-band frames other than RPC requests the rUDP demux dropped on a session's full inbox, cumulative.",
        |t| t.udp_control_frames_dropped_full,
    ),
    tr(
        "gsb_transport_udp_acks_not_forwarded_total",
        "Inbound rUDP ACKs the demux could not hand to a session's writer (its outbound channel full; the peer re-asks), cumulative.",
        |t| t.udp_acks_not_forwarded,
    ),
    tr(
        "gsb_transport_udp_datagrams_oversized_total",
        "Inbound rUDP datagrams empty or over the datagram budget, dropped, cumulative.",
        |t| t.udp_datagrams_oversized,
    ),
    tr(
        "gsb_transport_udp_datagrams_malformed_total",
        "Inbound rUDP datagrams too short for their kind, of an unknown kind, or with an undecodable frame, dropped, cumulative.",
        |t| t.udp_datagrams_malformed,
    ),
    tr(
        "gsb_transport_udp_bad_cookies_total",
        "rUDP handshake proofs with a forged or expired cookie, dropped unanswered, cumulative.",
        |t| t.udp_bad_cookies,
    ),
    tr(
        "gsb_transport_udp_frags_refused_total",
        "Inbound rUDP FRAG datagrams refused (the server never reassembles), cumulative.",
        |t| t.udp_frags_refused,
    ),
    tr(
        "gsb_transport_udp_sessions_dropped_accept_full_total",
        "Established rUDP sessions dropped because the accept loop's endpoint queue was full (the client's proof re-send retries), cumulative.",
        |t| t.udp_sessions_dropped_accept_full,
    ),
    tr(
        "gsb_transport_udp_frames_dropped_oversized_total",
        "Game-band frames an rUDP writer dropped unsent: over the fragmentation ceiling, cumulative.",
        |t| t.udp_frames_dropped_oversized,
    ),
    tr(
        "gsb_transport_udp_control_frames_abandoned_total",
        "Reliable control frames still unacknowledged when an rUDP session's reliable band was declared dead, cumulative.",
        |t| t.udp_control_frames_abandoned,
    ),
    tr(
        "gsb_transport_udp_frames_drained_total",
        "Frames an rUDP writer took off the outbound channel after its session was over, never sent, cumulative.",
        |t| t.udp_frames_drained,
    ),
    tr(
        "gsb_transport_ws_close_frames_dropped_total",
        "WebSocket close frames (the close echo or a protocol-failure close) dropped on a connection's full control queue, cumulative.",
        |t| t.ws_close_frames_dropped,
    ),
    tr(
        "gsb_transport_ws_pongs_dropped_total",
        "WebSocket pongs dropped on a connection's full control queue, cumulative.",
        |t| t.ws_pongs_dropped,
    ),
    tr(
        "gsb_transport_handshakes_refused_total",
        "Connections a handshaking door (WebSocket, TLS, QUIC) refused unhandshaken at its bound on handshakes in flight, cumulative.",
        |t| t.handshakes_refused,
    ),
    tr(
        "gsb_transport_handshakes_timed_out_total",
        "Handshakes a handshaking door cut at their deadline, cumulative.",
        |t| t.handshakes_timed_out,
    ),
    tr(
        "gsb_transport_handshakes_failed_total",
        "Handshakes that failed at a handshaking door (bad upgrade request, failed TLS handshake), cumulative.",
        |t| t.handshakes_failed,
    ),
    tr(
        "gsb_transport_metrics_dropped_total",
        "Samples the transport tasks dropped on the full metrics channel (their counts ride the next one; also in gsb_metrics_dropped_total), cumulative.",
        |t| t.metrics_dropped,
    ),
    tr(
        "gsb_transport_stream_frames_unwritten_total",
        "Outbound frames a stream door (TCP, TLS, QUIC, WebSocket) took and never wrote because its writer stopped first (a failed write, a write stall, a WebSocket peer's close): the rest of the batch being written and everything still queued (counted as shipped/sent before), cumulative.",
        |t| t.stream_frames_unwritten,
    ),
    tr(
        "gsb_transport_stream_batches_unwritten_total",
        "Outbound batches still queued in a connection's outbound channel when its stream writer ended on a failed write or a write stall, cumulative.",
        |t| t.stream_batches_unwritten,
    ),
    tr(
        "gsb_transport_stream_requests_dropped_closed_total",
        "RPC requests a stream reader had read when the connection's inbox was already closed (the server ended the session), never answered, cumulative.",
        |t| t.stream_requests_dropped_closed,
    ),
    tr(
        "gsb_transport_stream_actions_dropped_closed_total",
        "Game-band frames a stream reader had read when the connection's inbox was already closed, cumulative.",
        |t| t.stream_actions_dropped_closed,
    ),
    tr(
        "gsb_transport_stream_control_frames_dropped_closed_total",
        "Base-band frames other than RPC requests a stream reader had read when the connection's inbox was already closed, cumulative.",
        |t| t.stream_control_frames_dropped_closed,
    ),
    tr(
        "gsb_transport_ws_control_frames_unwritten_total",
        "WebSocket control frames (pongs, close frames) the socket writer never wrote because it stopped first (a failed write, the peer's close), cumulative.",
        |t| t.ws_control_frames_unwritten,
    ),
    tr(
        "gsb_transport_ws_frames_dropped_after_close_total",
        "WebSocket game frames dropped because a close frame had already gone out (no data after a close), cumulative.",
        |t| t.ws_frames_dropped_after_close,
    ),
    tr(
        "gsb_transport_udp_game_datagrams_send_failed_total",
        "Game-band rUDP datagrams (a RAW frame or one fragment) the socket refused, lost (never retransmitted), cumulative.",
        |t| t.udp_game_datagrams_send_failed,
    ),
    tr(
        "gsb_transport_udp_control_datagrams_send_failed_total",
        "Reliable rUDP control datagrams the socket refused, first sends and retransmissions (retransmitted until the liveness bound), cumulative.",
        |t| t.udp_control_datagrams_send_failed,
    ),
    tr(
        "gsb_transport_udp_acks_send_failed_total",
        "rUDP ACKs (the handshake accept included) the socket refused; the client re-sends, cumulative.",
        |t| t.udp_acks_send_failed,
    ),
    tr(
        "gsb_transport_udp_challenges_send_failed_total",
        "rUDP handshake challenges the socket refused; the client asks again, cumulative.",
        |t| t.udp_challenges_send_failed,
    ),
    tr(
        "gsb_transport_udp_requests_dropped_closed_total",
        "RPC requests the rUDP demux decoded for a session whose connection had already closed its inbox, never answered, cumulative.",
        |t| t.udp_requests_dropped_closed,
    ),
    tr(
        "gsb_transport_udp_actions_dropped_closed_total",
        "Game-band frames the rUDP demux decoded for a session whose connection had already closed its inbox, cumulative.",
        |t| t.udp_actions_dropped_closed,
    ),
    tr(
        "gsb_transport_udp_control_frames_dropped_closed_total",
        "Base-band frames other than RPC requests the rUDP demux decoded for a session whose connection had already closed its inbox, cumulative.",
        |t| t.udp_control_frames_dropped_closed,
    ),
    tr(
        "gsb_transport_udp_datagrams_no_session_total",
        "REL, RAW and ACK datagrams from an address with no rUDP session (over, or never established), dropped, cumulative.",
        |t| t.udp_datagrams_no_session,
    ),
    tr(
        "gsb_transport_udp_frames_unsent_total",
        "Frames an rUDP writer never sent because the reliable band died: the rest of its batch and everything still queued (counted as shipped/sent before), cumulative.",
        |t| t.udp_frames_unsent,
    ),
    tr(
        "gsb_transport_writer_verdicts_deferred_total",
        "Writer verdicts (write stall, dead rUDP band) that found the connection's mailbox full with no reserved slot: delivered after the outbound channel closed, the close possibly booked as outbound_dead, cumulative.",
        |t| t.writer_verdicts_deferred,
    ),
];

#[cfg(test)]
mod tests;
