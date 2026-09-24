//! The client's local view of the world: applying snapshots, fulls
//! and deltas exactly as a real client would, so a wrong stream is
//! visible as a wrong view.
//!
//! The rules are the kit's reference client (`gsb_kit::client`, the
//! client rules of `kit.proto`): a full REPLACES the view; a delta with
//! a baseline applies ON TOP in the fixed order `removed` →
//! `cell_exits` → `entities`, even across a sequence gap; a delta with
//! no baseline is DROPPED until the next full (`gap_drops`); a sequence
//! `<=` the last accepted one is discarded; the one-shot private full is
//! applied unconditionally, and a private delta is an error. This file
//! is the demo's half: what one record and one cell exit mean.

use super::*;

mod run;
pub(crate) use run::*;

use gsb_demo::game::{CellExit, EntityRecord};
use gsb_kit::client::ClientDecoder;
use prost::Message;

/// The loadgen client's view: wire id → wire position.
pub(crate) type ClientView = gsb_kit::client::ClientView<DemoDecoder>;

/// The demo's decode seam. A record's cell uses the server's own
/// formula (floor of the WIRE coordinates / cell_size, the kit's
/// `Grid2`), so a `CellExit` forgets exactly the entities the server
/// considers to be in that cell; a `CellExit` carries the cell's INDEX.
pub(crate) struct DemoDecoder {
    pub(crate) cell_size: f32,
}

impl ClientDecoder for DemoDecoder {
    type Record = (i32, i32);
    type Cell = (i32, i32);

    #[inline]
    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32), (i32, i32)), prost::DecodeError> {
        let e = EntityRecord::decode(body)?;
        let cell = (
            (e.x as f32 / self.cell_size).floor() as i32,
            (e.y as f32 / self.cell_size).floor() as i32,
        );
        Ok((e.entity, cell, (e.x, e.y)))
    }

    #[inline]
    fn cell_exit(&self, body: &[u8]) -> Result<(i32, i32), prost::DecodeError> {
        let c = CellExit::decode(body)?;
        Ok((c.x, c.y))
    }
}
