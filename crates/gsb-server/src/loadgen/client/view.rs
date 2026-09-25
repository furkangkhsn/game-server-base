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
//! applied unconditionally, and a private delta is an error. What one
//! record and one cell exit mean is the game's half — its bot's decoder
//! (`bot/`); the view lives in the per-client bot, behind `BotClient`.

use super::*;

mod run;
pub(crate) use run::*;
