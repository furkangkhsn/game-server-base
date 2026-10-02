//! How the clients' rUDP sessions ended, by reason (BACKLOG B128): one
//! count per session the CLIENT declared over — its reliable band died,
//! a stateless reset came, a record-layer limit was crossed
//! (`gsb_net::udp::UdpEnd`). A session ends once, so it is counted once,
//! when its wire is let go (the end of a run, a churn cycle's drop);
//! every key is always on the RESULT and `CLIENT` lines, zeros included
//! (all zero on TCP and WebSocket, whose end is the server's EOF — read
//! as `Recv::Closed` like these, and counted server-side under
//! `server_closes`).
//!
//! Before B128 such a session read as silence until the run's deadline;
//! now its reads report `Recv::Closed` and the client stops at once, as
//! on a stream — these counters say why.

use gsb_client::Conn;
use gsb_net::udp::UdpEnd;

/// One client's ended rUDP sessions, by reason.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UdpEnds {
    pub(crate) rel_dead: u64,
    pub(crate) reset: u64,
    pub(crate) seal_limit: u64,
}

/// How many reasons [`UdpEnds`] has.
pub(crate) const UDP_END_REASONS: usize = 3;

impl UdpEnds {
    /// Count `wire`'s session, if it is an rUDP one that ended (call once
    /// per session, as its wire is let go).
    pub(crate) fn note(&mut self, wire: &Conn) {
        if let Some(why) = wire.udp_client().and_then(|c| c.ended()) {
            *match why {
                UdpEnd::RelDead => &mut self.rel_dead,
                UdpEnd::Reset => &mut self.reset,
                UdpEnd::SealLimit => &mut self.seal_limit,
            } += 1;
        }
    }

    /// Every reason as `(RESULT key, count)`, in a fixed order.
    pub(crate) fn fields(&self) -> [(&'static str, u64); UDP_END_REASONS] {
        [
            ("udp_ends_rel_dead", self.rel_dead),
            ("udp_ends_reset", self.reset),
            ("udp_ends_seal_limit", self.seal_limit),
        ]
    }

    /// The inverse of [`Self::fields`]' values (the `CLIENT` line).
    pub(crate) fn from_values([rel_dead, reset, seal_limit]: [u64; UDP_END_REASONS]) -> Self {
        Self {
            rel_dead,
            reset,
            seal_limit,
        }
    }

    /// Every reason's count, summed.
    pub(crate) fn total(&self) -> u64 {
        self.fields().iter().map(|(_, n)| n).sum()
    }

    /// Add another client's counts.
    pub(crate) fn add(&mut self, o: &Self) {
        self.rel_dead += o.rel_dead;
        self.reset += o.reset;
        self.seal_limit += o.seal_limit;
    }

    /// The reasons as ` key=value` pairs, each preceded by a space.
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
        let e = UdpEnds::from_values([1, 2, 3]);
        assert_eq!(e.fields().map(|(_, n)| n), [1, 2, 3]);
        assert_eq!(e.total(), 6);
        let mut s = UdpEnds::default();
        s.add(&e);
        s.add(&e);
        assert_eq!(s, UdpEnds::from_values([2, 4, 6]));
        assert_eq!(
            e.keys(),
            " udp_ends_rel_dead=1 udp_ends_reset=2 udp_ends_seal_limit=3"
        );
    }

    /// The labels the keys carry are the transport's own names.
    #[test]
    fn the_keys_are_the_reasons_labels() {
        for (why, (k, _)) in [UdpEnd::RelDead, UdpEnd::Reset, UdpEnd::SealLimit]
            .into_iter()
            .zip(UdpEnds::default().fields())
        {
            assert_eq!(k, format!("udp_ends_{}", why.label()));
        }
    }
}
