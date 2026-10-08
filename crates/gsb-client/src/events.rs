//! Typed events (`docs/EVENTS.md` §15, round e6): what the server pushed
//! to this client, nobody having asked — a chat line, a friend online, a
//! balance — as `PUSH` frames ([`op::base::PUSH`], [`base::Push`]),
//! taken here by type.
//!
//! [`Events`] is the sans-IO core (the FFI core's event half, B28): the
//! client loop hands it every frame ([`Events::ingest`]; it consumes the
//! `PUSH` frames and leaves the rest), and it
//!
//! - checks each publisher's numbers and reports what went missing
//!   ([`Events::take_gaps`]): within one `stream` a publisher numbers its
//!   CLIENT legs to this identity 1, 2, 3 …; a number past the next one
//!   is a [`Gap`] — the server lost those legs and counted them (a parked
//!   session, a full connection inbox, a session that did not read
//!   pushes) — and the game re-syncs (re-reads the friend list, the
//!   balance). A new `stream` is the server's new row for this identity
//!   (a session after being offline): every publisher's count starts
//!   again (`restarted`), never a gap. The first number a table sees from
//!   a publisher in a stream is checked against 1: a later one means this
//!   client never saw the earlier ones (they were lost, or went to
//!   another session of the same identity) — a gap too;
//! - calls the listeners registered for the event's type
//!   ([`Events::on`]) right away, or queues the event for
//!   [`Events::take`] — a bounded queue ([`Events::with_capacity`],
//!   default [`DEFAULT_CAPACITY`]; past it the OLDEST queued event is
//!   dropped, counted `dropped_unread`);
//! - counts every outcome once ([`EventCounters`]).
//!
//! Only events the `.proto` sends to the client compile here: `on` and
//! `take` take a [`ClientEvent`] (`to: CLIENT` or `to: BOTH`), so a
//! WORLD-only event — one no client ever receives — is a compile error,
//! not a listener that never fires:
//!
//! ```compile_fail,E0277
//! use gsb_client::events::Events;
//! use gsb_protocol::event::{Dest, Event, WorldEvent};
//! #[derive(Clone, PartialEq, prost::Message)]
//! struct GoldReceived {
//!     #[prost(uint64, tag = "1")]
//!     amount: u64,
//! }
//! impl Event for GoldReceived { const OP: u16 = 2001; const TO: Dest = Dest::World; }
//! impl WorldEvent for GoldReceived {}
//! let mut events = Events::new();
//! let _ = events.take::<GoldReceived>(); // `GoldReceived: ClientEvent` is not satisfied
//! ```
//!
//! Over a connection, [`RequestClient::with_events`] runs it beside the
//! typed requests (one reader, one bounded receive per turn):
//! [`RequestClient::poll`] reads one turn, [`RequestClient::events`]
//! hands the table out. A client that reads pushes says so in its AUTH
//! ([`Credentials::with_events`]); one that does not is never sent any
//! (the server counts each such leg, `events_client_unsupported`).
//!
//! [`RequestClient::with_events`]: crate::requests::RequestClient::with_events
//! [`RequestClient::poll`]: crate::requests::RequestClient::poll
//! [`RequestClient::events`]: crate::requests::RequestClient::events
//! [`Credentials::with_events`]: crate::session::Credentials::with_events

use std::collections::{HashMap, VecDeque};

use gsb_protocol::FrameBody;
use gsb_protocol::base;
use gsb_protocol::event::ClientEvent;
use gsb_protocol::op;
use prost::Message;

/// The queue's bound when none is given (`docs/EVENTS.md` §20 E15).
pub const DEFAULT_CAPACITY: usize = 1024;

/// One event, typed, with where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Pushed<E> {
    /// The event.
    pub body: E,
    /// Who pushed it (`PublisherId`; 0 is the engine itself).
    pub publisher: u32,
    /// Its CLIENT-leg number from `publisher` within `stream`, from 1.
    pub seq: u64,
    /// The server's row incarnation the number belongs to.
    pub stream: u64,
}

/// Pushes this client never saw: `missed` numbers from `publisher` in
/// `stream`, just before `seq` (the one that showed the gap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// The publisher whose numbers skipped.
    pub publisher: u32,
    /// The stream they skipped in.
    pub stream: u64,
    /// How many numbers are missing.
    pub missed: u64,
    /// The number that arrived after them.
    pub seq: u64,
}

/// The event half's counts — one meaning each. Every `PUSH` frame ends
/// in exactly one of `undecodable`, `stale`, `handled`, `taken`,
/// `dropped_unread`, `body_undecodable`, or is still queued:
/// `received = stale + handled + taken + dropped_unread +
/// body_undecodable + queued` (an undecodable envelope is not received).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventCounters {
    /// `PUSH` frames whose envelope decoded.
    pub received: u64,
    /// `PUSH` frames whose envelope did not decode (dropped).
    pub undecodable: u64,
    /// Gaps detected ([`Events::take_gaps`]).
    pub gaps: u64,
    /// Numbers missing, summed over the gaps.
    pub missed: u64,
    /// Pushes numbered at or below one already seen from the same
    /// publisher in the same stream (a duplicate or a reordering — never
    /// in this design, counted if it ever happens): dropped.
    pub stale: u64,
    /// Stream changes after the first: a new server row for this
    /// identity, every publisher's count reset.
    pub restarted: u64,
    /// Pushes handed to the listeners of their type ([`Events::on`]).
    pub handled: u64,
    /// Pushes handed out by [`Events::take`].
    pub taken: u64,
    /// Queued pushes dropped, oldest first, from the full queue.
    pub dropped_unread: u64,
    /// Pushes whose body did not decode as the type they were handled
    /// or taken as (a client and server built from different `.proto`s):
    /// dropped.
    pub body_undecodable: u64,
}

impl EventCounters {
    /// Every counter as `(name, value)`, in declaration order (a load
    /// report's keys).
    pub fn fields(&self) -> [(&'static str, u64); 10] {
        [
            ("received", self.received),
            ("undecodable", self.undecodable),
            ("gaps", self.gaps),
            ("missed", self.missed),
            ("stale", self.stale),
            ("restarted", self.restarted),
            ("handled", self.handled),
            ("taken", self.taken),
            ("dropped_unread", self.dropped_unread),
            ("body_undecodable", self.body_undecodable),
        ]
    }

    /// Add another set (a fold over many clients).
    pub fn add(&mut self, o: &Self) {
        self.received += o.received;
        self.undecodable += o.undecodable;
        self.gaps += o.gaps;
        self.missed += o.missed;
        self.stale += o.stale;
        self.restarted += o.restarted;
        self.handled += o.handled;
        self.taken += o.taken;
        self.dropped_unread += o.dropped_unread;
        self.body_undecodable += o.body_undecodable;
    }
}

/// A listener, type-erased: decodes the body and calls the caller's
/// closure; `false` when the body did not decode.
type Listener = Box<dyn FnMut(&base::Push) -> bool + Send>;

/// The sans-IO event table (see the module docs).
pub struct Events {
    capacity: usize,
    /// The stream the numbers below belong to (`None`: no push yet).
    stream: Option<u64>,
    /// The last number seen per publisher in `stream`.
    last: HashMap<u32, u64>,
    listeners: HashMap<u16, Vec<Listener>>,
    queue: VecDeque<base::Push>,
    gaps: Vec<Gap>,
    counts: EventCounters,
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events")
            .field("capacity", &self.capacity)
            .field("stream", &self.stream)
            .field("queued", &self.queue.len())
            .field("counts", &self.counts)
            .finish_non_exhaustive()
    }
}

impl Events {
    /// A table queueing up to [`DEFAULT_CAPACITY`] events.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A table queueing up to `capacity` events nobody listens for (0:
    /// such an event is dropped at once, counted `dropped_unread`).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            stream: None,
            last: HashMap::new(),
            listeners: HashMap::new(),
            queue: VecDeque::new(),
            gaps: Vec::new(),
            counts: EventCounters::default(),
        }
    }

    /// Call `f` with every `E` from now on, as it arrives (several
    /// listeners of one type are called in registration order). Events
    /// of a type with a listener are never queued.
    pub fn on<E: ClientEvent>(&mut self, mut f: impl FnMut(Pushed<E>) + Send + 'static) {
        let l: Listener = Box::new(move |p| match E::from_wire(&p.body) {
            Ok(body) => {
                f(Pushed {
                    body,
                    publisher: p.publisher,
                    seq: p.seq,
                    stream: p.stream,
                });
                true
            }
            Err(_) => false,
        });
        self.listeners.entry(E::OP).or_default().push(l);
    }

    /// Feed one frame: `true` when it was a `PUSH` (consumed — the
    /// caller does nothing more with it), `false` for any other frame.
    pub fn ingest(&mut self, f: &FrameBody) -> bool {
        if f.op != op::base::PUSH {
            return false;
        }
        match base::Push::decode(f.payload.as_ref()) {
            Ok(p) => self.push(p),
            Err(_) => self.counts.undecodable += 1,
        }
        true
    }

    /// Feed one already-decoded push (what [`Self::ingest`] does after
    /// the envelope).
    pub fn push(&mut self, p: base::Push) {
        self.counts.received += 1;
        if self.stream != Some(p.stream) {
            if self.stream.is_some() {
                self.counts.restarted += 1;
            }
            self.stream = Some(p.stream);
            self.last.clear();
        }
        let last = self.last.get(&p.publisher).copied().unwrap_or(0);
        if p.seq <= last {
            self.counts.stale += 1;
            return;
        }
        if p.seq > last + 1 {
            let missed = p.seq - last - 1;
            self.counts.gaps += 1;
            self.counts.missed += missed;
            self.gaps.push(Gap {
                publisher: p.publisher,
                stream: p.stream,
                missed,
                seq: p.seq,
            });
        }
        self.last.insert(p.publisher, p.seq);
        let Ok(op) = u16::try_from(p.op) else {
            // No event type has this op: nobody can take it.
            self.queue_push(p);
            return;
        };
        if let Some(ls) = self.listeners.get_mut(&op) {
            let mut ok = true;
            for l in ls.iter_mut() {
                ok &= l(&p);
            }
            if ok {
                self.counts.handled += 1;
            } else {
                self.counts.body_undecodable += 1;
            }
            return;
        }
        self.queue_push(p);
    }

    fn queue_push(&mut self, p: base::Push) {
        if self.capacity == 0 {
            self.counts.dropped_unread += 1;
            return;
        }
        if self.queue.len() >= self.capacity {
            self.queue.pop_front();
            self.counts.dropped_unread += 1;
        }
        self.queue.push_back(p);
    }

    /// The queued `E` events, oldest first (the others stay queued).
    pub fn take<E: ClientEvent>(&mut self) -> Vec<Pushed<E>> {
        let want = u32::from(E::OP);
        if !self.queue.iter().any(|p| p.op == want) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut rest = VecDeque::with_capacity(self.queue.len());
        for p in self.queue.drain(..) {
            if p.op != want {
                rest.push_back(p);
                continue;
            }
            match E::from_wire(&p.body) {
                Ok(body) => {
                    self.counts.taken += 1;
                    out.push(Pushed {
                        body,
                        publisher: p.publisher,
                        seq: p.seq,
                        stream: p.stream,
                    });
                }
                Err(_) => self.counts.body_undecodable += 1,
            }
        }
        self.queue = rest;
        out
    }

    /// The gaps detected since the last call, in arrival order.
    pub fn take_gaps(&mut self) -> Vec<Gap> {
        std::mem::take(&mut self.gaps)
    }

    /// Events waiting in the queue.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The stream of the last push (`None`: none yet).
    pub fn stream(&self) -> Option<u64> {
        self.stream
    }

    /// The counts so far.
    pub fn counters(&self) -> EventCounters {
        self.counts
    }
}

#[cfg(test)]
mod tests;
