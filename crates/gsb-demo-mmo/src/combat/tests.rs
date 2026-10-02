//! The MMO's combat at the logic level, on one shard of its real kit
//! room: which targets an attack reaches and which attackers a remote
//! strike accepts — local entities and lent records side by side, over
//! a `SeamStage` (the core's actor-free seam) — and the kill feed's
//! loss counts.

use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::LogicCounters;
use gsb_core::room::{Action, GameLogic, TickCtx};
use gsb_core::shard::{BorderRecord, EffectId, EffectOutcome, RemoteEffect, SeamStage, ShardLogic};
use gsb_kit::identity::WireId;
use prost::Message;

use crate::codec::MmoWire;
use crate::components::{Kind, Pos3, Vitals};
use crate::effect::MmoEffect;
use crate::world::{ATTACK_DAMAGE, PLAYER_HP};
use crate::{MmoShard, Realm, op};

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: Default::default(),
    }
}

/// A point on shard 0's ground, on the line all these fights stand on.
fn at(x: f32) -> Pos3 {
    Pos3::new(x, 0.0, -256.0)
}

/// The logins on shard 0's ground: attacker `a` and, from it, `b` 10 m
/// (in reach) and `c` 50 m (out of reach; 40 m from `b`).
fn realm() -> Realm {
    Realm::empty()
        .with_login("a", at(-240.0))
        .with_login("b", at(-230.0))
        .with_login("c", at(-190.0))
}

/// Neighbour 1 lends: `NEAR` 20 m from `a` (in reach; 10 m from `b`),
/// `FAR` 70 m (out; 60 m from `b`) and `DOWN`, a defeated record 5 m
/// away.
const NEAR: u64 = 9001;
const FAR: u64 = 9002;
const DOWN: u64 = 9003;

fn stage(tick: u64) -> SeamStage<MmoWire> {
    let mut stage = SeamStage::new(0, tick);
    for (wire, x, hp) in [
        (NEAR, -220.0, PLAYER_HP),
        (FAR, -170.0, PLAYER_HP),
        (DOWN, -235.0, 0),
    ] {
        let vitals = Vitals {
            kind: Kind::Player,
            hp,
        };
        let state = MmoWire::of(&at(x), &vitals);
        stage.lend(1, BorderRecord { wire, state });
    }
    stage
}

fn attack(player: PlayerId, target: u64) -> Action {
    let msg = crate::mmo::Attack { target, seq: 0 };
    Action {
        conn: ConnectionId(1),
        player,
        op: op::MMO_ATTACK,
        payload: msg.encode_to_vec().into(),
    }
}

/// `wire`'s hit points in `world`.
fn hp(world: &mut World, wire: u64) -> u16 {
    let mut q = world.query::<(&WireId, &Vitals)>();
    let found = q.iter(world).find(|(w, _)| w.get() == wire);
    found.expect("an entity").1.hp
}

/// On the sharded path, `a`'s attacks on every candidate: the local
/// entity in reach is struck here, the LENT one in reach is sent to its
/// lender, nothing else is touched — out of reach (local or lent), a
/// defeated record, itself, a wire nobody knows. A remote strike on `b`
/// is refused from out of range, local or LENT, applied otherwise (an
/// attacker nobody sees is not re-checked).
#[test]
fn attacks_reach_local_and_lent_targets() {
    let mut world = World::new();
    let mut shard = crate::mmo_shard(0, &realm());
    let [a, b, c] = [(1, "a"), (2, "b"), (3, "c")]
        .map(|(n, s)| shard.on_join_as(&mut world, ConnectionId(n), s));
    let targets = [b.entity, c.entity, a.entity, NEAR, FAR, DOWN, 777];
    let mut actions: Vec<Action> = targets.iter().map(|&t| attack(a.player, t)).collect();
    let mut stage = stage(5);
    shard.ingest_seam(&mut world, &ctx(5), &mut actions, &mut stage.seam());

    let struck = PLAYER_HP - ATTACK_DAMAGE;
    let hps = [a, b, c].map(|j| hp(&mut world, j.entity));
    assert_eq!(hps, [PLAYER_HP, struck, PLAYER_HP]);
    let sent: Vec<(usize, u64, u64)> = stage
        .emitted()
        .iter()
        .map(|(to, e)| (*to, e.target, e.source))
        .collect();
    assert_eq!(sent, [(1, NEAR, a.entity)], "only the lent target in reach");

    let outcomes = [NEAR, FAR, c.entity, a.entity, 0].map(|source| {
        let effect = RemoteEffect {
            target: b.entity,
            source,
            id: EffectId {
                origin: 1,
                epoch: 0,
                seq: source,
            },
            at_tick: 5,
            hops: 0,
            payload: MmoEffect::Strike { damage: 1 }.encode(),
        };
        shard.apply_remote_effect(&mut world, 6, &effect, &mut stage.seam())
    });
    use EffectOutcome::{Applied, Rejected};
    assert_eq!(outcomes, [Applied, Rejected, Rejected, Applied, Applied]);
    assert_eq!(hp(&mut world, b.entity), struck - 3);
}

/// `shard`'s logic counter `name` (F9), as its sample would carry it.
fn counted(shard: &MmoShard, world: &World, name: &str) -> Option<u64> {
    let mut out = LogicCounters::new();
    shard.logic_counters(world, &mut out);
    out.get(name)
}

/// The kill feed (BACKLOG B81): a hit the feed cannot take is counted
/// by why — full (`combat_hits_dropped_full`) or its reader gone
/// (`combat_hits_dropped_closed`) — on the logic-counter seam (F9); a
/// count is put once it is non-zero.
#[test]
fn a_dropped_hit_is_counted_full_or_closed() {
    let mut world = World::new();
    let mut shard = crate::mmo_shard(0, &realm());
    let (feed, hits) = gsb_core::channel::channel(1);
    shard.game_mut().set_combat_feed(feed);
    let a = shard.on_join_as(&mut world, ConnectionId(1), "a");
    let b = shard.on_join_as(&mut world, ConnectionId(2), "b");
    let strike = |shard: &mut MmoShard, world: &mut World, n| {
        let mut actions: Vec<Action> = (0..n).map(|_| attack(a.player, b.entity)).collect();
        shard.ingest(world, &ctx(5), &mut actions);
    };
    let (full, closed) = ("combat_hits_dropped_full", "combat_hits_dropped_closed");
    strike(&mut shard, &mut world, 1);
    assert_eq!(counted(&shard, &world, full), None, "nothing dropped yet");
    strike(&mut shard, &mut world, 2);
    assert_eq!(hits.len(), 1, "the feed holds one hit");
    assert_eq!(counted(&shard, &world, full), Some(2));
    assert_eq!(counted(&shard, &world, closed), None);
    drop(hits);
    strike(&mut shard, &mut world, 1); // the fourth blow defeats `b`: one hit
    assert_eq!(counted(&shard, &world, full), Some(2));
    assert_eq!(counted(&shard, &world, closed), Some(1));
}
