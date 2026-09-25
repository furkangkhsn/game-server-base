//! Remote effects (`docs/CROSS-SHARD.md` §2–§4): gameplay acting on an
//! entity another shard owns. Seeing a target is not touching it — the
//! attacker's shard sees the target as a BORROWED record, validates the
//! hit locally (range, angle: anti-cheat stays with the attacker), and
//! hands the effect to the target's authority as ONE idempotent message
//! ([`ShardMsg::RemoteEffect`]). The authority applies it to its own
//! world; nothing else may write a foreign entity.
//!
//! The envelope is the core's (identities, stamps, the dedup key); the
//! payload is opaque bytes the game encodes and decodes. Bytes rather
//! than a typed generic on purpose: a typed payload would be a third
//! type parameter on every shard message, actor and registry (the ripple
//! CROSS-SHARD §8.1 rejected for the team export), and it would reopen
//! the process-boundary codec question (DISTRIBUTED §4b) that bytes have
//! already answered — the envelope is plain integers, so the core can
//! own its codec, and the payload's codec is the game's by construction.
//!
//! Protocol constants below are CORRECTNESS parameters (the dedup
//! window's bound is derived from them), hardcoded like the tombstone
//! TTL, not operator knobs.

use bytes::Bytes;

mod book;
mod window;

pub(crate) use book::{EffectBook, EffectOutbox};
pub(crate) use window::Seen;

/// How many effects one shard may EMIT per tick (all targets, all
/// neighbours together; forwards do not count — they re-send an effect
/// already minted). Beyond it [`CrossSeam::emit`](crate::shard::CrossSeam::emit) refuses with
/// [`EmitRefused::Budget`] — the caller learns synchronously, nothing is
/// dropped behind its back. The budget is what bounds the dedup window
/// (see [`EFFECT_WINDOW`]).
pub const EFFECT_BUDGET_PER_TICK: usize = 128;

/// The transport envelope of an effect, in ticks: an effect older than
/// this (`tick - at_tick`) is dropped wherever it is — in the sender's
/// retry buffer, at a forwarder, at the authority — and counted
/// `expired`. A CEILING, not the gameplay policy: a game refuses stale
/// effects much earlier in its own apply hook (a 30-tick-late melee hit
/// must not land — DISTRIBUTED §4 "staleness"; the stamp travels so it
/// can). 7 ticks ≈ 230 ms at 30 Hz.
pub const EFFECT_MAX_AGE_TICKS: u64 = 7;

/// The dedup window per origin shard, in sequence numbers. Derived, not
/// tuned: an origin mints at most [`EFFECT_BUDGET_PER_TICK`] sequence
/// numbers per tick, and nothing older than [`EFFECT_MAX_AGE_TICKS`] is
/// ever admitted, so every admissible effect's sequence number lies
/// within `BUDGET × (MAX_AGE + 1)` of the highest one seen from its
/// origin — a genuine effect never falls out of the window, and the
/// window is all the state a duplicate check needs.
pub const EFFECT_WINDOW: u64 = EFFECT_BUDGET_PER_TICK as u64 * (EFFECT_MAX_AGE_TICKS + 1);

/// How many times an effect may be forwarded after its target migrated
/// on (old owner → new owner). One hop covers a single migration in
/// flight; the bound only has to make a ping-ponging target finite.
pub const EFFECT_MAX_HOPS: u8 = 3;

/// Failed sends (the neighbour's bounded inbox was full) wait here and
/// are retried on the next tick, oldest first, until they are sent or
/// expire. Bounded: everything the budget can mint inside the age
/// envelope fits; an overflow is dropped and counted (`dropped_full`).
pub const EFFECT_RETRY_CAP: usize = EFFECT_BUDGET_PER_TICK * (EFFECT_MAX_AGE_TICKS as usize + 1);

/// How long an old owner remembers where an entity it handed on went
/// (ticks after the migration): long enough for any effect a neighbour
/// could still aim at the lent copy it saw last (the copy is lent until
/// the migration tick, the attacker's view lags it by up to two ticks,
/// and the effect may spend up to [`EFFECT_MAX_AGE_TICKS`] in transit).
pub const EFFECT_FORWARD_TTL_TICKS: u64 = EFFECT_MAX_AGE_TICKS + 4;

/// The identity of one effect — the idempotency key (RECONNECT's
/// exactly-once discipline: who minted it, in which incarnation, where
/// in that origin's stream). Minted by the ORIGIN shard's actor, never
/// by the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EffectId {
    /// The shard that minted the effect (its index in the room).
    pub origin: usize,
    /// The room incarnation it was minted in (the registry's install
    /// generation): wire ids — the target identity — are unique only
    /// within one incarnation, so the epoch completes the target's
    /// identity too. An effect of another epoch is refused.
    pub epoch: u64,
    /// The origin's monotonic effect counter (from 1).
    pub seq: u64,
}

/// One effect on a foreign entity, as it travels between shards.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteEffect {
    /// The target's wire identity (unique for the room incarnation:
    /// ranges are disjoint and a migrating entity keeps its id).
    pub target: u64,
    /// The source entity's wire identity — attribution (kill credit,
    /// statistics) and the first ordering key. `0` = no source entity.
    pub source: u64,
    /// The idempotency key.
    pub id: EffectId,
    /// The origin's tick at emission: the one-tick apply alignment
    /// (applied at the authority's tick `at_tick + 1`) and the
    /// staleness stamp.
    pub at_tick: u64,
    /// Forwards so far (old owner → new owner), bounded by
    /// [`EFFECT_MAX_HOPS`].
    pub hops: u8,
    /// Game-owned bytes (what the effect does).
    pub payload: Bytes,
}

/// Why [`CrossSeam::emit`](crate::shard::CrossSeam::emit) refused an effect. Nothing was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitRefused {
    /// No neighbour lends the target: there is no authority to route to
    /// (it is not visible from here — or it is one of this shard's own
    /// entities, which the game writes directly).
    NotLent,
    /// The target is this shard's own entity (reported by callers that
    /// know the local set, e.g. the kit): apply it directly instead.
    Local,
    /// This tick's [`EFFECT_BUDGET_PER_TICK`] is spent.
    Budget,
}

/// What the authority's game did with an effect (the answer of
/// [`ShardLogic::apply_remote_effect`](crate::shard::ShardLogic::apply_remote_effect); counted, never retried).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectOutcome {
    /// Applied to the target.
    Applied,
    /// Refused by game policy (stale, out of range on re-validation,
    /// undecodable payload).
    Rejected,
    /// No such entity here: it died, or it never lived here.
    NoTarget,
}
