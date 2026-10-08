//! Idle cost is structural (docs/EVENTS.md §3.2): a quiet room's EVENTS
//! phase is one empty `try_recv`; the table never allocates; nothing
//! counts. (Allocation counting would need a counting `GlobalAlloc`,
//! i.e. `unsafe` — the capacity check is the structural proof, as in s0.)

use e0_events::contract::RoomId;
use e0_events::events::EventCounts;
use e0_events::harness::{Link, Server, Wallets};
use e0_events::room::{Caps, RoomCounts};
use e0_events::router::RouterCounts;

#[test]
fn a_quiet_room_pays_one_empty_receive_per_tick() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    s.sharded(RoomId(2), vec![Wallets::default(), Wallets::default()], Caps::default());
    let _bank = s.router.publisher();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(1), 0, 8, 0);
    s.connect("ann", Link::default());
    s.join("ann", RoomId(2), 1, 8, 0);
    for _ in 0..1000 {
        s.step();
    }
    for (room, shards) in [(RoomId(1), 1u16), (RoomId(2), 2)] {
        for i in 0..shards {
            let r = s.shard(room, i);
            assert_eq!(r.events().capacity(), 0, "no buffer ever allocated");
            assert_eq!(r.events().counts(), &EventCounts::default());
            assert_eq!(r.counts(), &RoomCounts::default());
        }
    }
    assert_eq!(s.router.counts(), &RouterCounts::default());
}
