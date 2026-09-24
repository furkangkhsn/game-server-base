//! [`ClientView`]: the view one connection holds and the kit's client
//! rules that change it (the rules themselves are listed in the parent
//! module's docs and in `kit.proto`).

use std::collections::HashMap;

use super::wire::{Fields, Value, each_varint, input_ack};
use super::{Apply, ClientDecoder, ClientError, Counters, PrivateEvent, Snapshot};

/// One connection's view of its snapshot group, under the kit's client
/// rules: wire id → what the game's [`ClientDecoder`] stores (plus the
/// record's cell, kept for the cell-exit rule).
pub struct ClientView<D: ClientDecoder> {
    decoder: D,
    entities: HashMap<u64, (D::Cell, D::Record)>,
    /// The last ACCEPTED sequence; `None` = no baseline yet.
    last_seq: Option<u64>,
    counters: Counters,
    scratch: Scratch<D>,
}

/// One frame, decoded before the view changes (reused across frames).
struct Scratch<D: ClientDecoder> {
    records: Vec<(u64, D::Cell, D::Record)>,
    removed: Vec<u64>,
    cells: Vec<D::Cell>,
}

impl<D: ClientDecoder> Scratch<D> {
    fn clear(&mut self) {
        self.records.clear();
        self.removed.clear();
        self.cells.clear();
    }
}

/// A decoded `WorldSnapshot` header (its lists are in the scratch).
struct Header {
    sequence: u64,
    delta: bool,
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
                records: Vec::new(),
                removed: Vec::new(),
                cells: Vec::new(),
            },
        }
    }

    /// Apply one GROUP snapshot frame (the game's snapshot opcode): a
    /// full replaces the view, a delta with a baseline applies on top, a
    /// delta without one is dropped, a duplicate is discarded. An
    /// undecodable frame is an error and changes nothing.
    pub fn apply_snapshot(&mut self, frame: &[u8]) -> Result<Snapshot, ClientError> {
        let head = self.decode(frame)?;
        let apply = if self.last_seq.is_some_and(|last| head.sequence <= last) {
            self.counters.stale += 1;
            Apply::Stale
        } else if !head.delta {
            self.replace(head.sequence);
            Apply::Full
        } else if self.last_seq.is_none() {
            self.counters.gap_drops += 1;
            Apply::NoBaseline
        } else {
            self.merge(head.sequence);
            Apply::Delta
        };
        self.scratch.clear();
        Ok(Snapshot {
            sequence: head.sequence,
            apply,
        })
    }

    /// Apply one `Private` frame (the game's private opcode): an ack is
    /// reported, a one-shot full is applied UNCONDITIONALLY, a snapshot
    /// flagged `delta` is an error and changes nothing.
    pub fn apply_private(&mut self, frame: &[u8]) -> Result<PrivateEvent, ClientError> {
        let mut ack = None;
        let mut snapshot = None;
        let walked = Fields::new(frame).try_for_each(|field| {
            // `payload` is a oneof: the last arm on the wire wins.
            match field? {
                (1, Value::Len(body)) => (ack, snapshot) = (Some(input_ack(body)?), None),
                (2, Value::Len(body)) => (ack, snapshot) = (None, Some(body)),
                (1..=4, Value::Len(_)) => {}
                (1..=4, _) => return Err(ClientError::Envelope("wrong wire type")),
                _ => {}
            }
            Ok(())
        });
        if let Err(e) = walked {
            self.counters.errors += 1;
            return Err(e);
        }
        if let Some(up_to) = ack {
            return Ok(PrivateEvent::Ack(up_to));
        }
        let Some(body) = snapshot else {
            return Ok(PrivateEvent::Empty);
        };
        let head = self.decode(body)?;
        if head.delta {
            self.scratch.clear();
            self.counters.errors += 1;
            return Err(ClientError::PrivateDelta);
        }
        self.replace(head.sequence);
        self.counters.private_fulls += 1;
        self.scratch.clear();
        Ok(PrivateEvent::Full {
            sequence: head.sequence,
        })
    }

    /// The record of wire id `id`, if in view.
    pub fn get(&self, id: u64) -> Option<&D::Record> {
        self.entities.get(&id).map(|(_, record)| record)
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
        self.entities.iter().map(|(&id, (_, record))| (id, record))
    }

    /// Every wire id in view, in no particular order.
    pub fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.entities.keys().copied()
    }

    /// Every record in view, in no particular order.
    pub fn values(&self) -> impl Iterator<Item = &D::Record> {
        self.entities.values().map(|(_, record)| record)
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

    /// Decode a whole `WorldSnapshot` into the scratch; on error, count
    /// it and leave the scratch empty.
    fn decode(&mut self, frame: &[u8]) -> Result<Header, ClientError> {
        let decoded = self.decode_into(frame);
        if decoded.is_err() {
            self.scratch.clear();
            self.counters.errors += 1;
        }
        decoded
    }

    fn decode_into(&mut self, frame: &[u8]) -> Result<Header, ClientError> {
        let mut head = Header {
            sequence: 0,
            delta: false,
        };
        let (decoder, s) = (&self.decoder, &mut self.scratch);
        for field in Fields::new(frame) {
            match field? {
                (1, Value::Varint(v)) => head.sequence = v,
                (2, Value::Len(body)) => {
                    s.records
                        .push(decoder.record(body).map_err(ClientError::Body)?);
                }
                (3, value) => each_varint(value, |id| s.removed.push(id))?,
                (4, Value::Len(body)) => {
                    s.cells
                        .push(decoder.cell_exit(body).map_err(ClientError::Body)?);
                }
                (5, Value::Varint(v)) => head.delta = v != 0,
                (1..=5, _) => return Err(ClientError::Envelope("wrong wire type")),
                _ => {}
            }
        }
        Ok(head)
    }

    /// A full: the scratch's records become the whole view.
    fn replace(&mut self, sequence: u64) {
        self.entities.clear();
        for (id, cell, record) in self.scratch.records.drain(..) {
            self.entities.insert(id, (cell, record));
        }
        self.last_seq = Some(sequence);
        self.counters.fulls += 1;
    }

    /// A delta on top: `removed`, then `cell_exits`, then the upserts.
    fn merge(&mut self, sequence: u64) {
        let s = &mut self.scratch;
        for id in &s.removed {
            self.entities.remove(id);
        }
        if !s.cells.is_empty() {
            self.entities.retain(|_, (cell, _)| !s.cells.contains(cell));
        }
        for (id, cell, record) in s.records.drain(..) {
            self.entities.insert(id, (cell, record));
        }
        self.last_seq = Some(sequence);
        self.counters.deltas += 1;
    }
}
