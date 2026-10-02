//! The send half's feed of the path signal: a sample at most every
//! `SAMPLE_EVERY` while bytes move, news into the connection actor's
//! inbox, never awaited. A CHILD of [`super`].

use gsb_core::channel::{Mailbox, TrySend, try_send};
use gsb_core::conn::ConnIn;
use gsb_core::path::PathSignal;
use tokio::time::Instant;

use super::{QuicPath, SAMPLE_EVERY, Sample};

/// One connection's feed (owned by its send half: the writer pump is the
/// one task that knows when bytes move — no task of its own).
pub(in crate::quic) struct PathFeed {
    conn: quinn::Connection,
    inbox: Mailbox<ConnIn>,
    cadence: Cadence,
}

impl PathFeed {
    /// Feed `conn`'s path to the actor behind `inbox`; the first write
    /// samples.
    pub(in crate::quic) fn new(conn: quinn::Connection, inbox: Mailbox<ConnIn>) -> Self {
        Self {
            conn,
            inbox,
            cadence: Cadence::new(Instant::now()),
        }
    }

    /// The writer moved bytes: sample if one is due, and tell the actor
    /// what is news (or still owed).
    pub(in crate::quic) fn wrote(&mut self) {
        let now = Instant::now();
        if self.cadence.due(now) {
            self.cadence.tell(Sample::of(&self.conn, now), &self.inbox);
        }
    }
}

/// The feed's rules without the connection: when a sample is due, and
/// what reaches the inbox.
struct Cadence {
    path: QuicPath,
    signal: PathSignal,
    /// The earliest instant of the next sample.
    next: Instant,
    /// The actor is gone: nothing more to tell.
    done: bool,
}

impl Cadence {
    fn new(now: Instant) -> Self {
        Self {
            path: QuicPath::default(),
            signal: PathSignal::default(),
            next: now,
            done: false,
        }
    }

    /// A sample is due at `now` (and the next one is not before
    /// `now + SAMPLE_EVERY`).
    fn due(&mut self, now: Instant) -> bool {
        if self.done || now < self.next {
            return false;
        }
        self.next = now + SAMPLE_EVERY;
        true
    }

    /// `s`'s path, to the actor when it is news: a full inbox keeps the
    /// newest owed for the next sample, a closed one ends the feed.
    fn tell(&mut self, s: Sample, inbox: &Mailbox<ConnIn>) {
        self.signal.offer(self.path.on_sample(s));
        let Some(owed) = self.signal.owed() else {
            return;
        };
        match try_send(inbox, ConnIn::Path(owed)) {
            TrySend::Sent => self.signal.delivered(),
            TrySend::Full => {}
            TrySend::Closed => self.done = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gsb_core::channel::channel;
    use gsb_core::path::PathPhase;

    use super::*;

    fn sample(at: Instant, events: u64) -> Sample {
        Sample {
            at,
            rtt: Duration::from_millis(50),
            cwnd: 10_000,
            congestion_events: events,
            lost_packets: 0,
            sent_packets: 0,
            tx_bytes: 0,
        }
    }

    #[test]
    fn a_sample_is_due_at_most_every_period() {
        let t0 = Instant::now();
        let mut c = Cadence::new(t0);
        assert!(c.due(t0), "the first write samples");
        assert!(!c.due(t0 + SAMPLE_EVERY / 2));
        assert!(c.due(t0 + SAMPLE_EVERY));
        assert!(!c.due(t0 + SAMPLE_EVERY + SAMPLE_EVERY / 2));
    }

    #[test]
    fn a_full_inbox_keeps_the_newest_owed_and_a_closed_one_ends_the_feed() {
        let t0 = Instant::now();
        let (tx, mut rx) = channel::<ConnIn>(1);
        let mut c = Cadence::new(t0);
        c.tell(sample(t0, 0), &tx);
        assert!(matches!(rx.try_recv(), Ok(ConnIn::Path(p)) if p.phase == PathPhase::Open));
        // Not news: nothing sent.
        c.tell(sample(t0 + SAMPLE_EVERY, 0), &tx);
        assert!(rx.try_recv().is_err());
        // News into a full inbox stays owed; the newer state replaces it.
        tx.try_send(ConnIn::Closed {
            reason: "filler".into(),
        })
        .expect("room");
        c.tell(sample(t0 + SAMPLE_EVERY * 2, 1), &tx);
        c.tell(sample(t0 + SAMPLE_EVERY * 3, 2), &tx);
        assert!(matches!(rx.try_recv(), Ok(ConnIn::Closed { .. })));
        c.tell(sample(t0 + SAMPLE_EVERY * 4, 3), &tx);
        assert!(
            matches!(rx.try_recv(), Ok(ConnIn::Path(p)) if p.phase == PathPhase::Paced),
            "the newest, never the suspect one it replaced"
        );
        assert!(rx.try_recv().is_err());
        drop(rx);
        // News (the window shrank tenfold) finds the actor gone.
        let shrunk = Sample {
            cwnd: 1_000,
            ..sample(t0 + SAMPLE_EVERY * 5, 9)
        };
        c.tell(shrunk, &tx);
        assert!(!c.due(t0 + SAMPLE_EVERY * 9), "the actor is gone");
    }
}
