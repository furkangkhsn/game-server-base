//! (g) Idle cost: with nothing in flight and nothing pushed, RESPONSES
//! and EVENTS are one empty `try_recv` and an `is_empty` probe — no hook
//! call, no sweep, no buffer growth.
//!
//! Allocation is proven structurally, not with a counting allocator (that
//! needs `unsafe impl GlobalAlloc`, which the project bans): the phase
//! buffers are `Vec::new()` (never allocates) and their capacity is still
//! 0 after the idle run; the deadline sweep — the only allocating step —
//! did not run.

mod common;

use common::*;
use s0_calls::fake::FakeService;
use s0_calls::request::ConnId;
use s0_calls::room::{Room, RoomConfig};

#[test]
fn an_idle_room_pays_one_poll_per_tick_and_nothing_else() {
    let (econ, _fake) = FakeService::new("economy", 8);
    let mut room = Room::new(RoomConfig::default(), Shop::new(econ, 0), shop_table());
    let (_tx, _out) = room.join(ConnId(1), 64);
    for _ in 0..1000 {
        room.tick();
    }
    let s = room.stats;
    assert_eq!(s.ticks, 1000);
    assert_eq!(
        s.response_polls, 1000,
        "one drain (= one empty try_recv) per tick"
    );
    assert_eq!(s.sweeps, 0, "no deadline sweep while nothing is in flight");
    assert_eq!(
        s.on_done_calls, 0,
        "the logic is not called for an empty batch"
    );
    assert_eq!(s.on_events_calls, 0);
    assert_eq!(room.buffer_capacities(), (0, 0), "no buffer ever allocated");
}

#[test]
fn the_sweep_runs_only_while_a_call_is_in_flight() {
    let (econ, mut fake) = FakeService::new("economy", 8);
    let mut ledger = Ledger::with_balance(0);
    let mut shop = Shop::new(econ, 0);
    shop.grant_on_update = Some(1);
    let mut room = Room::new(RoomConfig::default(), shop, shop_table());
    for _ in 0..10 {
        room.tick();
        fake.step(room.now(), |m, s| ledger.handle(3, m, s));
    }
    // In flight after tick 1's SYSTEMS; answered after tick 4; absorbed
    // in tick 5. Ticks 2..=5 swept (tick 5's drain empties the table
    // before the sweep check — 2, 3, 4 sweep), then nothing.
    assert_eq!(room.stats.sweeps, 3);
    assert_eq!(room.stats.on_done_calls, 1);
    let before = room.stats;
    for _ in 0..100 {
        room.tick();
    }
    assert_eq!(room.stats.sweeps, before.sweeps);
    assert_eq!(room.stats.on_done_calls, before.on_done_calls);
}
