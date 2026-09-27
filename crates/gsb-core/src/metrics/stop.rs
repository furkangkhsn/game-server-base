//! What a stopping room or shard still held that is not a session's
//! own (BACKLOG B68): the operations queued in its control channel (or
//! inbox) that it never processed, and on a shard the cross-shard work
//! in flight — migrations landing, remote effects, team and border
//! updates. B62 counted what the SESSIONS held at the stop (unread
//! input, owed answers, in-flight requests); these are the rest.
//!
//! Counted once, at `finish()`, after the channel is closed (so nothing
//! can join them afterwards: a later send fails at its sender, which
//! counts it as its own), and reported with the final sample
//! ([`crate::metrics::MetricsEvent::RoomFinal`]) — cumulative, like the
//! rest of the row. Zero on every periodic sample.

/// Declares [`StopCounts`] and its whole-set operations (so a field
/// added to the list cannot be left out of one of them).
macro_rules! stop_counts {
    ($( $(#[$doc:meta])* $field:ident, )*) => {
        /// What a stopping room/shard still held, by kind (see the module
        /// docs). One type in the room's counters, its sample and its
        /// report row.
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct StopCounts {
            $( $(#[$doc])* pub $field: u64, )*
        }

        impl StopCounts {
            /// Add another set (the load generator's fold over rows).
            pub fn add(&mut self, o: &Self) {
                $( self.$field = self.$field.saturating_add(o.$field); )*
            }

            /// Every field as `(name, value)`, in declaration order (the
            /// log line's keys, the RESULT keys, the loadgen wire's
            /// order).
            pub fn fields(&self) -> [(&'static str, u64); STOP_COUNT] {
                [ $( (stringify!($field), self.$field), )* ]
            }

            /// The inverse of [`Self::fields`]' values (the loadgen wire).
            pub fn from_values(v: [u64; STOP_COUNT]) -> Self {
                let mut i = 0;
                $( let $field = v[i]; i += 1; )*
                let _ = i;
                Self { $( $field, )* }
            }
        }

        /// How many counters [`StopCounts`] has.
        pub const STOP_COUNT: usize = [$( stringify!($field), )*].len();
    };
}

stop_counts! {
    /// `Join` ops still queued when the room/shard stopped: never
    /// admitted. The connection's join is answered by its dispatcher
    /// (the reply was dropped): `RoomGone`.
    joins_unprocessed,
    /// Resume attempts still queued at the stop, never looked up: on a
    /// single room every one; on a shard (the resume is BROADCAST to
    /// every shard) only the one whose parked row holds the identity —
    /// the shard whose answer mattered. Answered by the dispatcher like
    /// a join. A sharded resume whose identity no shard holds parked
    /// counts on none of them (no shard can tell; BACKLOG B75).
    resumes_unprocessed,
    /// `Leave` ops still queued at the stop that would have despawned a
    /// member here (the stale-leave guard passes; on a shard, the owning
    /// one). No answer is owed; the stop ends the member anyway.
    leaves_unprocessed,
    /// `Detach` ops (a member's transport died) still queued at the stop
    /// that would have asked the game's disconnect policy here: the
    /// policy never ran.
    detaches_unprocessed,
    /// Shard: entities (players and NPCs) migrating INTO this shard —
    /// sent by a neighbour, counted there as `migrations_out` — still in
    /// its inbox or deferred when it stopped: never installed, gone with
    /// the stop. A migrating player's unread input is counted with it
    /// (`requests_dropped_unread` / `actions_dropped_unread`).
    migrations_in_dropped,
    /// Shard: remote effects it still held to send at the stop (the
    /// retry buffer and the tick's outbox).
    effects_unsent,
    /// Shard: remote effects received and not applied at the stop
    /// (waiting for their apply tick, or still in the inbox).
    effects_unapplied,
    /// Shard: team imports (another shard's visible sets, relayed by the
    /// registry's hub) still in the inbox at the stop — view copies; the
    /// source still holds the records.
    team_imports_unapplied,
    /// Shard: border updates (a neighbour's strip exchange or its resync
    /// request) still in the inbox at the stop — view copies too.
    border_updates_unapplied,
}
