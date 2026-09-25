//! [`TeamRoom`]: team fog of war (MOBA-style), over any [`TeamGame`].
//!
//! ## What it changes (and what it deliberately does not touch)
//!
//! [`OpenRoom`](crate::room::OpenRoom) uses `GroupKey = ()` (one snapshot
//! group per room, everyone sees the whole world) and
//! [`AoiRoom`](crate::aoi::AoiRoom) uses `GroupKey = Cell` (spatial).
//! `TeamRoom` makes the group key the **team identity**: one group per
//! team, as many teams as the game assigns (KIT-ARCHITECTURE §8.4 — the
//! count used to be fixed at two). This is the proof that grouping is *not* a spatial concept:
//! a connection's group is a function of *who the player is* (game state —
//! the team its entity belongs to, kept in the world as the
//! [`TeamMember`] component), not *where the player is*. Note what this
//! means for the seam: [`GameLogic::group_of`](gsb_core::room::GameLogic::group_of) here **reads the world** —
//! exactly like `AoiRoom`'s does — but it reads a *game-state* component
//! instead of a position, and the core's group machinery (per-group
//! snapshot, per-group ledger, per-tick re-evaluation) treats the two
//! identically. Team membership is game state that *happens to live in the
//! world*; grouping is not spatial, and no hidden "group = position"
//! assumption remains in the design (see `docs/ROADMAP.md`, item D).
//! The rest of the core is untouched; only the key and the content of each
//! group's snapshot change. **No `gsb-core` change.**
//!
//! ## Visibility set: own team + enemy units in team vision
//!
//! Team `T`'s snapshot contains:
//!
//! - **every** entity of `T` (own-team visibility has no range limit),
//! - **every neutral entity** (an entity with a `Position` but no connection
//!   — bullets, wards, traps — has no team and is broadcast to *all* teams;
//!   this is what keeps the broadcast set exactly "has a `Position`",
//!   structurally, like in `OpenRoom`/`AoiRoom`),
//! - an enemy entity **only if** at least one of `T`'s own units is within
//!   `vision_radius` of it.
//!
//! **Vision source model (the design decision).** Every player unit is a
//! vision source with the same uniform radius (`vision_radius`,
//! configurable, default 25 world units). An enemy is visible to `T` iff it
//! is within radius of *any* of `T`'s units. Considered and rejected:
//!
//! - *Only designated "ward" entities grant vision (or per-unit radius
//!   components)* — would require new components (`Team` + `VisionRadius`
//!   on non-player entities), which this round's frame forbids (all three
//!   strategies run the *same* game: same components, same movement, same
//!   wire format). With the fixed component set, "every unit sees" is the
//!   only expressible model; team-owned projectiles/wards would need the
//!   `Team` component and are a future game extension, not part of this
//!   seam.
//! - *Per-connection groups (`GroupKey = (Team, ConnectionId)`)* — would
//!   make every connection its own snapshot group: per-connection payloads,
//!   per-connection encoding, and the "one group per team" property (the whole point
//!   of the comparison) would be gone. Rejected.
//! - *Enemy entities grant vision for their own team only* — that is what
//!   own-team visibility already is; the spec's rule is that the *enemy's*
//!   package is what gets gated by vision, and gating must use *the gating
//!   team's* sources.
//!
//! The radius is uniform because the component set has no per-unit vision
//! field; it is configuration (like the AOI cell size) because the right
//! value is a game-design knob, not an architectural constant.
//!
//! ## The cost of team vision (why the cache exists)
//!
//! Enemy visibility is a *distance* test (unlike the spatial and PVS
//! rooms' set unions), so it is computed in `update` once per tick
//! and cached: the world is stable during the broadcast phase, and both
//! `snapshot` calls (one per team, unspecified order) must answer from the
//! same tick's state (the "same tick" cheat-test guarantee — an enemy is in
//! exactly the packages the cache says it is in, no more).
//!
//! Cost: for each enemy entity, a 3×3 neighborhood query on a grid of
//! `vision_radius`-sized cells (the same cell trick `AoiRoom` uses
//! internally, but here the cells are a *cache*, not the group key), then
//! an exact squared-distance filter against the candidate cells' units of
//! the other team. Sparse layouts make the neighborhood cheap; a fully
//! clustered layout degrades toward O(N²) (measured in the load test — see
//! `docs/ROADMAP.md`).
//!
//! ## Invariants preserved (see `tests/team.rs` and the inline tests)
//!
//! - **Identity**: the wire id is minted once (`on_join` / orphan stamp in
//!   `update`) and never changes; an enemy entering or leaving vision keeps
//!   it (the visibility transition is not an identity transition —
//!   previously untested, now pinned).
//! - **Cheat test**: what is absent from a team's package never reaches
//!   that team's clients — the *content* of the per-connection batches is
//!   asserted, not just a bandwidth number.
//! - **Broadcast set**: the broadcast set is exactly "has a `Position`"
//!   (orphan stamping in `update`); neutral entities go to *every* team.
//! - **Full mode (the default) is self-contained**: no delta, no history;
//!   the per-team ledger compares exactly the wire content of that team's
//!   last emitted snapshot, so an enemy dropping out of vision *removes
//!   its record* from the next snapshot and the client reads "gone" from
//!   the full replacement alone.
//! - **Delta mode ([`TeamRoom::with_delta`])**: the same content, shipped
//!   as the change since the team's last frame (`removed` + upserts) with
//!   fulls for a fresh team, on the keep-alive cadence and one-shot to a
//!   member without a baseline — the AOI room's envelope and client
//!   rules; see `frames` for the design.
//! - **Per-group ledger**: the teams' ledgers are independent (the
//!   `GameLogic::snapshot` per-group bookkeeping contract); one team's
//!   emission never changes the other team's "unchanged?" answer in the
//!   same tick.

mod build;
mod content;
mod frames;
mod logic;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use bevy_ecs::prelude::{Component, Entity};
use gsb_core::id::PlayerId;

use crate::codec::RecordCodec;
use crate::common::{Baselines, InputSeq, ParkEntry, ParkPolicy, SetLedger};
use crate::game::{Game, TeamGame, Wire};
use crate::identity::Minter;
use crate::space::Vision;

/// A player's team — the team-fog group key. How many teams exist is
/// the game's assignment policy ([`TeamGame::team_of`]); membership is
/// *game state*, kept in the world as the entity's [`TeamMember`]
/// component (see its docs), never a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Team(pub u8);

/// Default vision radius in world units (see module docs).
pub const DEFAULT_VISION_RADIUS: f32 = 25.0;

/// Team membership — **the entity's team, kept in the world as a
/// component**. This is the round's item D: the previous design derived a
/// connection's team from `conn` parity in `group_of`, which meant
/// `group_of` never read the world (the easiest possible proof that
/// grouping is non-spatial). That made the team *unrepresentable as game
/// state* and unchangeable at runtime. Now the team *is* world state:
///
/// - Written exactly once at join, by `on_join` (from the game's
///   join-time assignment rule, [`TeamGame::team_of`] — the demo: conn
///   parity, i.e. "signup order"), on the player's entity.
/// - Read by `group_of` (the connection's snapshot group) and by `rebuild`
///   (own-team visibility + who grants vision for the team), both of which
///   now look the entity up and read this component off the **world**.
/// - Changed at runtime by a plain component write; on the next tick's
///   group re-evaluation the connection's group follows, and the wire
///   identity is untouched (a group transition is not an identity
///   transition — pinned by
///   `runtime_team_change_moves_the_group_and_keeps_the_wire_identity`).
///
/// A neutral (ownerless) entity simply has no `TeamMember`; it is
/// broadcast to *every* team and grants no vision — expressed
/// structurally by the component's absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct TeamMember(pub Team);

/// The game's broadcast marker (the codec's `Marker`).
type Marker<G> = <<G as Game>::Codec as RecordCodec>::Marker;
/// The game's record query (the codec's `Query`).
type RecordQuery<G> = <<G as Game>::Codec as RecordCodec>::Query;

/// One unit's record in the per-tick cache: wire id, wire value (what
/// the snapshot carries), and the vision position (what the vision test
/// uses; `None` = the entity has no vision position: it grants no
/// vision and no enemy ever sees it).
type UnitRec<W, P> = (u64, W, Option<P>);

/// The team-fog room: one group per team, team-vision content, per-team
/// "no change" ledger. Generic over the game (`G`, which assigns the
/// teams — [`TeamGame`]) and the vision model (`V`, [`Vision`]; the
/// kit's 2D preset is [`VisionGrid2`](crate::space::VisionGrid2)).
pub struct TeamRoom<G: TeamGame, V: Vision> {
    /// The game (its hooks, its codec and its own state).
    game: G,
    /// The vision model (the demo: a uniform radius on the ground plane
    /// — see module docs, "Vision source model").
    vision: V,
    /// Which entity belongs to which player (Faz 2: keyed by the STABLE
    /// player identity — the mapping survives resume unchanged).
    player_entity: HashMap<PlayerId, Entity>,
    /// The player-identity counter (the room's [`PlayerId`] minting
    /// policy); monotonic, never reused within the room's lifetime.
    next_player_id: u64,
    /// The disconnect-park policy + ledger (see `crate::common` and
    /// RECONNECT §3/§9; the hook bodies are shared with every room).
    park: ParkPolicy,
    park_ledger: HashMap<String, ParkEntry>,
    /// The room's single wire-identity counter (mirrors the other rooms).
    minter: Minter,
    /// The per-team tables below are indexed by the team number and
    /// grow to the highest team the world has shown (never shrink — a
    /// team slot is a few empty containers).
    ///
    /// Per-team ledger (the shared set-content delta engine,
    /// `crate::common::SetLedger`): the exact wire content of that
    /// team's last emitted snapshot — the "no change" test of the full
    /// mode, the delta baseline of the delta mode. Keyed by group (team)
    /// per the [`GameLogic::snapshot`] contract: one call must not change
    /// another team's answer in the same tick.
    ledgers: Vec<SetLedger<Wire<G>>>,
    /// Delta mode ([`Self::with_delta`]); `false` = full frames only.
    delta: bool,
    /// Delta mode: which team's view each player's session holds a
    /// baseline for (the one-shot private full's decision).
    baselines: Baselines<Team>,
    /// The room's step counter (one per `update`): the ledgers' notion of
    /// "the previous step" (a room may step every k-th global tick, so
    /// the tick index cannot say it).
    step: u64,
    /// The global tick of the current step (set in `update`): the
    /// `private` seam has no `TickCtx`.
    tick: u64,
    /// Per-tick cache, rebuilt in [`Self::update`] (each entity exactly
    /// once): per team, its units; plus the neutral (ownerless)
    /// entities, which go to *every* team's snapshot.
    team_units: Vec<Vec<UnitRec<Wire<G>, V::Pos>>>,
    neutral: Vec<(u64, Wire<G>)>,
    /// Per-tick grid cache for the enemy-vision test: `(cell, team) →
    /// that team's unit positions in the cell`. A *cache*, not the group
    /// key (unlike `AoiRoom`, whose cells *are* the groups).
    cells: HashMap<(V::Cell, Team), Vec<V::Pos>>,
    /// Per-tick content, rebuilt in [`Self::update`]: `team →
    /// (wire id → wire value)` — exactly what that team's snapshot
    /// carries (own team ∪ neutral ∪ in-vision enemies). `snapshot`
    /// answers from this so every team's snapshot is the *same tick's*
    /// state.
    contents: Vec<HashMap<u64, Wire<G>>>,
    /// Per-player input sequence state (strategy-independent; see
    /// `crate::common::emit_private`).
    input: InputSeq,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `GameLogic::encoded_records`).
    encoded: u64,
}

// Faz 1 trait split: shared hooks on the `GameLogic` supertrait; no
// room-exclusive hook used (empty `RoomLogic` impl at the bottom).
