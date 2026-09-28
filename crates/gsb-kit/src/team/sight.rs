//! Per-unit sight (BACKLOG A8): the [`SightRadius`] component a game
//! puts on a vision source, and the per-tick grid both team rooms test
//! enemy visibility against (a child of the team module).

use std::collections::HashMap;

use bevy_ecs::prelude::Component;

use crate::space::Vision;
use crate::team::Team;

/// A vision source's OWN sight radius, in the vision model's unit —
/// opt-in, the game's to write (a hero sees farther than a minion, a
/// ward sees a small circle). A source without it sees by the model's
/// radius ([`Vision::sees`]); with it, by [`Vision::sees_within`] (the
/// kit's presets clamp it to `[1, MAX_SIGHT_CELLS · radius]` —
/// [`MAX_SIGHT_CELLS`](crate::space::MAX_SIGHT_CELLS)). It matters only
/// on an entity that grants its team sight (a
/// [`TeamMember`](super::TeamMember) with a vision position); the rooms
/// read it on every visibility rebuild, so a write or a change counts
/// from then on. The team composite carries it across a migration
/// ([`TeamMig`](crate::sharded::TeamMig)). Nothing on the wire changes:
/// it decides which records a team receives, never their bytes.
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub struct SightRadius(pub f32);

/// One vision source: its position and its own radius (`None`: the
/// model's).
type Source<P> = (P, Option<f32>);

/// The grid: `(cell, team) →` the team's sources in the cell.
type Cells<V> = HashMap<(<V as Vision>::Cell, Team), Vec<Source<<V as Vision>::Pos>>>;

/// The per-tick grid of a room's vision sources: `(cell, team) → the
/// team's sources in the cell`, and per team the largest
/// [`SightRadius`] among its sources this tick. A cache, not a group
/// key.
///
/// **Cost.** A team none of whose sources carries a radius is tested
/// exactly as before A8: the model's [`Vision::neighborhood`] (the
/// presets' 3×3 / 27 cells) and [`Vision::sees`]. A team with one reads
/// [`Vision::neighborhood_within`] its largest radius — per target,
/// `(2k + 1)²` cells in 2D (`(2k + 1)³` in 3D), `k = ⌈reach / cell⌉ ≤
/// MAX_SIGHT_CELLS` — and each candidate source by its own radius. The
/// reach is per team: one team's hero never widens another team's test.
pub(crate) struct SightGrid<V: Vision> {
    cells: Cells<V>,
    /// Per team (by number): its largest own radius this tick; `None`
    /// (or no slot): none of its sources carries one.
    reach: Vec<Option<f32>>,
}

impl<V: Vision> Default for SightGrid<V> {
    fn default() -> Self {
        Self {
            cells: HashMap::new(),
            reach: Vec::new(),
        }
    }
}

impl<V: Vision> SightGrid<V> {
    /// Forget the previous tick's sources (the allocations stay).
    pub(crate) fn clear(&mut self) {
        self.cells.clear();
        self.reach.clear();
    }

    /// Add a source of `team` at `pos`, with its own radius if it has
    /// one.
    pub(crate) fn add(&mut self, vision: &V, team: Team, pos: V::Pos, sight: Option<SightRadius>) {
        let sight = sight.map(|s| s.0);
        if let Some(r) = sight {
            let t = usize::from(team.0);
            if self.reach.len() <= t {
                self.reach.resize(t + 1, None);
            }
            // `f32::max` skips a NaN: the preset clamps what is left.
            let slot = &mut self.reach[t];
            *slot = Some(slot.map_or(r, |m| m.max(r)));
        }
        self.cells
            .entry((vision.cell(&pos), team))
            .or_default()
            .push((pos, sight));
    }

    /// Whether any source of `team` sees `target`.
    pub(crate) fn sees(&self, vision: &V, team: Team, target: &V::Pos) -> bool {
        let hit = |cell: V::Cell| {
            self.cells.get(&(cell, team)).is_some_and(|sources| {
                sources.iter().any(|(viewer, sight)| match sight {
                    None => vision.sees(viewer, target),
                    Some(r) => vision.sees_within(viewer, *r, target),
                })
            })
        };
        let cell = vision.cell(target);
        match self.reach.get(usize::from(team.0)).copied().flatten() {
            None => vision.neighborhood(cell).any(hit),
            Some(reach) => vision.neighborhood_within(cell, reach).any(hit),
        }
    }
}
