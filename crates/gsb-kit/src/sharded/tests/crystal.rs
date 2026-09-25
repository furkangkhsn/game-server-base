//! Crystallization (`docs/CROSS-SHARD.md` §4 layer 4, "C2 sonucu") on
//! the fixture game: a fight across the seam is detected after K ticks
//! (not before), only the higher wire id moves, a held pair does not
//! flip back while it fights, the hold ends when the fight is over or
//! the entity leaves the band, the state stays bounded, and a room that
//! did not opt in behaves exactly as before. Driven through the kit's
//! `ShardLogic` hooks over a `SeamStage` (the core's actor-free seam);
//! the real actors are the MMO's tests.

use bevy_ecs::prelude::Entity;
use bytes::Bytes;
use gsb_core::shard::{EffectId, EffectOutcome, RemoteEffect, SeamStage, ShardLogic};

use super::*;
use crate::game::{Game, InputSeq, ShardGame};
use crate::sharded::{Crystallize, Seam, ShardPin};
use crate::space::GridPartition2;
use crate::testing::{FixMig, Fixture, WirePos};

mod hold;
mod signal;

/// The fixture game, fighting: each systems run strikes across the seam
/// (`Seam::emit`) and reports local hits (`Seam::contact`) from a
/// script; every remote effect lands.
#[derive(Default)]
pub(super) struct Dueling {
    game: Fixture,
    /// `(source, target)`: struck across the seam by the next run.
    pub(super) strike: Vec<(u64, u64)>,
    /// `(source, target)`: local hits the next run reports.
    pub(super) local: Vec<(u64, u64)>,
}

impl Game for Dueling {
    type Codec = <Fixture as Game>::Codec;
    const SNAPSHOT_OP: u16 = Fixture::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = Fixture::PRIVATE_OP;
    fn codec(&self) -> &Self::Codec {
        self.game.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.game.spawn_player(world, conn)
    }
    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<gsb_core::room::Action>,
        players: &std::collections::HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        self.game.ingest(world, ctx, actions, players, seq);
    }
    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.game.systems(world, ctx);
    }
}

impl ShardGame for Dueling {
    type Mig = FixMig;
    fn capture(&self, world: &World, entity: Entity) -> FixMig {
        self.game.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: FixMig) -> Entity {
        self.game.restore(world, mig)
    }
    fn systems_seam(&mut self, _w: &mut World, _ctx: &TickCtx, seam: &mut Seam<'_, '_, WirePos>) {
        for (source, target) in std::mem::take(&mut self.strike) {
            seam.emit(target, source, Bytes::from_static(b"hit"))
                .expect("the target is lent");
        }
        for (source, target) in std::mem::take(&mut self.local) {
            seam.contact(source, target);
        }
    }
    fn apply_remote_effect(
        &mut self,
        _w: &mut World,
        _target: Entity,
        _effect: &RemoteEffect,
        _tick: u64,
        _seam: &mut Seam<'_, '_, WirePos>,
    ) -> EffectOutcome {
        EffectOutcome::Applied
    }
}

pub(super) type Duel = super::super::ShardedRoom<Dueling, GridPartition2<Position>>;

/// K = 10, window = 5, release = 20, band 10 (enter 5) — on a 2-shard
/// map of half 50 (shard 0: x < 0, shard 1: x ≥ 0; border 12.5).
pub(super) const POLICY: Crystallize = Crystallize {
    after: 10,
    window: 5,
    release: 20,
    margin: 10.0,
};

/// Shard `index` of `shards` (the 8-neighbourhood when 4), opted in to
/// `policy` when given.
pub(super) fn duel(index: usize, shards: usize, policy: Option<Crystallize>) -> Duel {
    let mut partition = GridPartition2::new(shards, 50.0);
    if shards == 4 {
        partition = partition.with_diagonals();
    }
    let room = Duel::with_game(Dueling::default(), partition, index);
    match policy {
        Some(p) => room.with_crystallize(p),
        None => room,
    }
}

/// A player joined on `room` and put at `(x, y)`; its wire id.
pub(super) fn spawn(world: &mut World, room: &mut Duel, conn: u64, x: f32, y: f32) -> u64 {
    let adm = room.on_join(world, ConnectionId(conn));
    let entity = room.player_entity[&adm.player];
    world.entity_mut(entity).insert(Position { x, y });
    adm.entity
}

/// A stage at `tick` for shard `origin`, lending `(lender, wire, x, y)`.
pub(super) fn stage(
    origin: usize,
    tick: u64,
    lent: &[(usize, u64, i32, i32)],
) -> SeamStage<WirePos> {
    let mut stage = SeamStage::new(origin, tick);
    for &(lender, wire, x, y) in lent {
        stage.lend(
            lender,
            BorderRecord {
                wire,
                state: WirePos { x, y },
            },
        );
    }
    stage
}

/// `source`'s hit on `target`, sent by `origin` at `at`.
pub(super) fn hit(target: u64, source: u64, origin: usize, at: u64) -> RemoteEffect {
    RemoteEffect {
        target,
        source,
        id: EffectId {
            origin,
            epoch: 0,
            seq: at,
        },
        at_tick: at,
        hops: 0,
        payload: Bytes::from_static(b"hit"),
    }
}

/// What the migrate phase would send this tick: `(neighbour, wire,
/// pin)` over every neighbour.
pub(super) fn moves(world: &mut World, room: &mut Duel) -> Vec<(usize, u64, Option<ShardPin>)> {
    let neighbors = room.neighbors().to_vec();
    let mut out = Vec::new();
    for n in neighbors {
        for m in room.collect_migrations(world, n) {
            out.push((n, m.wire, m.state.pin));
        }
    }
    out
}

/// One tick of a duel on `room` (shard `index`) between its own `me`
/// and `foe`, lent by `lender` at `(fx, 0)`: on odd ticks `foe`'s hit
/// lands here, on even ticks `me` strikes back. The moves it produces.
pub(super) fn duel_tick(
    world: &mut World,
    room: &mut Duel,
    t: u64,
    (me, foe, lender, fx): (u64, u64, usize, i32),
) -> Vec<(usize, u64, Option<ShardPin>)> {
    let index = room.index();
    let mut stage = stage(index, t, &[(lender, foe, fx, 0)]);
    if t % 2 == 1 {
        let got =
            room.apply_remote_effect(world, t, &hit(me, foe, lender, t - 1), &mut stage.seam());
        assert_eq!(got, EffectOutcome::Applied);
    } else {
        room.game_mut().strike.push((me, foe));
    }
    room.update_seam(world, &ctx(t), &mut stage.seam());
    moves(world, room)
}
