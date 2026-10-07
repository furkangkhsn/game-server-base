//! Idle cost: a room with members but no traffic pays, per tick, one
//! empty `try_recv` on the completion channel (RESPONSES) and one on the
//! request channel (READ) — no hook work in the table, no allocation.
//!
//! Allocation is proven structurally (a counting allocator needs
//! `unsafe impl GlobalAlloc`, which the project bans): every per-tick
//! buffer of the table starts as `Vec::new()`/`HashMap::new()` (which do
//! not allocate) and still has capacity 0 after the idle run.

mod common;

use common::*;
use s0_calls::room::FIXED_DT;

fn idle(_: &mut (), _: &mut Calls, _: &mut TickCtx) {}

#[test]
fn an_idle_room_polls_once_per_phase_and_allocates_nothing() {
    let mut room = scripted(
        RoomConfig::default(),
        (),
        |c| c.accept::<Equip>().unwrap(),
        idle,
    );
    let i = room.inputs();
    i.join(ALICE, "alice");
    i.join(BOB, "bob");
    for _ in 0..1000 {
        assert!(room.tick(FIXED_DT).is_empty());
    }
    assert_eq!(
        room.stats.read_polls, 1000,
        "READ: one empty try_recv per tick"
    );
    let (polls, caps, counters) = {
        let calls = &room.logic().calls;
        (calls.polls, calls.buffer_capacities(), calls.counters)
    };
    assert_eq!(polls, 1000, "RESPONSES: one empty try_recv per tick");
    assert_eq!(caps, [0; 6], "no per-tick buffer ever allocated");
    assert_eq!(
        counters,
        Counters::default(),
        "nothing happened, nothing counted"
    );
}
