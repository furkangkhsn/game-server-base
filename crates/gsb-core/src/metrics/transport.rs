//! The transport's own losses (BACKLOG B58): what the network layer
//! (`gsb-net`) drops below the connection actors — the rUDP demux and
//! writers, the WebSocket reader, the handshake intake of the doors that
//! handshake off the accept loop. Until B58 these lived only in the
//! transport tasks' stop logs.
//!
//! The path is the one every producer uses: the transport task keeps its
//! counters in its own state and hands the collector DELTAS over the
//! bounded metrics channel ([`MetricsEvent::Transport`]) with a
//! synchronous `try_send`, at most once per flush interval while it is
//! busy and once more as it ends; its flushed baseline advances only when
//! the channel took the sample (the B59 rule), and the last one goes out
//! past a full channel. The collector sums them into one server-wide
//! slice, [`crate::metrics::MetricReport::transport`] (every door of a
//! kind together — the per-door split stays in the stop logs).
//!
//! One type serves as the delta a producer sends and as the cumulative
//! slice of the report: the fields are the same counters.

use crate::metrics::MetricsEvent;

/// Declares [`TransportCounters`]: the fields, and the three whole-set
/// operations a delta counter set needs (so a field added to the list
/// can never be left out of one of them).
macro_rules! transport_counters {
    ($( $(#[$doc:meta])* $field:ident, )*) => {
        /// The transport's loss counters (see the module docs): a delta in
        /// a [`MetricsEvent::Transport`], cumulative in a report.
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct TransportCounters {
            $( $(#[$doc])* pub $field: u64, )*
        }

        impl TransportCounters {
            /// Every field's growth since `base` (the flushed baseline).
            pub fn since(&self, base: &Self) -> Self {
                Self { $( $field: self.$field.saturating_sub(base.$field), )* }
            }

            /// Nothing counted.
            pub fn is_zero(&self) -> bool {
                true $( && self.$field == 0 )*
            }

            /// Add a delta.
            pub fn add(&mut self, d: &Self) {
                $( self.$field = self.$field.saturating_add(d.$field); )*
            }

            /// Every field as `(name, value)`, in declaration order (the
            /// log line's keys and the loadgen wire's order).
            pub fn fields(&self) -> [(&'static str, u64); TRANSPORT_COUNT] {
                [ $( (stringify!($field), self.$field), )* ]
            }

            /// The inverse of [`Self::fields`]' values (the loadgen wire).
            pub fn from_values(v: [u64; TRANSPORT_COUNT]) -> Self {
                let mut i = 0;
                $( let $field = v[i]; i += 1; )*
                let _ = i;
                Self { $( $field, )* }
            }
        }

        /// How many counters [`TransportCounters`] has.
        pub const TRANSPORT_COUNT: usize = [$( stringify!($field), )*].len();
    };
}

transport_counters! {
    /// rUDP: RPC requests the demux dropped on a session's FULL inbox
    /// (its connection actor is not keeping up) — acknowledged to the
    /// client on the reliable band, so never retransmitted; never reached
    /// the connection actor, never answered. A term of the RPC ledger
    /// (`docs/RPC-CONTROL-PLANE.md` §8.3), split from the rest by opcode
    /// like every other loss ([`crate::conn::FrameKind`]).
    udp_requests_dropped_full,
    /// rUDP: game-band frames dropped the same way.
    udp_actions_dropped_full,
    /// rUDP: other base-band (control) frames dropped the same way.
    udp_control_frames_dropped_full,
    /// rUDP: inbound ACKs the demux could not hand to a session's writer
    /// (its outbound channel full); the peer's retransmission re-asks.
    udp_acks_not_forwarded,
    /// rUDP: inbound datagrams empty or over the datagram budget, dropped.
    udp_datagrams_oversized,
    /// rUDP: inbound datagrams too short for their kind, of an unknown
    /// kind, or whose frame did not decode, dropped.
    udp_datagrams_malformed,
    /// rUDP: handshake proofs with a forged or expired cookie, dropped
    /// unanswered.
    udp_bad_cookies,
    /// rUDP: inbound FRAG datagrams (client-to-server fragmentation is
    /// refused), dropped.
    udp_frags_refused,
    /// rUDP: established sessions dropped because the accept loop's
    /// endpoint queue was full (the client's proof re-send retries).
    udp_sessions_dropped_accept_full,
    /// rUDP writer: game-band frames over the fragmentation ceiling,
    /// dropped unsent.
    udp_frames_dropped_oversized,
    /// rUDP writer: reliable control frames still unacknowledged when the
    /// band was declared dead (the session ends with them).
    udp_control_frames_abandoned,
    /// rUDP writer: the session's frames (game and control) taken off its
    /// outbound channel after the session was over (the room had not
    /// processed the end yet), never sent. The demux's piggybacked ACKs
    /// ride the same channel and are not counted (B73: not frames of the
    /// session).
    udp_frames_drained,
    /// WebSocket: close frames (the echo of the client's close, or the
    /// server's protocol-failure close) dropped on the connection's full
    /// control queue; the teardown follows without them.
    ws_close_frames_dropped,
    /// WebSocket: pongs dropped on the full control queue.
    ws_pongs_dropped,
    /// Handshake doors (WebSocket, TLS, QUIC): connections refused
    /// unhandshaken at the bound on handshakes in flight.
    handshakes_refused,
    /// Handshake doors: handshakes cut at their deadline.
    handshakes_timed_out,
    /// Handshake doors: handshakes that failed (a bad upgrade request, a
    /// failed TLS handshake).
    handshakes_failed,
    /// The transport tasks' own samples dropped on the full metrics
    /// channel (their deltas stay for the next one — the B59 rule); also
    /// in the report's top-level `metrics_dropped`.
    metrics_dropped,
    /// Stream doors (TCP, TLS, QUIC, WebSocket; B66): outbound frames
    /// the door took and never wrote because its writer stopped first —
    /// the writer pump on a failed write or a write stall: the rest of
    /// the batch it was writing (the failed or stalled frame included)
    /// and every frame of the batches still queued in the connection's
    /// outbound channel; the WebSocket socket writer on a failed socket
    /// write or the peer's close handshake: the game frames still in its
    /// queue. The room and the connection actor counted them as
    /// shipped/sent (the channel took them).
    stream_frames_unwritten,
    /// Stream doors: outbound batches still queued in the connection's
    /// outbound channel when the writer pump ended on a failed write or a
    /// write stall (their frames are in `stream_frames_unwritten`).
    stream_batches_unwritten,
    /// Stream doors: RPC requests a reader pump had read when the
    /// connection actor's inbox was already closed (the server ended the
    /// session — B60 closes the inbox at the end), never processed, never
    /// answered. A term of the RPC ledger.
    stream_requests_dropped_closed,
    /// Stream doors: game-band frames lost the same way.
    stream_actions_dropped_closed,
    /// Stream doors: other base-band frames lost the same way.
    stream_control_frames_dropped_closed,
    /// WebSocket: control frames (pongs, close frames) the socket writer
    /// never wrote because it stopped first (a failed socket write, or
    /// the peer's close handshake with no close of the server's sent).
    ws_control_frames_unwritten,
    /// WebSocket: game frames (the connection's outbound frames) the
    /// socket writer dropped because a close frame had already gone out
    /// (RFC 6455 §5.5.1: no data after a close) — on a refused stream,
    /// the connection's notice and the fan-out still in flight.
    ws_frames_dropped_after_close,
    /// rUDP writer (B66): game-band datagrams (a RAW frame, or one FRAG
    /// fragment of a large one) the socket refused — lost, the band does
    /// not retransmit.
    udp_game_datagrams_send_failed,
    /// rUDP writer: reliable control datagrams the socket refused, a
    /// first send or a retransmission (the band retransmits them until
    /// its liveness bound).
    udp_control_datagrams_send_failed,
    /// rUDP demux: cumulative ACKs (the handshake's accept included) the
    /// socket refused; the client re-sends what they would have
    /// acknowledged.
    udp_acks_send_failed,
    /// rUDP demux: handshake challenges the socket refused; the client
    /// asks again.
    udp_challenges_send_failed,
    /// rUDP demux: RPC requests decoded for a session whose connection
    /// actor had already closed its inbox (the session is removed at
    /// once) — never processed, never answered. A term of the RPC ledger.
    /// A reliable frame here was not yet acknowledged, and the session is
    /// gone for its re-send.
    udp_requests_dropped_closed,
    /// rUDP demux: game-band frames lost the same way.
    udp_actions_dropped_closed,
    /// rUDP demux: other base-band frames lost the same way.
    udp_control_frames_dropped_closed,
    /// rUDP demux: REL, RAW, ACK and REPORT datagrams from an address
    /// with no session (its session is over, or there never was one),
    /// dropped undecoded.
    udp_datagrams_no_session,
    /// rUDP writer: frames never sent because the session's reliable band
    /// died — the rest of the batch being sent (the undeliverable control
    /// frame included) and every frame still queued in the outbound
    /// channel. The room and the connection actor counted them as
    /// shipped/sent.
    udp_frames_unsent,
    /// Writers' verdicts (a stream pump's write stall, an rUDP band's
    /// death) that found the connection's mailbox full with no slot
    /// reserved at the writer's birth: delivered only after the outbound
    /// channel closed, so the session's close may be booked as
    /// `outbound_dead` instead of the verdict.
    writer_verdicts_deferred,
    /// Handshake doors (WebSocket, TLS, QUIC; B74): handshakes in flight
    /// when their door closed, cut unfinished — the connection closed
    /// unhandshaken.
    handshakes_cut_closed,
    /// Handshake doors: handshakes that FINISHED but whose endpoint was
    /// still queued for the accept loop when the door closed — dropped,
    /// never a session (they are also in the doors' `completed`).
    handshakes_unaccepted_closed,
    /// rUDP demux (B74): proofs verified while the accept side was gone
    /// (the listener dropped) — the session torn down at once, no accept
    /// sent.
    udp_sessions_dropped_accept_gone,
    /// rUDP: sessions established — their accept already sent to the
    /// client — whose endpoint was still queued for the accept loop when
    /// the listener closed or went away: dropped, never a connection.
    udp_sessions_unaccepted_closed,
    /// WebSocket (B80): the server's teardown close — its own close at
    /// the end of a session, whatever its code (1001 "Going Away", or
    /// since B30 a verdict's 1008 / 1013) — never queued because the
    /// socket writer had already stopped on a failed socket write: the
    /// session ended without it. Named `ws_going_away_unsent_closed`
    /// until F66 (the name said 1001 only).
    ws_teardown_closes_unsent_closed,
    /// WebSocket: the server's teardown close abandoned while it waited
    /// for a slot in the socket writer's full queue — the writer pump's
    /// write-stall window ran out with no byte written.
    ws_teardown_closes_unsent_stalled,
    /// WebSocket (B83): close frames (the close echo, or the server's
    /// protocol-failure close) the reader could not queue because the
    /// control queue was CLOSED — the socket writer had already stopped
    /// on a failed socket write (the full queue is
    /// `ws_close_frames_dropped`).
    ws_close_frames_dropped_closed,
    /// WebSocket: pongs refused by the closed control queue, the same
    /// way (the full queue is `ws_pongs_dropped`).
    ws_pongs_dropped_closed,
    /// rUDP writers (B2): control-band frames re-sent because their
    /// retransmit timer expired before the ACK came. Not a loss by
    /// itself — the frame is still delivered — but the signal of one (a
    /// lost frame or ACK) or of a timer shorter than the path's round
    /// trip. By cause: every re-send today is a timer expiry (the band
    /// has no fast retransmit); another cause gets its own counter.
    udp_control_retransmits_timeout,
    /// Handshake doors (WebSocket, TLS, QUIC; D11): connections refused
    /// unhandshaken because their source address (IPv4, IPv6 /64) held
    /// the door's per-source cap (`max_handshakes_per_source`).
    handshakes_refused_per_source,
    /// QUIC (D11): connections from a source not yet proven its own, at
    /// the per-source cap, answered with a stateless Retry (prove the
    /// address, then come back) instead of a slot.
    handshakes_retried_per_source,
    /// Ops HTTP surface (B49): connections closed at once, unread and
    /// unanswered, because the surface already had `http_max_connections`
    /// connection tasks live.
    ops_http_conns_refused,
    /// Ops HTTP surface (B49): responses whose write (the half-close
    /// included) outran `http_write_timeout_secs` — a peer that sent its
    /// request and did not read the answer; the connection was closed.
    ops_http_writes_timed_out,
    /// rUDP door (B85): datagrams the KERNEL dropped on the door's one
    /// socket because its receive queue was full — before the demux could
    /// see them. Per socket (the `drops` column of the socket's
    /// `/proc/net/udp` line, read once a second off the demux); Linux
    /// only, 0 elsewhere. The system-wide `RcvbufErrors` mixes every UDP
    /// socket of the host; this is the door's own.
    udp_datagrams_dropped_kernel,
    /// rUDP writers (game-band feedback): announcements received — REPORTs
    /// with probe id 0, a client asking to be probed (one session sends
    /// one, or up to three while its first probe is lost).
    udp_game_announces_received,
    /// rUDP writers (game-band feedback): PROBEs the socket took (one per
    /// probe interval per announced session).
    udp_game_probes_sent,
    /// rUDP writers (game-band feedback): PROBEs the socket refused (lost;
    /// the next one goes an interval later).
    udp_game_probes_send_failed,
    /// rUDP writers (game-band feedback): probes whose report never came
    /// back while the session lived — the probe or its report was lost,
    /// or a newer probe was answered first (or the client stopped
    /// answering). Once the writers end, `probes_sent = reports_received
    /// + probes_unanswered + probes_open_at_end`.
    udp_game_probes_unanswered,
    /// rUDP writers (game-band feedback): reports answering one of the
    /// session's probes, applied (each an RTT sample and an interval of
    /// loss accounting).
    udp_game_reports_received,
    /// rUDP writers (game-band feedback): reports for a probe already
    /// answered or superseded (reordered or duplicated), ignored.
    udp_game_reports_late,
    /// rUDP writers (game-band feedback): reports refused — an id the
    /// session never sent (or a session that never announced), or a
    /// received count that runs backwards. Nothing of them is applied.
    udp_game_reports_invalid,
    /// rUDP writers (game-band feedback): reports applied with their
    /// received count clamped to what the server had sent by their arrival
    /// (a duplicated datagram, or a false claim).
    udp_game_reports_clamped,
    /// rUDP demux (game-band feedback): reports it could not hand to the
    /// session's writer (its outbound channel full or closed), lost.
    udp_game_reports_not_forwarded,
    /// rUDP writers (game-band feedback): game datagrams (RAW and FRAG)
    /// sent within the intervals the applied reports cover — the
    /// denominator of the reported loss.
    udp_game_datagrams_reported_sent,
    /// rUDP writers (game-band feedback): of those, the datagrams the
    /// clients reported missing (the path's loss, as the clients saw it; a
    /// reordering surplus is carried, not counted).
    udp_game_datagrams_reported_lost,
    /// rUDP writers (game-band feedback): RTT samples taken from answered
    /// probes (each also feeds the session's reliable-band estimator).
    udp_game_rtt_samples,
    /// rUDP writers (game-band feedback): the sum of those samples, in
    /// microseconds (÷ `udp_game_rtt_samples` = the mean probe round trip).
    udp_game_rtt_sum_us,
    /// rUDP writers (game-band feedback): probes still awaiting their
    /// report when the session ended — cut off, not lost (a client that
    /// left or stopped reading answers nothing). Apart from
    /// `udp_game_probes_unanswered` so that one stays a loss signal.
    udp_game_probes_open_at_end,
    /// Ops HTTP surface (B90): requests whose routing — the room
    /// bookkeeper's and the registry's answers (`/rooms`, room open and
    /// close) — outran `http_route_timeout_secs`; answered `504`, the
    /// outcome of an open or close unknown.
    ops_http_routes_timed_out,
    /// Stream doors' reader pumps and the rUDP demux's idle sweep (F72):
    /// idle windows RESTARTED because their deadline fired more than the
    /// stall grace late (`gsb_net::pump::IDLE_STALL_GRACE`) — the process
    /// was not running when the client's silence would have been
    /// observed (a swap storm, a VM pause, a starved runtime), so the
    /// silence was the server's own, not the client's. Once per silence:
    /// a client still silent through the restarted window is closed
    /// `idle_timeout` at its end, however late that fires. Not a loss —
    /// every count is a session the old wall-clock window would have
    /// closed.
    idle_windows_restarted_late,
    /// rUDP writers (congestion response, `udp_congestion = "pace"`):
    /// game-band frames that waited in a paced session's pacing queue —
    /// its path's estimated rate was below what the room sent. Each is
    /// later sent, dropped (`udp_game_frames_dropped_paced`) or unsent
    /// (`udp_game_frames_unsent_paced`).
    udp_game_frames_queued_paced,
    /// rUDP writers (congestion response): game-band frames dropped
    /// unsent from a pacing queue because newer frames filled the queue's
    /// budget (the path's estimated rate × 50 ms) — the oldest first, a
    /// fragmented frame whole; never the newest, never one with fragments
    /// already sent.
    udp_game_frames_dropped_paced,
    /// rUDP writers (congestion response): game-band frames still in a
    /// pacing queue when the session ended for the transport (its writer
    /// stopped, its reliable band died, or the session was over) — never
    /// sent, or sent in part (a fragmented frame cut short).
    udp_game_frames_unsent_paced,
    /// rUDP writers (congestion response): times a session's game band
    /// started being paced (two congestion signals in a row — loss or a
    /// standing queue — on an open session).
    udp_game_paced_episodes,
    /// rUDP writers (congestion response): pacing-rate decreases — each
    /// episode's start, every further signal while paced, and a whole
    /// ring of probes unanswered while paced.
    udp_game_paced_rate_cuts,
    /// rUDP demux (connection migration, B3; `udp_migration = true`):
    /// sessions granted a connection id (CID) — the clients that asked,
    /// on a door that grants them. Not a loss.
    udp_cids_assigned,
    /// rUDP demux (migration): CIDs or path-challenge nonces the OS
    /// entropy source could not supply (or, once in 2^64, a CID that
    /// collided): the session went without a CID, or the candidate path
    /// unvalidated — never with a weaker value.
    udp_entropy_draws_failed,
    /// rUDP demux (migration): CID-tagged datagrams naming no session (a
    /// session already gone, or a forged CID), dropped.
    udp_cid_unknown,
    /// rUDP demux (migration): path validations begun — a tagged datagram
    /// from an address that is not its session's. Each ends migrated,
    /// timed out, superseded or open at the session's end.
    udp_path_validations_started,
    /// rUDP demux (migration): PATH_CHALLENGE datagrams the socket took
    /// (a validation's first and its re-sends).
    udp_path_challenges_sent,
    /// rUDP demux (migration): PATH_CHALLENGE datagrams the socket
    /// refused (retried on the candidate's next datagram, an interval on).
    udp_path_challenges_send_failed,
    /// rUDP demux (migration): challenges withheld because they would
    /// have sent more than 3× the bytes received from the unvalidated
    /// address.
    udp_path_amplification_capped,
    /// rUDP demux (migration): candidate paths refused because the
    /// address is another session's (one address, one session).
    udp_path_address_in_use,
    /// rUDP demux (migration): PATH_RESPONSE datagrams that answered no
    /// pending validation (no validation, another address, another
    /// nonce), dropped.
    udp_path_responses_unmatched,
    /// rUDP demux (migration): matching responses whose path-change
    /// notice the session's writer channel refused (full or closed); the
    /// validation stays pending and the next challenge round retries.
    udp_path_changes_not_forwarded,
    /// rUDP demux (migration): validations that ended without a matching
    /// response within 3 s (a spoofed or vanished candidate); the session
    /// stayed on its old path.
    udp_path_validations_timed_out,
    /// rUDP demux (migration): validations replaced by a newer candidate
    /// address before they completed.
    udp_path_validations_superseded,
    /// rUDP demux (migration): validations still pending (and in time)
    /// when their session ended.
    udp_path_validations_open_at_end,
    /// rUDP demux (migration): sessions moved to a validated new client
    /// address — no handshake, no resume.
    udp_migrations,
    /// rUDP demux (migration): of those, the moves that changed the port
    /// only (a NAT rebinding; the writer keeps the path estimate).
    udp_migrations_port_only,
    /// rUDP demux (per-source cap, BACKLOG B89): verified proofs refused
    /// because their source held `max_handshakes_per_source` pending sessions
    /// (established, not yet taken by the accept loop) — no session, no
    /// accept; the client re-sends its proof.
    udp_proofs_refused_per_source,
}

impl TransportCounters {
    /// The event carrying this delta.
    pub fn event(self) -> MetricsEvent {
        MetricsEvent::Transport(self)
    }
}
