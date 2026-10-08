//! The Rust client's event half (docs/EVENTS.md §8): sans-IO, like
//! `gsb_client::requests::Requests` — the client loop hands it every
//! frame; it consumes `PUSH` frames, checks the per-publisher sequence,
//! and either calls a registered handler (`on::<E>`) or queues the event
//! for `take::<E>()`. Only CLIENT / BOTH events compile here.
//!
//! Gap detection: per publisher, the last CLIENT-leg number seen.
//! `seq > last + 1` → a gap of `seq - last - 1` (the server lost them —
//! counted on its side too; the client re-syncs, e.g. re-requests the
//! friend list); `seq == 1` after a stream → a new stream (a fresh
//! session's new router row); `seq <= last` otherwise → stale (never in
//! this design: the fence keeps FIFO; counted if it ever happens).

use std::collections::{BTreeMap, VecDeque};

use crate::contract::{ClientEvent, PublisherId, WireMessage};
use crate::wire::{Frame, PUSH, Push};

/// One event, typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Pushed<E> {
    /// The body.
    pub body: E,
    /// Who published.
    pub from: PublisherId,
    /// The CLIENT-leg sequence number.
    pub seq: u64,
}

/// A detected gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// The publisher.
    pub from: PublisherId,
    /// How many numbers were skipped.
    pub missed: u64,
}

/// The client-side counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClientCounts {
    /// PUSH frames consumed.
    pub received: u64,
    /// Numbers missing (sum of gaps).
    pub missed: u64,
    /// Out-of-order or duplicate numbers.
    pub stale: u64,
    /// New streams (a fresh session).
    pub restarted: u64,
    /// Envelopes or bodies that did not decode.
    pub undecodable: u64,
    /// Queued events dropped unread (queue full).
    pub dropped_unread: u64,
}

type Handler = Box<dyn FnMut(&[u8], PublisherId, u64) -> bool>;

/// Queued events, at most this many (the oldest goes first, counted).
pub const QUEUE_CAP: usize = 1024;

/// The event half.
#[derive(Default)]
pub struct EventsClient {
    last: BTreeMap<PublisherId, u64>,
    handlers: BTreeMap<u16, Vec<Handler>>,
    queue: VecDeque<Push>,
    gaps: Vec<Gap>,
    counts: ClientCounts,
}

impl EventsClient {
    /// Listen for `E` (C#: `client.On<E>(handler)`; Godot: a signal; JS:
    /// a listener; Bevy: an `Event` writer — docs/EVENTS.md §8.3).
    pub fn on<E: ClientEvent>(&mut self, mut f: impl FnMut(Pushed<E>) + 'static) {
        let h: Handler = Box::new(move |b, from, seq| match E::from_bytes(b) {
            Ok(body) => {
                f(Pushed { body, from, seq });
                true
            }
            Err(_) => false,
        });
        self.handlers.entry(E::OP).or_default().push(h);
    }

    /// Feed one frame; `true` when it was a PUSH (consumed).
    pub fn ingest(&mut self, f: &Frame) -> bool {
        if f.op != PUSH {
            return false;
        }
        let Ok(p) = Push::from_bytes(&f.payload) else {
            self.counts.undecodable += 1;
            return true;
        };
        self.counts.received += 1;
        let from = PublisherId(p.publisher);
        match self.last.get(&from).copied() {
            Some(last) if p.seq == 1 && last >= 1 => self.counts.restarted += 1,
            Some(last) if p.seq > last + 1 => {
                let missed = p.seq - last - 1;
                self.counts.missed += missed;
                self.gaps.push(Gap { from, missed });
            }
            Some(last) if p.seq <= last => {
                self.counts.stale += 1;
                return true;
            }
            None if p.seq > 1 => {
                // Joined a stream late (events before this session's
                // first PUSH were lost: parked, unsupported …).
                let missed = p.seq - 1;
                self.counts.missed += missed;
                self.gaps.push(Gap { from, missed });
            }
            _ => {}
        }
        self.last.insert(from, p.seq);
        let op = p.op as u16;
        if let Some(hs) = self.handlers.get_mut(&op) {
            let mut ok = true;
            for h in hs.iter_mut() {
                ok &= h(&p.body, from, p.seq);
            }
            if !ok {
                self.counts.undecodable += 1;
            }
            return true;
        }
        if self.queue.len() >= QUEUE_CAP {
            self.queue.pop_front();
            self.counts.dropped_unread += 1;
        }
        self.queue.push_back(p);
        true
    }

    /// The queued `E` events, in arrival order.
    pub fn take<E: ClientEvent>(&mut self) -> Vec<Pushed<E>> {
        let mut out = Vec::new();
        let mut rest = VecDeque::with_capacity(self.queue.len());
        for p in self.queue.drain(..) {
            if p.op as u16 == E::OP {
                match E::from_bytes(&p.body) {
                    Ok(body) => out.push(Pushed {
                        body,
                        from: PublisherId(p.publisher),
                        seq: p.seq,
                    }),
                    Err(_) => self.counts.undecodable += 1,
                }
            } else {
                rest.push_back(p);
            }
        }
        self.queue = rest;
        out
    }

    /// The gaps detected since the last call.
    pub fn take_gaps(&mut self) -> Vec<Gap> {
        std::mem::take(&mut self.gaps)
    }

    /// The counters.
    pub fn counts(&self) -> &ClientCounts {
        &self.counts
    }
}
