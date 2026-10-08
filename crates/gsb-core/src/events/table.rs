//! The [`Events`] table: the room-side half of the event path, the twin
//! of [`Calls`](crate::calls::Calls) — actor state, ECS-agnostic (a plain
//! struct), lent to the game each tick. The EVENTS phase fills it
//! (decoded, owner resolved, `inbound.rs`); systems take from it, typed;
//! what nobody took is dropped at tick end and counted
//! (`events_untaken`, `docs/EVENTS.md` §4.4) — never carried into the
//! next tick.

use std::any::{Any, TypeId};
use std::collections::HashMap;

use crate::calls::{BootError, DecodeError, Identity, Owner};
use crate::events::{PublisherId, WorldEvent};
use crate::metrics::EventCounts;

/// Who a WORLD leg was addressed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addressee {
    /// A player of this room, by its verified identity (`push`,
    /// `push_many`).
    Player(Identity),
    /// The whole room (`push_room`, `push_all`): its owner is
    /// [`Owner::ACTOR`].
    Room,
}

/// One event of type `E` this tick, as the game takes it.
#[derive(Debug, Clone)]
pub struct Arrived<E> {
    /// The decoded event.
    pub body: E,
    /// Who it was addressed to.
    pub to: Addressee,
    /// The owner the addressee is bound to — the same opaque key
    /// [`Incoming::owner`](crate::calls::Incoming::owner) carries: the
    /// session's explicit binding (`calls.bind`), else
    /// `GameLogic::call_owner(player)`; [`Owner::ACTOR`] for a room-wide
    /// event.
    pub owner: Owner,
    /// The publisher (`PublisherId(0)` = the engine).
    pub from: PublisherId,
    /// The WORLD leg's sequence number (per publisher → this addressee,
    /// from 1): a gap is a leg lost on the way, counted where it was
    /// lost.
    pub seq: u64,
}

/// An event of any type, from [`Events::take_all`] (canonical order).
pub struct AnyArrived {
    op: u16,
    to: Addressee,
    owner: Owner,
    from: PublisherId,
    seq: u64,
    body: Box<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for AnyArrived {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyArrived")
            .field("op", &self.op)
            .field("to", &self.to)
            .field("owner", &self.owner)
            .field("from", &self.from)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

impl AnyArrived {
    /// The event's op.
    pub fn op(&self) -> u16 {
        self.op
    }
    /// Who it was addressed to.
    pub fn to(&self) -> &Addressee {
        &self.to
    }
    /// The addressee's owner.
    pub fn owner(&self) -> Owner {
        self.owner
    }
    /// The publisher.
    pub fn from(&self) -> PublisherId {
        self.from
    }
    /// The WORLD leg's sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }
    /// `Ok` if this is an `E`; the event back otherwise (chainable).
    pub fn downcast<E: WorldEvent>(self) -> Result<Arrived<E>, AnyArrived> {
        match self.body.downcast::<E>() {
            Ok(b) => Ok(Arrived {
                body: *b,
                to: self.to,
                owner: self.owner,
                from: self.from,
                seq: self.seq,
            }),
            Err(body) => Err(AnyArrived { body, ..self }),
        }
    }
}

type Decode = fn(&[u8]) -> Result<Box<dyn Any + Send + Sync>, DecodeError>;

fn decode_as<E: WorldEvent>(b: &[u8]) -> Result<Box<dyn Any + Send + Sync>, DecodeError> {
    E::from_wire(b).map(|e| Box::new(e) as Box<dyn Any + Send + Sync>)
}

/// One event this tick, waiting for the game.
struct Slot {
    op: u16,
    type_id: TypeId,
    to: Addressee,
    owner: Owner,
    from: PublisherId,
    seq: u64,
    body: Box<dyn Any + Send + Sync>,
}

/// The per-actor event table (`docs/EVENTS.md` §4.3). A room and a shard
/// each own one and lend it to the game
/// ([`GameLogic::boot_events`](crate::room::GameLogic::boot_events),
/// [`GameLogic::update_with_events`](crate::room::GameLogic::update_with_events)).
///
/// The table never allocates while nothing arrives: a quiet room's
/// EVENTS phase is one empty `try_recv` and its buffer stays
/// `Vec::new()` (§3.2).
#[derive(Default)]
pub struct Events {
    /// The call table's request ops, copied at boot: requests and events
    /// share one op space (§4.1), so an event claiming a request's op is
    /// a boot error here too.
    requests: HashMap<u16, &'static str>,
    accepts: HashMap<u16, (TypeId, &'static str, Decode)>,
    arrived: Vec<Slot>,
    counts: EventCounts,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events")
            .field("accepts", &self.accepts.len())
            .field("arrived", &self.arrived.len())
            .field("counts", &self.counts)
            .finish()
    }
}

impl Events {
    /// An empty table that knows the request ops `requests` claims
    /// already (the actor's call table, booted first).
    pub(crate) fn sharing_ops(requests: &crate::calls::Calls) -> Self {
        Self {
            requests: requests.ops.clone(),
            ..Self::default()
        }
    }

    /// Boot: this room takes `E` (`take::<E>()`). A WORLD leg of an op no
    /// `accept` claims is counted `events_no_handler` at EVENTS and never
    /// reaches the game. Only `to: WORLD | BOTH` events compile here.
    ///
    /// # Errors
    ///
    /// [`BootError::EventOpCollision`] when another event type, or a
    /// request type of the actor's call table, already claims `E::OP`;
    /// [`BootError::EventEngineOp`] for an op in the engine band.
    pub fn accept<E: WorldEvent>(&mut self) -> Result<(), BootError> {
        let name = std::any::type_name::<E>();
        if E::OP < gsb_protocol::op::GAME_BAND_START {
            return Err(BootError::EventEngineOp {
                op: E::OP,
                ty: name,
            });
        }
        let first = self
            .requests
            .get(&E::OP)
            .copied()
            .or_else(|| self.accepts.get(&E::OP).map(|a| a.1));
        if let Some(first) = first {
            return Err(BootError::EventOpCollision {
                op: E::OP,
                first,
                second: name,
            });
        }
        self.accepts
            .insert(E::OP, (TypeId::of::<E>(), name, decode_as::<E>));
        Ok(())
    }

    /// Whether `op` is accepted.
    pub fn handles(&self, op: u16) -> bool {
        self.accepts.contains_key(&op)
    }

    /// This tick's `E` events, in arrival order (per publisher → addressee
    /// FIFO, `docs/EVENTS.md` §10).
    pub fn take<E: WorldEvent>(&mut self) -> Vec<Arrived<E>> {
        if self.arrived.is_empty() {
            return Vec::new();
        }
        let want = TypeId::of::<E>();
        let mut out = Vec::new();
        let mut rest = Vec::with_capacity(self.arrived.len());
        for s in self.arrived.drain(..) {
            if s.type_id == want {
                match s.body.downcast::<E>() {
                    Ok(body) => out.push(Arrived {
                        body: *body,
                        to: s.to,
                        owner: s.owner,
                        from: s.from,
                        seq: s.seq,
                    }),
                    Err(_) => unreachable!("the slot's type id matches"),
                }
            } else {
                rest.push(s);
            }
        }
        self.arrived = rest;
        self.counts.events_taken += out.len() as u64;
        out
    }

    /// Every event of this tick, in the canonical (EVENTS drain) order;
    /// `a.downcast::<E>()`.
    pub fn take_all(&mut self) -> Vec<AnyArrived> {
        if self.arrived.is_empty() {
            return Vec::new();
        }
        self.counts.events_taken += self.arrived.len() as u64;
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

    /// How many events wait this tick.
    pub fn len(&self) -> usize {
        self.arrived.len()
    }

    /// Nothing waits this tick.
    pub fn is_empty(&self) -> bool {
        self.arrived.is_empty()
    }

    /// The table's counters (cumulative; the EVENTS phase's included).
    pub fn counts(&self) -> &EventCounts {
        &self.counts
    }

    /// The buffer's capacity (the idle-cost proof: 0 in a room nothing
    /// was ever pushed to).
    pub fn capacity(&self) -> usize {
        self.arrived.capacity()
    }

    pub(crate) fn counts_mut(&mut self) -> &mut EventCounts {
        &mut self.counts
    }

    /// EVENTS: one delivered WORLD leg enters the table (the caller
    /// resolved its owner and checked [`Self::handles`]). An undecodable
    /// body is counted and dropped.
    pub(crate) fn admit(
        &mut self,
        op: u16,
        body: &[u8],
        to: Addressee,
        owner: Owner,
        from: PublisherId,
        seq: u64,
    ) {
        let Some((type_id, _, decode)) = self.accepts.get(&op) else {
            self.counts.events_no_handler += 1;
            return;
        };
        let Ok(body) = decode(body) else {
            self.counts.events_undecodable += 1;
            return;
        };
        self.counts.events_admitted += 1;
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

    /// Tick end: what nobody took is dropped and counted.
    pub(crate) fn end_tick(&mut self) {
        if self.arrived.is_empty() {
            return;
        }
        self.counts.events_untaken += self.arrived.len() as u64;
        self.arrived.clear();
    }
}
