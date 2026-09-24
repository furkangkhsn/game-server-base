//! [`ArenaGame`] — the arena's kit hooks (KIT-ARCHITECTURE §4.3/§4.6):
//! [`Game`] (spawn at the team base, `MoveTo` input, the movement
//! system, a bot that retreats to base) and [`TeamGame`] (round-robin
//! team assignment by join order).
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
//! **The team is decided in `spawn_player`, not in `team_of`.** The kit
//! calls `spawn_player` first and `team_of` right after, but an arena
//! spawns a unit AT ITS TEAM'S BASE — the spawn point depends on the
//! team. So the spawn hook makes the decision and records it on the
//! unit ([`HomeBase`]), and `team_of` reads it back. (Recorded as a
//! design finding: `docs/KIT-ARCHITECTURE.md` §10, "Faz 3 sonucu".)

use std::collections::HashMap;
use std::f32::consts::TAU;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use gsb_kit::game::{Game, InputSeq, TeamGame};
use gsb_kit::team::Team;
use prost::Message;

use crate::codec::{ArenaCodec, Cm3};
use crate::components::{DEFAULT_SPEED, HomeBase, MoveTarget3, Pos3, Speed};
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

    /// Round-robin the joiner onto a team (module docs) and spawn its
    /// unit at that team's base, beside the team-mates already there.
    fn spawn_player(&mut self, world: &mut World, _conn: ConnectionId) -> Entity {
        let teams = u64::from(self.teams);
        let team = (self.joined % teams) as u8;
        let slot = (self.joined / teams) as f32;
        self.joined += 1;
        let mut at = self.base_of(team);
        at.x += SLOT_SPACING * slot;
        world
            .spawn((at.clamped(), Speed(DEFAULT_SPEED), HomeBase(team)))
            .id()
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
            let Some(&HomeBase(team)) = unit.get::<HomeBase>() else {
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
}

/// The team the joiner was spawned for (see the module docs: decided
/// by [`Game::spawn_player`], recorded as the unit's [`HomeBase`]).
impl TeamGame for ArenaGame {
    fn team_of(&mut self, world: &World, _conn: ConnectionId, entity: Entity) -> Team {
        Team(world.get::<HomeBase>(entity).map_or(0, |b| b.0))
    }
}

#[cfg(test)]
mod tests;
