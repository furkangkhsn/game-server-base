//! Events at the room boundary (round e5, `docs/EVENTS.md` §11): TEST
//! mode's publisher (`Sim::publisher`, a push before step `T` is in step
//! `T`'s EVENTS) and — feature `record` — a live run with events pushed
//! from two threads at jittered times, recorded, replayed twice to the
//! same hashes without any publisher; one tick's events moved a tick
//! later diverge at exactly that tick; the log survives a file.

use crate::calls::{Envelope, Identity};
use crate::determinism::{Delay, Sim, SimRoom};
use crate::id::{PlayerId, RoomId};
use crate::room::RoomConfig;

use super::game::*;

fn config(seed: u64) -> RoomConfig {
    RoomConfig {
        id: RoomId(42),
        seed: Some(seed),
        tick_hz: 30.0,
        keepalive_hz: 0.0,
        metrics_cadence_hz: 0.0,
        ..Default::default()
    }
}

fn shop(sim: &mut Sim, publisher: Option<crate::events::Publisher>) -> Shop {
    let bank = sim.service(
        "bank",
        64,
        Delay::ticks(2, |env: Envelope<BankMsg>| async move { bank_answer(env) }),
    );
    let inv = sim.service(
        "inventory",
        64,
        Delay::ticks(1, |env: Envelope<InvMsg>| async move { inv_answer(env) }),
    );
    Shop {
        bank,
        inv,
        rolls: 0,
        publisher,
    }
}

/// A CLIENT-only event (never a room's).
#[derive(Clone, PartialEq, prost::Message)]
struct Toast {
    #[prost(uint32, tag = "1")]
    n: u32,
}
impl crate::events::Event for Toast {
    const OP: u16 = 1290;
    const TO: crate::events::Dest = crate::events::Dest::Client;
}
impl crate::events::ClientEvent for Toast {}

/// TEST mode: a push made before step `T` is taken at step `T`'s EVENTS
/// — exactly then; addressed to a member it reaches that member's owner,
/// room-wide every player; an identity that is no member's, another
/// room and a CLIENT-only event never reach this room. The gold received
/// pays a buy of the same step (EVENTS before READ, the take before the
/// request in the systems).
#[test]
fn a_sim_publisher_reaches_the_room_at_the_next_step() {
    let mut sim = Sim::new();
    let bank_pub = sim.publisher("bank");
    assert_eq!(bank_pub.id().0, 1, "minted from 1");
    assert_eq!(sim.publisher("bank").id(), bank_pub.id(), "one id per name");
    let logic = shop(&mut sim, None);
    let mut room = SimRoom::new(config(3), World::new(), Box::new(logic));
    let mut alice = room.join(1, "alice");
    let _bob = room.join(2, "bob");
    for _ in 0..3 {
        room.tick(&mut sim); // the bags load
    }
    let gold = |room: &SimRoom<World>, p: u64| room.world()[&PlayerId(p)].gold;
    assert_eq!((gold(&room, 1), gold(&room, 2)), (500, 500));

    // Before step 4: 700 to alice (she buys for 1000 in the same step),
    // 5 to everyone, and what the room must not see.
    let alice_id = Identity::new("alice");
    bank_pub
        .push(&alice_id, &GoldReceived { amount: 700 })
        .expect("pushed");
    bank_pub
        .push_room(RoomId(42), &GoldReceived { amount: 5 })
        .expect("pushed");
    bank_pub
        .push(&Identity::new("carol"), &GoldReceived { amount: 1 })
        .expect("pushed");
    bank_pub
        .push_room(RoomId(7), &GoldReceived { amount: 1 })
        .expect("pushed");
    bank_pub.push(&alice_id, &Toast { n: 1 }).expect("pushed");
    alice.request(1, &Buy { price: 1000 });
    room.tick(&mut sim);
    assert_eq!((gold(&room, 1), gold(&room, 2)), (1205, 505), "at step 4");
    let c = room.event_counts();
    assert_eq!((c.events_admitted, c.events_taken), (2, 2), "{c:?}");
    assert_eq!(c.events_world_not_here, 0, "no leg for a non-member");
    for _ in 0..3 {
        room.tick(&mut sim);
    }
    let got = answers(&mut alice);
    assert_eq!(got.len(), 1);
    assert_eq!(verdict(&got[0]), Ok(205), "paid by the gold received");
}

#[cfg(feature = "record")]
mod recorded {
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::calls::mix;
    use crate::determinism::{
        Codecs, Input, InputLog, Recorder, Replay, ReplayReport, fresh_seed, read_log, write_log,
    };
    use crate::events::{Publisher, PublisherId};

    struct Live {
        log: Arc<InputLog>,
        hashes: Vec<Option<u64>>,
        codecs: Codecs,
        admitted: u64,
        /// What the room itself pushed live (its cashback).
        room_pushed: u64,
    }

    /// Two threads push 60 events at jittered times (0–8 ms) while the
    /// room steps on the wall clock; two clients buy.
    fn live() -> Live {
        let mut sim = Sim::new();
        let room_pub = sim.publisher("shop");
        let logic = shop(&mut sim, Some(room_pub.clone()));
        let mut room = SimRoom::new(config(fresh_seed(42)), World::new(), Box::new(logic));
        let (recorder, mut inbox) = Recorder::channel(1 << 14, "shop-e5");
        room.record(recorder);
        let alice = room.join(1, "alice");
        let bob = room.join(2, "bob");
        let base = fresh_seed(7);
        let pushers: Vec<_> = (0..2u64)
            .map(|k| {
                let p = sim.publisher(if k == 0 { "bank" } else { "gifts" });
                thread::spawn(move || {
                    let (alice, bob) = (Identity::new("alice"), Identity::new("bob"));
                    for i in 0..30u64 {
                        let wait = mix(base ^ (k << 32) ^ i) % 9;
                        thread::sleep(Duration::from_millis(wait));
                        let e = GoldReceived { amount: 10 + i };
                        let pushed = match (k, i % 3) {
                            (0, _) => p.push(&alice, &e),
                            (_, 0) => p.push_all(&e),
                            _ => p.push_many(&[alice.clone(), bob.clone()], &e),
                        };
                        pushed.expect("the sim router has room");
                    }
                })
            })
            .collect();
        let mut t = 0u64;
        // Once the pushers ended and nothing is pending: two more steps
        // take what is still on its way (the last cashback).
        let mut tail = 0;
        while tail < 2 {
            t += 1;
            assert!(t < 2000, "the pushers end");
            match t {
                3..=30 if t.is_multiple_of(4) => alice.request(t, &Buy { price: 300 }),
                5..=30 if t.is_multiple_of(5) => bob.request(t, &Buy { price: 450 }),
                _ => {}
            }
            thread::sleep(Duration::from_millis(2));
            room.deliver(&mut sim);
            assert!(room.tick_at(Instant::now()));
            sim.step(t);
            if t >= 40 && pushers.iter().all(|h| h.is_finished()) && room.calls().pending_len() == 0
            {
                tail += 1;
            }
        }
        for h in pushers {
            h.join().expect("pusher");
        }
        let log = InputLog::collect(&mut inbox).expect("the header arrived");
        let admitted = room.event_counts().events_admitted;
        assert!(admitted >= 60, "the threads' legs and more: {admitted}");
        let room_pushed = room_pub.stats().counts().events_pushed;
        assert!(room_pushed >= 2, "the room paid cashback: {room_pushed}");
        Live {
            log: Arc::new(log),
            hashes: room.hashes().to_vec(),
            codecs: room.calls().codecs().clone(),
            admitted,
            room_pushed,
        }
    }

    /// Replay `log` with a detached room publisher: the verdict and the
    /// handle's detached count.
    fn replay(log: &Arc<InputLog>) -> (ReplayReport, u64) {
        let publisher = Publisher::detached(PublisherId(1), "shop");
        let logic = Shop {
            bank: crate::calls::Route::new("bank", 1).0,
            inv: crate::calls::Route::new("inventory", 1).0,
            rolls: 0,
            publisher: Some(publisher.clone()),
        };
        let mut r = Replay::new(Arc::clone(log), World::new(), Box::new(logic));
        let report = r.run();
        assert_eq!(publisher.stats().counts().events_pushed, 0);
        (report, publisher.stats().detached())
    }

    fn recorded_events(log: &InputLog) -> u64 {
        log.ticks
            .iter()
            .flat_map(|t| &t.inputs)
            .map(|i| match i {
                Input::Events(list) => list.len() as u64,
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn a_live_run_with_events_replays_to_the_same_hashes_without_a_publisher() {
        let l = live();
        assert_eq!(l.log.hashes(), l.hashes);
        assert_eq!(recorded_events(&l.log), l.admitted, "every admitted leg");
        for _ in 0..2 {
            let (report, detached) = replay(&l.log);
            assert_eq!(report.hashes, l.hashes, "every step equals the live run's");
            assert_eq!(report.first_divergent, None);
            assert_eq!(report.counts.replay_diverged, 0);
            assert_eq!(report.events.events_admitted, l.admitted);
            assert_eq!(report.events.events_taken, l.admitted);
            assert_eq!(
                detached, l.room_pushed,
                "the room pushed as live, into nothing"
            );
        }

        // Negative control: one tick's events a tick later diverge at
        // exactly that tick.
        let mut log = Arc::try_unwrap(l.log).expect("sole owner");
        let (i, at) = log
            .ticks
            .iter()
            .enumerate()
            .find_map(|(i, t)| {
                t.inputs
                    .iter()
                    .position(|x| matches!(x, Input::Events(_)))
                    .map(|at| (i, at))
            })
            .expect("an EVENTS input");
        let moved = log.ticks[i].inputs.remove(at);
        log.ticks[i + 1].inputs.insert(0, moved);
        let late = log.ticks[i].tick;
        let (report, _) = replay(&Arc::new(log));
        assert_eq!(report.first_divergent, Some(late), "at the moved tick");
    }

    #[test]
    fn a_log_file_with_events_replays_like_the_log_in_memory() {
        let l = live();
        let path = std::env::temp_dir().join(format!("gsb-e5-{}-events.log", std::process::id()));
        write_log(&path, &l.log, &l.codecs).expect("written");
        let read = read_log(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        assert_eq!(recorded_events(&read), l.admitted);
        let first = |log: &InputLog| {
            log.ticks
                .iter()
                .flat_map(|t| &t.inputs)
                .find_map(|i| match i {
                    Input::Events(list) => Some(list.clone()),
                    _ => None,
                })
                .expect("an EVENTS input")
        };
        assert_eq!(
            first(&read),
            first(&l.log),
            "the events read back as written"
        );
        let (report, _) = replay(&Arc::new(read));
        assert_eq!(report.hashes, l.hashes);
        assert_eq!(report.first_divergent, None);
    }

    /// A recorded event the replaying game does not take (its op not
    /// accepted) is `replay_diverged`, reported at its tick.
    #[test]
    fn a_recorded_event_the_replay_does_not_admit_diverges() {
        let mut sim = Sim::new();
        let bank = sim.publisher("bank");
        let logic = shop(&mut sim, None);
        let mut room = SimRoom::new(config(5), World::new(), Box::new(logic));
        let (recorder, mut inbox) = Recorder::channel(64, "shop-e5");
        room.record(recorder);
        let _alice = room.join(1, "alice");
        room.tick(&mut sim);
        bank.push(&Identity::new("alice"), &GoldReceived { amount: 3 })
            .expect("pushed");
        room.tick(&mut sim);
        room.tick(&mut sim);
        let log = Arc::new(InputLog::collect(&mut inbox).expect("header"));
        let (report, _) = replay(&log);
        assert_eq!(report.first_divergent, None);
        assert_eq!(report.events.events_admitted, 1);

        let mut log = Arc::try_unwrap(log).expect("sole owner");
        let at = log.ticks[1].tick;
        for i in &mut log.ticks[1].inputs {
            if let Input::Events(list) = i {
                list[0].op = 1299;
            }
        }
        let (report, _) = replay(&Arc::new(log));
        assert_eq!(report.counts.replay_diverged, 1);
        assert_eq!(report.events.events_no_handler, 1);
        assert_eq!(report.first_divergent, Some(at));
    }
}
