//! The room's own channel, the phase order, the CLIENT-direct leg, BOTH,
//! room/all addressing, and push_many's single encode (docs/EVENTS.md
//! §3, §5, §6).

use std::sync::atomic::Ordering::Relaxed;

use e0_events::contract::{Identity, RoomId};
use e0_events::game::{BalanceChanged, ChatMessage, FriendOnline, GoldReceived};
use e0_events::harness::{Link, Server, Wallets, gold};
use e0_events::room::{Action, Caps};

fn gr(amount: u64) -> GoldReceived {
    GoldReceived {
        from: "alice".into(),
        amount,
        balance: 0,
        version: 0,
    }
}

fn id(s: &str) -> Identity {
    Identity::new(s)
}

/// s0 routed WORLD pushes through the member's INPUT channel: a client
/// that flooded it dropped its own pushes. Here the room has its own
/// mailbox: the flood drops only the flood.
#[test]
fn a_spamming_client_does_not_lose_its_events() {
    let mut s = Server::<Wallets>::new(1024);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    let bank = s.router.publisher();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(1), 0, 8, 0);
    s.step();

    let mut dropped = 0u64;
    for _ in 0..10 {
        let input = s.players[&id("bob")].input.clone().expect("in a room");
        for _ in 0..1000 {
            if input.try_send(Action { op: 9, amount: 0 }).is_err() {
                dropped += 1;
            }
        }
        for _ in 0..5 {
            bank.push(&id("bob"), &gr(1)).unwrap();
        }
        s.step();
    }
    let room = s.shard(RoomId(1), 0);
    assert!(dropped > 9_000, "the flood dropped itself: {dropped}");
    assert_eq!(room.game().seen.len(), 50, "every event arrived");
    assert_eq!(gold(room.game(), "bob"), Some(50));
    assert_eq!(room.events().counts().admitted, 50);
}

/// CONTROL → RESPONSES → EVENTS → READ → SYSTEMS → END in every tick.
#[test]
fn the_phase_order_is_control_responses_events_read() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    s.shard_mut(RoomId(1), 0).record();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(1), 0, 8, 0);
    for _ in 0..3 {
        s.step();
    }
    let log = s.shard_mut(RoomId(1), 0).take_record();
    for t in &log {
        let phases: Vec<&str> = t.inputs.iter().map(|i| i.phase()).filter(|p| *p != "CONTROL").collect();
        assert_eq!(phases, ["RESPONSES", "EVENTS", "READ", "SYSTEMS", "END"]);
        // CONTROL inputs, when present, come first.
        let first_other = t.inputs.iter().position(|i| i.phase() != "CONTROL").unwrap();
        assert!(t.inputs[first_other..].iter().all(|i| i.phase() != "CONTROL"));
    }
    assert_eq!(log[0].inputs[0].phase(), "CONTROL", "the join");
}

/// Why EVENTS runs before READ: gold that arrived this tick pays for a
/// purchase made this tick (the game takes events, then input).
#[test]
fn gold_arriving_this_tick_pays_for_a_purchase_this_tick() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    let bank = s.router.publisher();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(1), 0, 8, 0);
    s.step();

    bank.push(&id("bob"), &gr(500)).unwrap();
    s.players[&id("bob")].input.as_ref().unwrap().try_send(Action { op: 1, amount: 500 }).unwrap();
    s.step();
    let g = s.shard(RoomId(1), 0).game();
    assert_eq!(g.bought, vec![("bob".to_owned(), 500)]);
    assert_eq!(g.refused, 0);
    assert_eq!(gold(g, "bob"), Some(0));
}

#[test]
fn a_client_event_never_touches_a_room() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    s.shard_mut(RoomId(1), 0).record();
    let chat = s.router.publisher();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(1), 0, 8, 0);
    s.step();

    chat.push(
        &id("bob"),
        &ChatMessage {
            channel: "guild".into(),
            from: "carol".into(),
            text: "gg".into(),
        },
    )
    .unwrap();
    s.step();
    let got = s.client("bob").take::<ChatMessage>();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body.text, "gg");
    assert_eq!(s.router.counts().world_sent, 0);
    assert_eq!(s.router.counts().client_sent, 1);
    let room = s.shard_mut(RoomId(1), 0);
    assert_eq!(room.events().counts().admitted + room.events().counts().no_handler, 0);
    let log = room.take_record();
    assert!(
        log.iter().flat_map(|t| &t.inputs).all(|i| !matches!(i, e0_events::record::Input::Events(v) if !v.is_empty())),
        "the room's EVENTS phase drained nothing"
    );
}

/// BOTH: the room updates the wallet AND the client gets a toast; the
/// legs count separately, so a WORLD-only event from the same publisher
/// is not a gap on the client.
#[test]
fn both_runs_both_legs_with_their_own_numbers() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    let shop = s.router.publisher();
    s.connect("bob", Link::default());
    s.join("bob", RoomId(1), 0, 8, 10);
    s.step();

    shop.push(&id("bob"), &gr(5)).unwrap(); // WORLD seq 1
    let bc = |v: u64| BalanceChanged {
        balance: 1000 + v,
        version: v,
        reason: "web shop".into(),
    };
    shop.push(&id("bob"), &bc(1)).unwrap(); // WORLD seq 2, CLIENT seq 1
    shop.push(&id("bob"), &bc(2)).unwrap(); // WORLD seq 3, CLIENT seq 2
    s.step();

    assert_eq!(gold(s.shard(RoomId(1), 0).game(), "bob"), Some(1002));
    let toasts = s.client("bob").take::<BalanceChanged>();
    assert_eq!(toasts.iter().map(|t| t.seq).collect::<Vec<_>>(), vec![1, 2]);
    assert!(s.client("bob").take_gaps().is_empty(), "WORLD legs are not client gaps");
    let world: Vec<u64> = s.shard(RoomId(1), 0).game().seen.iter().map(|e| e.2).collect();
    assert_eq!(world, vec![1, 2, 3]);
}

#[test]
fn push_room_and_push_all_address_rooms_and_sessions() {
    let mut s = Server::<Wallets>::new(64);
    s.room(RoomId(1), Wallets::default(), Caps::default());
    s.room(RoomId(2), Wallets::default(), Caps::default());
    let ops = s.router.publisher();
    for (n, r) in [("a", 1), ("b", 1), ("c", 2)] {
        s.connect(n, Link::default());
        s.join(n, RoomId(r), 0, 8, 0);
    }
    s.connect("lobby", Link::default()); // online, in no room
    s.step();

    let bc = BalanceChanged {
        balance: 0,
        version: 0,
        reason: "double gold weekend".into(),
    };
    ops.push_room(RoomId(1), &bc).unwrap();
    s.step();
    assert_eq!(s.shard(RoomId(1), 0).game().notes, vec!["double gold weekend"]);
    assert!(s.shard(RoomId(2), 0).game().notes.is_empty());
    for (n, want) in [("a", 1), ("b", 1), ("c", 0), ("lobby", 0)] {
        assert_eq!(s.client(n).take::<BalanceChanged>().len(), want, "{n}");
    }

    ops.push_all(&bc).unwrap();
    s.step();
    assert_eq!(s.shard(RoomId(2), 0).game().notes.len(), 1);
    for n in ["a", "b", "c", "lobby"] {
        assert_eq!(s.client(n).take::<BalanceChanged>().len(), 1, "{n}");
    }
}

/// One encode, one router message, one shared body for 1000 recipients;
/// the per-recipient work (a header + a copy) is in their connection
/// actors.
#[test]
fn push_many_encodes_once_and_shares_the_body() {
    let mut s = Server::<Wallets>::new(64);
    let friends = s.router.publisher();
    let names: Vec<String> = (0..1000).map(|i| format!("p{i}")).collect();
    for n in &names {
        s.connect(n, Link::default());
    }
    let ids: Vec<Identity> = names.iter().map(|n| id(n)).collect();
    friends.push_many(&ids, &FriendOnline { who: "alice".into() }).unwrap();
    assert_eq!(s.router.pump(), 1, "one router message");
    s.step();
    let mut ptrs = std::collections::BTreeSet::new();
    for p in s.players.values() {
        ptrs.extend(p.actor.seen_bodies.iter().copied());
    }
    assert_eq!(friends.stats().encodes.load(Relaxed), 1);
    assert_eq!(ptrs.len(), 1, "every connection got the same Bytes");
    assert_eq!(s.router.counts().client_sent, 1000);
    for n in &names {
        assert_eq!(s.client(n).take::<FriendOnline>().len(), 1);
    }
}
