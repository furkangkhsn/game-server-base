//! Events are recorded inputs (docs/EVENTS.md §11): a live run whose
//! events land in whatever tick the threads' timing gives them is
//! recorded; two replays of the record give the same per-tick hashes,
//! with no router and no publisher; moving one tick's events to the next
//! tick makes the hashes diverge exactly there.

use std::time::{Duration, Instant};

use e0_events::contract::{ConnId, Identity, Owner, RoomId};
use e0_events::game::{BalanceChanged, GoldReceived};
use e0_events::harness::Wallets;
use e0_events::record::Input;
use e0_events::room::{Action, Caps, Completion, Control, Room};
use e0_events::router::{ConnLink, Router, RouterMsg};
use e0_events::wire::FEATURE_PUSH;
use tokio::sync::mpsc;

fn id(s: &str) -> Identity {
    Identity::new(s)
}

#[test]
fn a_live_run_replays_to_the_same_hashes() {
    let mut router = Router::new(4096);
    let bank = router.publisher();
    let shop = router.publisher();
    let (mut room, h) = Room::new(RoomId(7), Wallets::default(), Caps::default());
    room.record();
    router.handle(RouterMsg::RegisterRoom {
        room: RoomId(7),
        mailboxes: vec![h.events.clone()],
    });
    let mut inputs = Vec::new();
    let mut keep = Vec::new();
    for (i, n) in ["alice", "bob"].into_iter().enumerate() {
        let (inbox, rx) = mpsc::channel(8);
        keep.push(rx);
        router.handle(RouterMsg::Online {
            identity: id(n),
            conn: ConnLink {
                id: ConnId(i as u64),
                inbox,
                features: FEATURE_PUSH,
            },
        });
        router.handle(RouterMsg::Joined {
            identity: id(n),
            room: RoomId(7),
            shard: None,
        });
        let (tx, input) = mpsc::channel(8);
        inputs.push(tx);
        h.control
            .try_send(Control::Join {
                identity: id(n),
                conn: ConnId(i as u64),
                input,
                carried: 10,
            })
            .unwrap();
    }

    // Publishers on their own threads, jittered by the clock.
    let seed = Instant::now().elapsed().subsec_nanos() as u64 | 1;
    let threads: Vec<_> = [(bank, "bob", seed), (shop, "alice", seed.rotate_left(17))]
        .into_iter()
        .map(|(p, who, mut x)| {
            std::thread::spawn(move || {
                for i in 0..30u64 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    std::thread::sleep(Duration::from_micros(200 + x % 2500));
                    if i % 3 == 0 {
                        let _ = p.push(
                            &id(who),
                            &BalanceChanged {
                                balance: 1000 + i,
                                version: i + 1,
                                reason: "shop".into(),
                            },
                        );
                    } else {
                        let _ = p.push(
                            &id(who),
                            &GoldReceived {
                                from: "x".into(),
                                amount: i,
                                balance: 0,
                                version: 0,
                            },
                        );
                    }
                }
            })
        })
        .collect();
    let completions = h.completions.clone();
    for t in 0..120u64 {
        router.pump();
        if t % 7 == 3 {
            let _ = inputs[1].try_send(Action { op: 1, amount: 4 });
        }
        if t % 11 == 5 {
            let _ = completions.try_send(Completion {
                owner: Owner(1),
                value: 500 + t,
            });
        }
        room.tick();
        std::thread::sleep(Duration::from_millis(1));
    }
    for t in threads {
        t.join().unwrap();
    }
    for _ in 0..3 {
        router.pump();
        room.tick();
    }
    let log = room.take_record();
    let live: Vec<u64> = log.iter().map(|t| t.hash).collect();
    let event_ticks = log
        .iter()
        .filter(|t| t.inputs.iter().any(|i| matches!(i, Input::Events(v) if !v.is_empty())))
        .count();
    assert!(event_ticks > 5, "events spread over many ticks: {event_ticks}");
    assert_eq!(room.events().counts().admitted, 60);

    for _ in 0..2 {
        assert_eq!(Room::replay(RoomId(7), Wallets::default(), &log), live);
    }

    // Negative control: one tick's events, one tick late.
    let mut shifted = log.clone();
    let k = shifted
        .iter()
        .position(|t| t.inputs.iter().any(|i| matches!(i, Input::Events(v) if !v.is_empty())))
        .unwrap();
    let moved = shifted[k]
        .inputs
        .iter_mut()
        .find_map(|i| match i {
            Input::Events(v) => Some(std::mem::take(v)),
            _ => None,
        })
        .unwrap();
    for i in &mut shifted[k + 1].inputs {
        if let Input::Events(v) = i {
            let mut all = moved.clone();
            all.append(v);
            *v = all;
        }
    }
    let replayed = Room::replay(RoomId(7), Wallets::default(), &shifted);
    let first = replayed.iter().zip(&live).position(|(a, b)| a != b);
    assert_eq!(first, Some(k), "the hash sees the event's tick");
}
