//! What a client counts as an error, by reason (BACKLOG B88, B14): one
//! exact name per cause, every one always on the RESULT and `CLIENT`
//! lines (zeros included); `errors` is their sum, as before. Before B88
//! a frame sent outside a room (`NotInRoom`), an unknown code and an
//! undecodable snapshot were one number.

/// One client's errors, by reason.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClientErrors {
    /// `ERROR` code 6 (`NotInRoom`): a game or RPC frame the server read
    /// while the session was in no room — a client that sent before its
    /// JOIN result (B88), or after the room ended its membership.
    pub(crate) not_in_room: u64,
    /// Any other `ERROR` code that is no guardrail decision of its own
    /// (an unknown or unspecified code included — base.proto's
    /// forward-compatibility rule: never guessed onto a known decision).
    pub(crate) other_code: u64,
    /// A snapshot the client view refused (undecodable, wrong mode).
    pub(crate) bad_snapshot: u64,
    /// A private frame the view refused (undecodable, a private delta),
    /// or an empty one carrying no RPC answers either.
    pub(crate) bad_private: u64,
    /// Churn: a connect that failed (the cycle sleeps out and retries).
    pub(crate) connect_failed: u64,
    /// Churn: a frame with an empty payload.
    pub(crate) empty_frame: u64,
}

/// How many reasons [`ClientErrors`] has.
pub(crate) const ERROR_REASONS: usize = 6;

impl ClientErrors {
    /// Every reason as `(RESULT key, count)`, in a fixed order.
    pub(crate) fn fields(&self) -> [(&'static str, u64); ERROR_REASONS] {
        [
            ("errors_not_in_room", self.not_in_room),
            ("errors_other_code", self.other_code),
            ("errors_bad_snapshot", self.bad_snapshot),
            ("errors_bad_private", self.bad_private),
            ("errors_connect_failed", self.connect_failed),
            ("errors_empty_frame", self.empty_frame),
        ]
    }

    /// The inverse of [`Self::fields`]' values (the `CLIENT` line).
    pub(crate) fn from_values(v: [u64; ERROR_REASONS]) -> Self {
        let [
            not_in_room,
            other_code,
            bad_snapshot,
            bad_private,
            connect_failed,
            empty_frame,
        ] = v;
        Self {
            not_in_room,
            other_code,
            bad_snapshot,
            bad_private,
            connect_failed,
            empty_frame,
        }
    }

    /// Every reason's count, summed: the `errors=` key.
    pub(crate) fn total(&self) -> u64 {
        self.fields().iter().map(|(_, n)| n).sum()
    }

    /// Add another client's counts.
    pub(crate) fn add(&mut self, o: &Self) {
        let mut v = self.fields().map(|(_, n)| n);
        for (a, (_, b)) in v.iter_mut().zip(o.fields()) {
            *a += b;
        }
        *self = Self::from_values(v);
    }

    /// The reasons as ` key=value` pairs, each preceded by a space (the
    /// RESULT and `CLIENT` lines' tail).
    pub(crate) fn keys(&self) -> String {
        self.fields()
            .iter()
            .map(|(k, n)| format!(" {k}={n}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_round_trips_and_sums() {
        let e = ClientErrors::from_values([1, 2, 3, 4, 5, 6]);
        assert_eq!(e.fields().map(|(_, n)| n), [1, 2, 3, 4, 5, 6]);
        assert_eq!(e.total(), 21);
        let mut s = ClientErrors::default();
        s.add(&e);
        s.add(&e);
        assert_eq!(s.total(), 42);
        assert_eq!(
            e.keys(),
            " errors_not_in_room=1 errors_other_code=2 errors_bad_snapshot=3 \
             errors_bad_private=4 errors_connect_failed=5 errors_empty_frame=6"
        );
    }
}
