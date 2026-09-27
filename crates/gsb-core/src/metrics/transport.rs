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
    /// rUDP writer: frames taken off a session's outbound channel after the
    /// session was over (the room had not processed the end yet), never
    /// sent.
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
}

impl TransportCounters {
    /// The event carrying this delta.
    pub fn event(self) -> MetricsEvent {
        MetricsEvent::Transport(self)
    }
}
