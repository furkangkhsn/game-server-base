//! The pacing queue: a paced session's game-band messages waiting for
//! the path, released datagram by datagram from a token bucket, the
//! oldest dropped past the queue budget. Pure: every method takes its
//! clock and rate; the writer does the I/O. A CHILD of [`super`].
//!
//! - **A message is all or nothing** (FRAG atomicity): a fragmented
//!   frame is queued as its whole set of datagrams; it is dropped whole
//!   or sent whole. The message whose first datagram is on the wire is
//!   never dropped — its other fragments are what makes it useful — and
//!   one cut by the session's end counts as unsent, whole.
//! - **Latest wins:** the newest message is always kept, whatever its
//!   size; the oldest go first.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::PACE_BURST;

/// One game-band frame: its datagrams (one RAW, or its FRAG set).
#[derive(Debug)]
struct Message {
    datagrams: VecDeque<Vec<u8>>,
    frag: bool,
    started: bool,
}

/// A datagram the queue released.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::udp) struct Released {
    pub(in crate::udp) datagram: Vec<u8>,
    /// A FRAG fragment, and whether it was its message's last.
    pub(in crate::udp) frag: bool,
    pub(in crate::udp) last: bool,
}

/// What the queue did, for the session's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::udp) struct QueueCounts {
    /// Frames that entered the queue: each is later sent, dropped or
    /// unsent.
    pub(in crate::udp) queued: u64,
    pub(in crate::udp) dropped: u64,
    pub(in crate::udp) unsent: u64,
}

/// See the module docs.
#[derive(Debug)]
pub(in crate::udp) struct PaceQueue {
    msgs: VecDeque<Message>,
    /// Bytes still queued (sent datagrams leave it).
    bytes: usize,
    max_datagram: usize,
    /// The token bucket, in bytes (negative: the control band borrowed
    /// ahead), and when it was last refilled.
    tokens: f64,
    refilled: Instant,
    pub(in crate::udp) counts: QueueCounts,
}

impl PaceQueue {
    pub(in crate::udp) fn new(max_datagram: usize, now: Instant) -> Self {
        Self {
            msgs: VecDeque::new(),
            bytes: 0,
            max_datagram,
            tokens: 0.0,
            refilled: now,
            counts: QueueCounts::default(),
        }
    }

    pub(in crate::udp) fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }

    /// Pacing starts (or restarts) at `rate`: a full bucket.
    pub(in crate::udp) fn start(&mut self, now: Instant, rate: f64) {
        (self.tokens, self.refilled) = (self.depth(rate), now);
    }

    /// Queue one message, then drop the oldest past `cap` bytes.
    pub(in crate::udp) fn push(&mut self, datagrams: Vec<Vec<u8>>, frag: bool, cap: usize) {
        self.bytes += datagrams.iter().map(Vec::len).sum::<usize>();
        self.msgs.push_back(Message {
            datagrams: datagrams.into(),
            frag,
            started: false,
        });
        self.counts.queued += 1;
        self.trim(cap);
    }

    /// Drop the oldest messages until at most `cap` bytes are queued —
    /// never the newest, never one already on the wire.
    pub(in crate::udp) fn trim(&mut self, cap: usize) {
        while self.bytes > cap {
            let oldest = usize::from(self.msgs.front().is_some_and(|m| m.started));
            if oldest + 1 >= self.msgs.len() {
                return; // only the newest (and the one on the wire) left
            }
            let m = self.msgs.remove(oldest).expect("index checked");
            self.bytes -= m.datagrams.iter().map(Vec::len).sum::<usize>();
            self.counts.dropped += 1;
        }
    }

    /// Charge `bytes` the session sent outside the queue (the control
    /// band) to the bucket: the game band waits that much longer.
    pub(in crate::udp) fn charge(&mut self, now: Instant, rate: f64, bytes: usize) {
        self.refill(now, rate);
        self.tokens -= bytes as f64;
    }

    /// The next datagram the bucket affords at `rate` (paced).
    pub(in crate::udp) fn pop(&mut self, now: Instant, rate: f64) -> Option<Released> {
        self.refill(now, rate);
        let need = self.msgs.front()?.datagrams.front()?.len() as f64;
        if self.tokens < need {
            return None;
        }
        self.tokens -= need;
        self.take()
    }

    /// The next datagram, unpaced (the session opened again: what is
    /// queued goes at once, in order).
    pub(in crate::udp) fn pop_any(&mut self) -> Option<Released> {
        self.take()
    }

    /// How long until the bucket affords the next datagram at `rate`
    /// (`None`: nothing queued).
    pub(in crate::udp) fn wait(&self, now: Instant, rate: f64) -> Option<Duration> {
        let need = self.msgs.front()?.datagrams.front()?.len() as f64;
        let dt = now.saturating_duration_since(self.refilled).as_secs_f64();
        let have = (self.tokens + rate * dt).min(self.depth(rate));
        Some(Duration::from_secs_f64(((need - have) / rate).max(0.0)))
    }

    /// The session is over: whatever is queued is never sent, counted
    /// (a message with fragments already out included — it is useless
    /// to the client without the rest).
    pub(in crate::udp) fn abandon(&mut self) {
        self.counts.unsent += self.msgs.len() as u64;
        self.msgs.clear();
        self.bytes = 0;
    }

    fn take(&mut self) -> Option<Released> {
        let m = self.msgs.front_mut()?;
        let datagram = m.datagrams.pop_front()?;
        m.started = true;
        let (frag, last) = (m.frag, m.datagrams.is_empty());
        if last {
            self.msgs.pop_front();
        }
        self.bytes -= datagram.len();
        Some(Released {
            datagram,
            frag,
            last,
        })
    }

    fn refill(&mut self, now: Instant, rate: f64) {
        let dt = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.tokens = (self.tokens + rate * dt).min(self.depth(rate));
        self.refilled = now;
    }

    fn depth(&self, rate: f64) -> f64 {
        (rate * PACE_BURST.as_secs_f64()).max(self.max_datagram as f64)
    }
}
