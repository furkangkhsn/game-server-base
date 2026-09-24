//! The shared park bookkeeping every room class uses for reconnect:
//! the policy, the ledger entry, and the four hooks the core calls.

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::PlayerId;
use gsb_core::room::{Detach, ExpireTo, ResumeFound};

use crate::common::*;
use crate::game::Game;

#[cfg(test)]
mod tests;

/// The kit rooms' disconnect-park policy: how long a dropped
/// transport's entity is held, and where the hold ends.
///
/// - `grace = Some(0)` disables parking entirely ([`Detach::Despawn`] —
///   the byte-for-byte pre-reconnect behavior), so an operator can turn
///   the feature off without losing the code path;
/// - `grace = Some(d)` holds for at most `d` (the core's own deadline);
/// - `grace = None` holds until the game's
///   [`Game::may_release`](crate::game::Game::may_release) veto clears
///   (combat-held — the core asks every tick);
///
/// and an ended hold goes `to` the bot ([`ExpireTo::AiHandover`], the
/// default) or releases the slot ([`ExpireTo::Despawn`]). The default
/// grace lives at [`crate::DEFAULT_DISCONNECT_GRACE`] (the one public
/// constant the server config defaults from).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParkPolicy {
    pub grace: Option<Duration>,
    pub to: ExpireTo,
}

impl Default for ParkPolicy {
    fn default() -> Self {
        Self {
            grace: Some(crate::DEFAULT_DISCONNECT_GRACE),
            to: ExpireTo::AiHandover,
        }
    }
}

/// One entry of a kit room's park ledger (§4: "park defteri logic'te yaşar" —
/// the ledger lives in the game logic; the core only queries it through
/// [`gsb_core::room::GameLogic::resume_lookup`]).
///
/// Keyed by identity (the resume key: ticket player or local-auth name),
/// because THAT is what survives the transport death. The entry carries
/// the parked session's STABLE [`PlayerId`] (Faz 2): it is what
/// `resume_lookup` answers (the core finds its row by ONE lookup) and
/// what the bot synthesizes input under — stability across resume comes
/// exactly from riding this record (§14.2).
#[derive(Debug)]
pub(crate) struct ParkEntry {
    /// The parked player's stable identity.
    pub player: PlayerId,
    /// The parked bevy entity (kept alive by the hold).
    pub entity: Entity,
    /// The hold expired toward [`ExpireTo::AiHandover`]: the bot owns the
    /// entity now (`bot_fed` on the core row is this flag's core-side
    /// twin). Cleared when a resume consumes the entry.
    pub bot: bool,
}

/// The policy answer to a transport death (the `on_disconnect` hook
/// body shared by every kit room): park the entity for the configured
/// grace toward the configured end, recording the ledger entry.
/// A connection we do not know (stale detach) cannot park anything.
///
/// Visibility note (§3.2): nothing else changes — the entity keeps its
/// components, group membership and slot, and the snapshot pass keeps
/// encoding it. Under the `all` strategy teammates simply keep seeing
/// the parked hero standing where its last command left it; a team-fog
/// strategy WOULD hide or mark that record here (a per-strategy filter
/// in its snapshot encoder), which is game-band content, not core
/// machinery — deliberately not implemented in the base.
pub(crate) fn park_on_disconnect(
    player_entity: &HashMap<PlayerId, Entity>,
    player: PlayerId,
    identity: &str,
    policy: &ParkPolicy,
    ledger: &mut HashMap<String, ParkEntry>,
) -> Detach {
    if policy.grace.is_some_and(|g| g.is_zero()) || identity.is_empty() {
        // Disabled (or nothing to resume with): the old semantics.
        return Detach::Despawn;
    }
    match player_entity.get(&player) {
        Some(&entity) => {
            ledger.insert(
                identity.to_string(),
                ParkEntry {
                    player,
                    entity,
                    bot: false,
                },
            );
            Detach::Hold {
                grace: policy.grace,
                to: policy.to,
            }
        }
        // Stale detach (no entity of ours): fall through to despawn,
        // which the core turns into the ordinary no-op funnel.
        None => Detach::Despawn,
    }
}

/// The `on_detach_expired` hook body: an expired hold either releases the
/// identity (despawn arm — the core runs `on_leave`, we just forget the
/// entry so a later join is a transparent fresh join) or latches the bot
/// marker (AI arm — the entity keeps playing, driven by
/// [`Game::bot_actions`](crate::game::Game::bot_actions)).
pub(crate) fn park_on_expire(
    ledger: &mut HashMap<String, ParkEntry>,
    player: PlayerId,
    to: ExpireTo,
) {
    match to {
        ExpireTo::Despawn => ledger.retain(|_, e| e.player != player),
        ExpireTo::AiHandover => {
            for e in ledger.values_mut().filter(|e| e.player == player) {
                e.bot = true;
            }
        }
    }
}

/// The `may_release` hook body: the game's veto on ending an untimed
/// (combat-held) hold, asked about the parked player's entity. A player
/// without an entity here has nothing to hold: released.
pub(crate) fn park_may_release<G: Game>(
    game: &mut G,
    player_entity: &HashMap<PlayerId, Entity>,
    world: &mut World,
    player: PlayerId,
) -> bool {
    match player_entity.get(&player) {
        Some(&entity) => game.may_release(world, entity),
        None => true,
    }
}

/// The `resume_lookup` hook body: the ledger answers whether the identity
/// is parked. The answer is the parked session's STABLE [`PlayerId`] —
/// the key the core's own tables are keyed by, so the core finds its row
/// with one lookup and a resumed session keeps the same identity (Faz 2;
/// pre-Faz-2 this resolved the entity's wire serial for the core's scan
/// over the detached rows).
pub(crate) fn park_lookup(
    _world: &World,
    ledger: &HashMap<String, ParkEntry>,
    identity: &str,
) -> ResumeFound {
    match ledger.get(identity) {
        Some(entry) => ResumeFound::Held(entry.player),
        None => ResumeFound::Never,
    }
}

/// The `on_resume` hook body (Faz 2 shrink): consume the ledger entry
/// (the bot loses the entity; the human's numbered inputs take over).
/// With every table keyed by the stable [`PlayerId`] there is nothing
/// left to RE-KEY here — the pre-Faz-2 `conn_entity` rename is gone (the
/// player→entity mapping kept its key across the whole disconnect).
/// What remains is exactly what is session-scoped: the seq/ack reset of
/// DESIGN §14.2 (the resumed session numbers from 1; dropping the entry
/// makes `ingest`'s `or_default` mint a fresh one). Strategy-specific
/// per-session tables, where a room keeps any, are dropped by the room
/// itself before calling into here.
pub(crate) fn park_resume(
    ledger: &mut HashMap<String, ParkEntry>,
    input: &mut InputSeq,
    identity: &str,
    player: PlayerId,
) {
    ledger.remove(identity);
    input.end(player);
}
