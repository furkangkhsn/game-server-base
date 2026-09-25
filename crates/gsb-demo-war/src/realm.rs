//! The realm's data — what a live game loads from its character
//! database: the saved characters, keyed by the player's AUTHENTICATED
//! identity (K4, `docs/GAME-MODULE.md`): each one's faction and where it
//! logged out. Every shard's game and the server's join router read the
//! one table, so they agree on where a login appears
//! ([`Realm::placement`]).
//!
//! **A player with no saved character** (an unknown identity, or an
//! anonymous session — the empty identity) is assigned a faction by a
//! deterministic rule, [`faction_of`]: the 64-bit FNV-1a hash of the
//! identity, modulo the faction count. It spawns at its faction's base,
//! a few metres off the base's centre by the same hash. Deterministic,
//! so the router (which only sees the identity) and the spawn agree
//! without sharing state; stable, so a returning unsaved player comes
//! back to the same side. Rejected: round-robin by join order (the
//! arena's rule) — the router would have to predict the room's join
//! count, which it cannot see; the transport session id — the arena's
//! reasoning (server-global ids can all share a residue), and a
//! reconnecting player would change sides.

use std::collections::HashMap;
use std::sync::Arc;

use gsb_kit::team::Team;

use crate::components::Pos3;
use crate::world::{FACTIONS, base};

/// A saved character: its faction and where it stands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Saved {
    pub faction: Team,
    pub at: Pos3,
}

/// The realm: the saved characters. Every shard's game instance is built
/// from the same realm.
#[derive(Debug, Clone, Default)]
pub struct Realm {
    /// Saved characters, keyed by the player's AUTHENTICATED identity —
    /// the ticket's validated player, or the claimed `Auth.name` on the
    /// local-auth development path (where anyone can log in as anyone).
    pub logins: Arc<HashMap<String, Saved>>,
}

/// 64-bit FNV-1a of `bytes`.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The faction of a player with no saved character: FNV-1a of the
/// identity modulo [`FACTIONS`] (module docs).
#[must_use]
pub fn faction_of(identity: &str) -> Team {
    Team((fnv1a(identity.as_bytes()) % u64::from(FACTIONS)) as u8)
}

impl Realm {
    /// A realm with no saved characters.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Save a character of `faction` at `at` for the player who logs in
    /// as `identity`.
    #[must_use]
    pub fn with_login(mut self, identity: &str, faction: Team, at: Pos3) -> Self {
        Arc::make_mut(&mut self.logins).insert(identity.to_string(), Saved { faction, at });
        self
    }

    /// The saved character of `identity` (`None`: none saved).
    #[must_use]
    pub fn saved(&self, identity: &str) -> Option<Saved> {
        self.logins.get(identity).copied()
    }

    /// Where the player who logged in as `identity` appears, and on which
    /// side: its saved character, or — unsaved — [`faction_of`]'s
    /// faction at that faction's base, up to 10 m off its centre (by the
    /// identity's hash: unsaved players do not all stack on one spot).
    #[must_use]
    pub fn placement(&self, identity: &str) -> Saved {
        if let Some(saved) = self.saved(identity) {
            return saved;
        }
        let h = fnv1a(identity.as_bytes());
        let faction = faction_of(identity);
        let jitter = |bits: u64| ((bits % 21) as f32) - 10.0;
        let mut at = base(faction);
        at.x += jitter(h >> 8);
        at.z += jitter(h >> 24);
        Saved { faction, at }
    }
}
