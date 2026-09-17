//! The configuration surface: the three selection axes, the listener
//! grammar, and the validation that refuses an unserveable
//! combination before any socket exists.

mod listeners;
mod resolve;

mod axes;
pub use axes::*;

pub(crate) use listeners::*;
pub(crate) use resolve::ListenerSpec;
pub use resolve::{ConfigError, ServerError};

/// The LEGACY config spelling of two of the three selection axes
/// (`docs/ROADMAP.md`, P2 "Konfigürasyon düzeltmesi"): the demo rooms'
/// visibility strategy (config-selectable; all run
/// the SAME game — same components, movement, wire format — and differ
/// only in how the world is partitioned into snapshot groups, see
/// `docs/DESIGN.md` §8).
///
/// WHY it survives unchanged: backward compatibility — every pre-axes
/// config file, test and caller encodes its choice in this one key. It is
/// an INPUT ENCODING only: [`Config::resolve_selection`] decodes it into
/// the authoritative axes ([`Topology`] × [`VisibilityAxis`] ×
/// [`Communication`]) before anything runs, so no factory ever matches on
/// this enum again. Its `"sharded"` variant is really a topology
/// statement, which the decode makes explicit.
///
/// Each variant is a different `RoomLogic` group key, so each needs its
/// own registry instantiation (a `RoomFactory` is generic over the group
/// key — the pick happens at this config boundary, entirely on the
/// server side; `gsb-core` stays generic and untouched).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// `GroupKey = ()`: everyone sees the whole world. The baseline
    /// (per-connection bandwidth O(entities)); the comparison point all
    /// other strategies are measured against.
    All,
    /// `GroupKey = Cell`: spatial AOI (3×3 cell block, see
    /// [`gsb_game::aoi`]).
    Spatial,
    /// `GroupKey = Team`: team fog of war (2 groups, team vision; see
    /// [`gsb_game::team`]).
    Team,
    /// `GroupKey = Sector`: per-map-segment PVS (static visibility table
    /// over hand-authored convex sectors; see [`gsb_game::pvs`]).
    Pvs,
    /// Grid of shards (see [`gsb_game::sharded`]): the room is `shard_count`
    /// actors, each owning a rectangular region of the map, with entity
    /// migration across region boundaries and boundary visibility. This is
    /// a different *topology* (N actors + N worlds), not just a group key,
    /// so it plugs in via `ShardLogic`/`BuiltRoom::Sharded` rather than
    /// `RoomLogic`. Selecting it uses [`Config::shard_count`] (1..=256).
    Sharded,
}

impl Default for Visibility {
    /// Default: `All` — the whole-world baseline. Rationale (see
    /// `docs/ROADMAP.md`, visibility turn): (1) it is the backward-
    /// compatible behavior of every existing config (previously
    /// `aoi = false`); (2) it is the measurement baseline all restricted
    /// strategies are compared against — a restricted default would make
    /// the baseline opt-in; (3) for a new server author, "everyone sees
    /// everything" is the least-surprising starting point: restricting
    /// what clients can see is a *gameplay* decision and should be an
    /// explicit choice, not a default.
    fn default() -> Self {
        Self::All
    }
}

impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::All => "all",
            Self::Spatial => "spatial",
            Self::Team => "team",
            Self::Pvs => "pvs",
            Self::Sharded => "sharded",
        };
        f.write_str(s)
    }
}

/// The TOPOLOGY axis of the three-axis room selection (`docs/ROADMAP.md`,
/// P2 "Konfigürasyon düzeltmesi"): who computes the world, and as how many
/// authoritative pieces. Orthogonal to WHAT a group is ([`VisibilityAxis`])
/// and HOW snapshots are packaged ([`Communication`]).
///
/// A separate key from the legacy [`Visibility`] spelling because
/// `visibility = "sharded"` was never a visibility statement — it changed
/// the ACTOR/OWNERSHIP structure (N shard actors + N worlds instead of one).
/// The new key names that concept directly; the legacy spelling keeps
/// working as an input encoding (see [`Config::resolve_selection`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Topology {
    /// ONE room actor over ONE world — every shipped strategy except the
    /// grid (the default, and the behavior of every pre-axes config).
    #[default]
    Single,
    /// The map is cut into a `Config::shard_count`-cell grid; each shard is
    /// its own actor + world with entity migration across the seams (see
    /// `gsb_game::sharded`). Requires the count to be 1..=256.
    Sharded,
}

impl std::fmt::Display for Topology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Single => "single",
            Self::Sharded => "sharded",
        };
        f.write_str(s)
    }
}

/// The COMMUNICATION axis: how snapshot data is packaged and carried to a
/// client — a full frame every time, or deltas converging on the keepalive
/// full (`docs/ROADMAP.md`, P2; the N-delta + 1-full convergence rule).
///
/// WHY the axis exists even though only one room serves client-facing
/// delta yet: the spatial room already diffs per cell INTERNALLY, so the
/// axis records a real distinction the roadmap generalizes (per-link
/// derivation, Faz C). Under `single × spatial` an explicit request
/// resolves to that same AoiRoom; everywhere else it fails at startup
/// with an error pointing at the phase that will deliver it — a request
/// is never silently downgraded.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Communication {
    /// Every snapshot frame carries the group's full state — what the
    /// `all`, `team` and `pvs` rooms speak on the wire (the default, and
    /// the only packaging those rooms have). NOT selectable under
    /// `spatial`: those rooms have no full-frame mode, so the combination
    /// refuses startup rather than resolving to a room that deltas.
    #[default]
    AlwaysFull,
    /// Client-facing delta frames with periodic/full keepalive
    /// convergence. Served today by `spatial` on EITHER topology — the
    /// single-world AoiRoom's internal per-cell diff, and the Faz B
    /// composite's per-shard cell-delta broadcast (`sharded × spatial`,
    /// same wire format). Every other combination refuses startup with an
    /// error naming the roadmap phase that will deliver it.
    Delta,
}

impl std::fmt::Display for Communication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::AlwaysFull => "always-full",
            Self::Delta => "delta",
        };
        f.write_str(s)
    }
}

/// The resolved VISIBILITY axis: within the world, WHO sees WHOM — the
/// group-key choice of the room that will run (`docs/DESIGN.md` §8).
///
/// Deliberately NOT the legacy [`Visibility`] enum: that one fuses two
/// axes (its `"sharded"` spelling is really a [`Topology`] statement), so
/// a resolved selection carrying it could name impossible states (a
/// "sharded group key"). This axis is produced only by
/// [`Config::resolve_selection`]; there is no separate TOML key — the
/// legacy `visibility` key doubles as its input encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityAxis {
    /// `GroupKey = ()`: everyone sees the whole world (the baseline).
    All,
    /// `GroupKey = Cell`: spatial AOI, 3×3 cell block (see
    /// `gsb_game::aoi`).
    Spatial,
    /// `GroupKey = Team`: team fog of war, 2 groups (see
    /// `gsb_game::team`).
    Team,
    /// `GroupKey = Sector`: static PVS over hand-authored convex sectors
    /// (see `gsb_game::pvs`).
    Pvs,
}

impl From<Visibility> for VisibilityAxis {
    /// Decode the legacy five-value spelling into the axis. `"sharded"`
    /// folds into [`VisibilityAxis::All`]: by the time this conversion
    /// runs, the topology half of the spelling has already been extracted
    /// (see [`Config::resolve_selection`]). The legacy key cannot name
    /// "cells within a shard" — an operator who wants that writes the
    /// explicit `topology = "sharded"` next to `visibility = "spatial"`.
    fn from(legacy: Visibility) -> Self {
        match legacy {
            Visibility::All | Visibility::Sharded => Self::All,
            Visibility::Spatial => Self::Spatial,
            Visibility::Team => Self::Team,
            Visibility::Pvs => Self::Pvs,
        }
    }
}

impl std::fmt::Display for VisibilityAxis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::All => "all",
            Self::Spatial => "spatial",
            Self::Team => "team",
            Self::Pvs => "pvs",
        };
        f.write_str(s)
    }
}

/// The concrete room build a validated selection maps onto: one variant
/// per factory that exists TODAY (no new strategies — Faz A only re-expresses
/// the old surface). Exposed so tests and tooling can assert WHICH room a
/// config resolves to without starting a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomKind {
    /// `gsb_game::room::OpenRoom` — single actor, whole-world groups
    /// (open visibility: everyone sees everything).
    Open,
    /// `gsb_game::aoi::AoiRoom` — single actor, spatial AOI cells.
    Aoi,
    /// `gsb_game::team::TeamRoom` — single actor, team fog of war.
    Team,
    /// `gsb_game::pvs::SectorRoom` — single actor, sector PVS.
    Sector,
    /// `gsb_game::sharded::ShardedRoom` grid — N shard actors, whole-world
    /// groups per shard (`BuiltRoom::Sharded`).
    Sharded,
    /// `gsb_game::sharded::ShardedSpatialRoom` grid — the SAME N-actor
    /// topology, but each shard broadcasts with cell-grouped spatial AOI
    /// deltas over its own region (the Faz B composite;
    /// `BuiltRoom::Sharded`, different shard logic).
    ShardedSpatial,
}

/// The fully-resolved three-axis selection: the raw config surface
/// (legacy spellings + explicit keys) reduced to validated axes plus the
/// room build they map onto. Produced ONLY by
/// [`Config::resolve_selection`] — everything downstream of it (the
/// factory pick in `start_inner`) reads THIS, never the legacy string, so
/// the axes are authoritative and the old spellings are mere encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSelection {
    /// Who computes the world.
    pub topology: Topology,
    /// Who sees whom within it.
    pub visibility: VisibilityAxis,
    /// How snapshots are packaged for clients.
    pub communication: Communication,
    /// The existing factory this triple resolves to.
    pub kind: RoomKind,
}
