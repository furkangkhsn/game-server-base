//! What a migrating entity carries across a seam ([`KitMig`]): the
//! game's captured state and the kit's records — the park record
//! ([`ShardParkRecord`]), the input session ([`ShardInputRecord`]) and a
//! crystallization pin ([`ShardPin`]).

use std::ops::Deref;

use gsb_core::id::PlayerId;

/// The full state of a migrating entity: the GAME's captured state
/// (`ShardGame::Mig` — the demo: position, speed if any, pending move
/// target) and the KIT's player records (park, input session). Opaque
/// to the core; the game rebuilds its half on
/// [`ShardedRoom`](super::ShardedRoom)'s
/// `on_migrate_in`, the kit its own.
///
/// The `park` field is the RECONNECT §14.2 rule in action: a parked (or
/// bot-fed) player's ledger record is part of the migrating PLAYER state,
/// not a side table — an entity that crosses a seam while detached
/// carries its park record along, so the receiving shard's ledger answers
/// the resume and keeps feeding the bot. The `input` field is the same
/// rule for the input sequence state (GAME-MODULE §5, K1–K3).
///
/// Nothing serializes it today: the in-process link moves it. A future
/// out-of-process link (DISTRIBUTED §4b) needs a codec for it — owned by
/// the logic that defines the type, the kit — covering every field,
/// `input` included.
///
/// Derefs to the game's state, so its fields read straight through
/// (`mig.pos` for the demo's `mig.game.pos`).
#[derive(Debug, Clone)]
pub struct KitMig<M> {
    /// The game's captured state.
    pub game: M,
    /// The entity's park record, if it is parked or bot-fed (`None` for
    /// every live session and every NPC).
    pub park: Option<ShardParkRecord>,
    /// The owning player's input session, if it has one (`None` for
    /// every NPC).
    pub input: Option<ShardInputRecord>,
    /// The crystallization pin, when the entity moves BECAUSE a fight
    /// crystallizes onto the receiving shard (`None` for every other
    /// migration — a region crossing, a release).
    pub pin: Option<ShardPin>,
}

impl<M> Deref for KitMig<M> {
    type Target = M;

    fn deref(&self) -> &M {
        &self.game
    }
}

/// A park-ledger entry in transit: carried inside [`KitMig`] because
/// that is what survives migrations (the receiving shard files it in its
/// own ledger under `identity`, against the entity it rebuilt).
#[derive(Debug, Clone)]
pub struct ShardParkRecord {
    /// The resume key of the parked session.
    pub identity: String,
    /// The parked session's STABLE player identity (Faz 2): what
    /// `resume_lookup` answers (the core finds its row by one lookup)
    /// and what the bot synthesizes input under. Travels with the record
    /// across migrations, so the identity is stable end to end.
    pub player: PlayerId,
    /// The parked entity's wire id — stable across migrations.
    pub wire: u64,
    /// Latched at AI-handover expiry: the bot owns the entity.
    pub bot: bool,
}

/// A player's input session in transit (the kit's sequence/ack rule —
/// `InputSeq`): carried inside [`KitMig`] because the session moves with
/// the player. The source reads it when it reports the crossing and
/// forgets it when the move commits (`on_migrate_out`); the receiving
/// shard continues the session from it, so
///
/// - the input the source processed in the tick the player left — the
///   one that moved it, typically — is acked by the receiving shard (the
///   source hands the session off in its migrate phase, before its
///   broadcast phase would have sent that ack),
/// - the sequence rule keeps its mark: an input numbered at or below
///   `hwm` is still a duplicate or a late datagram after the crossing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardInputRecord {
    /// The highest input sequence processed (the high-water mark).
    pub hwm: u64,
    /// The highest sequence already acked to the client; `hwm > acked`
    /// is an ack the receiving shard still owes.
    pub acked: u64,
}

/// A crystallization pin in transit (CROSS-SHARD §4 layer 4, "C2
/// sonucu"): the entity moves to the shard that owns its fight partner,
/// and the receiving shard HOLDS it there — ownership decoupled from
/// the partition's region — until the fight has been quiet for the
/// policy's `release` ticks or the entity strays out of the band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardPin {
    /// The partner's wire id: the receiving shard's own entity the fight
    /// is with (it is held there too, for as long as the pair).
    pub partner: u64,
    /// The tick of the pair's latest contact: the release clock keeps
    /// counting from it across the move.
    pub last: u64,
}
