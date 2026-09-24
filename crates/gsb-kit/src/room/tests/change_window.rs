//! The change-detection window (KIT-ARCHITECTURE §4.4): the kit closes
//! it — `World::clear_trackers`, world-wide — exactly once per tick, at
//! the end of `update`.

use super::*;

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

/// §8.3: without a per-tick `clear_trackers` the world's removed-
/// component buffers only grow — every despawn a leave ever caused stays
/// buffered for the room's lifetime (the core never calls it: there is
/// no system scheduler). The room closes the window in `update`, so a
/// tick's despawn is readable until that tick's update and gone after
/// it: the buffer is bounded by one tick's churn, not by the room's age.
#[test]
fn open_room_closes_the_change_window_every_tick() {
    let mut world = World::new();
    let mut room = OpenRoom::new();
    for tick in 1..=5u64 {
        let admission = room.on_join(&mut world, ConnectionId(tick));
        room.on_leave(&mut world, admission.player);
        assert_eq!(
            world.removed::<WireId>().count(),
            1,
            "tick {tick}: only this tick's despawn is pending"
        );
        room.update(&mut world, &ctx(tick));
        assert_eq!(
            world.removed::<WireId>().count(),
            0,
            "tick {tick}: the update closed the window"
        );
    }
}

/// A game whose systems close the change window themselves — the
/// ownership violation §4.4 rules out.
struct ClearsTrackers(crate::testing::Fixture);

impl crate::game::Game for ClearsTrackers {
    type Codec = crate::testing::FixCodec;

    fn codec(&self) -> &Self::Codec {
        self.0.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.0.spawn_player(world, conn)
    }
    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<gsb_core::room::Action>,
        players: &std::collections::HashMap<PlayerId, Entity>,
        seq: &mut crate::common::InputSeq,
    ) {
        self.0.ingest(world, ctx, actions, players, seq);
    }
    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.0.systems(world, ctx);
        world.clear_trackers();
    }
}

/// The kit owns the tick's one `clear_trackers` call: a hook that
/// makes it too would hide its own writes from the codec's `Dirty`
/// filter, so debug builds stop it at the hook boundary.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "the kit owns the change window")]
fn a_hook_closing_the_change_window_is_caught() {
    let mut world = World::new();
    let mut room = super::super::OpenRoom::with_game(ClearsTrackers(Default::default()));
    room.update(&mut world, &ctx(1));
}
