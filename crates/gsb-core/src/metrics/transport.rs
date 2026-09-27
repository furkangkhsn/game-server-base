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
    /// rUDP demux: REL, RAW and ACK datagrams from an address with no
    /// session (its session is over, or there never was one), dropped
    /// undecoded.
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
}

impl TransportCounters {
    /// The event carrying this delta.
    pub fn event(self) -> MetricsEvent {
        MetricsEvent::Transport(self)
    }
}
