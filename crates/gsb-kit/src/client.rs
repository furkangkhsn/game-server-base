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
//! → `(wire id, cell, what the view stores)`, one cell-exit body → the
//! cell. The envelope itself is walked in place (no allocation): record
//! and cell bodies reach the decoder as sub-slices of the frame, and a
//! frame is decoded completely before the view changes — an undecodable
//! frame is an error and leaves the view as it was. Scratch buffers are
//! reused, so the steady state allocates nothing beyond the view's own
//! map growth.
//!
//! RPC responses (`Private.responses`) and the game's private payload
//! (`Private.game`) are not part of the view; they are skipped. A client
//! that uses them decodes the frame with its typed mirror too.

use std::fmt;

mod view;
mod wire;

pub use view::ClientView;

#[cfg(test)]
mod tests;

/// The game's decode seam: what a record body and a cell-exit body mean.
/// Bodies are exactly what the game's server-side
/// [`RecordCodec::encode`](crate::codec::RecordCodec::encode) and
/// [`CellSpace::encode_cell`](crate::space::CellSpace::encode_cell)
/// wrote (a typed mirror's `EntityRecord` / `CellExit` decode them).
pub trait ClientDecoder {
    /// What the view keeps per entity (a position, a whole record, …).
    type Record;
    /// A cell of the game's cell space, as a client derives it from a
    /// record (the same formula the server's cell space uses) and as a
    /// cell exit names it.
    type Cell: PartialEq;

    /// One entity record body → its wire id, its cell, and what the view
    /// stores for it.
    fn record(&self, body: &[u8]) -> Result<(u64, Self::Cell, Self::Record), prost::DecodeError>;

    /// One cell-exit body → the cell every held record in it leaves.
    fn cell_exit(&self, body: &[u8]) -> Result<Self::Cell, prost::DecodeError>;
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
    /// The kit envelope is malformed (truncated, an invalid varint, a
    /// known field under the wrong wire type, a group).
    Envelope(&'static str),
    /// The game's decoder rejected a record or cell-exit body.
    Body(prost::DecodeError),
    /// A `Private` snapshot flagged `delta` (a protocol error: the
    /// one-shot view is always a full).
    PrivateDelta,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope(why) => write!(f, "malformed kit envelope: {why}"),
            Self::Body(e) => write!(f, "undecodable record or cell body: {e}"),
            Self::PrivateDelta => f.write_str("a private snapshot flagged delta"),
        }
    }
}

impl std::error::Error for ClientError {}
