//! [`ArenaGame`] — the arena's kit hooks (KIT-ARCHITECTURE §4.3/§4.6):
//! [`Game`] (`MoveTo` input, the movement system, a bot that retreats
//! to base) and [`TeamGame`] (round-robin team assignment by join
//! order, and the spawn at the team's base).
//!
//! **Team assignment: round-robin by join order** (`n`-th join → team
//! `n mod teams`). Team sizes never differ by more than one over a
//! match's joins, whatever the transport session ids are, and the order
//! is room-local and deterministic. Rejected:
//! - *transport id modulo* (the 2D demo's parity rule): session ids are
//!   server-global, so one room's joiners can all share a residue and
//!   land on one team;
//! - *fill the smallest team*: balances after leaves too, but a leave
//!   in the arena is a disconnect that PARKS the unit (the kit's
//!   park/resume keeps the entity and its team), so the roster the
//!   rule would rebalance barely moves — not worth a world scan per
//!   join.
//!
//! **The team is decided AT the spawn.** An arena spawns a unit AT ITS
//! TEAM'S BASE — the spawn point depends on the team — so the arena
//! overrides the kit's `TeamGame::spawn_team_player`: one step chooses
//! the team and spawns at its base, and the kit records the team as the
//! unit's `TeamMember` (the bot reads it back to find the base). Phase 3
//! had to decide in `spawn_player` and keep a duplicate team component
//! for `team_of` to read back — the kit asked for the team after the
//! spawn (`docs/KIT-ARCHITECTURE.md` §10, finding A1).
//!
//! **The client learns its team from the wire**: the session's first
//! private frame carries a `Welcome` (team, team count) in the kit's
//! per-game slot (`Game::session_private`; GAME-MODULE G3-3 — before it
//! a client had to infer its team from where its unit spawned).

use std::collections::HashMap;
use std::f32::consts::TAU;

use bevy_ecs::prelude::{Entity, World};
use bytes::BytesMut;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use gsb_kit::game::{Game, InputSeq, TeamGame};
use gsb_kit::team::{Team, TeamMember};
use prost::Message;

use crate::codec::{ArenaCodec, Cm3};
use crate::components::{DEFAULT_SPEED, MoveTarget3, Pos3, Speed};
use crate::movement::Movement;
use crate::{input, op};

/// The default number of teams: three — the arena is the kit's proof
/// that team fog is not a two-sided concept (§8.4).
pub const DEFAULT_TEAMS: u8 = 3;

/// The team bases sit evenly on a ring of this radius (metres) around
/// the arena's centre, on the floor. With three teams two bases are
/// 43.3 m apart — far outside [`VISION_RADIUS`](crate::VISION_RADIUS),
/// so a fresh spawn sees only its own team.
pub const BASE_RING: f32 = 25.0;

/// Spacing between team-mates spawning at the same base (metres).
const SLOT_SPACING: f32 = 1.5;

/// The arena game: units on a 3D floor with platforms, one per player,
/// moving toward their latest `MoveTo` target.
pub struct ArenaGame {
    /// How many teams share the arena (at least one).
    teams: u8,
    /// Joins so far — the round-robin's position (never reset: a
    /// room-lifetime order).
    joined: u64,
    codec: ArenaCodec,
    movement: Movement,
}

impl ArenaGame {
    /// An arena for `teams` teams (clamped to at least one).
    #[must_use]
    pub fn with_teams(teams: u8) -> Self {
        Self {
            teams: teams.max(1),
            joined: 0,
            codec: ArenaCodec,
            movement: Movement::default(),
        }
    }

    /// The number of teams.
    pub fn teams(&self) -> u8 {
        self.teams
    }

    /// Team `team`'s base: its point on the base ring, on the floor.
    #[must_use]
    pub fn base_of(&self, team: u8) -> Pos3 {
        let angle = TAU * f32::from(team) / f32::from(self.teams);
        Pos3::new(BASE_RING * angle.cos(), 0.0, BASE_RING * angle.sin())
    }
}

impl Default for ArenaGame {
    fn default() -> Self {
        Self::with_teams(DEFAULT_TEAMS)
    }
}

impl Game for ArenaGame {
    type Codec = ArenaCodec;

    const SNAPSHOT_OP: u16 = op::ARENA_SNAPSHOT;
    const PRIVATE_OP: u16 = op::ARENA_PRIVATE;

    fn codec(&self) -> &ArenaCodec {
        &self.codec
    }

    /// The unit of a joiner, at its team's base (the team room calls
    /// [`TeamGame::spawn_team_player`] instead, which this is the first
    /// half of).
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.spawn_team_player(world, conn).0
    }

    /// A bot-fed unit (its player's disconnect grace ran out) retreats
    /// to its base, through the ordinary input path (an unnumbered
    /// `MoveTo`, decoded by [`Game::ingest`] like a client's). Sent only
    /// while the unit is not already heading there.
    fn bot_actions(
        &mut self,
        world: &World,
        _ctx: &TickCtx,
        bots: impl Iterator<Item = (PlayerId, Entity)>,
        out: &mut Vec<Action>,
    ) {
        for (player, entity) in bots {
            let Ok(unit) = world.get_entity(entity) else {
                continue;
            };
            let Some(&TeamMember(Team(team))) = unit.get::<TeamMember>() else {
                continue;
            };
            let base = Cm3::from(self.base_of(team));
            if unit.get::<MoveTarget3>().map(|t| Cm3::from(t.0)) == Some(base) {
                continue; // already retreating
            }
            let msg = crate::arena::MoveTo {
                x: base.x,
                y: base.y,
                z: base.z,
                seq: 0,
            };
            out.push(Action {
                conn: ConnectionId(0), // no transport behind a bot
                player,
                op: op::ARENA_MOVE_TO,
                payload: msg.encode_to_vec().into(),
            });
        }
    }

    fn ingest(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        input::ingest(players, world, actions, seq);
    }

    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.movement.run(world, ctx.dt.as_secs_f32());
    }

    /// The session's [`Welcome`](crate::arena::Welcome): the unit's team
    /// (as the kit recorded it) and the number of teams — what a client
    /// needs to know its side and find its base. A unit without a team
    /// (none in the team room) is told nothing.
    fn session_private(&mut self, world: &World, entity: Entity, out: &mut BytesMut) -> bool {
        let Some(&TeamMember(Team(team))) = world.get::<TeamMember>(entity) else {
            return false;
        };
        let welcome = crate::arena::Welcome {
            team: team.into(),
            teams: self.teams.into(),
        };
        welcome
            .encode(out)
            .expect("protobuf encode into an in-memory buffer failed");
        true
    }
}

/// The team is chosen at the spawn (module docs).
impl TeamGame for ArenaGame {
    /// Round-robin the joiner onto a team (module docs) and spawn its
    /// unit at that team's base, beside the team-mates already there.
    fn spawn_team_player(&mut self, world: &mut World, _conn: ConnectionId) -> (Entity, Team) {
        let teams = u64::from(self.teams);
        let team = (self.joined % teams) as u8;
        let slot = (self.joined / teams) as f32;
        self.joined += 1;
        let mut at = self.base_of(team);
        at.x += SLOT_SPACING * slot;
        let unit = world.spawn((at.clamped(), Speed(DEFAULT_SPEED))).id();
        (unit, Team(team))
    }

    /// The team of `entity`, as the kit recorded it. The kit does not ask
    /// (the arena overrides `spawn_team_player`); the answer stays true
    /// for any caller.
    fn team_of(&mut self, world: &World, _conn: ConnectionId, entity: Entity) -> Team {
        world.get::<TeamMember>(entity).map_or(Team(0), |m| m.0)
    }
}

#[cfg(test)]
mod tests;
