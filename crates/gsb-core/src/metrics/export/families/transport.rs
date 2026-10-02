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
pub(in crate::metrics::export) const TRANSPORT: [Scalar<TransportCounters>; 86] = [
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
        "Session frames (game and control; not the demux's piggybacked ACKs) an rUDP writer took off the outbound channel after its session was over, never sent, cumulative.",
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
        "REL, RAW, ACK and REPORT datagrams from an address with no rUDP session (over, or never established), dropped, cumulative.",
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
    // B74: what a closing door (and rUDP's accept side) drops.
    tr(
        "gsb_transport_handshakes_cut_closed_total",
        "Handshakes in flight at a handshaking door (WebSocket, TLS, QUIC) when the door closed, cut unfinished, cumulative.",
        |t| t.handshakes_cut_closed,
    ),
    tr(
        "gsb_transport_handshakes_unaccepted_closed_total",
        "Finished handshakes whose endpoint was still queued for the accept loop when their door closed: dropped, never a session, cumulative.",
        |t| t.handshakes_unaccepted_closed,
    ),
    tr(
        "gsb_transport_udp_sessions_dropped_accept_gone_total",
        "rUDP handshake proofs verified while the accept side was gone (the listener dropped): the session torn down, no accept sent, cumulative.",
        |t| t.udp_sessions_dropped_accept_gone,
    ),
    tr(
        "gsb_transport_udp_sessions_unaccepted_closed_total",
        "Established rUDP sessions (their accept sent) whose endpoint was still queued for the accept loop when the listener closed or went away: dropped, never a connection, cumulative.",
        |t| t.udp_sessions_unaccepted_closed,
    ),
    // B80: the WebSocket teardown close that could not be delivered,
    // whatever its code (F66: `ws_going_away_unsent_*` until then).
    tr(
        "gsb_transport_ws_teardown_closes_unsent_closed_total",
        "WebSocket teardown closes (the server's own close at the end of a session, whatever its code: 1001, 1008 or 1013) never queued because the socket writer had already stopped on a failed socket write, cumulative.",
        |t| t.ws_teardown_closes_unsent_closed,
    ),
    tr(
        "gsb_transport_ws_teardown_closes_unsent_stalled_total",
        "WebSocket teardown closes (the server's own close at the end of a session, whatever its code: 1001, 1008 or 1013) abandoned while waiting for a slot in the socket writer's full queue: the write-stall window ran out with no byte written, cumulative.",
        |t| t.ws_teardown_closes_unsent_stalled,
    ),
    // B83: the WebSocket reader's control replies behind a stopped writer.
    tr(
        "gsb_transport_ws_close_frames_dropped_closed_total",
        "WebSocket close frames (the close echo or a protocol-failure close) the reader could not queue because the control queue was closed: the socket writer had already stopped on a failed socket write, cumulative.",
        |t| t.ws_close_frames_dropped_closed,
    ),
    tr(
        "gsb_transport_ws_pongs_dropped_closed_total",
        "WebSocket pongs the reader could not queue because the control queue was closed: the socket writer had already stopped on a failed socket write, cumulative.",
        |t| t.ws_pongs_dropped_closed,
    ),
    // B2: the reliable band's re-sends, by cause.
    tr(
        "gsb_transport_udp_control_retransmits_timeout_total",
        "Control-band (reliable) frames the rUDP writers re-sent because their retransmit timer expired before the ACK came (a lost frame, a lost ACK, or a timer shorter than the path's round trip; the band has no fast retransmit), cumulative.",
        |t| t.udp_control_retransmits_timeout,
    ),
    // D11: the handshake doors' per-source cap.
    tr(
        "gsb_transport_handshakes_refused_per_source_total",
        "Connections a handshaking door (WebSocket, TLS, QUIC) refused unhandshaken because their source address (IPv4, IPv6 /64) held its per-source cap, cumulative.",
        |t| t.handshakes_refused_per_source,
    ),
    tr(
        "gsb_transport_handshakes_retried_per_source_total",
        "QUIC connections from a source not yet proven its own, at the per-source cap, answered with a stateless Retry instead of a slot, cumulative.",
        |t| t.handshakes_retried_per_source,
    ),
    // B49: the ops HTTP surface's two limits.
    tr(
        "gsb_transport_ops_http_conns_refused_total",
        "Connections the ops HTTP surface closed at once, unread and unanswered, at its cap on live connections (http_max_connections), cumulative.",
        |t| t.ops_http_conns_refused,
    ),
    tr(
        "gsb_transport_ops_http_writes_timed_out_total",
        "Ops HTTP responses whose write outran http_write_timeout_secs (a peer that did not read its answer); the connection was closed, cumulative.",
        |t| t.ops_http_writes_timed_out,
    ),
    // B85: what the kernel dropped before the demux saw it.
    tr(
        "gsb_transport_udp_datagrams_dropped_kernel_total",
        "Datagrams the kernel dropped on the rUDP door's socket because its receive queue was full, before the demux could read them (per socket, from the socket's /proc/net/udp drops column; Linux only, 0 elsewhere), cumulative.",
        |t| t.udp_datagrams_dropped_kernel,
    ),
    // Round 2 of rUDP hardening: the game band's probes and reports.
    tr(
        "gsb_transport_udp_game_announces_received_total",
        "Game-band report announcements (REPORT with probe id 0: a client asks to be probed) the rUDP writers received, cumulative.",
        |t| t.udp_game_announces_received,
    ),
    tr(
        "gsb_transport_udp_game_probes_sent_total",
        "Game-band PROBE datagrams the rUDP writers sent to announced sessions, cumulative.",
        |t| t.udp_game_probes_sent,
    ),
    tr(
        "gsb_transport_udp_game_probes_send_failed_total",
        "Game-band PROBE datagrams the socket refused (lost; the next one goes an interval later), cumulative.",
        |t| t.udp_game_probes_send_failed,
    ),
    tr(
        "gsb_transport_udp_game_probes_unanswered_total",
        "Game-band probes whose report never came back while the session lived (the probe or its report lost, or superseded by a newer answered probe), cumulative.",
        |t| t.udp_game_probes_unanswered,
    ),
    tr(
        "gsb_transport_udp_game_reports_received_total",
        "Game-band reports answering one of the session's probes, applied (an RTT sample and an interval of loss accounting each), cumulative.",
        |t| t.udp_game_reports_received,
    ),
    tr(
        "gsb_transport_udp_game_reports_late_total",
        "Game-band reports for a probe already answered or superseded (reordered or duplicated), ignored, cumulative.",
        |t| t.udp_game_reports_late,
    ),
    tr(
        "gsb_transport_udp_game_reports_invalid_total",
        "Game-band reports refused: a probe id the session never sent, or a received count that runs backwards; nothing applied, cumulative.",
        |t| t.udp_game_reports_invalid,
    ),
    tr(
        "gsb_transport_udp_game_reports_clamped_total",
        "Game-band reports applied with their received count clamped to what the server had sent by their arrival (a duplicated datagram or a false claim), cumulative.",
        |t| t.udp_game_reports_clamped,
    ),
    tr(
        "gsb_transport_udp_game_reports_not_forwarded_total",
        "Game-band reports the rUDP demux could not hand to the session's writer (its outbound channel full or closed), lost, cumulative.",
        |t| t.udp_game_reports_not_forwarded,
    ),
    tr(
        "gsb_transport_udp_game_datagrams_reported_sent_total",
        "Game-band datagrams (RAW and FRAG) sent within the intervals answered reports cover: the denominator of the reported loss, cumulative.",
        |t| t.udp_game_datagrams_reported_sent,
    ),
    tr(
        "gsb_transport_udp_game_datagrams_reported_lost_total",
        "Of those, the game-band datagrams the clients reported missing (path loss as the clients saw it), cumulative.",
        |t| t.udp_game_datagrams_reported_lost,
    ),
    tr(
        "gsb_transport_udp_game_rtt_samples_total",
        "RTT samples the rUDP writers took from answered game-band probes, cumulative.",
        |t| t.udp_game_rtt_samples,
    ),
    tr(
        "gsb_transport_udp_game_rtt_sum_us_total",
        "Sum of the game-band probe RTT samples in microseconds (divide by gsb_transport_udp_game_rtt_samples_total for the mean), cumulative.",
        |t| t.udp_game_rtt_sum_us,
    ),
    tr(
        "gsb_transport_udp_game_probes_open_at_end_total",
        "Game-band probes still awaiting their report when the session ended: cut off, not lost (a client that left or stopped reading answers nothing), cumulative.",
        |t| t.udp_game_probes_open_at_end,
    ),
    // B90: the ops HTTP surface's routing deadline.
    tr(
        "gsb_transport_ops_http_routes_timed_out_total",
        "Ops HTTP requests whose routing (the room bookkeeper's and the registry's answers: /rooms, room open and close) outran http_route_timeout_secs; answered 504, an open or close may still take effect, cumulative.",
        |t| t.ops_http_routes_timed_out,
    ),
    tr(
        "gsb_transport_idle_windows_restarted_late_total",
        "Idle windows (stream reader pumps, the rUDP idle sweep) restarted because their deadline fired more than the stall grace late: the process, not the client, was silent; restarted once per silence, cumulative.",
        |t| t.idle_windows_restarted_late,
    ),
    tr(
        "gsb_transport_udp_game_frames_queued_paced_total",
        "Game-band frames that waited in a paced rUDP session's pacing queue (its path's estimated rate was below the room's sending); each is later sent, dropped or unsent, cumulative.",
        |t| t.udp_game_frames_queued_paced,
    ),
    tr(
        "gsb_transport_udp_game_frames_dropped_paced_total",
        "Game-band frames dropped unsent from a pacing queue because newer frames filled its budget (estimated path rate x 50 ms): oldest first, a fragmented frame whole, cumulative.",
        |t| t.udp_game_frames_dropped_paced,
    ),
    tr(
        "gsb_transport_udp_game_frames_unsent_paced_total",
        "Game-band frames still in a pacing queue when the rUDP session ended for the transport (writer stopped, reliable band died, or session over), never sent or sent in part, cumulative.",
        |t| t.udp_game_frames_unsent_paced,
    ),
    tr(
        "gsb_transport_udp_game_paced_episodes_total",
        "Times an rUDP session's game band started being paced (two congestion signals in a row, loss or a standing queue, on an open session), cumulative.",
        |t| t.udp_game_paced_episodes,
    ),
    tr(
        "gsb_transport_udp_game_paced_rate_cuts_total",
        "Pacing-rate decreases of rUDP sessions: each episode's start, each further congestion signal while paced, and each ring of probes unanswered while paced, cumulative.",
        |t| t.udp_game_paced_rate_cuts,
    ),
    tr(
        "gsb_transport_udp_cids_assigned_total",
        "rUDP sessions granted a connection id (the clients that asked, on a door with udp_migration on), cumulative.",
        |t| t.udp_cids_assigned,
    ),
    tr(
        "gsb_transport_udp_entropy_draws_failed_total",
        "rUDP connection ids or path-challenge nonces the OS entropy source could not supply (the session went without a CID, or the candidate path unvalidated), cumulative.",
        |t| t.udp_entropy_draws_failed,
    ),
    tr(
        "gsb_transport_udp_cid_unknown_total",
        "CID-tagged rUDP datagrams naming no session (gone, or forged), dropped, cumulative.",
        |t| t.udp_cid_unknown,
    ),
    tr(
        "gsb_transport_udp_path_validations_started_total",
        "rUDP path validations begun (a tagged datagram from an address not its session's); each ends migrated, timed out, superseded or open at the session's end, cumulative.",
        |t| t.udp_path_validations_started,
    ),
    tr(
        "gsb_transport_udp_path_challenges_sent_total",
        "rUDP PATH_CHALLENGE datagrams the socket took (first sends and re-sends), cumulative.",
        |t| t.udp_path_challenges_sent,
    ),
    tr(
        "gsb_transport_udp_path_challenges_send_failed_total",
        "rUDP PATH_CHALLENGE datagrams the socket refused (retried on the candidate's next datagram), cumulative.",
        |t| t.udp_path_challenges_send_failed,
    ),
    tr(
        "gsb_transport_udp_path_amplification_capped_total",
        "rUDP path challenges withheld because they would exceed 3x the bytes received from the unvalidated address, cumulative.",
        |t| t.udp_path_amplification_capped,
    ),
    tr(
        "gsb_transport_udp_path_address_in_use_total",
        "rUDP candidate paths refused because the address is another session's, cumulative.",
        |t| t.udp_path_address_in_use,
    ),
    tr(
        "gsb_transport_udp_path_responses_unmatched_total",
        "rUDP PATH_RESPONSE datagrams that answered no pending validation (none pending, another address or nonce), dropped, cumulative.",
        |t| t.udp_path_responses_unmatched,
    ),
    tr(
        "gsb_transport_udp_path_changes_not_forwarded_total",
        "Matching rUDP path responses whose path-change notice the session's writer channel refused (the validation stays pending), cumulative.",
        |t| t.udp_path_changes_not_forwarded,
    ),
    tr(
        "gsb_transport_udp_path_validations_timed_out_total",
        "rUDP path validations that ended without a matching response within 3 s; the session stayed on its old path, cumulative.",
        |t| t.udp_path_validations_timed_out,
    ),
    tr(
        "gsb_transport_udp_path_validations_superseded_total",
        "rUDP path validations replaced by a newer candidate address before they completed, cumulative.",
        |t| t.udp_path_validations_superseded,
    ),
    tr(
        "gsb_transport_udp_path_validations_open_at_end_total",
        "rUDP path validations still pending, in time, when their session ended, cumulative.",
        |t| t.udp_path_validations_open_at_end,
    ),
    tr(
        "gsb_transport_udp_migrations_total",
        "rUDP sessions moved to a validated new client address (no handshake, no resume), cumulative.",
        |t| t.udp_migrations,
    ),
    tr(
        "gsb_transport_udp_migrations_port_only_total",
        "rUDP migrations that changed the client's port only (a NAT rebinding; the path estimate is kept), cumulative.",
        |t| t.udp_migrations_port_only,
    ),
    tr(
        "gsb_transport_udp_proofs_refused_per_source_total",
        "Verified rUDP proofs refused because their source held its max_handshakes_per_source pending sessions (no session, no accept; the client re-sends), cumulative.",
        |t| t.udp_proofs_refused_per_source,
    ),
];

#[cfg(test)]
mod tests;
