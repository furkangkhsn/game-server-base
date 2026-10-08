//! Routing by identity across rooms and shards, through a migration
//! (single and chained) and a park/resume (docs/EVENTS.md §6, §7).

use e0_events::contract::{Identity, RoomId};
use e0_events::game::{ChatMessage, GoldReceived};
use e0_events::harness::{Link, Server, Wallets, gold};
use e0_events::room::Caps;
use e0_events::router::Place;

fn gr(amount: u64) -> GoldReceived {
    GoldReceived {
        from: "alice".into(),
        amount,
        balance: 0,
        version: 0,
    }
}

fn bob() -> Identity {
    Identity::new("bob")
}

/// The bob-entries of a shard's `seen` list: their sequence numbers.
fn seqs(s: &Server<Wallets>, room: RoomId, shard: u16) -> Vec<u64> {
    s.shard(room, shard).game().seen.iter().filter(|e| e.0 == "bob").map(|e| e.2).collect()
}

#[test]
fn gold_reaches_bob_in_another_room() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    s.room(RoomId(2), Wallets::default(), Caps::default());
    let bank = s.router.publisher();
    s.connect("alice", Link::default());
    s.connect("bob", Link::default());
    s.join("alice", RoomId(1), 0, 8, 500);
    s.join("bob", RoomId(2), 0, 8, 0);
    s.step();

    bank.push(&bob(), &gr(100)).unwrap();
    s.step();

    assert_eq!(gold(s.shard(RoomId(2), 0).game(), "bob"), Some(100));
    assert_eq!(seqs(&s, RoomId(2), 0), vec![1]);
    assert_eq!(s.shard(RoomId(1), 0).events().counts().admitted, 0, "alice's room never sees it");
    assert_eq!(s.router.counts().world_sent, 1);
}

/// The hazard, staged by hand: events already queued at the source when
/// the player crosses, then the router learns the new place and sends
/// DIRECT to the target, and the target ticks before the source forwards.
/// With the fence the room takes 1..=6 in order; without it, 4,5,6,1,2,3.
fn migration(fencing: bool) -> Vec<u64> {
    let mut s = Server::<Wallets>::new(64);
    s.sharded(RoomId(3), vec![Wallets::default(), Wallets::default()], Caps::default());
    for i in 0..2 {
        s.shard_mut(RoomId(3), i).fencing(fencing);
    }
    let bank = s.router.publisher();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(3), 0, 8, 0);
    s.step();

    for _ in 0..3 {
        bank.push(&bob(), &gr(1)).unwrap(); // seq 1..3
    }
    s.router.pump(); // → shard 0's mailbox
    s.shard_mut(RoomId(3), 0).migrate_out(&bob(), 1); // the seam
    s.shard_mut(RoomId(3), 1).tick(); // MigrateIn; Relocated → router
    s.router.pump(); // place = shard 1; Seal → shard 0
    assert_eq!(s.router.place(&bob()), Some(Place::In(RoomId(3), 1)));
    for _ in 0..3 {
        bank.push(&bob(), &gr(1)).unwrap(); // seq 4..6, DIRECT to shard 1
    }
    s.router.pump();
    s.shard_mut(RoomId(3), 1).tick(); // direct legs arrive first
    s.shard_mut(RoomId(3), 0).tick(); // the source forwards 1..3 (+ fence)
    s.shard_mut(RoomId(3), 1).tick();
    s.shard_mut(RoomId(3), 1).tick();

    assert_eq!(gold(s.shard(RoomId(3), 1).game(), "bob"), Some(6), "nothing lost either way");
    let c = s.shard(RoomId(3), 0).counts().clone();
    assert_eq!(c.forwarded, 3);
    seqs(&s, RoomId(3), 1)
}

#[test]
fn a_migration_keeps_per_publisher_fifo_with_the_fence() {
    assert_eq!(migration(true), vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn without_the_fence_a_migration_reorders() {
    assert_eq!(migration(false), vec![4, 5, 6, 1, 2, 3]);
}

#[test]
fn a_chained_migration_keeps_fifo_too() {
    let mut s = Server::<Wallets>::new(64);
    s.sharded(RoomId(4), vec![Wallets::default(), Wallets::default(), Wallets::default()], Caps::default());
    let bank = s.router.publisher();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(4), 0, 8, 0);
    s.step();
    let r = RoomId(4);

    bank.push(&bob(), &gr(1)).unwrap(); // 1
    bank.push(&bob(), &gr(1)).unwrap(); // 2
    s.router.pump(); // → shard 0
    s.shard_mut(r, 0).migrate_out(&bob(), 1); // A → B (mig 1)
    s.shard_mut(r, 1).tick(); // B: MigrateIn, hold
    s.router.pump(); // place B; seal A
    bank.push(&bob(), &gr(1)).unwrap(); // 3
    bank.push(&bob(), &gr(1)).unwrap(); // 4
    s.router.pump(); // → B direct
    s.shard_mut(r, 1).tick(); // B holds 3, 4
    s.shard_mut(r, 1).migrate_out(&bob(), 2); // B → C (mig 2) before A's fence
    s.shard_mut(r, 2).tick(); // C: MigrateIn, hold
    s.router.pump(); // place C; seal B
    bank.push(&bob(), &gr(1)).unwrap(); // 5
    s.router.pump(); // → C direct
    for _ in 0..4 {
        s.shard_mut(r, 2).tick(); // C first every round: the worst order
        s.shard_mut(r, 1).tick();
        s.shard_mut(r, 0).tick();
    }
    s.shard_mut(r, 2).tick();

    assert_eq!(seqs(&s, r, 2), vec![1, 2, 3, 4, 5]);
    assert_eq!(gold(s.shard(r, 2).game(), "bob"), Some(5));
    for i in 0..3 {
        let c = s.shard(r, i).counts();
        assert_eq!((c.fence_timeouts, c.fence_lost, c.not_here), (0, 0, 0), "shard {i}");
    }
}

#[test]
fn park_and_resume_keep_routing_and_the_client_sees_its_gap() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    let chat = s.router.publisher();
    let bank = s.router.publisher();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(1), 0, 8, 0);
    s.step();
    let line = |t: &str| ChatMessage {
        channel: "world".into(),
        from: "carol".into(),
        text: t.into(),
    };

    chat.push(&bob(), &line("hi")).unwrap(); // client seq 1
    s.step();
    assert_eq!(s.client("bob").take::<ChatMessage>().len(), 1);

    s.park("bob");
    chat.push(&bob(), &line("missed")).unwrap(); // seq 2: no session
    bank.push(&bob(), &gr(70)).unwrap(); // WORLD: the parked entity is still there
    s.step();
    assert_eq!(s.router.counts().client_parked, 1);
    assert_eq!(gold(s.shard(RoomId(1), 0).game(), "bob"), Some(70), "the room reacts while parked");

    s.resume("bob", Link::default(), 0);
    chat.push(&bob(), &line("back")).unwrap(); // seq 3
    s.step();
    let got = s.client("bob").take::<ChatMessage>();
    assert_eq!(got.iter().map(|e| (e.seq, e.body.text.as_str())).collect::<Vec<_>>(), vec![(3, "back")]);
    let gaps = s.client("bob").take_gaps();
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].missed, 1, "the client knows it missed one and re-syncs");
    assert_eq!(gaps[0].from, chat.id());
}

#[test]
fn leaving_between_route_and_drain_is_counted_not_here() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    let bank = s.router.publisher();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(1), 0, 8, 0);
    s.step();
    bank.push(&bob(), &gr(5)).unwrap();
    s.router.pump(); // in the room's mailbox
    s.leave("bob"); // CONTROL runs before EVENTS
    s.step();
    assert_eq!(s.shard(RoomId(1), 0).counts().not_here, 1);
    bank.push(&bob(), &gr(5)).unwrap();
    s.step();
    assert_eq!(s.router.counts().world_no_room, 1, "between rooms: counted at the router");
}
