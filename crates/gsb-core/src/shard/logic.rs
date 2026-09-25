//! The sharding seam: the hooks a [`GameLogic`] must add to own a
//! slice of a map.
//!
//! NOT split further: a trait is one item.
use std::fmt::Debug;

use crate::id::PlayerId;
use crate::room::{Action, GameLogic, TickCtx};
use crate::shard::*;

/// Game-side behaviour of a SHARD actor: everything [`GameLogic`] (this
/// crate's `room` supertrait) already shares — the tick seam, snapshot
/// groups, membership, and the reconnect surface — plus this actor's
/// exclusive sharding seam below. The compile-time separation survives
/// the unification DELIBERATELY (design candidate A of
/// `docs/TRAIT-ARCHITECTURE.md` §3, not the single-trait B): forgetting
/// `neighbors()` or the serial range still does not compile, so a silent
/// migration break stays structurally impossible.
///
/// Implemented by the game crate; the core never inspects the world `W`
/// or the state `S::State`.
pub trait ShardLogic<W>: GameLogic<W> {
    /// The full state of a migrating entity (game-shaped; opaque to the
    /// core). `Debug` so a `ShardMsg` can derive it.
    type State: Debug + Send + 'static;

    /// This shard's index within its room (0-based; the factory supplies
    /// it at construction and the registry relies on it for the sample id
    /// and the neighbor wiring).
    fn index(&self) -> usize;

    /// The total number of shards in this room (the topology; the factory
    /// supplies it at construction).
    fn shard_count(&self) -> usize;

    /// This shard's wire-id range base: ids are minted as
    /// `serial_base + serial_used + 1`. Ranges of all shards of a room
    /// must be disjoint (the identity invariant).
    fn serial_base(&self) -> u64;
    /// This shard's wire-id range size.
    fn serial_range(&self) -> u64;
    /// How many identities this shard has minted so far.
    fn serial_used(&self) -> u64;

    /// The neighbor shard indices this shard exchanges with (the grid
    /// topology is the game's business).
    fn neighbors(&self) -> &[usize];

    /// Entities that moved into neighbor `neighbor`'s region by the end of
    /// this tick (phase 4): the full state plus the owning connection for
    /// player entities. The shard sends the `Migrate` messages itself and
    /// despawns the entities on the next tick (see module docs,
    /// "Migration protocol"); the logic only *reports* the crossings.
    fn collect_migrations(&mut self, world: &mut W, neighbor: usize)
    -> Vec<Migrating<Self::State>>;

    /// Install a migrating entity: spawn it with its full state, keeping
    /// its wire id. `player` is present for player entities (the shard
    /// handles the binding table itself); the logic must record the
    /// player→entity bookkeeping so that `on_leave` and the next
    /// `collect_migrations` see it. The player id is UNCHANGED by the
    /// move (stable identity — Faz 2).
    fn on_migrate_in(
        &mut self,
        world: &mut W,
        wire: u64,
        state: Self::State,
        player: Option<PlayerId>,
    );

    /// Remove an entity that migrated out (phase 4, the mark fired):
    /// despawn it and clean its bookkeeping.
    fn on_migrate_out(&mut self, world: &mut W, wire: u64);

    /// This shard's boundary entities for the phase-5 export to the
    /// neighbors. Each record pairs the CORE-managed wire identity with a
    /// LOGIC-owned payload ([`GameLogic::Strip`] — the visibility-strip
    /// content is the game's decision: position-only games keep it
    /// minimal, combat/prediction games extend it). The actor diffs this
    /// set against its per-neighbor ledgers and ships only deltas
    /// (upserts + exits) — except where a Full is due (first contact,
    /// resync, periodic cadence, post-drop healing), when the same set
    /// ships whole.
    ///
    /// Change detection is WHOLESALE [`PartialEq`] on the payload: any
    /// field difference produces an upsert. A game that wants looser
    /// equivalence (ignore-jitter) implements it in ITS type — quantize
    /// or round inside the `PartialEq`, or report an already-quantized
    /// payload from this method — so the core's diff stays one comparison
    /// and the equivalence policy lives where the payload lives.
    fn collect_border(&self, world: &W) -> Vec<BorderRecord<Self::Strip>>;

    /// The wire ids of this shard's OWN entities (the snapshot's own
    /// records). Used to keep a migrating entity from appearing twice in
    /// one snapshot — as its own record AND as the neighbor's one-tick-
    /// stale borrowed copy of itself (module docs, "Boundary
    /// visibility"): when both are present, the own record wins.
    fn own_wires(&self, world: &W) -> Vec<u64>;

    // -- The cross-seam surface (`docs/CROSS-SHARD.md` §2–§4). Every
    //    method has a default, so a logic that never looks across the
    //    seam implements nothing new; the single-room actor never calls
    //    any of them. ----------------------------------------------------

    /// Phase 2b on the sharded path: [`GameLogic::ingest`] with the
    /// cross-seam view (read the borrowed strip, emit remote effects).
    /// The actor calls THIS; the default forwards to `ingest`.
    fn ingest_seam(
        &mut self,
        world: &mut W,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        _seam: &mut CrossSeam<'_, Self::Strip>,
    ) {
        self.ingest(world, ctx, actions);
    }

    /// Phase 3 on the sharded path: [`GameLogic::update`] with the
    /// cross-seam view. The actor calls THIS; the default forwards to
    /// `update`.
    fn update_seam(
        &mut self,
        world: &mut W,
        ctx: &TickCtx,
        _seam: &mut CrossSeam<'_, Self::Strip>,
    ) {
        self.update(world, ctx);
    }

    /// Apply one remote effect addressed to an entity of this shard
    /// (CONTROL, after the drain, before any of this tick's input: the
    /// core has already checked the epoch, the age envelope, the
    /// duplicate window and the forwarding table; effects of one tick
    /// arrive here in `(source, origin, seq)` order). `tick` is this
    /// shard's tick — `tick - effect.at_tick` is the effect's age, for
    /// the game's own staleness policy. The seam lets the authority
    /// re-validate against the SOURCE's lent record (game policy) or
    /// answer with an effect of its own.
    ///
    /// Answer [`EffectOutcome::NoTarget`] when `effect.target` is not an
    /// entity of this world. Default: `NoTarget` — a logic that never
    /// emits never receives.
    fn apply_remote_effect(
        &mut self,
        _world: &mut W,
        _tick: u64,
        _effect: &RemoteEffect,
        _seam: &mut CrossSeam<'_, Self::Strip>,
    ) -> EffectOutcome {
        EffectOutcome::NoTarget
    }

    /// The team exchange (`docs/CROSS-SHARD.md` §8b), once per tick in
    /// the TEAMS phase — after BORDER, before the broadcast's snapshot
    /// calls. `borrowed` is the flattened, own-filtered border view the
    /// snapshots receive; `imported` is what the OTHER shards exported
    /// for the teams this shard views (merged per team, TTL-expired).
    /// Return this shard's export — its viewed teams and each team's
    /// visible set here, the records encoded by the game's codec — for
    /// the core to send to the registry hub. The logic keeps whatever it
    /// needs of `imported` for this tick's snapshots (the records are
    /// `Bytes`: cloning one is a reference count).
    ///
    /// Default: `None` — the logic takes no part: it sends nothing and
    /// receives nothing (the hub relays only to shards that list viewed
    /// teams).
    fn team_exchange(
        &mut self,
        _world: &mut W,
        _ctx: &TickCtx,
        _borrowed: &[BorderRecord<Self::Strip>],
        _imported: &TeamImports,
    ) -> Option<TeamExport> {
        None
    }
}

// ---------------------------------------------------------------------
// The ShardLink seam (`docs/DISTRIBUTED.md` §3): everything shard↔
// neighbor crosses this interface. Today's only implementation is
// in-process; the Ipc/Net links arrive as new types behind the same two
// methods when their §10 triggers fire.
// ---------------------------------------------------------------------
