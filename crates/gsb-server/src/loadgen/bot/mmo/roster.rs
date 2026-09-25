//! The bots' characters (K4, `docs/GAME-MODULE.md`): the MMO realm the
//! load generator hosts — in process and as the `--serve` child — saves
//! a character for every bot name it can produce, so a bot's login
//! restores it where its roaming starts and the population begins
//! spread over the four shards.
//!
//! The loadgen's bots take the local-auth path: the character key is the
//! claimed `Auth.name` (`lg-{id}`, [`crate::bot::bot_name`]) — the
//! development path, where a name is not an authenticated identity.

use std::time::Duration;

use gsb_demo_mmo::world::SHARDS;
use gsb_demo_mmo::{Pos3, Realm};

use crate::bot::bot_name;

/// Characters on the roster: bots `0..ROSTER` have one (an orchestrated
/// run of 10 000 clients is well inside); a bot beyond it logs in
/// unsaved and starts on waystone 0.
pub(crate) const ROSTER: u64 = 1 << 16;

/// Bot `id`'s home waystone: `id mod 4` (waystone `i` lies in shard
/// `i`'s region).
pub(crate) fn home_waystone(id: u64) -> usize {
    (id % SHARDS as u64) as usize
}

/// Where bot `id`'s character is saved: the start of its roaming ring
/// round its home waystone.
pub(crate) fn home(id: u64) -> Pos3 {
    let [x, z] = super::ring(home_waystone(id), id, Duration::ZERO);
    Pos3::new(x, 0.0, z)
}

/// The live realm ([`Realm::standard`]: its camps, packs and flyer) with
/// the bots' characters.
pub(crate) fn realm() -> Realm {
    (0..ROSTER).fold(Realm::standard(), |realm, id| {
        realm.with_login(&bot_name(id), home(id))
    })
}
