//! The call table's counters (SERVICES.md §9, round q1): what each
//! room's or shard's [`Calls`](crate::calls::Calls) table issued, how
//! its calls ended, where their results went, and what became of the
//! client requests the table took. Each has one meaning (B32, "count
//! every loss").
//!
//! The request gates that existed before the table keep their own
//! counters and serve both paths (the table's and `handle_request`'s):
//! a malformed envelope is `requests_rejected_malformed`, an in-flight
//! duplicate `requests_rejected_dup`, an op nobody takes
//! `requests_rejected_no_handler`, a congested connection's refusal
//! `requests_refused_congested`; a request whose session ended while it
//! was owed an answer is `requests_abandoned`, an answer for a session
//! that ended before a batch carried it `requests_undelivered`. The
//! built-in path of `RequestDecision::External` counts on those request
//! counters too (`requests_external`, `requests_timed_out`,
//! `requests_late`), never here.
//!
//! Ledgers: `calls_issued = calls_sent + calls_refused_{full,gone,share}
//! + calls_exported + (drafts not yet flushed)`; every sent or refused
//! call ends once in `results_*` (or is exported, or still in flight);
//! a non-late result is `responses_delivered + responses_owner_gone`
//! plus the answer-only calls (`.answers` without `.owner`/`.with`).

/// Declares [`CallCounts`] and its whole-set operations (the
/// [`StopCounts`](crate::metrics::StopCounts) pattern: a field added to
/// the list cannot be left out of one of them).
macro_rules! call_counts {
    ($( $(#[$doc:meta])* $field:ident, )*) => {
        /// A call table's counters (see the module docs). One type in the
        /// table, the room's sample and its report row.
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct CallCounts {
            $( $(#[$doc])* pub $field: u64, )*
        }

        impl CallCounts {
            /// Add another set (a fold over rows).
            pub fn add(&mut self, o: &Self) {
                $( self.$field = self.$field.saturating_add(o.$field); )*
            }

            /// Every field as `(name, value)`, in declaration order (the
            /// log line's keys, the RESULT keys, the loadgen wire's
            /// order).
            pub fn fields(&self) -> [(&'static str, u64); CALL_COUNT] {
                [ $( (stringify!($field), self.$field), )* ]
            }

            /// The inverse of [`Self::fields`]' values (the loadgen wire).
            pub fn from_values(v: [u64; CALL_COUNT]) -> Self {
                let mut i = 0;
                $( let $field = v[i]; i += 1; )*
                let _ = i;
                Self { $( $field, )* }
            }
        }

        /// How many counters [`CallCounts`] has.
        pub const CALL_COUNT: usize = [$( stringify!($field), )*].len();
    };
}

call_counts! {
    /// Calls written into the table (`calls.call(..)`, a `forward`
    /// rule, an `import`).
    calls_issued,
    /// Calls a service's mailbox took at FLUSH.
    calls_sent,
    /// Calls refused at FLUSH: the service's mailbox was full.
    calls_refused_full,
    /// Calls refused at FLUSH: the service was gone.
    calls_refused_gone,
    /// Calls refused at FLUSH: this table's share of the service was used
    /// up.
    calls_refused_share,
    /// Calls carried out of the table by `export` (migration).
    calls_exported,
    /// Calls taken in by `import` (re-issued under their own key).
    calls_imported,
    /// Calls that ended with the service's answer.
    results_ok,
    /// Calls that ended with the service's typed error.
    results_service_error,
    /// Calls that ended refused (a FLUSH refusal, or a host's shed).
    results_refused,
    /// Calls that ended timed out.
    results_timed_out,
    /// Calls the service dropped unanswered.
    results_abandoned,
    /// Answers for a key this table no longer holds (exported away):
    /// applied nowhere.
    results_late,
    /// Results handed to the game (`take_resolved`'s buckets).
    responses_delivered,
    /// Results handed out and never taken by tick end: dropped.
    responses_unhandled,
    /// Results whose owner no longer exists: never handed out (an
    /// `.answers` client is still answered).
    responses_owner_gone,
    /// Client answers a call's `.answers` (or a `forward` rule) sent.
    answers_sent,
    /// Requests of a registered op whose payload did not decode as its
    /// type: answered by the engine.
    requests_undecodable,
    /// Requests from a session bound to no owner: answered by the
    /// engine, never shown to the game.
    requests_unbound,
    /// Requests a `forward` rule sent straight to a service.
    requests_forwarded,
    /// Requests the game answered itself (`calls.reply`).
    requests_answered,
    /// Requests nobody took by tick end (`take_incoming` was not called
    /// for them): answered by the engine.
    requests_untaken,
    /// Requests taken but neither answered nor attached to a call by
    /// tick end: answered by the engine.
    requests_unanswered,
    /// Replies (or `.answers`) to a request the engine had already
    /// closed: ignored.
    requests_replied_late,
    /// A replay met a recorded ending that does not fit the game: no
    /// call of its type waits under its key (the log is another game's,
    /// or another binary's). Never applied; never in a true replay.
    replay_diverged,
    /// A replay met a recorded outcome it could not decode (a log file
    /// whose call type no codec covers): not applied.
    replay_undecodable,
    /// Input-log records a recording room handed its sink (the header,
    /// then one per step).
    records_written,
    /// Step records (or the header) a recording room's full or gone sink
    /// refused: lost, the log has a hole.
    records_dropped,
}
