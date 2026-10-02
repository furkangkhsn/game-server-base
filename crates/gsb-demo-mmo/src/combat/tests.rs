//! The MMO's combat at the logic level, on one shard of its real kit
//! room: the kill feed's loss counts.

use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::LogicCounters;
use gsb_core::room::{Action, GameLogic, TickCtx};
use prost::Message;

use crate::components::Pos3;
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

/// The logins on shard 0's ground: `a` and, 10 m from it, `b`.
fn realm() -> Realm {
    let at = |x| Pos3::new(x, 0.0, -256.0);
    Realm::empty()
        .with_login("a", at(-240.0))
        .with_login("b", at(-230.0))
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
