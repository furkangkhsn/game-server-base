//! Determinism at the room boundary.
//!
//! 1. A LIVE run (tokio services with random 0–8 ms latencies, real
//!    sleeps between ticks, measured dt) is recorded: per tick, the
//!    control inputs, the responses in arrival order, the reads, dt.
//! 2. REPLAY mode runs the same game twice from the log: no service is
//!    called, recorded outcomes arrive at their recorded ticks by key —
//!    and every per-tick state hash equals the live run's.
//! 3. TEST mode (fixed dt, seeded rng, `Delay::ticks`) is identical run
//!    to run, and a different seed is a different run.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use s0_calls::room::RoomLogic;

const LIVE_TICKS: u64 = 80;

#[test]
fn a_live_run_replays_to_the_same_hashes_without_calling_services() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap();
    let be = FakeBackend::with_jitter(8);
    let mut server = Server::new(rt.handle().clone());
    server.service(Bank::serve(be.clone()), |s| {
        s.timeout(Duration::from_secs(2))
    });
    server.service(Inventory::serve(be.clone()), |s| {
        s.timeout(Duration::from_secs(2))
    });
    server.service(Friends::serve(be.clone()), |s| s);

    let cfg = RoomConfig {
        id: 42,
        seed: 0x5eed,
        share: 64,
    };
    let mut room = bevy_game::room(cfg, Clients::from(&server));
    room.record();
    let mut live = Answers::new();
    let mut last = Instant::now();
    for t in 0..LIVE_TICKS {
        script(t, &room.inputs());
        std::thread::sleep(Duration::from_millis(2));
        let dt = last.elapsed().as_secs_f32();
        last = Instant::now();
        for (c, r) in room.tick(dt) {
            live.push((t, c, r));
        }
    }
    let log = Arc::new(room.take_log().expect("recording"));
    assert_eq!(log.ticks.len() as u64, LIVE_TICKS);
    let arrivals: Vec<usize> = log.ticks.iter().map(|t| t.responses.len()).collect();
    // How many calls the run makes depends on latency (a load that lands
    // after tick 3 turns the transfer into NOT_LOADED) — which is exactly
    // why the run must be recorded to be reproduced.
    assert!(
        arrivals.iter().sum::<usize>() >= 2,
        "at least the two loads: {arrivals:?}"
    );
    assert_eq!(
        room.logic_mut().with_calls(|c, _| c.pending_len()),
        0,
        "nothing left in flight"
    );
    let calls_after_live = be.calls();

    for _ in 0..2 {
        let mut replay = Room::replay(
            Arc::clone(&log),
            64,
            bevy_game::build(Clients::from(&ReplayHost)),
        );
        let mut again = Answers::new();
        let mut t = 0;
        while replay.replay_left() > 0 {
            for (c, r) in replay.tick(0.0) {
                again.push((t, c, r));
            }
            t += 1;
        }
        assert_eq!(
            replay.hashes, log.hashes,
            "per-tick state equals the live run"
        );
        assert_eq!(again, live, "the same answers, at the same ticks");
        assert_eq!(replay.counters().replay_diverged, 0);
    }
    assert_eq!(
        be.calls(),
        calls_after_live,
        "no service was called during replay"
    );

    // Negative control: the hashes see arrival timing. Deliver the first
    // recorded response one tick later and the replay diverges.
    let mut log = Arc::try_unwrap(log).ok().expect("sole owner");
    let first = log
        .ticks
        .iter()
        .position(|t| !t.responses.is_empty())
        .expect("a response");
    let moved = std::mem::take(&mut log.ticks[first].responses);
    log.ticks[first + 1].responses.splice(0..0, moved);
    let log = Arc::new(log);
    let mut late = Room::replay(
        Arc::clone(&log),
        64,
        bevy_game::build(Clients::from(&ReplayHost)),
    );
    late.replay_all();
    assert_ne!(
        late.hashes, log.hashes,
        "one tick of lateness changes the run"
    );
    // (If the shift made the game issue different calls, a recorded
    // answer may meet a call of another type: `replay_diverged` counts it,
    // the answer is never applied to the wrong call.)
}

#[test]
fn test_mode_is_identical_run_to_run() {
    let run = |seed: u64| {
        let be = FakeBackend::new();
        let mut sim = sim(&be);
        let mut room = hecs_game::room(
            RoomConfig {
                seed,
                ..RoomConfig::default()
            },
            Clients::from(&sim),
        );
        room.hash_every_tick();
        let answers = run_sim(&mut room, &mut sim, SCENARIO_TICKS);
        (room.hashes.clone(), answers)
    };
    let a = run(1);
    let b = run(1);
    assert_eq!(a, b);
    let c = run(2);
    assert_ne!(a.0, c.0, "the seed is part of the run");
}
