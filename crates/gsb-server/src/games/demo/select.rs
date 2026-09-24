//! The demo's three-axis resolver: the config's legacy spellings and
//! explicit axis keys reduced to one validated selection.

use tracing::warn;

use super::axes::{ResolvedSelection, RoomKind, VisibilityAxis};
use crate::config::{Communication, Topology, Visibility};
use crate::{Config, ServerError};

/// Reduce the raw config surface to the validated three-axis selection
/// (topology × visibility × communication — `docs/ROADMAP.md`, P2
/// "Konfigürasyon düzeltmesi", Faz A) and map it onto the room build
/// that will run.
///
/// This is THE gate between "what the operator wrote" and "what will
/// run": the demo module matches on the returned
/// [`ResolvedSelection::kind`] instead of the raw legacy string, so
/// the axes are authoritative and legacy spellings stay input
/// encodings. Derivation + precedence:
///
/// 1. TOPOLOGY — explicit [`Config::topology`] wins; omission derives
///    from the legacy encoding (`visibility = "sharded"` ⇒ sharded).
/// 2. VISIBILITY — decoded from the legacy [`Config::visibility`] key
///    (`"sharded"` folds into `all`; its topology half was taken in
///    step 1).
/// 3. COMMUNICATION — explicit [`Config::communication`] wins; omission
///    derives `spatial ⇒ delta, otherwise always-full` (what today's
///    rooms actually do).
///
/// Combination validation runs on the RESOLVED triple. Only six
/// combinations have an implementation today (single × {all, team,
/// pvs} × always-full, single × spatial × delta, sharded × all ×
/// always-full, and sharded × spatial × delta — the Faz B composite);
/// everything else is rejected HERE with an error naming the roadmap
/// phase/document that will deliver it — a supported-combination check
/// must refuse at startup, never misconfigure a running server.
///
/// The communication axis is therefore FUNCTIONALLY DETERMINED by the
/// visibility axis today, and validation enforces that in both
/// directions rather than letting either spelling drift from the room
/// that runs:
///
/// - under `spatial` (either topology) the room speaks delta and only
///   delta — AoiRoom's internal per-cell diff, the Faz B composite's
///   per-shard cell-delta broadcast — so the derived value and an
///   explicit `delta` agree on one room, and an explicit
///   `always-full` is REJECTED (no spatial room has a full-frame mode);
/// - under `all`/`team`/`pvs` the room speaks full frames only, so an
///   explicit `delta` is REJECTED (client-facing delta packaging waits
///   on the shared codec round).
///
/// The consequence worth stating: no ACCEPTED [`ResolvedSelection`]
/// can report a `communication` its room does not speak.
pub fn resolve_selection(cfg: &Config) -> Result<ResolvedSelection, ServerError> {
    // Stage 1 — TOPOLOGY: explicit key wins over the legacy spelling;
    // a contradiction warns (behavior still follows the explicit key).
    let legacy_sharded = cfg.visibility == Visibility::Sharded;
    let topology = match cfg.topology {
        Some(explicit) => {
            if legacy_sharded && explicit == Topology::Single {
                warn!(
                    resolved = %explicit,
                    "`topology` takes precedence: ignoring the legacy \
                     visibility = \"sharded\" spelling"
                );
            }
            explicit
        }
        None if legacy_sharded => Topology::Sharded,
        None => Topology::Single,
    };

    // Stage 2 — VISIBILITY axis: decode the legacy five-value spelling.
    let visibility = VisibilityAxis::from(cfg.visibility);

    // Stage 3 — COMMUNICATION: explicit key wins over the derived
    // default (the default mirrors what the mapped room does today).
    let derived_communication = match visibility {
        VisibilityAxis::Spatial => Communication::Delta,
        VisibilityAxis::All | VisibilityAxis::Team | VisibilityAxis::Pvs => {
            Communication::AlwaysFull
        }
    };
    let communication = cfg.communication.unwrap_or(derived_communication);

    // Stage 4 — combination validation, structural axes first (they
    // decide what the world IS), then the packaging axis. Each
    // supported mapping names its factory; each REJECTION names the
    // roadmap phase/document that delivers it.
    let kind = match (topology, visibility) {
        (Topology::Single, VisibilityAxis::All) => RoomKind::Open,
        (Topology::Single, VisibilityAxis::Spatial) => RoomKind::Aoi,
        (Topology::Single, VisibilityAxis::Team) => RoomKind::Team,
        (Topology::Single, VisibilityAxis::Pvs) => RoomKind::Sector,
        (Topology::Sharded, VisibilityAxis::All) => RoomKind::Sharded,
        // The Faz B composite: every shard of the grid broadcasts with
        // cell-grouped spatial visibility over its OWN region, the
        // borrowed border strip folded into the per-cell delta ledger.
        (Topology::Sharded, VisibilityAxis::Spatial) => RoomKind::ShardedSpatial,
        // Locality-contrary combos: team/pvs interest reaches across
        // shard seams, which needs a cross-shard subscription layer
        // nobody has built (see docs/CROSS-SHARD.md §4 — interaction
        // designs stay shard-local; docs/DISTRIBUTED.md horizon item).
        (Topology::Sharded, other @ (VisibilityAxis::Team | VisibilityAxis::Pvs)) => {
            return Err(ServerError::ShardedCrossInterest(other.to_string()));
        }
    };

    // An EXPLICIT delta request resolves only where a client-facing
    // delta implementation exists TODAY: `spatial` — on either
    // topology. Under `single` that is AoiRoom's internal per-cell
    // diff; under `sharded` it is the Faz B composite's per-shard
    // cell-delta broadcast (the same wire format). Everywhere else
    // (all/team/pvs) delta frames wait for their packaging: fail
    // cleanly instead of silently serving full frames under a config
    // that asked for deltas.
    if cfg.communication == Some(Communication::Delta) && visibility != VisibilityAxis::Spatial {
        return Err(match topology {
            Topology::Single => ServerError::SingleDelta,
            Topology::Sharded => ServerError::ShardedDelta,
        });
    }

    // The MIRROR of the check above, and for the same reason. Spatial
    // rooms speak delta and only delta: AoiRoom's unit of encoding is
    // the per-cell diff and the Faz B composite's is the per-shard
    // cell delta — neither has a full-frame mode to select (their
    // fulls are the keep-alive / late-join recovery path, not a wire
    // setting). An explicit `always-full` here therefore names
    // packaging nothing serves, exactly as an explicit `delta` does
    // under all/team/pvs. Refusing keeps `ResolvedSelection` truthful
    // BY CONSTRUCTION: no accepted selection can report a
    // communication its room does not speak. An OMITTED key still
    // derives `Delta` above, so a config that never mentioned the axis
    // is untouched.
    if cfg.communication == Some(Communication::AlwaysFull) && visibility == VisibilityAxis::Spatial
    {
        return Err(match topology {
            Topology::Single => ServerError::SingleAlwaysFull,
            Topology::Sharded => ServerError::ShardedAlwaysFull,
        });
    }

    Ok(ResolvedSelection {
        topology,
        visibility,
        communication,
        kind,
    })
}

impl Config {
    /// Compatibility shim (GAME-MODULE §6 decision 1): the demo's
    /// resolver, callable where it always was. The demo module runs the
    /// same function at startup; see [`resolve_selection`].
    pub fn resolve_selection(&self) -> Result<ResolvedSelection, ServerError> {
        resolve_selection(self)
    }
}
