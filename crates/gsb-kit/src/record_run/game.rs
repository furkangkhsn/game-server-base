//! The twin rooms' game: the fixture game over a codec `C` — the
//! fixture's own ([`FixCodec`], `entities`) or its record-run twin
//! ([`PackedCodec`]) — with identity spawns, two inputs and wandering
//! NPCs, so the rooms see joins, leaves, walks, team changes, spawns,
//! moves and despawns without a test touching the world.

use std::collections::HashMap;

use bevy_ecs::prelude::{Changed, Component, Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};

use crate::codec::RecordCodec;
use crate::common::InputSeq;
use crate::game::{Game, ShardGame, TeamGame};
use crate::team::{Team, TeamMember};
use crate::testing::{DEFAULT_SPEED, FixMig, Fixture, Position, Speed, WirePos};

/// The teleport input: `x, y` as two little-endian `f32`s.
pub(super) const MOVE: u16 = 1960;
/// The team-change input: one byte, the new team.
pub(super) const SWITCH: u16 = 1961;

/// A codec over the fixture's record (either framing).
pub(super) trait FixLike:
    RecordCodec<
        Marker = Position,
        Query = &'static Position,
        Dirty = Changed<Position>,
        Wire = WirePos,
    > + Default
{
}

impl<C> FixLike for C where
    C: RecordCodec<
            Marker = Position,
            Query = &'static Position,
            Dirty = Changed<Position>,
            Wire = WirePos,
        > + Default
{
}

/// An NPC the game spawned on tick `born`.
#[derive(Debug, Clone, Copy, Component)]
pub(super) struct Npc {
    born: u64,
}

/// The game (module docs). `anchor` is where its NPCs wander: a shard
/// passes a spot inside its own region, near a seam, so its NPCs stay
/// its own and show in its neighbours' border strips.
#[derive(Default)]
pub(super) struct Pair<C> {
    codec: C,
    fixture: Fixture,
    anchor: (f32, f32),
}

impl<C: FixLike> Pair<C> {
    pub(super) fn at(anchor: (f32, f32)) -> Self {
        Self {
            codec: C::default(),
            fixture: Fixture::default(),
            anchor,
        }
    }
}

/// An identity is `x:y:team` (e.g. `-40:12.5:2`).
pub(super) fn parse(identity: &str) -> (f32, f32, u8) {
    let mut parts = identity.split(':');
    let mut next = || parts.next().expect("x:y:team");
    let x = next().parse().expect("x");
    let y = next().parse().expect("y");
    let team = next().parse().expect("team");
    (x, y, team)
}

/// The teleport payload.
pub(super) fn to(x: f32, y: f32) -> bytes::Bytes {
    let mut b = x.to_le_bytes().to_vec();
    b.extend_from_slice(&y.to_le_bytes());
    b.into()
}

impl<C: FixLike> Game for Pair<C> {
    type Codec = C;

    const SNAPSHOT_OP: u16 = <Fixture as Game>::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = <Fixture as Game>::PRIVATE_OP;

    fn codec(&self) -> &C {
        &self.codec
    }

    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.fixture.spawn_player(world, conn)
    }

    fn spawn_player_as(
        &mut self,
        world: &mut World,
        _conn: ConnectionId,
        identity: &str,
    ) -> Entity {
        let (x, y, _) = parse(identity);
        world.spawn((Position { x, y }, Speed(DEFAULT_SPEED))).id()
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
            let Some(&e) = players.get(&a.player) else {
                continue;
            };
            match (a.op, a.payload.len()) {
                (MOVE, 8) => {
                    let x = f32::from_le_bytes(a.payload[..4].try_into().expect("4 bytes"));
                    let y = f32::from_le_bytes(a.payload[4..].try_into().expect("4 bytes"));
                    world.entity_mut(e).insert(Position { x, y });
                }
                (SWITCH, 1) => {
                    world.entity_mut(e).insert(TeamMember(Team(a.payload[0])));
                }
                _ => {}
            }
        }
    }

    /// Every other tick an NPC spawns (at most 30 live — a crowd, so
    /// frames carry runs longer than 127 bytes); each walks a circle of
    /// radius 3–14 round the anchor (a new wire value most ticks); an
    /// NPC older than 120 ticks is despawned.
    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        let tick = ctx.tick;
        let mut q = world.query::<(Entity, &Npc)>();
        let npcs: Vec<(Entity, u64)> = q.iter(world).map(|(e, n)| (e, n.born)).collect();
        for &(e, born) in &npcs {
            if tick - born > 120 {
                world.despawn(e);
            } else {
                let pos = self.orbit(born, tick);
                world.entity_mut(e).insert(pos);
            }
        }
        if tick.is_multiple_of(2) && npcs.len() < 30 {
            world.spawn((Npc { born: tick }, self.orbit(tick, tick)));
        }
    }
}

impl<C> Pair<C> {
    fn orbit(&self, born: u64, tick: u64) -> Position {
        let r = 3.0 + (born % 12) as f32;
        let angle = (tick - born) as f32 * 0.09 + born as f32;
        Position {
            x: self.anchor.0 + r * angle.cos(),
            y: self.anchor.1 + r * angle.sin(),
        }
    }
}

impl<C: FixLike> TeamGame for Pair<C> {
    fn team_of(&mut self, world: &World, conn: ConnectionId, entity: Entity) -> Team {
        self.fixture.team_of(world, conn, entity)
    }

    /// The team the identity names.
    fn spawn_team_player_as(
        &mut self,
        world: &mut World,
        conn: ConnectionId,
        identity: &str,
    ) -> (Entity, Team) {
        let (_, _, team) = parse(identity);
        (self.spawn_player_as(world, conn, identity), Team(team))
    }
}

impl<C: FixLike> ShardGame for Pair<C> {
    type Mig = FixMig;

    fn capture(&self, world: &World, entity: Entity) -> FixMig {
        self.fixture.capture(world, entity)
    }

    fn restore(&mut self, world: &mut World, mig: FixMig) -> Entity {
        self.fixture.restore(world, mig)
    }
}
