//! "Cephe" in miniature — the actor tests' game: the fixture game whose
//! players spawn where (and on the team) their identity says, and move
//! by one input (a teleport); another input asks the game to kick the
//! sender (E8). Nothing else moves.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};

use crate::common::InputSeq;
use crate::game::{Game, ShardGame, TeamGame};
use crate::team::{SightRadius, Team};
use crate::testing::{DEFAULT_SPEED, FixCodec, FixMig, Fixture, Position, Speed};

/// The teleport input: `x, y` as two little-endian `f32`s.
pub(super) const MOVE: u16 = 1950;
/// "Kick me": the game kicks the sender through the kit's verb
/// ([`crate::game::kick`], E8).
pub(super) const KICK: u16 = 1951;

/// The fixture with identity spawns and the teleport input.
#[derive(Default)]
pub(super) struct Front(Fixture);

/// An identity is `team:x:y` (e.g. `1:-40:-50`), optionally with the
/// unit's own sight radius (A8): `team:x:y:sight`.
pub(super) fn parse(identity: &str) -> (u8, f32, f32) {
    let mut parts = identity.split(':');
    let mut next = || parts.next().expect("team:x:y");
    let team = next().parse().expect("team");
    let x = next().parse().expect("x");
    let y = next().parse().expect("y");
    (team, x, y)
}

/// The identity's own sight radius, if it names one.
fn sight(identity: &str) -> Option<SightRadius> {
    let r = identity.split(':').nth(3)?;
    Some(SightRadius(r.parse().expect("sight")))
}

/// The teleport payload.
pub(super) fn to(x: f32, y: f32) -> bytes::Bytes {
    let mut b = x.to_le_bytes().to_vec();
    b.extend_from_slice(&y.to_le_bytes());
    b.into()
}

impl Game for Front {
    type Codec = FixCodec;

    const SNAPSHOT_OP: u16 = <Fixture as Game>::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = <Fixture as Game>::PRIVATE_OP;

    fn codec(&self) -> &FixCodec {
        self.0.codec()
    }

    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.0.spawn_player(world, conn)
    }

    fn ingest(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        _seq: &mut InputSeq,
    ) {
        for a in actions.drain(..) {
            if a.op == KICK
                && let Some(&e) = players.get(&a.player)
            {
                crate::game::kick(world, e, "asked to leave");
                continue;
            }
            if a.op != MOVE || a.payload.len() != 8 {
                continue;
            }
            let x = f32::from_le_bytes(a.payload[..4].try_into().expect("4 bytes"));
            let y = f32::from_le_bytes(a.payload[4..].try_into().expect("4 bytes"));
            if let Some(&e) = players.get(&a.player) {
                world.entity_mut(e).insert(Position { x, y });
            }
        }
    }

    fn systems(&mut self, _world: &mut World, _ctx: &TickCtx) {}
}

impl TeamGame for Front {
    fn team_of(&mut self, _world: &World, _conn: ConnectionId, _entity: Entity) -> Team {
        Team(0)
    }

    /// The saved character: its team, its spot and its own sight radius
    /// (if any), from the identity.
    fn spawn_team_player_as(
        &mut self,
        world: &mut World,
        _conn: ConnectionId,
        identity: &str,
    ) -> (Entity, Team) {
        let (team, x, y) = parse(identity);
        let e = world.spawn((Position { x, y }, Speed(DEFAULT_SPEED))).id();
        if let Some(sight) = sight(identity) {
            world.entity_mut(e).insert(sight);
        }
        (e, Team(team))
    }
}

impl ShardGame for Front {
    type Mig = FixMig;

    fn capture(&self, world: &World, entity: Entity) -> FixMig {
        self.0.capture(world, entity)
    }

    fn restore(&mut self, world: &mut World, mig: FixMig) -> Entity {
        self.0.restore(world, mig)
    }
}
