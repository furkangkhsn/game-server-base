//! Which targets an attack reaches, and which attackers a remote strike
//! accepts — local units and lent records side by side, on one shard of
//! the war room driven through the kit's hooks over a `SeamStage` (the
//! core's actor-free seam), and on the plain input path without a seam.
//! Locked before the combat resolved its targets through the kit's
//! `Seam::find` (KIT-ARCHITECTURE §10 "F6"): the same targets, the same
//! refusals.

use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::room::{Action, GameLogic, TickCtx};
use gsb_core::shard::{BorderRecord, EffectId, EffectOutcome, RemoteEffect, SeamStage, ShardLogic};
use gsb_kit::identity::WireId;
use gsb_kit::team::{Team, TeamRoom};
use prost::Message;

use crate::codec::WarWire;
use crate::components::{Kind, Pos3, Unit};
use crate::effect::WarEffect;
use crate::world::{ATTACK_DAMAGE, PLAYER_HP};
use crate::{Realm, WarGame, op, war_shard};

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: Default::default(),
        paths: Default::default(),
    }
}

/// The logins on shard 3's ground: attacker `a` (faction 0) and, from
/// it, enemy `b` 10 m (in reach), enemy `c` 30 m (out of reach), ally
/// `d` 5 m; `e` (faction 2) 30 m from `b`.
fn realm() -> Realm {
    let at = |x| Pos3::ground(x, 100.0);
    Realm::empty()
        .with_login("a", Team(0), at(100.0))
        .with_login("b", Team(1), at(110.0))
        .with_login("c", Team(1), at(130.0))
        .with_login("d", Team(0), at(105.0))
        .with_login("e", Team(2), at(140.0))
}

/// A lent record: a unit of `kind` and `faction` at `(x, 100)`.
fn lent(wire: u64, x: f32, kind: Kind, faction: u8, hp: u16) -> BorderRecord<WarWire> {
    let unit = Unit {
        kind,
        faction: Some(Team(faction)),
        hp,
    };
    let state = WarWire::of(&Pos3::ground(x, 100.0), &unit);
    BorderRecord { wire, state }
}

/// Neighbour 2 lends: enemy `E` 15 m from `a` (in reach; `b`'s ally),
/// enemy `F` 40 m (out), ally `G` 3 m, an enemy TOWER 4 m (with hit
/// points: the kind alone keeps it out of the fight), a fallen
/// enemy 6 m, and `FAR` (faction 2) 30 m from `b`.
const E: u64 = 9001;
const F: u64 = 9002;
const G: u64 = 9003;
const TOWER: u64 = 9004;
const FALLEN: u64 = 9005;
const FAR: u64 = 9006;

fn stage(tick: u64) -> SeamStage<WarWire> {
    let mut stage = SeamStage::new(3, tick);
    for r in [
        lent(E, 115.0, Kind::Player, 1, PLAYER_HP),
        lent(F, 140.0, Kind::Player, 1, PLAYER_HP),
        lent(G, 103.0, Kind::Player, 0, PLAYER_HP),
        lent(TOWER, 104.0, Kind::Tower, 1, PLAYER_HP),
        lent(FALLEN, 106.0, Kind::Player, 1, 0),
        lent(FAR, 140.0, Kind::Player, 2, PLAYER_HP),
    ] {
        stage.lend(2, r);
    }
    stage
}

fn attack(conn: u64, player: PlayerId, target: u64) -> Action {
    let msg = crate::war::Attack { target, seq: 0 };
    Action {
        conn: ConnectionId(conn),
        player,
        op: op::WAR_ATTACK,
        payload: msg.encode_to_vec().into(),
    }
}

/// `wire`'s hit points in `world`.
fn hp(world: &mut World, wire: u64) -> u16 {
    let mut q = world.query::<(&WireId, &Unit)>();
    let unit = q.iter(world).find(|(w, _)| w.get() == wire);
    unit.expect("a unit").1.hp
}

/// On the sharded path, `a`'s attacks on every candidate: the enemy in
/// reach is struck here, the lent enemy in reach is sent to its lender,
/// nothing else is touched — out of reach, an ally, a tower, a fallen
/// player, itself, a wire nobody knows. A remote strike on `b` is
/// refused from `b`'s allies and from out of reach, local or lent,
/// applied otherwise (an attacker nobody sees is not re-checked).
#[test]
fn attacks_reach_the_same_targets_local_and_lent() {
    let mut world = World::new();
    let mut shard = war_shard(3, &realm());
    let join = |shard: &mut crate::WarShard, world: &mut World, conn, name| {
        shard.on_join_as(world, ConnectionId(conn), name)
    };
    let a = join(&mut shard, &mut world, 1, "a");
    let [b, c, d, e] =
        [(2, "b"), (3, "c"), (4, "d"), (5, "e")].map(|(n, s)| join(&mut shard, &mut world, n, s));
    let targets = [
        b.entity, c.entity, d.entity, a.entity, E, F, G, TOWER, FALLEN, 777,
    ];
    let mut actions: Vec<Action> = targets.iter().map(|&t| attack(1, a.player, t)).collect();
    let mut stage = stage(5);
    shard.ingest_seam(&mut world, &ctx(5), &mut actions, &mut stage.seam());

    let hps = [a, b, c, d].map(|j| hp(&mut world, j.entity));
    let struck = PLAYER_HP - ATTACK_DAMAGE;
    assert_eq!(hps, [PLAYER_HP, struck, PLAYER_HP, PLAYER_HP]);
    let sent: Vec<(usize, u64, u64)> = stage
        .emitted()
        .iter()
        .map(|(to, e)| (*to, e.target, e.source))
        .collect();
    assert_eq!(sent, [(2, E, a.entity)], "only the lent enemy in reach");

    let outcomes = [G, E, FAR, d.entity, c.entity, e.entity, 0].map(|source| {
        let effect = RemoteEffect {
            target: b.entity,
            source,
            id: EffectId {
                origin: 2,
                epoch: 0,
                seq: source,
            },
            at_tick: 5,
            hops: 0,
            payload: WarEffect::Strike { damage: 1 }.encode(),
        };
        shard.apply_remote_effect(&mut world, 6, &effect, &mut stage.seam())
    });
    use EffectOutcome::{Applied, Rejected};
    let want = [
        Applied, Rejected, Rejected, Applied, Rejected, Rejected, Applied,
    ];
    assert_eq!(outcomes, want);
    assert_eq!(hp(&mut world, b.entity), struck - 3);
}

/// Without a seam (the war in the kit's single-world team room — the
/// plain input path): the same local targets.
#[test]
fn without_a_seam_attacks_reach_the_same_local_targets() {
    let mut world = World::new();
    let game = WarGame::for_shard(3, &realm());
    let mut room = TeamRoom::with_game(game, crate::world::vision());
    let [a, b, c, d] = [(1, "a"), (2, "b"), (3, "c"), (4, "d")]
        .map(|(n, s)| room.on_join_as(&mut world, ConnectionId(n), s));
    let targets = [b.entity, c.entity, d.entity, a.entity, E];
    let mut actions: Vec<Action> = targets.iter().map(|&t| attack(1, a.player, t)).collect();
    room.ingest(&mut world, &ctx(5), &mut actions);
    let hps = [a, b, c, d].map(|j| hp(&mut world, j.entity));
    let struck = PLAYER_HP - ATTACK_DAMAGE;
    assert_eq!(hps, [PLAYER_HP, struck, PLAYER_HP, PLAYER_HP]);
}

/// `shard`'s logic counter `name` (F9), as its sample would carry it.
fn counted(shard: &crate::WarShard, world: &World, name: &str) -> Option<u64> {
    let mut out = gsb_core::metrics::LogicCounters::new();
    shard.logic_counters(world, &mut out);
    out.get(name)
}

/// The kill feed (BACKLOG B81): a hit the feed cannot take is counted
/// by why — full (`combat_hits_dropped_full`) or its reader gone
/// (`combat_hits_dropped_closed`) — on the logic-counter seam (F9),
/// next to `war_kills`; a count is put once it is non-zero.
#[test]
fn a_dropped_hit_is_counted_full_or_closed() {
    let mut world = World::new();
    let mut shard = war_shard(3, &realm());
    let (feed, hits) = gsb_core::channel::channel(1);
    shard.game_mut().set_combat_feed(feed);
    let a = shard.on_join_as(&mut world, ConnectionId(1), "a");
    let b = shard.on_join_as(&mut world, ConnectionId(2), "b");
    let mut stage = stage(5);
    let mut strike = |shard: &mut crate::WarShard, world: &mut World, n| {
        let mut actions: Vec<Action> = (0..n).map(|_| attack(1, a.player, b.entity)).collect();
        shard.ingest_seam(world, &ctx(5), &mut actions, &mut stage.seam());
    };
    let (full, closed) = ("combat_hits_dropped_full", "combat_hits_dropped_closed");
    strike(&mut shard, &mut world, 1);
    assert_eq!(counted(&shard, &world, full), None, "nothing dropped yet");
    strike(&mut shard, &mut world, 2);
    assert_eq!(hits.len(), 1, "the feed holds one hit");
    assert_eq!(counted(&shard, &world, full), Some(2));
    assert_eq!(counted(&shard, &world, closed), None);
    drop(hits);
    strike(&mut shard, &mut world, 1); // the fourth blow fells `b`: one hit
    assert_eq!(counted(&shard, &world, full), Some(2));
    assert_eq!(counted(&shard, &world, closed), Some(1));
    assert_eq!(counted(&shard, &world, "war_kills"), Some(1));
}
