//! The bots' characters (K4, `docs/GAME-MODULE.md`): the war realm the
//! load generator hosts — in process and as the `--serve` child — saves
//! a character for every bot name it can produce: faction `id mod 3`, on
//! the ring of post `(id / 3) mod 14` — so every post is held by all
//! three factions and the population begins spread over the four shards.
//!
//! The loadgen's bots take the local-auth path: the character key is the
//! claimed `Auth.name` (`lg-{id}`, [`crate::bot::bot_name`]).

use std::time::Duration;

use gsb_demo_war::world::FACTIONS;
use gsb_demo_war::{Pos3, Realm};
use gsb_kit::team::Team;

use crate::bot::bot_name;

/// Characters on the roster: bots `0..ROSTER` have one; a bot beyond it
/// logs in unsaved (its faction by the war's identity hash, at its
/// faction's base).
pub(crate) const ROSTER: u64 = 1 << 16;

/// Bot `id`'s faction.
pub(crate) fn faction(id: u64) -> Team {
    Team((id % u64::from(FACTIONS)) as u8)
}

/// Bot `id`'s home post.
pub(crate) fn home_post(id: u64) -> usize {
    (id / u64::from(FACTIONS)) as usize % super::posts().len()
}

/// Where bot `id`'s character is saved: the start of its ring round its
/// home post.
pub(crate) fn home(id: u64) -> Pos3 {
    let [x, z] = super::ring(home_post(id), id, Duration::ZERO);
    Pos3::ground(x, z)
}

/// The realm with the bots' characters.
pub(crate) fn realm() -> Realm {
    (0..ROSTER).fold(Realm::empty(), |realm, id| {
        realm.with_login(&bot_name(id), faction(id), home(id))
    })
}
