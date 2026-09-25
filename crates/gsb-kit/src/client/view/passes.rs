//! The two walks that apply one `WorldSnapshot` (see the parent
//! modules' docs): pass 1 reads the header and decodes the removals and
//! cell exits, validating the whole envelope before the view changes;
//! pass 2 decodes each record body straight into the view.

use super::super::wire::{Fields, Value, each_varint};
use super::{ClientDecoder, ClientError, ClientView, Header};

impl<D: ClientDecoder> ClientView<D> {
    /// Walk a whole `WorldSnapshot` (pass 1): its header, and its
    /// removals and cell exits into the scratch; the record bodies are
    /// only located (pass 2 decodes them straight into the view). On
    /// error, count it and leave the scratch empty.
    #[inline]
    pub(super) fn decode(&mut self, frame: &[u8]) -> Result<Header, ClientError> {
        let decoded = self.decode_into(frame);
        if decoded.is_err() {
            self.scratch.clear();
            self.counters.errors += 1;
        }
        decoded
    }

    #[inline]
    fn decode_into(&mut self, frame: &[u8]) -> Result<Header, ClientError> {
        let mut head = Header {
            sequence: 0,
            delta: false,
            records: 0..0,
        };
        let (decoder, s) = (&self.decoder, &mut self.scratch);
        let mut fields = Fields::new(frame);
        loop {
            let at = frame.len() - fields.remaining();
            let Some(field) = fields.next() else {
                break;
            };
            match field? {
                (1, Value::Varint(v)) => head.sequence = v,
                // Located only: pass 2 decodes it into the view.
                (2, Value::Len(_)) => {
                    if head.records.is_empty() {
                        head.records.start = at;
                    }
                    head.records.end = frame.len() - fields.remaining();
                }
                (3, value) => each_varint(value, |id| s.removed.push(id))?,
                (4, Value::Len(body)) => {
                    s.cells.push(decoder.cell_exit(body)?);
                }
                (5, Value::Varint(v)) => head.delta = v != 0,
                (1..=5, _) => return Err(ClientError::Malformed("wrong wire type")),
                _ => {}
            }
        }
        Ok(head)
    }

    /// A full: the frame's records become the whole view.
    #[inline]
    pub(super) fn replace(&mut self, frame: &[u8], head: &Header) -> Result<(), ClientError> {
        self.entities.clear();
        self.upsert(&frame[head.records.clone()])?;
        self.last_seq = Some(head.sequence);
        self.counters.fulls += 1;
        Ok(())
    }

    /// A delta on top: `removed`, then `cell_exits` (one pass over the
    /// view for all of them; a held record's cell is derived only here),
    /// then the upserts.
    #[inline]
    pub(super) fn merge(&mut self, frame: &[u8], head: &Header) -> Result<(), ClientError> {
        let (decoder, s) = (&self.decoder, &self.scratch);
        for id in &s.removed {
            self.entities.remove(id);
        }
        if !s.cells.is_empty() {
            self.entities
                .retain(|_, record| !s.cells.contains(&decoder.cell_of(record)));
        }
        self.upsert(&frame[head.records.clone()])?;
        self.last_seq = Some(head.sequence);
        self.counters.deltas += 1;
        Ok(())
    }

    /// Pass 2: every record body of `records` (the frame's record span,
    /// whole fields), decoded straight into the view. A body the game's decoder rejects (after the view started
    /// changing) leaves the view WITHOUT a baseline — never half a frame:
    /// deltas drop until the next full.
    #[inline]
    fn upsert(&mut self, records: &[u8]) -> Result<(), ClientError> {
        let decoded = Fields::new(records).try_for_each(|field| {
            if let (2, Value::Len(body)) = field? {
                let (id, record) = self.decoder.record(body)?;
                self.entities.insert(id, record);
            }
            Ok(())
        });
        if decoded.is_err() {
            self.entities.clear();
            self.last_seq = None;
            self.scratch.clear();
            self.counters.errors += 1;
        }
        decoded
    }
}
