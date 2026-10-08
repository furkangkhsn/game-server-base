//! The room-side event table (docs/EVENTS.md §4) — the twin of `Calls`:
//! actor state, ECS-free, lent to the logic each tick. The EVENTS phase
//! fills it (decoded, owner resolved); systems TAKE from it, typed;
//! whatever nobody took is dropped at the tick's end and counted
//! (`events_untaken`) — never kept for a later tick (§4.4).

use std::any::{Any, TypeId};
use std::collections::BTreeMap;

use crate::contract::{Event, Owner, PublisherId, WorldEvent};
use crate::room::Addressee;

/// One event this tick, typed.
#[derive(Debug, Clone)]
pub struct Arrived<E> {
    /// The body.
    pub body: E,
    /// Who it was addressed to (a player, or the whole room).
    pub to: Addressee,
    /// The addressee's owner (`Owner::ACTOR` for room-wide events): the
    /// same opaque handle `Calls` uses (`owner.entity()` in bevy/hecs).
    pub owner: Owner,
    /// Who published it.
    pub from: PublisherId,
    /// Its WORLD-leg sequence number (per publisher → this addressee).
    pub seq: u64,
}

/// One event this tick, type-erased (`take_all`'s canonical order).
pub struct AnyArrived {
    /// The op.
    pub op: u16,
    /// The addressee.
    pub to: Addressee,
    /// Its owner.
    pub owner: Owner,
    /// The publisher.
    pub from: PublisherId,
    /// The sequence number.
    pub seq: u64,
    body: Box<dyn Any + Send>,
}

impl AnyArrived {
    /// Is it an `E`?
    pub fn is<E: WorldEvent>(&self) -> bool {
        self.op == E::OP
    }
    /// The typed event, or itself back.
    pub fn downcast<E: WorldEvent>(self) -> Result<Arrived<E>, Self> {
        if self.op != E::OP {
            return Err(self);
        }
        match self.body.downcast::<E>() {
            Ok(b) => Ok(Arrived {
                body: *b,
                to: self.to,
                owner: self.owner,
                from: self.from,
                seq: self.seq,
            }),
            Err(body) => Err(Self { body, ..self }),
        }
    }
}

/// A boot-time refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// Two event types claim one op.
    OpCollision { op: u16 },
}

/// The table's counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EventCounts {
    /// Entered the table (decoded, owner resolved).
    pub admitted: u64,
    /// An op this room never `accept`ed.
    pub no_handler: u64,
    /// A body that did not decode.
    pub undecodable: u64,
    /// Taken by the logic.
    pub taken: u64,
    /// Dropped at the tick's end: nobody took it.
    pub untaken: u64,
}

type Decode = fn(&[u8]) -> Option<Box<dyn Any + Send>>;

fn decode_as<E: Event>(b: &[u8]) -> Option<Box<dyn Any + Send>> {
    E::from_bytes(b).ok().map(|e| Box::new(e) as Box<dyn Any + Send>)
}

struct Slot {
    op: u16,
    type_id: TypeId,
    to: Addressee,
    owner: Owner,
    from: PublisherId,
    seq: u64,
    body: Box<dyn Any + Send>,
}

/// The table.
#[derive(Default)]
pub struct Events {
    decoders: BTreeMap<u16, (TypeId, Decode)>,
    arrived: Vec<Slot>,
    counts: EventCounts,
}

impl Events {
    /// Boot: this room takes `E` (an op nobody accepted is counted
    /// `no_handler` at EVENTS and never reaches the logic). Only WORLD /
    /// BOTH events compile here.
    pub fn accept<E: WorldEvent>(&mut self) -> Result<(), BootError> {
        if self.decoders.contains_key(&E::OP) {
            return Err(BootError::OpCollision { op: E::OP });
        }
        self.decoders.insert(E::OP, (TypeId::of::<E>(), decode_as::<E>));
        Ok(())
    }

    /// EVENTS: one delivered event enters the table.
    pub(crate) fn admit(&mut self, op: u16, body: &[u8], to: Addressee, owner: Owner, from: PublisherId, seq: u64) {
        let Some((type_id, decode)) = self.decoders.get(&op) else {
            self.counts.no_handler += 1;
            return;
        };
        let Some(body) = decode(body) else {
            self.counts.undecodable += 1;
            return;
        };
        self.counts.admitted += 1;
        self.arrived.push(Slot {
            op,
            type_id: *type_id,
            to,
            owner,
            from,
            seq,
            body,
        });
    }

    /// This tick's `E` events, in arrival order (per publisher → addressee
    /// FIFO; §10).
    pub fn take<E: WorldEvent>(&mut self) -> Vec<Arrived<E>> {
        if self.arrived.is_empty() {
            return Vec::new();
        }
        let want = TypeId::of::<E>();
        let mut out = Vec::new();
        let mut rest = Vec::with_capacity(self.arrived.len());
        for s in self.arrived.drain(..) {
            if s.type_id == want {
                let Ok(body) = s.body.downcast::<E>() else {
                    unreachable!("the slot's type id matches")
                };
                out.push(Arrived {
                    body: *body,
                    to: s.to,
                    owner: s.owner,
                    from: s.from,
                    seq: s.seq,
                });
            } else {
                rest.push(s);
            }
        }
        self.arrived = rest;
        self.counts.taken += out.len() as u64;
        out
    }

    /// Every event of this tick, in the canonical (EVENTS-drain) order.
    pub fn take_all(&mut self) -> Vec<AnyArrived> {
        self.counts.taken += self.arrived.len() as u64;
        self.arrived
            .drain(..)
            .map(|s| AnyArrived {
                op: s.op,
                to: s.to,
                owner: s.owner,
                from: s.from,
                seq: s.seq,
                body: s.body,
            })
            .collect()
    }

    /// Tick end: what nobody took is dropped and counted.
    pub(crate) fn end_tick(&mut self) {
        if self.arrived.is_empty() {
            return;
        }
        self.counts.untaken += self.arrived.len() as u64;
        self.arrived.clear();
    }

    /// The counters.
    pub fn counts(&self) -> &EventCounts {
        &self.counts
    }

    /// The buffer's capacity (the idle-cost proof).
    pub fn capacity(&self) -> usize {
        self.arrived.capacity()
    }
}
