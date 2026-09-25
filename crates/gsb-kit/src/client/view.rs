//! [`ClientView`]: the view one connection holds and the kit's client
//! rules that change it (the rules themselves are listed in the parent
//! module's docs and in `kit.proto`).

use std::collections::HashMap;
use std::ops::Range;

use super::wire::{Fields, Value, input_ack};
use super::{Apply, ClientDecoder, ClientError, Counters, PrivateEvent, Snapshot};

mod passes;

/// One connection's view of its snapshot group, under the kit's client
/// rules: wire id → what the game's [`ClientDecoder`] stores.
pub struct ClientView<D: ClientDecoder> {
    decoder: D,
    entities: HashMap<u64, D::Record>,
    /// The last ACCEPTED sequence; `None` = no baseline yet.
    last_seq: Option<u64>,
    counters: Counters,
    scratch: Scratch<D>,
}

/// A frame's removals and cell exits, decoded before the view changes
/// (reused across frames; small — the records are not buffered).
struct Scratch<D: ClientDecoder> {
    removed: Vec<u64>,
    cells: Vec<D::Cell>,
}

impl<D: ClientDecoder> Scratch<D> {
    fn clear(&mut self) {
        self.removed.clear();
        self.cells.clear();
    }
}

/// A decoded `WorldSnapshot` header (its lists are in the scratch).
struct Header {
    sequence: u64,
    delta: bool,
    /// The byte span of the frame from its first `entities` field to the
    /// end of its last (empty: none) — all pass 2 has to walk.
    records: Range<usize>,
    /// The byte span of the record run's body (`records`, field 6), if
    /// the frame carries one (then `records` is empty).
    run: Option<Range<usize>>,
}

impl<D: ClientDecoder + Default> Default for ClientView<D> {
    fn default() -> Self {
        Self::new(D::default())
    }
}

impl<D: ClientDecoder> ClientView<D> {
    /// An empty view with no baseline.
    pub fn new(decoder: D) -> Self {
        Self {
            decoder,
            entities: HashMap::new(),
            last_seq: None,
            counters: Counters::default(),
            scratch: Scratch {
                removed: Vec::new(),
                cells: Vec::new(),
            },
        }
    }

    /// Apply one GROUP snapshot frame (the game's snapshot opcode): a
    /// full replaces the view, a delta with a baseline applies on top, a
    /// delta without one is dropped, a duplicate is discarded. A
    /// malformed envelope or cell exit is an error and changes nothing; a
    /// record body the game's decoder rejects is an error that leaves the
    /// view without a baseline (see the module docs).
    pub fn apply_snapshot(&mut self, frame: &[u8]) -> Result<Snapshot, ClientError> {
        let head = self.decode(frame)?;
        let apply = if self.last_seq.is_some_and(|last| head.sequence <= last) {
            self.counters.stale += 1;
            Apply::Stale
        } else if !head.delta {
            self.replace(frame, &head)?;
            Apply::Full
        } else if self.last_seq.is_none() {
            self.counters.gap_drops += 1;
            Apply::NoBaseline
        } else {
            self.merge(frame, &head)?;
            Apply::Delta
        };
        self.scratch.clear();
        Ok(Snapshot {
            sequence: head.sequence,
            apply,
        })
    }

    /// Apply one `Private` frame (the game's private opcode): the game's
    /// session payload, if any, goes to the decoder
    /// ([`ClientDecoder::session_private`]) first; an ack is reported, a
    /// one-shot full is applied UNCONDITIONALLY, a snapshot flagged
    /// `delta` is an error and changes nothing (errors otherwise as in
    /// [`Self::apply_snapshot`]).
    pub fn apply_private(&mut self, frame: &[u8]) -> Result<PrivateEvent, ClientError> {
        let mut ack = None;
        let mut snapshot = None;
        let mut game = None;
        let walked = Fields::new(frame).try_for_each(|field| {
            // `payload` is a oneof: the last arm on the wire wins.
            match field? {
                (1, Value::Len(body)) => (ack, snapshot) = (Some(input_ack(body)?), None),
                (2, Value::Len(body)) => (ack, snapshot) = (None, Some(body)),
                (4, Value::Len(body)) => game = Some(body),
                (1..=4, Value::Len(_)) => {}
                (1..=4, _) => return Err(ClientError::Malformed("wrong wire type")),
                _ => {}
            }
            Ok(())
        });
        if let Err(e) = walked {
            self.counters.errors += 1;
            return Err(e);
        }
        if let Some(body) = game
            && let Err(e) = self.decoder.session_private(body)
        {
            self.counters.errors += 1;
            return Err(e);
        }
        if let Some(up_to) = ack {
            return Ok(PrivateEvent::Ack(up_to));
        }
        let Some(body) = snapshot else {
            return Ok(if game.is_some() {
                PrivateEvent::Session
            } else {
                PrivateEvent::Empty
            });
        };
        let head = self.decode(body)?;
        if head.delta {
            self.scratch.clear();
            self.counters.errors += 1;
            return Err(ClientError::PrivateDelta);
        }
        self.replace(body, &head)?;
        self.counters.private_fulls += 1;
        self.scratch.clear();
        Ok(PrivateEvent::Full {
            sequence: head.sequence,
        })
    }

    /// The record of wire id `id`, if in view.
    pub fn get(&self, id: u64) -> Option<&D::Record> {
        self.entities.get(&id)
    }

    /// Whether wire id `id` is in view.
    pub fn contains(&self, id: u64) -> bool {
        self.entities.contains_key(&id)
    }

    /// How many entities are in view.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether the view is empty.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Every `(wire id, record)` in view, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &D::Record)> {
        self.entities.iter().map(|(&id, record)| (id, record))
    }

    /// Every wire id in view, in no particular order.
    pub fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.entities.keys().copied()
    }

    /// Every record in view, in no particular order.
    pub fn values(&self) -> impl Iterator<Item = &D::Record> {
        self.entities.values()
    }

    /// Whether a full has been applied (deltas apply only on one).
    pub fn has_baseline(&self) -> bool {
        self.last_seq.is_some()
    }

    /// The last accepted sequence (`None` before the first full).
    pub fn last_sequence(&self) -> Option<u64> {
        self.last_seq
    }

    /// The counters so far.
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// The game's decoder.
    pub fn decoder(&self) -> &D {
        &self.decoder
    }
}
