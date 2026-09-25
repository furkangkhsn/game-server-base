//! The per-game half of a load client (GAME-MODULE §4.4, §6 decision 8):
//! a [`LoadBot`] per hosted game says what that game's client SENDS (its
//! next input, the flood input, the churn input) and how it READS the
//! game's frames (its snapshot / private opcodes and the kit's reference
//! client over the game's [`ClientDecoder`](gsb_kit::client::ClientDecoder)).
//! Everything else — connect, auth, join, the paced send / bounded
//! receive loop, input numbering and ack tracking, leave, the report —
//! is the game-agnostic client in `client/`.
//!
//! Dispatch is dynamic (`dyn`): one virtual call per received frame and
//! per sent input, next to a protobuf walk of the whole frame.

use std::sync::Arc;
use std::time::Duration;

use gsb_kit::client::{ClientError, Counters, PrivateEvent, Snapshot};

use crate::Args;

mod demo;
mod flags;

pub(crate) use demo::*;
pub(crate) use flags::*;

/// One game the load generator can drive.
pub(crate) trait LoadBot: Send + Sync {
    /// The game's snapshot opcode (`Game::SNAPSHOT_OP`).
    fn snapshot_op(&self) -> u16;
    /// The game's private opcode (`Game::PRIVATE_OP`).
    fn private_op(&self) -> u16;
    /// A fresh bot for client `id` (its view and its input schedule).
    fn client(&self, id: u64) -> Box<dyn BotClient>;
    /// The flood client's frame (`--flood-id`): UNNUMBERED (seq 0) — the
    /// flood probes the drop-attribution guards, not the sequence rule.
    fn flood_input(&self) -> (u16, Vec<u8>);
    /// The churn client's input numbered `seq` (`--churn-secs`): a plain
    /// per-id target; the churn client keeps no view.
    fn churn_input(&self, id: u64, seq: u64) -> (u16, Vec<u8>);
    /// One line for the run's header (stderr): what the bot does.
    fn describe(&self) -> String;
}

/// One client's game-specific state: the kit's reference client over the
/// game's decoder, and whatever the bot needs to pick its next input.
pub(crate) trait BotClient: Send {
    /// Apply one group snapshot frame (the kit's client rules).
    fn apply_snapshot(&mut self, frame: &[u8]) -> Result<Snapshot, ClientError>;
    /// Apply one private frame (ack, or the one-shot full).
    fn apply_private(&mut self, frame: &[u8]) -> Result<PrivateEvent, ClientError>;
    /// The view's counters.
    fn counters(&self) -> Counters;
    /// Entities in the view.
    fn view_len(&self) -> usize;
    /// The join reply: this client's own wire id.
    fn joined(&mut self, _entity: u64) {}
    /// The input for this move interval (`elapsed` since the client's
    /// loop started), numbered `seq` — or `None`: nothing to send this
    /// interval, and `seq` is not consumed.
    fn next_input(&mut self, elapsed: Duration, seq: u64) -> Option<(u16, Vec<u8>)>;
}

/// The games this build has a bot for, in catalog order — the server's
/// compiled-in games (one cargo feature each).
pub(crate) fn games() -> Vec<&'static str> {
    vec![gsb_server::games::demo::DemoModule::NAME]
}

/// The catalog's spelling of `name`, or the error naming every game this
/// build can drive.
pub(crate) fn game_named(name: &str) -> Result<&'static str, String> {
    games().into_iter().find(|g| *g == name).ok_or_else(|| {
        format!(
            "--game: unknown game `{name}` (compiled in: {})",
            games().join(", ")
        )
    })
}

/// The bot for the run's game (`args.game`, a catalog name — the parser
/// takes nothing else).
pub(crate) fn bot_for(args: &Args) -> Arc<dyn LoadBot> {
    match args.game {
        gsb_server::games::demo::DemoModule::NAME => Arc::new(DemoBot {
            profile: args.profile,
            still_frac: args.still_frac,
            spawn_half: args.spawn_half,
            cell_size: args.cell_size,
        }),
        other => unreachable!("--game `{other}` is not in the catalog the parser checks"),
    }
}
