//! The demo's resolved selection: the three axes decoded from the
//! config, and the room build they map onto. Produced by the resolver
//! in the sibling `select` module.

use crate::config::{Communication, Topology, Visibility};

/// The resolved VISIBILITY axis: within the world, WHO sees WHOM — the
/// group-key choice of the room that will run (`docs/DESIGN.md` §8).
///
/// Deliberately NOT the legacy [`Visibility`] enum: that one fuses two
/// axes (its `"sharded"` spelling is really a [`Topology`] statement), so
/// a resolved selection carrying it could name impossible states (a
/// "sharded group key"). This axis is produced only by
/// [`resolve_selection`](super::resolve_selection); there is no separate TOML key — the
/// legacy `visibility` key doubles as its input encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityAxis {
    /// `GroupKey = ()`: everyone sees the whole world (the baseline).
    All,
    /// `GroupKey = Cell`: spatial AOI, 3×3 cell block (see
    /// `gsb_demo::aoi`).
    Spatial,
    /// `GroupKey = Team`: team fog of war, 2 groups (see
    /// `gsb_demo::team`).
    Team,
    /// `GroupKey = Sector`: static PVS over hand-authored convex sectors
    /// (see `gsb_demo::pvs`).
    Pvs,
}

impl From<Visibility> for VisibilityAxis {
    /// Decode the legacy five-value spelling into the axis. `"sharded"`
    /// folds into [`VisibilityAxis::All`]: by the time this conversion
    /// runs, the topology half of the spelling has already been extracted
    /// (see [`resolve_selection`](super::resolve_selection)). The legacy key cannot name
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
    /// `gsb_demo::room::OpenRoom` — single actor, whole-world groups
    /// (open visibility: everyone sees everything).
    Open,
    /// `gsb_demo::aoi::AoiRoom` — single actor, spatial AOI cells.
    Aoi,
    /// `gsb_demo::team::TeamRoom` — single actor, team fog of war.
    Team,
    /// `gsb_demo::pvs::SectorRoom` — single actor, sector PVS.
    Sector,
    /// `gsb_demo::sharded::ShardedRoom` grid — N shard actors, whole-world
    /// groups per shard (`BuiltRoom::Sharded`).
    Sharded,
    /// `gsb_demo::sharded::ShardedSpatialRoom` grid — the SAME N-actor
    /// topology, but each shard broadcasts with cell-grouped spatial AOI
    /// deltas over its own region (the Faz B composite;
    /// `BuiltRoom::Sharded`, different shard logic).
    ShardedSpatial,
}

/// The fully-resolved three-axis selection: the raw config surface
/// (legacy spellings + explicit keys) reduced to validated axes plus the
/// room build they map onto. Produced ONLY by
/// [`resolve_selection`](super::resolve_selection) — everything downstream of it (the
/// factory pick in `DemoModule::spawn_registry`) reads THIS, never the legacy string, so
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
