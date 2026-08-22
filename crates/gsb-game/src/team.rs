//! [`TeamRoom`]: team fog of war (MOBA-style) [`RoomLogic`] for the demo game.
//!
//! ## What it changes (and what it deliberately does not touch)
//!
//! [`DemoRoom`](crate::room::DemoRoom) uses `GroupKey = ()` (one snapshot
//! group per room, everyone sees the whole world) and
//! [`AoiRoom`](crate::aoi::AoiRoom) uses `GroupKey = Cell` (spatial).
//! `TeamRoom` makes the group key the **team identity**: exactly two groups,
//! one per team. This is the proof that grouping is *not* a spatial concept:
//! a connection's group is a function of *who the player is* (game state —
//! the team its entity belongs to, kept in the world as the
//! [`TeamMember`] component), not *where the player is*. Note what this
//! means for the seam: [`RoomLogic::group_of`] here **reads the world** —
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
//!   structurally, like in `DemoRoom`/`AoiRoom`),
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
//!   `RoomLogic::snapshot` per-group bookkeeping contract); one team's
//!   emission never changes the other team's "unchanged?" answer in the
//!   same tick.

use std::collections::HashMap;
use std::hash::Hash;

use bevy_ecs::prelude::{Component, Entity, World};
use gsb_core::id::{ConnectionId, EntityId};
use gsb_core::room::{Action, RoomLogic, TickCtx};
use gsb_ecs::SystemRunner;
use prost::Message;

use crate::aoi::Cell;
use crate::components::{Position, WireId};
use crate::op;

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
/// "not in `conn_entity`" check.
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
    (-1, -1), (0, -1), (1, -1),
    (-1, 0), (0, 0), (1, 0),
    (-1, 1), (0, 1), (1, 1),
];

/// The grid cell containing `pos`, for a grid of `cell_size` (world units).
#[inline]
fn grid_cell(pos: Position, cell_size: f32) -> Cell {
    Cell((pos.x / cell_size).floor() as i32, (pos.y / cell_size).floor() as i32)
}

/// One unit's record in the per-tick cache: wire id, truncated wire
/// coordinates (what the snapshot carries), and the f32 simulation
/// coordinates (what the vision test uses).
type UnitRec = (u64, i32, i32, f32, f32);

/// The team-fog room: two group keys, team-vision content, per-team
/// "no change" ledger.
pub struct TeamRoom {
    runner: SystemRunner,
    /// Which entity belongs to which connection.
    conn_entity: HashMap<ConnectionId, Entity>,
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
    /// (team) per the [`RoomLogic::snapshot`] contract: one call must not
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
    /// Per-connection input sequence state (strategy-independent; see
    /// `crate::common::ingest` / `emit_ack`).
    input: HashMap<ConnectionId, crate::common::InputState>,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `RoomLogic::encoded_records`).
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
    /// (see `gsb_game::room::DemoRoom::with_spawn_half`).
    #[must_use]
    pub fn with_spawn_half(vision_radius: f32, half: f32) -> Self {
        Self {
            runner: crate::common::movement_runner(),
            conn_entity: HashMap::new(),
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
                    let team = m.0 .0 as usize;
                    cell[team].push((pos.x, pos.y));
                    self.team_units[team]
                        .push((wire_id.get(), x, y, pos.x, pos.y));
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
                let c = grid_cell(
                    Position { x: ex, y: ey },
                    r,
                );
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

impl RoomLogic<World> for TeamRoom {
    type GroupKey = Team;

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    /// The connection's group is its team — game state kept in the world
    /// (the entity's [`TeamMember`] component). This room's `group_of`
    /// reads the world, just like `AoiRoom`'s reads `Position`; the
    /// difference is *what* it reads (a membership component, not a
    /// position), which is what keeps the group key non-spatial. A
    /// connection in the room always has an entity with a `TeamMember`
    /// (written in `on_join`, removed with the entity on leave); the
    /// `unwrap_or` fallback only keeps the function total for bookkeeping
    /// edges (e.g. a conn evicted between `members` and the call).
    fn group_of(&self, world: &World, conn: ConnectionId) -> Team {
        let Some(&entity) = self.conn_entity.get(&conn) else {
            return Team(0);
        };
        world
            .entity(entity)
            .get::<TeamMember>()
            .map(|m| m.0)
            .unwrap_or(Team(0))
    }

    /// Encode `team`'s snapshot from this tick's content cache (module
    /// docs). Returns `false` when the team's wire content is unchanged
    /// since that team's last emit (per-team ledger; membership and
    /// visibility transitions change the content and flip it).
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        team: &Team,
        out: &mut bytes::BytesMut,
    ) -> bool {
        let t = team.0 as usize;
        let content = &self.contents[t];
        if self.last[t] == *content {
            return false;
        }

        let mut snap = crate::game::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(content.len()),
            removed: Vec::new(),
            cell_exits: Vec::new(),
            delta: false,
        };
        for (&wire_id, &(x, y)) in content {
            snap.entities.push(crate::game::EntityRecord {
                entity: wire_id,
                x,
                y,
            });
        }
        // In-memory encode cannot fail; treat a failure as a bug.
        snap
            .encode(out)
            .expect("protobuf encode into an in-memory buffer failed");

        self.encoded += content.len() as u64;
        self.last[t] = content.clone();
        true
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> EntityId {
        let wire = crate::common::on_join(
            &mut self.conn_entity,
            &mut self.next_wire_id,
            self.spawn_half,
            world,
            conn,
            &mut self.input,
        );
        // Team membership goes into the WORLD (the component), not just
        // this room's bookkeeping: `group_of` and `rebuild` read it from
        // world state, so a runtime team change is a plain component
        // write — no room hook, no protocol op, no bookkeeping to keep in
        // sync.
        let entity = self.conn_entity.get(&conn).copied().expect("inserted above");
        world.entity_mut(entity).insert(TeamMember(team_of(conn)));
        wire
    }

    fn on_leave(&mut self, world: &mut World, conn: ConnectionId) {
        crate::common::on_leave(&mut self.conn_entity, world, conn, &mut self.input)
    }

    fn ingest(&mut self, world: &mut World, _ctx: &TickCtx, actions: &mut Vec<Action>) {
        crate::common::ingest(&self.conn_entity, world, actions, &mut self.input)
    }

    /// The per-connection input acknowledgment (see `DemoRoom::private`).
    fn private(
        &mut self,
        _world: &mut World,
        conn: ConnectionId,
        _group: &Team,
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        crate::common::emit_private(&mut self.input, conn, responses, out)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::common::run_systems(&mut self.runner, world, ctx);

        // Orphan stamping (idempotent, mirrors the other rooms): entities
        // with a `Position` but no `WireId` get the next serial, so the
        // broadcast set is exactly "has a `Position`" — structural, never
        // silently invisible. Done before `rebuild` so freshly-stamped
        // entities are in this tick's content.
        crate::common::stamp_orphans(&mut self.next_wire_id, world);

        self.rebuild(world);
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }
}

#[cfg(test)]
mod tests {
    //! Logic-level team-fog tests (precise, direct `TeamRoom` calls; they
    //! need the room's private bookkeeping, so they live here rather than
    //! in `tests/team.rs`). `vision_radius = 25`.

    use std::collections::BTreeSet;
    use std::time::Duration;

    use bevy_ecs::prelude::World;
    use gsb_core::id::{ConnectionId, RoomId};
    use gsb_core::room::TickCtx;
    use crate::components::{Position, Speed, DEFAULT_SPEED};

    use super::*;

    fn ctx(tick: u64) -> TickCtx {
        TickCtx {
            room: RoomId(1),
            tick,
            dt: Duration::from_secs_f64(1.0 / 30.0),
        }
    }

    /// Join a player (wire id assigned; team = conn % 2) and move its
    /// entity to an exact position. Returns the wire id.
    fn place(world: &mut World, room: &mut TeamRoom, conn: ConnectionId, x: f32, y: f32) -> u64 {
        let wire = room.on_join(world, conn);
        let entity = *room.conn_entity.get(&conn).expect("conn registered");
        world.entity_mut(entity).insert(Position { x, y });
        wire
    }

    fn snap_ids(out: &bytes::BytesMut) -> BTreeSet<u64> {
        crate::game::WorldSnapshot::decode(out.as_ref())
            .expect("snapshot payload")
            .entities
            .iter()
            .map(|e| e.entity)
            .collect()
    }

    /// The spec's cheat test at the logic level (same tick): an enemy
    /// outside team 0's vision is ABSENT from team 0's snapshot while
    /// PRESENT in team 1's snapshot — information not shipped is not
    /// merely hard to decode, it is not in the bytes.
    #[test]
    fn enemy_out_of_vision_absent_in_one_team_present_in_other() {
        let mut world = World::new();
        let mut room = TeamRoom::new(25.0);

        // A: conn 2 → team 0, at (0,0).
        // B: conn 1 → team 1, at (10,0)  (10 < 25: in A's team's vision).
        // C: conn 3 → team 1, at (40,0)  (40 > 25: outside A's team's vision;
        //                                 team 1's own package always has C).
        let a = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
        let b = place(&mut world, &mut room, ConnectionId(1), 10.0, 0.0);
        let c = place(&mut world, &mut room, ConnectionId(3), 40.0, 0.0);
        room.update(&mut world, &ctx(1));

        // Team 0: own team {A} + in-vision enemies {B} → {A, B}; NOT C.
        let mut out0 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &mut out0));
        let t0 = snap_ids(&out0);
        assert!(t0.contains(&a) && t0.contains(&b), "team 0 sees A,B: {t0:?}");
        assert!(!t0.contains(&c), "C is out of team 0's vision: {t0:?}");

        // Team 1 (same tick): own team {B, C} + in-vision enemies {A}
        // (10 < 25) → {A, B, C}. C is in team 1's snapshot on the SAME
        // tick it is absent from team 0's.
        let mut out1 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &mut out1));
        let t1 = snap_ids(&out1);
        assert!(t1.contains(&a) && t1.contains(&b) && t1.contains(&c), "team 1: {t1:?}");
    }

    /// Own team is always visible, with NO range limit (400+ units away),
    /// and the range limit applies only to *enemy* visibility.
    #[test]
    fn own_team_always_visible_regardless_of_distance() {
        let mut world = World::new();
        let mut room = TeamRoom::new(25.0);

        let a = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);       // team 0
        let d = place(&mut world, &mut room, ConnectionId(4), 400.0, 400.0);  // team 0, far
        let b = place(&mut world, &mut room, ConnectionId(1), 10.0, 0.0);     // team 1

        room.update(&mut world, &ctx(1));

        // Team 0's snapshot has both own-team members (D is ~566 from A).
        let mut out0 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &mut out0));
        let t0 = snap_ids(&out0);
        assert!(t0.contains(&a) && t0.contains(&d), "own team is always in the package: {t0:?}");
        assert!(t0.contains(&b), "B is within 25 of A: {t0:?}");

        // Team 1's snapshot has B but NOT D (D is far from every team-1 unit).
        let mut out1 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &mut out1));
        let t1 = snap_ids(&out1);
        assert!(t1.contains(&b), "team 1 sees itself: {t1:?}");
        assert!(!t1.contains(&d), "D is out of team 1's vision: {t1:?}");
    }

    /// The previously untested corner: an enemy entering vision arrives
    /// with the SAME wire identity it already had (identity does not
    /// change on a visibility transition).
    #[test]
    fn enemy_entering_vision_keeps_its_wire_identity() {
        let mut world = World::new();
        let mut room = TeamRoom::new(25.0);

        let a = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);      // team 0
        let b = place(&mut world, &mut room, ConnectionId(1), 100.0, 100.0);  // team 1, far
        room.update(&mut world, &ctx(1));

        // Tick 1: B is in team 1's own package (id known to B's team's
        // clients) and out of team 0's vision.
        let mut out1 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &mut out1));
        let t1 = snap_ids(&out1);
        assert!(t1.contains(&b), "B in own package: {t1:?}");
        let mut out0 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &mut out0));
        assert!(!snap_ids(&out0).contains(&b), "B out of vision: {out0:?}");

        // B moves into A's vision: (5,0), distance 5 < 25.
        let entity_b = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(entity_b).insert(Position { x: 5.0, y: 0.0 });
        room.update(&mut world, &ctx(2));

        let mut out0b = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Team(0), &mut out0b), "vision change re-emits");
        let t0 = snap_ids(&out0b);
        assert!(
            t0.contains(&b),
            "B now in team 0's package — with the SAME wire id it had in \
             tick 1 (identity did not change on the visibility transition): {t0:?}"
        );
        assert!(t0.contains(&a));
    }

    /// An entity leaving vision DROPS OUT of the snapshot, and the
    /// snapshot stays self-contained: the client reads "gone" from the
    /// full replacement alone (no delta, no history, no remove event).
    #[test]
    fn enemy_leaving_vision_drops_from_snapshot_self_contained() {
        let mut world = World::new();
        let mut room = TeamRoom::new(25.0);

        let a = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
        let b = place(&mut world, &mut room, ConnectionId(1), 5.0, 0.0); // in vision
        room.update(&mut world, &ctx(1));

        let mut out0 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &mut out0));
        let t0 = snap_ids(&out0);
        assert!(t0.contains(&a) && t0.contains(&b), "B in vision: {t0:?}");

        // B leaves vision.
        let entity_b = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(entity_b).insert(Position { x: 300.0, y: 300.0 });
        room.update(&mut world, &ctx(2));

        let mut out0b = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Team(0), &mut out0b), "vision change re-emits");
        let snap = crate::game::WorldSnapshot::decode(out0b.as_ref()).expect("snapshot");
        let t0b: BTreeSet<u64> = snap.entities.iter().map(|e| e.entity).collect();
        assert!(!t0b.contains(&b), "B dropped out: {t0b:?}");
        assert_eq!(t0b, [a].into_iter().collect(), "exactly {a} remains — no stale record");
        assert_eq!(snap.sequence, 2, "sequence advanced (order/duplicate-safe)");

        // B's own team still has B (own-team visibility is unconditional).
        let mut out1b = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Team(1), &mut out1b));
        assert!(snap_ids(&out1b).contains(&b), "B still in own package");
    }

    /// Broadcast set (structural, like the other rooms): an entity
    /// spawned with a `Position` but no `WireId` (a bullet/ward — no
    /// connection, hence no team) is stamped in `update` and appears in
    /// BOTH teams' snapshots (neutral ⇒ broadcast to all teams).
    #[test]
    fn neutral_entity_is_broadcast_to_all_teams() {
        let mut world = World::new();
        let mut room = TeamRoom::new(25.0);

        let a = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0); // team 0
        let b = place(&mut world, &mut room, ConnectionId(1), 5.0, 0.0); // team 1, 5 from A
        let _orphan = world
            .spawn((Position { x: 2.0, y: 2.0 }, Speed(DEFAULT_SPEED)))
            .id();
        room.update(&mut world, &ctx(1));

        let mut out0 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &mut out0));
        let t0 = snap_ids(&out0);
        let mut out1 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &mut out1));
        let t1 = snap_ids(&out1);

        // B is within 25 of A, so BOTH teams see both players; the neutral
        // orphan goes to both packages as well.
        assert_eq!(t0, [a, b, 3].into_iter().collect(), "team 0: A, B (in vision), neutral: {t0:?}");
        assert_eq!(t1, [a, b, 3].into_iter().collect(), "team 1: B, A (in vision), neutral: {t1:?}");
        assert!(t0.contains(&3) && t1.contains(&3), "neutral stamped with fresh serial 3");
    }

    /// "No change" contract: with a static world (no movement targets),
    /// each team's second snapshot is silent — and the two teams'
    /// ledgers are independent (one team's emission does not consume the
    /// other team's change).
    #[test]
    fn team_no_change_when_static_and_ledgers_independent() {
        let mut world = World::new();
        let mut room = TeamRoom::new(25.0);

        place(&mut world, &mut room, ConnectionId(2), 7.0, 9.0); // team 0, no MoveTarget
        // Team 1 far away: the teams do NOT see each other's units
        // (193 > 25), so only the mover's team re-emits below.
        place(&mut world, &mut room, ConnectionId(1), 200.0, 0.0); // team 1
        room.update(&mut world, &ctx(1));

        // First emission: both changed (membership).
        let mut o = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &mut o));
        assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &mut o));

        // Second: both silent, regardless of order.
        room.update(&mut world, &ctx(2));
        let mut o2 = bytes::BytesMut::new();
        assert!(!room.snapshot(&mut world, &ctx(2), &Team(1), &mut o2), "team 1 silent");
        assert!(!room.snapshot(&mut world, &ctx(2), &Team(0), &mut o2), "team 0 silent");
        assert!(o2.is_empty(), "no bytes written on silence");

        // A movement in ONE team re-emits that team only.
        let entity = *room.conn_entity.get(&ConnectionId(2)).unwrap();
        world.entity_mut(entity).insert(Position { x: 8.0, y: 19.0 });
        room.update(&mut world, &ctx(3));
        let mut o3 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(3), &Team(0), &mut o3), "mover's team re-emits");
        let mut o4 = bytes::BytesMut::new();
        assert!(!room.snapshot(&mut world, &ctx(3), &Team(1), &mut o4), "other team still silent");
    }

    /// Item D, pinned: team membership is WORLD STATE (the
    /// [`TeamMember`] component), so a connection that changes team at
    /// runtime moves its snapshot group on the next re-evaluation — and
    /// the wire identity survives the transition (a group transition is
    /// not an identity transition, the same invariant the cell-crossing
    /// and vision-transition tests pin for the other strategies).
    ///
    /// Layout (vision radius 25): A(0,0) and A2(30,0) are team 0 (conn
    /// parity); B(100,0) and B2(200,0) are team 1. The teams start out of
    /// each other's vision entirely (A↔B is 100, A2↔B is 70, both > 25),
    /// so before the switch each package is exactly its own team — which
    /// also makes both transitions below visible in the bytes (if A were
    /// already in team 1's vision as an enemy, team 1's package would be
    /// byte-identical before and after: snapshots carry records, not
    /// "role" — the group move still happens, only less visibly).
    /// - Before: team 0 = {A, A2}; team 1 = {B, B2}.
    /// - A switches to team 1 (a plain component write — the game rule
    ///   that causes the switch is outside the seam; a future trade op
    ///   would do exactly this write).
    /// - After: team 1 = own {A, B, B2}; the enemy A2 is 30 from A (> 25)
    ///   and 70 from B ⇒ A *appears* in team 1's package (own-team
    ///   visibility has no range limit) with the SAME wire id minted at
    ///   its join. Team 0 = own {A2}; A is now an enemy 30 from A2
    ///   (> 25) ⇒ A *leaves* team 0's package.
    #[test]
    fn runtime_team_change_moves_the_group_and_keeps_the_wire_identity() {
        let mut world = World::new();
        let mut room = TeamRoom::new(25.0);

        let a = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0); // team 0
        let a2 = place(&mut world, &mut room, ConnectionId(4), 30.0, 0.0); // team 0
        let b = place(&mut world, &mut room, ConnectionId(1), 100.0, 0.0); // team 1
        let b2 = place(&mut world, &mut room, ConnectionId(3), 200.0, 0.0); // team 1
        room.update(&mut world, &ctx(1));

        // Baseline: group_of reads the world's TeamMember (parity at join
        // time), and each package is exactly its own team.
        assert_eq!(room.group_of(&world, ConnectionId(2)), Team(0));
        assert_eq!(room.group_of(&world, ConnectionId(1)), Team(1));
        let mut out0 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &mut out0));
        let t0 = snap_ids(&out0);
        let mut out1 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &mut out1));
        let t1 = snap_ids(&out1);
        assert_eq!(t0, [a, a2].into_iter().collect(), "team 0: {t0:?}");
        assert_eq!(t1, [b, b2].into_iter().collect(), "team 1: {t1:?}");

        // RUNTIME TEAM CHANGE: A (conn 2) switches to team 1. This is a
        // component write on world state — no room hook, no protocol op.
        let entity_a = *room.conn_entity.get(&ConnectionId(2)).unwrap();
        world.entity_mut(entity_a).insert(TeamMember(Team(1)));

        // The connection's group follows on the re-evaluation — the seam
        // question, pinned: group_of reads the world, and the world says
        // team 1 now.
        assert_eq!(
            room.group_of(&world, ConnectionId(2)),
            Team(1),
            "A's group moved with its TeamMember"
        );

        room.update(&mut world, &ctx(2));
        let mut out0b = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Team(0), &mut out0b), "team 0 re-emits");
        let mut out1b = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Team(1), &mut out1b), "team 1 re-emits");
        let t0b = snap_ids(&out0b);
        let t1b = snap_ids(&out1b);

        // A LEFT team 0's package (it is now an enemy 30 from A2, outside
        // team 0's vision): the group transition is visible in the bytes.
        assert_eq!(t0b, [a2].into_iter().collect(), "team 0: A is gone: {t0b:?}");

        // A APPEARED in team 1's package as OWN TEAM — with the SAME wire
        // id it had before the transition (identity did not change on the
        // group transition).
        assert!(
            t1b.contains(&a),
            "A is in team 1's package with the pre-transition wire id: {t1b:?}"
        );
        assert_eq!(t1b, [a, b, b2].into_iter().collect(), "team 1: {t1b:?}");
    }
}
