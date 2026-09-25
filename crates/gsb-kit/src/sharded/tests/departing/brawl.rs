//! The fixture game, brawling: hit points, scripted swings and
//! auto-attacks, a hit resolved the way the MMO resolves it — a WORLD
//! QUERY for the local target, else the seam's lent record and an
//! effect — and a record of what its hooks saw.

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::{Component, Entity, World};
use bytes::Bytes;
use gsb_core::room::Action;
use gsb_core::shard::{EffectId, EffectOutcome, EmitRefused, RemoteEffect};

use super::super::*;
use crate::game::{Game, InputSeq, ShardGame};
use crate::sharded::Seam;
use crate::space::GridPartition2;
use crate::testing::{FixMig, Fixture, WirePos};

/// A brawler's hit points (every player spawns with 100).
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub(super) struct Hp(pub(super) u16);

/// A brawler that strikes wire `0` on every tick, by itself.
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub(super) struct Auto(pub(super) u64);

/// What the systems of one tick saw of wire `probe`.
#[derive(Debug, Default, PartialEq)]
pub(super) struct View {
    pub(super) local: Option<Entity>,
    pub(super) lent: Option<(usize, WirePos)>,
    /// Wires a world query found / the seam lent (sorted).
    pub(super) world: Vec<u64>,
    pub(super) lent_all: Vec<u64>,
}

#[derive(Default)]
pub(super) struct Brawling {
    game: Fixture,
    /// `(source, target)`: the next systems run's swings.
    pub(super) swings: Vec<(u64, u64)>,
    pub(super) probe: u64,
    pub(super) view: View,
    pub(super) emits: Vec<Result<EffectId, EmitRefused>>,
    /// The entities each ingest's bot feed named.
    pub(super) botted: Vec<Vec<Entity>>,
    /// The wires a world query found in the last applied effect's hook.
    pub(super) effect_world: Vec<u64>,
}

/// A brawler's state in transit.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BrawlMig {
    fix: FixMig,
    pub(super) hp: u16,
    auto: Option<u64>,
}

impl Brawling {
    /// `source` strikes `target` for 10: on the world's entity with that
    /// wire if a query finds one, else across the seam if it is lent.
    fn swing(&mut self, world: &mut World, seam: &mut Seam<'_, '_, WirePos>, s: u64, t: u64) {
        let mut q = world.query::<(Entity, &WireId, &Hp)>();
        let local = q.iter(world).find(|(_, w, _)| w.get() == t).map(|x| x.0);
        match local {
            Some(victim) => {
                world.get_mut::<Hp>(victim).expect("hp").0 -= 10;
                seam.contact(s, t);
            }
            None if seam.lent(t).is_some() => {
                self.emits.push(seam.emit(t, s, Bytes::from_static(&[10])));
            }
            None => {}
        }
    }
}

impl Game for Brawling {
    type Codec = <Fixture as Game>::Codec;
    const SNAPSHOT_OP: u16 = Fixture::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = Fixture::PRIVATE_OP;
    fn codec(&self) -> &Self::Codec {
        self.game.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        let e = self.game.spawn_player(world, conn);
        world.entity_mut(e).insert(Hp(100));
        e
    }
    fn bot_actions(
        &mut self,
        _world: &World,
        _ctx: &TickCtx,
        bots: impl Iterator<Item = (PlayerId, Entity)>,
        _out: &mut Vec<Action>,
    ) {
        self.botted.push(bots.map(|(_, e)| e).collect());
    }
    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &std::collections::HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        self.game.ingest(world, ctx, actions, players, seq);
    }
    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.game.systems(world, ctx);
    }
}

impl ShardGame for Brawling {
    type Mig = BrawlMig;
    fn capture(&self, world: &World, entity: Entity) -> BrawlMig {
        let e = world.entity(entity);
        BrawlMig {
            fix: self.game.capture(world, entity),
            hp: e.get::<Hp>().expect("hp").0,
            auto: e.get::<Auto>().map(|a| a.0),
        }
    }
    fn restore(&mut self, world: &mut World, mig: BrawlMig) -> Entity {
        let e = self.game.restore(world, mig.fix);
        world.entity_mut(e).insert(Hp(mig.hp));
        if let Some(t) = mig.auto {
            world.entity_mut(e).insert(Auto(t));
        }
        e
    }
    fn systems_seam(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        seam: &mut Seam<'_, '_, WirePos>,
    ) {
        let mut wires: Vec<u64> = world
            .query::<&WireId>()
            .iter(world)
            .map(|w| w.get())
            .collect();
        wires.sort_unstable();
        let mut lent_all: Vec<u64> = seam.lent_iter().map(|l| l.wire).collect();
        lent_all.sort_unstable();
        self.view = View {
            local: seam.local(self.probe),
            lent: seam.lent(self.probe).map(|l| (l.lender, *l.state)),
            world: wires,
            lent_all,
        };
        let autos: Vec<(u64, u64)> = world
            .query::<(&WireId, &Auto)>()
            .iter(world)
            .map(|(w, a)| (w.get(), a.0))
            .collect();
        for (s, t) in std::mem::take(&mut self.swings).into_iter().chain(autos) {
            self.swing(world, seam, s, t);
        }
    }
    fn apply_remote_effect(
        &mut self,
        world: &mut World,
        target: Entity,
        effect: &RemoteEffect,
        _tick: u64,
        _seam: &mut Seam<'_, '_, WirePos>,
    ) -> EffectOutcome {
        self.effect_world = world
            .query::<&WireId>()
            .iter(world)
            .map(|w| w.get())
            .collect();
        self.effect_world.sort_unstable();
        world.get_mut::<Hp>(target).expect("hp").0 -= u16::from(effect.payload[0]);
        EffectOutcome::Applied
    }
}

pub(super) type Brawl = super::super::super::ShardedRoom<Brawling, GridPartition2<Position>>;

/// Shard `index` of two (shard 0: x < 0, shard 1: x ≥ 0; half 50).
pub(super) fn brawl(index: usize) -> Brawl {
    Brawl::with_game(Brawling::default(), GridPartition2::new(2, 50.0), index)
}

/// A brawler joined on `room` at `(x, 0)`: its wire id.
pub(super) fn spawn(world: &mut World, room: &mut Brawl, conn: u64, x: f32) -> u64 {
    let adm = room.on_join(world, ConnectionId(conn));
    let entity = room.player_entity[&adm.player];
    world.entity_mut(entity).insert(Position { x, y: 0.0 });
    adm.entity
}

/// `wire`'s hit points on `room`.
pub(super) fn hp(world: &World, room: &Brawl, wire: u64) -> u16 {
    world.get::<Hp>(room.wire_entity[&wire]).expect("hp").0
}

/// No entity of `world` is left disabled.
pub(super) fn none_disabled(world: &mut World) -> bool {
    world
        .query_filtered::<Entity, bevy_ecs::prelude::With<Disabled>>()
        .iter(world)
        .next()
        .is_none()
}
