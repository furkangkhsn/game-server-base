//! The kit's reference CLIENT: the client half of the snapshot envelope
//! (`kit.proto`), game-agnostic — one [`ClientView`] applies a
//! connection's `WorldSnapshot` and `Private` frames exactly as the
//! client rules in `kit.proto` state them, whatever the game.
//!
//! - A group FULL replaces the view.
//! - A group DELTA is applied on top, in the fixed order `removed` →
//!   `cell_exits` → `entities` (upserts), even across a sequence gap (the
//!   stream is event-driven; the records are absolute) — so a record that
//!   left a cell and came back in the same delta, or a removed id
//!   re-added in it, ends up present.
//! - A delta WITHOUT a baseline (no full applied yet) is dropped until
//!   the next full ([`Counters::gap_drops`]).
//! - A duplicate — a sequence `<=` the last ACCEPTED one — is discarded.
//!   Before the first accepted frame nothing is a duplicate: there is no
//!   last accepted sequence to compare with (a `0` sequence — the proto3
//!   default, which live ticks never carry — is not invented as one).
//! - The one-shot `Private` full is a per-connection baseline reset:
//!   applied UNCONDITIONALLY (it replaces the view and adopts its
//!   sequence, whatever the group stream accepted last). A `Private`
//!   snapshot flagged `delta` is a protocol error
//!   ([`ClientError::PrivateDelta`]), never applied.
//!
//! The game supplies the decode seam, [`ClientDecoder`]: one record body
//! → `(wire id, what the view stores)`, a stored record → its cell, one
//! cell-exit body → the cell. A record's cell is derived only when a
//! cell exit needs it (once per held record per delta that carries
//! exits), never per received record. A decoder may decode a body with
//! the game's generated type (`decode(body)?`) or walk it with
//! [`wire::Fields`] — the cheaper choice for a hot receive loop.
//!
//! A frame is applied in two walks over it, in place ([`wire::Fields`],
//! no allocation): the first reads the header and decodes the (few)
//! removals and cell exits into reused scratch buffers, validating the
//! whole envelope; the second decodes each record body straight into the
//! view — records are never buffered (a buffer per view, at thousands of
//! views, is memory a receive loop keeps missing in cache). So a
//! malformed envelope or cell exit is an error that changes nothing,
//! and a record body the game's decoder rejects — found while the view
//! is already changing — is an error that leaves the view EMPTY and
//! WITHOUT a baseline (never half a frame): deltas drop until the next
//! full restores it, as for a fresh client.
//!
//! RPC responses (`Private.responses`) and the game's private payload
//! (`Private.game`) are not part of the view; they are skipped. A client
//! that uses them decodes the frame with its typed mirror too.

use std::fmt;

mod view;
pub mod wire;

pub use view::ClientView;

#[cfg(test)]
mod tests;

/// The game's decode seam: what a record body and a cell-exit body mean
/// (a generated type's `decode(body)?` or a hand walk with
/// [`wire::Fields`]). Bodies are exactly what the game's server-side
/// [`RecordCodec::encode`](crate::codec::RecordCodec::encode) and
/// [`CellSpace::encode_cell`](crate::space::CellSpace::encode_cell)
/// wrote (a typed mirror's `EntityRecord` / `CellExit` decode them).
pub trait ClientDecoder {
    /// What the view keeps per entity (a position, a whole record, …).
    type Record;
    /// A cell of the game's cell space, as a cell exit names it.
    type Cell: PartialEq;

    /// One entity record body → its wire id and what the view stores
    /// for it.
    fn record(&self, body: &[u8]) -> Result<(u64, Self::Record), ClientError>;

    /// The cell a stored record lies in — the same formula the server's
    /// cell space applies to the record's wire value.
    fn cell_of(&self, record: &Self::Record) -> Self::Cell;

    /// One cell-exit body → the cell every held record in it leaves.
    fn cell_exit(&self, body: &[u8]) -> Result<Self::Cell, ClientError>;
}

/// The view's counters (what a load generator reports).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Fulls applied: group fulls AND one-shot private fulls.
    pub fulls: u64,
    /// One-shot private fulls applied (a subset of `fulls`).
    pub private_fulls: u64,
    /// Group deltas applied.
    pub deltas: u64,
    /// Group deltas dropped for lack of a baseline.
    pub gap_drops: u64,
    /// Group frames discarded as duplicates (sequence `<=` the last
    /// accepted).
    pub stale: u64,
    /// Frames rejected: undecodable envelopes or bodies, and private
    /// deltas.
    pub errors: u64,
}

/// What became of one group snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// A full replaced the view.
    Full,
    /// A delta was applied on top.
    Delta,
    /// A delta was dropped: no baseline yet.
    NoBaseline,
    /// A duplicate sequence: discarded (not an error).
    Stale,
}

/// One group snapshot's outcome and its sequence (read even when the
/// frame is discarded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// The frame's `sequence`.
    pub sequence: u64,
    /// What the view did with it.
    pub apply: Apply,
}

/// What one `Private` frame carried in its `payload` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateEvent {
    /// An input ack: the connection's `processed_up_to`.
    Ack(u64),
    /// A one-shot full, applied; its sequence is now the view's.
    Full {
        /// The full's `sequence`.
        sequence: u64,
    },
    /// No payload arm (a frame carrying only responses or game bytes).
    Empty,
}

/// Why a frame was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// Malformed protobuf — the kit envelope, or a body a game's decoder
    /// walked with [`wire::Fields`] (truncated, an invalid varint, a
    /// known field under the wrong wire type, a group).
    Malformed(&'static str),
    /// A body a game's decoder decoded with a generated type was
    /// rejected (`?` on a `prost::DecodeError` lands here).
    Body(prost::DecodeError),
    /// A `Private` snapshot flagged `delta` (a protocol error: the
    /// one-shot view is always a full).
    PrivateDelta,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed protobuf: {why}"),
            Self::Body(e) => write!(f, "undecodable record or cell body: {e}"),
            Self::PrivateDelta => f.write_str("a private snapshot flagged delta"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<prost::DecodeError> for ClientError {
    fn from(e: prost::DecodeError) -> Self {
        Self::Body(e)
    }
}
