//! [`TeamRoom`]: team fog of war (MOBA-style) game logic for the demo game.
//!
//! ## What it changes (and what it deliberately does not touch)
//!
//! [`OpenRoom`](crate::room::OpenRoom) uses `GroupKey = ()` (one snapshot
//! group per room, everyone sees the whole world) and
//! [`AoiRoom`](crate::aoi::AoiRoom) uses `GroupKey = Cell` (spatial).
//! `TeamRoom` makes the group key the **team identity**: exactly two groups,
//! one per team. This is the proof that grouping is *not* a spatial concept:
//! a connection's group is a function of *who the player is* (game state —
//! the team its entity belongs to, kept in the world as the
//! [`TeamMember`] component), not *where the player is*. Note what this
//! means for the seam: [`GameLogic::group_of`] here **reads the world** —
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
//!   per-connection encoding, and the "2 groups" property (the whole point
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
//! rooms' set unions), so it is computed in [`Self::update`] once per tick
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
//!   (orphan stamping in `update`); neutral entities go to *both* teams.
//! - **Self-contained**: no delta, no history; the per-team ledger compares
//!   exactly the wire content of that team's last emitted snapshot, so an
//!   enemy dropping out of vision *removes its record* from the next
//!   snapshot and the client reads "gone" from the full replacement alone.
//! - **Per-group ledger**: the two teams' ledgers are independent (the
//!   `GameLogic::snapshot` per-group bookkeeping contract); one team's
//!   emission never changes the other team's "unchanged?" answer in the
//!   same tick.

mod logic;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::hash::Hash;

use bevy_ecs::prelude::{Component, Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_ecs::SystemRunner;

use crate::aoi::Cell;
use crate::components::{Position, WireId};

/// A player's team — the team-fog group key. Exactly [`TEAM_COUNT`] teams
/// exist; membership is *game state*, kept in the world as the entity's
/// [`TeamMember`] component (see its docs), never a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Team(pub u8);

/// The number of teams (the demo is a 2-team game).
pub const TEAM_COUNT: u8 = 2;

/// Default vision radius in world units (see module docs).
pub const DEFAULT_VISION_RADIUS: f32 = 25.0;

/// Team membership — **the entity's team, kept in the world as a
/// component**. This is the round's item D: the previous design derived a
/// connection's team from `conn` parity in `group_of`, which meant
/// `group_of` never read the world (the easiest possible proof that
/// grouping is non-spatial). That made the team *unrepresentable as game
/// state* and unchangeable at runtime. Now the team *is* world state:
///
/// - Written exactly once at join, by `on_join` (from the join-time
///   assignment rule, [`team_of`] — conn parity, i.e. "signup order"), on
///   the player's entity.
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
/// broadcast to *both* teams and grants no vision — exactly as before, but
/// now expressed structurally by the component's absence instead of a
/// "not in `player_entity`" check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct TeamMember(pub Team);

/// The join-time team *assignment rule* (the demo: conn parity, i.e.
/// "signup order" — team 0, 1, 0, 1, …). This decides what `on_join`
/// *writes* into the entity's [`TeamMember`]; it is not consulted again
/// afterwards (runtime team changes are component writes, and `group_of`
/// reads the world, not this function).
#[inline]
fn team_of(conn: ConnectionId) -> Team {
    Team((conn.0 % u64::from(TEAM_COUNT)) as u8)
}

/// The (dx, dy) offsets of the 3×3 cell neighborhood. A grid of
/// `vision_radius`-sized cells makes the neighborhood a *superset* of the
/// radius-`vision_radius` disk (a corner cell can hold units up to
/// `vision_radius * √2` away), so the exact squared-distance filter below
/// is the only correctness mechanism.
const VISION_OFFSETS: [(i32, i32); 9] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (0, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// The grid cell containing `pos`, for a grid of `cell_size` (world units).
#[inline]
fn grid_cell(pos: Position, cell_size: f32) -> Cell {
    Cell(
        (pos.x / cell_size).floor() as i32,
        (pos.y / cell_size).floor() as i32,
    )
}

/// One unit's record in the per-tick cache: wire id, truncated wire
/// coordinates (what the snapshot carries), and the f32 simulation
/// coordinates (what the vision test uses).
type UnitRec = (u64, i32, i32, f32, f32);

/// The team-fog room: two group keys, team-vision content, per-team
/// "no change" ledger.
pub struct TeamRoom {
    runner: SystemRunner,
    /// Which entity belongs to which player (Faz 2: keyed by the STABLE
    /// player identity — the mapping survives resume unchanged).
    player_entity: HashMap<PlayerId, Entity>,
    /// The player-identity counter (the demo's [`PlayerId`] minting
    /// policy); monotonic, never reused within the room's lifetime.
    next_player_id: u64,
    /// The disconnect-park policy + ledger (see `crate::common` and
    /// RECONNECT §3/§9; the hook bodies are shared with every demo room).
    park: crate::common::ParkPolicy,
    park_ledger: HashMap<String, crate::common::ParkEntry>,
    /// The room's single wire-identity counter (mirrors the other rooms).
    next_wire_id: u64,
    /// World units an enemy must be within to be visible to a team (see
    /// module docs, "Vision source model").
    vision_radius: f32,
    /// Half-size of the square spawn map (see `gsb_game::room::spawn_pos`);
    /// configuration, not a strategy decision.
    spawn_half: f32,
    /// Per-team "no change" ledger: `team → (wire id → (x, y))`, the exact
    /// wire content of that team's last emitted snapshot. Keyed by group
    /// (team) per the [`GameLogic::snapshot`] contract: one call must not
    /// change the other team's answer in the same tick.
    last: [HashMap<u64, (i32, i32)>; TEAM_COUNT as usize],
    /// Per-tick cache, rebuilt in [`Self::update`] (each entity exactly
    /// once): per team, its units as `(wire id, truncated x, truncated y,
    /// f32 x, f32 y)`; plus the neutral (ownerless) entities, which go to
    /// *both* teams' snapshots.
    team_units: [Vec<UnitRec>; TEAM_COUNT as usize],
    neutral: Vec<(u64, i32, i32)>,
    /// Per-tick grid cache for the enemy-vision test: `cell → per-team
    /// f32 unit positions`. A *cache*, not the group key (unlike
    /// `AoiRoom`, whose cells *are* the groups).
    cells: HashMap<Cell, [Vec<(f32, f32)>; TEAM_COUNT as usize]>,
    /// Per-tick content, rebuilt in [`Self::update`]: `team →
    /// (wire id → (x, y))` — exactly what that team's snapshot carries
    /// (own team ∪ neutral ∪ in-vision enemies). `snapshot` answers from
    /// this so both teams' snapshots are the *same tick's* state.
    contents: [HashMap<u64, (i32, i32)>; TEAM_COUNT as usize],
    /// Per-player input sequence state (strategy-independent; see
    /// `crate::common::ingest` / `emit_private`).
    input: HashMap<PlayerId, crate::common::InputState>,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `GameLogic::encoded_records`).
    encoded: u64,
}

impl TeamRoom {
    /// Build a team-fog room with the given `vision_radius` (world units)
    /// over the default 100×100 spawn arena. Clamped to a sane minimum so a
    /// degenerate `0` cannot make vision "only the exact same point".
    #[must_use]
    pub fn new(vision_radius: f32) -> Self {
        Self::with_spawn_half(vision_radius, crate::room::DEFAULT_SPAWN_HALF)
    }

    /// Build a team-fog room over a square spawn map of half-size `half`
    /// (see `gsb_game::room::OpenRoom::with_spawn_half`).
    #[must_use]
    pub fn with_spawn_half(vision_radius: f32, half: f32) -> Self {
        Self {
            runner: crate::common::movement_runner(),
            player_entity: HashMap::new(),
            next_player_id: 0,
            park: crate::common::ParkPolicy::default(),
            park_ledger: HashMap::new(),
            next_wire_id: 0,
            vision_radius: vision_radius.max(1.0),
            spawn_half: half.max(1.0),
            last: [HashMap::new(), HashMap::new()],
            team_units: [Vec::new(), Vec::new()],
            neutral: Vec::new(),
            cells: HashMap::new(),
            contents: [HashMap::new(), HashMap::new()],
            input: HashMap::new(),
            encoded: 0,
        }
    }

    /// Set the disconnect-park grace (see
    /// [`crate::room::OpenRoom::with_disconnect_grace`]; RECONNECT §3).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.park.grace = grace;
        self
    }

    /// Rebuild the per-tick caches (module docs): team units, neutrals, the
    /// vision grid, and — the important part — each team's *content*: own
    /// team ∪ neutral ∪ enemy units in team vision.
    fn rebuild(&mut self, world: &mut World) {
        self.team_units[0].clear();
        self.team_units[1].clear();
        self.neutral.clear();
        self.cells.clear();

        // Membership is read from the WORLD (each entity's `TeamMember`
        // component, written at join): no reverse connection map — the
        // component *is* the table, and a runtime team change needs no
        // bookkeeping here at all. An entity without the component is
        // neutral (ownerless): broadcast to ALL teams — the broadcast set
        // stays exactly "has a `Position`".
        let r = self.vision_radius;
        let mut query = world.query::<(&WireId, &Position, Option<&TeamMember>)>();
        for (wire_id, pos, member) in query.iter(world) {
            let (x, y) = (pos.x as i32, pos.y as i32);
            let c = grid_cell(*pos, r);
            let cell = self.cells.entry(c).or_insert([Vec::new(), Vec::new()]);
            match member {
                Some(m) => {
                    let team = m.0.0 as usize;
                    cell[team].push((pos.x, pos.y));
                    self.team_units[team].push((wire_id.get(), x, y, pos.x, pos.y));
                }
                None => {
                    self.neutral.push((wire_id.get(), x, y));
                }
            }
        }
        // Content: own team + neutral, then enemy units in vision.
        self.contents[0].clear();
        self.contents[1].clear();
        for t in 0..TEAM_COUNT as usize {
            for &(id, x, y, _, _) in &self.team_units[t] {
                self.contents[t].insert(id, (x, y));
            }
            for &(id, x, y) in &self.neutral {
                self.contents[t].insert(id, (x, y));
            }
        }
        let r2 = r * r;
        for t in 0..TEAM_COUNT as usize {
            let enemy = 1 - t;
            for &(id, x, y, ex, ey) in &self.team_units[enemy] {
                let c = grid_cell(Position { x: ex, y: ey }, r);
                let mut visible = false;
                'outer: for (dx, dy) in VISION_OFFSETS {
                    let Some(cell_units) = self.cells.get(&Cell(c.0 + dx, c.1 + dy)) else {
                        continue;
                    };
                    for (vx, vy) in &cell_units[t] {
                        let ddx = vx - ex;
                        let ddy = vy - ey;
                        if ddx * ddx + ddy * ddy <= r2 {
                            visible = true;
                            break 'outer;
                        }
                    }
                }
                if visible {
                    self.contents[t].insert(id, (x, y));
                }
            }
        }
    }
}

// Faz 1 trait split: shared hooks on the `GameLogic` supertrait; no
// room-exclusive hook used (empty `RoomLogic` impl at the bottom).
