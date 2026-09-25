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

#[cfg(test)]
mod tests;

use gsb_demo::game::CellExit;
use gsb_kit::client::wire::{Fields, Value, sint32};
use gsb_kit::client::{ClientDecoder, ClientError};
use prost::Message;

/// The loadgen client's view: wire id → wire position.
pub(crate) type ClientView = gsb_kit::client::ClientView<DemoDecoder>;

/// The demo's decode seam. A record's cell uses the server's own
/// formula (floor of the WIRE coordinates / cell_size, the kit's
/// `Grid2`), so a `CellExit` forgets exactly the entities the server
/// considers to be in that cell; a `CellExit` carries the cell's INDEX.
///
/// The record — every entity of every frame, the receive loop's hot
/// path — is walked by hand (`game.proto`'s `EntityRecord { uint64
/// entity = 1; sint32 x = 2; sint32 y = 3; }`; pinned to the generated
/// decoder by this module's tests); the rare cell exit uses the
/// generated `CellExit`.
pub(crate) struct DemoDecoder {
    pub(crate) cell_size: f32,
}

impl ClientDecoder for DemoDecoder {
    type Record = (i32, i32);
    type Cell = (i32, i32);

    #[inline]
    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32)), ClientError> {
        let (mut entity, mut x, mut y) = (0, 0, 0);
        for field in Fields::new(body) {
            match field? {
                (1, Value::Varint(v)) => entity = v,
                (2, Value::Varint(v)) => x = sint32(v),
                (3, Value::Varint(v)) => y = sint32(v),
                (1..=3, _) => return Err(ClientError::Malformed("wrong wire type")),
                _ => {}
            }
        }
        Ok((entity, (x, y)))
    }

    #[inline]
    fn cell_of(&self, &(x, y): &(i32, i32)) -> (i32, i32) {
        (
            (x as f32 / self.cell_size).floor() as i32,
            (y as f32 / self.cell_size).floor() as i32,
        )
    }

    #[inline]
    fn cell_exit(&self, body: &[u8]) -> Result<(i32, i32), ClientError> {
        let c = CellExit::decode(body)?;
        Ok((c.x, c.y))
    }
}
