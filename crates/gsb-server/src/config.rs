//! The configuration surface: the three selection axes, the listener
//! grammar, and the validation that refuses an unserveable
//! combination before any socket exists.

mod listeners;
mod resolve;

mod axes;
pub use axes::*;

mod metrics;
pub use metrics::{MetricsConfig, OtlpSection};

mod top_keys;
#[cfg(feature = "game-demo")]
pub(crate) use top_keys::DEMO_KEYS;

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
/// an INPUT ENCODING only: the demo's resolver (`Config::resolve_selection`) decodes it into
/// the authoritative axes ([`Topology`] × `VisibilityAxis` ×
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
    /// [`gsb_demo::aoi`]).
    Spatial,
    /// `GroupKey = Team`: team fog of war (2 groups, team vision; see
    /// [`gsb_demo::team`]).
    Team,
    /// `GroupKey = Sector`: per-map-segment PVS (static visibility table
    /// over hand-authored convex sectors; see [`gsb_demo::pvs`]).
    Pvs,
    /// Grid of shards (see [`gsb_demo::sharded`]): the room is `shard_count`
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
/// authoritative pieces. Orthogonal to WHAT a group is (`VisibilityAxis`)
/// and HOW snapshots are packaged ([`Communication`]).
///
/// A separate key from the legacy [`Visibility`] spelling because
/// `visibility = "sharded"` was never a visibility statement — it changed
/// the ACTOR/OWNERSHIP structure (N shard actors + N worlds instead of one).
/// The new key names that concept directly; the legacy spelling keeps
/// working as an input encoding (see `Config::resolve_selection`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Topology {
    /// ONE room actor over ONE world — every shipped strategy except the
    /// grid (the default, and the behavior of every pre-axes config).
    #[default]
    Single,
    /// The map is cut into a `Config::shard_count`-cell grid; each shard is
    /// its own actor + world with entity migration across the seams (see
    /// `gsb_demo::sharded`). Requires the count to be 1..=256.
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
