//! Record → replay (feature `record`, SERVICES.md §10.2): a LIVE run —
//! tokio services with random latencies, real sleeps, measured `dt`, a
//! fresh seed — is recorded; two replays of its log reproduce every
//! step's hash and every client answer without calling a service; one
//! response moved a tick later diverges at exactly that tick; a recorded
//! ending that names another call type is `replay_diverged`, never
//! applied; and the log survives a file.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::game::*;
use crate::calls::{Envelope, Route, mix};
use crate::determinism::{
    Codecs, Delay, Input, InputLog, Recorder, Replay, Sim, SimRoom, fresh_seed, read_log,
    spawn_writer, write_log,
};
use crate::room::RoomConfig;

type Answers = Vec<(u64, u64, gsb_protocol::base::RpcResponse)>;

fn config(seed: u64) -> RoomConfig {
    RoomConfig {
        id: crate::id::RoomId(42),
        seed: Some(seed),
        tick_hz: 30.0,
        keepalive_hz: 0.0,
        metrics_cadence_hz: 0.0,
        ..Default::default()
    }
}

/// Routes to nowhere (a replay never sends).
fn nowhere() -> Shop {
    Shop {
        bank: Route::new("bank", 1).0,
        inv: Route::new("inventory", 1).0,
        rolls: 0,
        publisher: None,
    }
}

/// The script both runs follow, by tick.
fn script(t: u64, alice: &crate::determinism::SimClient, bob: &crate::determinism::SimClient) {
    match t {
        1 | 2 => alice.request(t, &Buy { price: 50 }),
        5 => {
            alice.request(10, &Buy { price: 100 });
            bob.request(10, &Buy { price: 30 });
        }
        6 => alice.request(11, &Buy { price: 1 }),
        9..=21 => {
            bob.request(100 + t, &Buy { price: 7 });
        }
        24 => alice.request(200, &Buy { price: 900 }),
        _ => {}
    }
}

/// Replay `log` once: its hashes, its answers, the replay's verdict.
fn replay(log: &Arc<InputLog>) -> (crate::determinism::ReplayReport, Answers) {
    let mut r = Replay::new(Arc::clone(log), World::new(), Box::new(nowhere()));
    let mut answers = Answers::new();
    while let Some(t) = r.tick() {
        for (c, a) in &t.answers {
            answers.push((t.tick, c.0, a.into()));
        }
    }
    (r.run(), answers)
}

struct Live {
    log: Arc<InputLog>,
    hashes: Vec<Option<u64>>,
    answers: Answers,
    codecs: Codecs,
    counts: crate::metrics::CallCounts,
    /// Calls each service received.
    served: Arc<[AtomicU64; 2]>,
}

/// The live run, recorded.
fn live() -> Live {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("runtime");
    let _in = rt.enter();
    let base = fresh_seed(1);
    let n = Arc::new(AtomicU64::new(0));
    let latency = move || Duration::from_millis(mix(base ^ n.fetch_add(1, Ordering::Relaxed)) % 9);
    let served: Arc<[AtomicU64; 2]> = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let (bank, _bank_svc) = {
        let latency = latency.clone();
        let served = Arc::clone(&served);
        crate::calls::serve("bank", 64, move |env: Envelope<BankMsg>| {
            served[0].fetch_add(1, Ordering::Relaxed);
            let wait = latency();
            async move {
                tokio::time::sleep(wait).await;
                bank_answer(env);
            }
        })
    };
    let (inv, _inv_svc) = {
        let served = Arc::clone(&served);
        crate::calls::serve("inventory", 64, move |env: Envelope<InvMsg>| {
            served[1].fetch_add(1, Ordering::Relaxed);
            let wait = latency();
            async move {
                tokio::time::sleep(wait).await;
                inv_answer(env);
            }
        })
    };
    let mut room = SimRoom::new(
        config(fresh_seed(42)),
        World::new(),
        Box::new(Shop {
            bank,
            inv,
            rolls: 0,
            publisher: None,
        }),
    );
    let (recorder, mut inbox) = Recorder::channel(1 << 14, "shop-1");
    room.record(recorder);
    let mut alice = room.join(1, "alice");
    let mut bob = room.join(2, "bob");
    let mut answers = Answers::new();
    let mut t = 0;
    while t < 40 || room.calls().pending_len() > 0 {
        t += 1;
        assert!(t < 2000, "the services answer");
        script(t, &alice, &bob);
        match t {
            // A legacy request (the built-in External call) and tips
            // (actions through `ingest`): live only (a runtime runs the
            // External future).
            3 => alice.raw_request(30, OP_PING, Vec::new()),
            7 | 8 => alice.action(OP_TIP, bytes::Bytes::new()),
            _ => {}
        }
        if t == 22 {
            // Bob leaves with a charge in flight: its result finds no
            // owner (`responses_owner_gone`), live and replayed.
            room.leave(&bob);
        }
        std::thread::sleep(Duration::from_millis(2));
        assert!(room.tick_at(Instant::now()));
        for (who, c) in [(1u64, &mut alice), (2, &mut bob)] {
            for a in answers_of(c) {
                answers.push((t, who, a));
            }
        }
    }
    let log = InputLog::collect(&mut inbox).expect("the header arrived");
    let c = room.counts();
    assert_eq!(c.records_dropped, 0);
    assert_eq!(
        c.records_written,
        t + 1,
        "the header and one record per step"
    );
    assert_eq!(log.ticks.len() as u64, t);
    assert_eq!(log.header.seed, room.seed());
    assert!(c.results_ok >= 2, "at least the two loads: {c:?}");
    Live {
        log: Arc::new(log),
        hashes: room.hashes().to_vec(),
        answers,
        codecs: room.calls().codecs().clone(),
        counts: c,
        served,
    }
}

/// An ending of one of the game's calls (it changes the world; the
/// External's answer only goes to its client).
fn game_ending(r: &crate::determinism::Recorded) -> bool {
    r.call != "gsb::External" && r.arrival == crate::determinism::Arrival::Ended
}

fn answers_of(c: &mut crate::determinism::SimClient) -> Vec<gsb_protocol::base::RpcResponse> {
    answers(c)
}

fn served(l: &Live) -> [u64; 2] {
    [
        l.served[0].load(Ordering::Relaxed),
        l.served[1].load(Ordering::Relaxed),
    ]
}

#[test]
fn a_live_run_replays_to_the_same_hashes_and_answers_without_calling_services() {
    let l = live();
    assert_eq!(l.log.hashes(), l.hashes, "the log carries the live hashes");
    assert!(
        l.answers
            .iter()
            .any(|(_, _, a)| a.ok && a.payload == b"pong"),
        "the External request was answered live"
    );
    let before = served(&l);
    for _ in 0..2 {
        let (report, answers) = replay(&l.log);
        assert_eq!(
            report.hashes, l.hashes,
            "every step's state equals the live run's"
        );
        assert_eq!(report.first_divergent, None);
        assert_eq!(answers, l.answers, "the same answers, at the same ticks");
        assert_eq!(report.counts.replay_diverged, 0);
        assert_eq!(report.counts.replay_undecodable, 0);
        // The table never sent (nor refused) anything: every ending came
        // from the log, and the endings went where they went live.
        let c = report.counts;
        assert_eq!(
            c.calls_sent + c.calls_refused_full + c.calls_refused_gone + c.calls_refused_share,
            0
        );
        let fields = |c: &crate::metrics::CallCounts| {
            [
                c.calls_issued,
                c.results_ok,
                c.results_service_error,
                c.responses_delivered,
                c.responses_owner_gone,
                c.answers_sent,
                c.requests_answered,
            ]
        };
        assert_eq!(fields(&c), fields(&l.counts));
    }
    assert_eq!(l.counts.responses_owner_gone, 1, "bob's last charge");
    assert_eq!(
        served(&l),
        before,
        "no service was called during the replays"
    );

    // Negative control: the hashes see arrival timing. Deliver the first
    // recorded RESPONSES that ends a game call one tick later: the replay
    // diverges at exactly that tick. (Not the External's alone: its
    // answer goes to the client and leaves the world as it was — it can
    // come first when the loads are slow.)
    let mut log = Arc::try_unwrap(l.log).expect("sole owner");
    let (i, at) = log
        .ticks
        .iter()
        .enumerate()
        .find_map(|(i, t)| {
            t.inputs
                .iter()
                .position(|x| matches!(x, Input::Responses(list) if list.iter().any(game_ending)))
                .map(|at| (i, at))
        })
        .expect("a response");
    let moved = log.ticks[i].inputs.remove(at);
    log.ticks[i + 1].inputs.insert(0, moved);
    let late_tick = log.ticks[i].tick;
    let (report, _) = replay(&Arc::new(log));
    assert_eq!(
        report.first_divergent,
        Some(late_tick),
        "reported at the moved tick"
    );
}

#[test]
fn a_recorded_ending_of_another_call_type_is_counted_and_never_applied() {
    let l = live();
    let mut log = Arc::try_unwrap(l.log).expect("sole owner");
    let (tick, renamed) = log
        .ticks
        .iter_mut()
        .find_map(|t| {
            let tick = t.tick;
            t.inputs.iter_mut().find_map(|x| match x {
                Input::Responses(list) => list.iter_mut().find(|r| game_ending(r)).map(|r| {
                    r.call = Cow::Borrowed("another::Call");
                    (tick, r.key)
                }),
                _ => None,
            })
        })
        .expect("a response");
    let (report, _) = replay(&Arc::new(log));
    assert!(report.counts.replay_diverged >= 1, "{:?}", report.counts);
    assert_eq!(
        report.first_divergent,
        Some(tick),
        "not applied: {renamed:?}"
    );
}

#[test]
fn a_log_file_replays_like_the_log_in_memory() {
    let l = live();
    let dir = std::env::temp_dir();
    let path = dir.join(format!("gsb-q4-{}-memory.log", std::process::id()));
    let stats = write_log(&path, &l.log, &l.codecs).expect("written");
    assert_eq!(stats.outcomes_unencodable, 0);
    assert_eq!(stats.frames_written, l.log.ticks.len() as u64);
    let read = read_log(&path).expect("read");
    assert_eq!(read.header, l.log.header);
    let (report, answers) = replay(&Arc::new(read));
    assert_eq!(report.hashes, l.hashes);
    assert_eq!(answers, l.answers);
    assert_eq!(report.counts.replay_undecodable, 0);

    // Without the codecs the outcomes are lost — and counted, on both
    // sides.
    let stats = write_log(&path, &l.log, &Codecs::default()).expect("written");
    assert!(stats.outcomes_unencodable > 0);
    let (report, _) = replay(&Arc::new(read_log(&path).expect("read")));
    // The first missing outcome leaves its call pending: the game takes
    // another path from there (later endings may meet no call at all).
    assert!(report.counts.replay_undecodable >= 1, "{:?}", report.counts);
    assert!(report.first_divergent.is_some());
    let _ = std::fs::remove_file(&path);
}

/// A TEST-mode room recorded straight to a file by the writer thread.
#[test]
fn the_writer_thread_writes_what_the_room_records() {
    let path = std::env::temp_dir().join(format!("gsb-q4-{}-writer.log", std::process::id()));
    let mut sim = Sim::new();
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
    let mut room = SimRoom::new(
        config(9),
        World::new(),
        Box::new(Shop {
            bank,
            inv,
            rolls: 0,
            publisher: None,
        }),
    );
    let (recorder, inbox) = Recorder::channel(64, "shop-1");
    let writer = spawn_writer(path.clone(), inbox);
    room.record(recorder);
    let alice = room.join(1, "alice");
    let bob = room.join(2, "bob");
    for t in 1..=30 {
        script(t, &alice, &bob);
        room.tick(&mut sim);
    }
    let hashes = room.hashes().to_vec();
    drop(room); // the recorder goes: the writer ends
    let stats = writer.join().expect("writer thread").expect("written");
    assert_eq!(stats.frames_written, 30);
    assert_eq!(stats.outcomes_unencodable, 0);
    let (report, _) = replay(&Arc::new(read_log(&path).expect("read")));
    assert_eq!(report.hashes, hashes);
    assert_eq!(report.first_divergent, None);
    let _ = std::fs::remove_file(&path);
}

/// A TEST-mode run (fake services), recorded in memory.
fn sim_run(ticks: u64) -> (Arc<InputLog>, Vec<Option<u64>>, Codecs) {
    let mut sim = Sim::new();
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
    let mut room = SimRoom::new(
        config(11),
        World::new(),
        Box::new(Shop {
            bank,
            inv,
            rolls: 0,
            publisher: None,
        }),
    );
    let (recorder, mut inbox) = Recorder::channel(1 << 10, "shop-1");
    room.record(recorder);
    let alice = room.join(1, "alice");
    let bob = room.join(2, "bob");
    for t in 1..=ticks {
        script(t, &alice, &bob);
        room.tick(&mut sim);
    }
    let hashes = room.hashes().to_vec();
    let codecs = room.calls().codecs().clone();
    drop(room);
    let log = InputLog::collect(&mut inbox).expect("the header arrived");
    (Arc::new(log), hashes, codecs)
}

/// B175: the CONTROL answers in the log — a join's admission, a hold's
/// `may_release` — are compared with the replay's own. A different one is
/// `replay_diverged` and reports its tick, even where the state hash
/// cannot see it (here: the world is keyed by the replay's own answer,
/// and `may_release` changes nothing).
#[test]
fn a_different_admission_or_release_answer_is_reported_like_a_hash() {
    let (log, hashes, _) = sim_run(12);
    let (report, _) = replay(&log);
    assert_eq!(report.hashes, hashes);
    assert_eq!(
        (report.first_divergent, report.counts.replay_diverged),
        (None, 0)
    );

    // The admission: the log says bob was admitted as another player.
    let mut tampered = Arc::try_unwrap(log).expect("sole owner");
    let join_tick = tampered.ticks[0].tick;
    let bob = tampered.ticks[0]
        .inputs
        .iter_mut()
        .find_map(|x| match x {
            Input::Join {
                identity, player, ..
            } if identity == "bob" => Some(player),
            _ => None,
        })
        .expect("bob's join is in the first record");
    *bob = crate::id::PlayerId(99);
    let tampered = Arc::new(tampered);
    let (report, _) = replay(&tampered);
    assert_eq!(report.hashes, hashes, "the hash cannot see it");
    assert_eq!(report.first_divergent, Some(join_tick));
    assert_eq!(report.counts.replay_diverged, 1);

    // The release: the log says the logic kept alice parked at tick 6
    // (`Shop` always releases) — also through a file.
    let (log, _, codecs) = sim_run(12);
    let mut tampered = Arc::try_unwrap(log).expect("sole owner");
    let at = tampered.ticks[5].tick;
    tampered.ticks[5].inputs.insert(
        0,
        Input::Release {
            player: crate::id::PlayerId(1),
            released: false,
        },
    );
    let path = std::env::temp_dir().join(format!("gsb-q6-{}-release.log", std::process::id()));
    write_log(&path, &tampered, &codecs).expect("written");
    let read = read_log(&path).expect("read");
    let _ = std::fs::remove_file(&path);
    assert!(matches!(
        read.ticks[5].inputs[0],
        Input::Release {
            released: false,
            ..
        }
    ));
    for log in [Arc::new(tampered), Arc::new(read)] {
        let mut r = Replay::new(log, World::new(), Box::new(nowhere()));
        let report = r.run();
        assert_eq!(report.first_divergent, Some(at));
        assert!(report.counts.replay_diverged >= 1, "{:?}", report.counts);
    }
}

/// B177: a recording TEST-mode room asks the logic its hash once per
/// step (the record's hash is the step's hash).
#[test]
fn a_recording_sim_room_hashes_once_per_step() {
    HASHED.with(|n| n.set(0));
    let (log, hashes, _) = sim_run(10);
    assert_eq!(HASHED.with(|n| n.get()), 10);
    assert_eq!(log.hashes(), hashes, "TEST mode keeps the recorded hash");
}

// B173: a generated service covers its wire-form calls itself.
use crate::calls::CallCx;

/// A wire-form answer.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Receipt {
    #[prost(uint64, tag = "1")]
    pub total: u64,
}
/// A wire-form error.
#[derive(Clone, PartialEq, prost::Message)]
pub struct VaultError {
    #[prost(uint32, tag = "1")]
    pub code: u32,
}

/// No wire form.
#[derive(Clone, Debug)]
pub struct Shelf;

#[crate::service]
pub trait Vault {
    /// Wire-form outcomes (prost messages).
    async fn deposit(&self, cx: &CallCx, price: u64) -> Result<Receipt, VaultError>;
    /// No wire form (`Shelf` is a plain struct; `u64` and `String`
    /// would have one — prost's wrapper messages).
    async fn note(&self, cx: &CallCx, text: String) -> Result<Shelf, String>;
}

/// `calls.record_service::<vault::Kind>()` registers the outcome codec of
/// every method whose `Response` and `Error` have a wire form, and skips
/// the others (written missing, counted) — no `record_as` per type.
#[test]
fn a_generated_service_registers_its_wire_form_calls() {
    let mut calls = crate::calls::Calls::default();
    let deposit = std::any::type_name::<vault::Deposit>();
    let note = std::any::type_name::<vault::Note>();
    assert!(!calls.codecs().covers(deposit));
    calls.record_service::<vault::Kind>();
    assert!(
        calls.codecs().covers(deposit),
        "a wire-form call is covered"
    );
    assert!(!calls.codecs().covers(note), "the other is not");
}
