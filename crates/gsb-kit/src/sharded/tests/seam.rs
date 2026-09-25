//! The kit's half of the cross-seam surface: the sharded rooms hand the
//! core's view to the game joined with their owned-wire table — own
//! wins over a lent copy, an effect on an own entity is refused as
//! local — and a remote effect reaches the game resolved to its target
//! entity (or is answered "no target"). Driven through both sharded
//! rooms over a `SeamStage` (the core's actor-free seam).

use bevy_ecs::prelude::Entity;
use bytes::Bytes;
use gsb_core::shard::{EffectId, EffectOutcome, EmitRefused, RemoteEffect, SeamStage, ShardLogic};

use super::*;
use crate::game::{Game, InputSeq, ShardGame, Wire};
use crate::sharded::Seam;
use crate::testing::{FixMig, Fixture, WirePos};

/// The fixture game, looking across the seam: its systems record what
/// the seam shows and emit a script; it records every effect applied.
#[derive(Default)]
struct Striking {
    game: Fixture,
    /// Lent wires the last systems run saw (sorted).
    seen: Vec<u64>,
    /// What `local` answered for `probe`.
    probe: u64,
    probed: Option<Entity>,
    /// `(target, source)` emitted by the next systems run, and the
    /// answers.
    script: Vec<(u64, u64)>,
    emits: Vec<Result<EffectId, EmitRefused>>,
    /// `(target entity, source wire)` of every applied effect.
    applied: Vec<(Entity, u64)>,
    /// Ticks `ingest_seam` ran at.
    ingested: Vec<u64>,
}

impl Game for Striking {
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

impl ShardGame for Striking {
    type Mig = FixMig;
    fn capture(&self, world: &World, entity: Entity) -> FixMig {
        self.game.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: FixMig) -> Entity {
        self.game.restore(world, mig)
    }
    fn ingest_seam(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        _actions: &mut Vec<gsb_core::room::Action>,
        _players: &std::collections::HashMap<PlayerId, Entity>,
        _seq: &mut InputSeq,
        seam: &mut Seam<'_, '_, Wire<Self>>,
    ) {
        self.ingested.push(seam.tick());
    }
    fn systems_seam(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        seam: &mut Seam<'_, '_, WirePos>,
    ) {
        self.seen = seam.lent_iter().map(|l| l.wire).collect();
        self.seen.sort_unstable();
        self.probed = seam.local(self.probe);
        for (target, source) in std::mem::take(&mut self.script) {
            self.emits
                .push(seam.emit(target, source, Bytes::from_static(b"hit")));
        }
    }
    fn apply_remote_effect(
        &mut self,
        _world: &mut World,
        target: Entity,
        effect: &RemoteEffect,
        _tick: u64,
        _seam: &mut Seam<'_, '_, WirePos>,
    ) -> EffectOutcome {
        self.applied.push((target, effect.source));
        EffectOutcome::Applied
    }
}

type Plain = super::super::ShardedRoom<Striking, crate::space::GridPartition2<Position>>;
type Spatial = super::super::ShardedSpatialRoom<
    Striking,
    crate::space::GridPartition2<Position>,
    crate::space::Grid2,
>;

fn plain() -> Plain {
    Plain::with_game(
        Striking::default(),
        crate::space::GridPartition2::new(4, 50.0),
        0,
    )
}

/// Shard 1's first wire (a foreign entity) and a stage lending it — and
/// lending `own` too: the one-tick double view of an entity that just
/// migrated in.
fn stage(own: u64, tick: u64) -> (u64, SeamStage<WirePos>) {
    let foreign = SHARD_SERIAL_RANGE + 1;
    let mut stage = SeamStage::new(0, tick);
    for wire in [foreign, own] {
        stage.lend(
            1,
            BorderRecord {
                wire,
                state: WirePos { x: 1, y: -10 },
            },
        );
    }
    (foreign, stage)
}

fn effect(target: u64, source: u64) -> RemoteEffect {
    RemoteEffect {
        target,
        source,
        id: EffectId {
            origin: 1,
            epoch: 0,
            seq: 1,
        },
        at_tick: 1,
        hops: 0,
        payload: Bytes::from_static(b"hit"),
    }
}

/// The game sees the lent half without the own entity's lent copy, its
/// own entity through `local`, and cannot emit at its own entity.
#[test]
fn the_game_sees_local_and_lent_with_own_winning() {
    let mut world = World::new();
    let mut room = plain();
    let adm = room.on_join(&mut world, ConnectionId(1));
    let own = adm.entity;
    let (foreign, mut stage) = stage(own, 1);
    room.game_mut().probe = own;
    room.game_mut().script = vec![(own, own), (foreign, own)];
    room.update_seam(&mut world, &ctx(1), &mut stage.seam());

    let g = room.game();
    assert_eq!(g.seen, [foreign], "own wins over its lent copy");
    assert_eq!(g.probed, room.player_entity.values().next().copied());
    assert_eq!(g.emits[0], Err(EmitRefused::Local));
    assert!(g.emits[1].is_ok());
    let sent: Vec<(usize, u64, u64)> = stage
        .emitted()
        .iter()
        .map(|(to, e)| (*to, e.target, e.source))
        .collect();
    assert_eq!(sent, [(1, foreign, own)], "to the lender, credited to own");

    room.ingest_seam(&mut world, &ctx(1), &mut Vec::new(), &mut stage.seam());
    assert_eq!(room.game().ingested, [1], "ingest runs through the seam");
}

/// An effect reaches the game as its target ENTITY; a wire that is not
/// (or no longer) a live entity here is "no target".
#[test]
fn a_remote_effect_reaches_the_game_as_its_target_entity() {
    let mut world = World::new();
    let mut room = plain();
    let own = room.on_join(&mut world, ConnectionId(1)).entity;
    let entity = *room.player_entity.values().next().expect("joined");
    let (foreign, mut stage) = stage(own, 2);

    let got = room.apply_remote_effect(&mut world, 2, &effect(own, foreign), &mut stage.seam());
    assert_eq!(got, EffectOutcome::Applied);
    assert_eq!(room.game().applied, [(entity, foreign)]);
    let got = room.apply_remote_effect(&mut world, 2, &effect(999, foreign), &mut stage.seam());
    assert_eq!(got, EffectOutcome::NoTarget);

    // Despawned by game code this tick, before the sweep forgot its wire.
    world.despawn(entity);
    let got = room.apply_remote_effect(&mut world, 2, &effect(own, foreign), &mut stage.seam());
    assert_eq!(got, EffectOutcome::NoTarget);
    assert_eq!(room.game().applied.len(), 1);
}

/// The spatial composite delegates all three hooks and still runs its
/// own tick half (the dirty pass buckets the joiner).
#[test]
fn the_spatial_composite_passes_the_seam_through() {
    let mut world = World::new();
    let mut room = Spatial::with_shard(plain(), crate::space::Grid2::new(10.0));
    let own = room.on_join(&mut world, ConnectionId(1)).entity;
    let (foreign, mut stage) = stage(own, 1);
    room.game_mut().script = vec![(foreign, own)];
    room.update_seam(&mut world, &ctx(1), &mut stage.seam());
    assert_eq!(room.game().seen, [foreign]);
    assert_eq!(stage.emitted().len(), 1);
    assert_eq!(room.book.last_cell.len(), 1, "the spatial half ran");
    let got = room.apply_remote_effect(&mut world, 1, &effect(own, foreign), &mut stage.seam());
    assert_eq!(got, EffectOutcome::Applied);
}
