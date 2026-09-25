//! Server stop must always complete (BACKLOG §1 row 4a).
//!
//! The hang: `ServerHandle::stop` enqueues the registry's `Shutdown` and
//! aborts the ticker at once. When more members disconnect at that moment
//! than a room's control channel holds, their routed DETACHes fill it, and
//! the room never drains it again (no ticks). A registry that then awaited
//! a bounded send of the room's `Shutdown` into that full channel waited
//! forever — and, because it holds a `Ticker`, the broadcast never closed
//! either, so neither the rooms nor the metrics collector (the thing
//! `stop()` awaits last) could ever see the global stop signal.
//!
//! Each test pins the worst interleaving deterministically: the ticker is
//! aborted FIRST (the room can no longer drain), then every member's
//! transport dies (`ConnClosed` → a routed DETACH per member), then — once
//! the detaches have filled the channel and parked on it — the registry
//! gets its message. The assertions are exactly what `stop()` depends on:
//! the registry task ends, the tick broadcast closes, and every room (or
//! shard) still runs its teardown (observed through its match result).

use std::sync::Arc;
use std::time::{Duration, Instant};

use gsb_core::channel::{FrameBatch, Inbox, Mailbox, channel};
use gsb_core::conn::ConnIn;
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::registry::{BuiltRoom, MatchResult, Registry, RegistryMsg, RoomFactory, RoomStatus};
use gsb_core::room::{Action, RoomConfig, RoomLogic};
use gsb_core::shard::ShardLogic;
use gsb_core::ticker::{TickInfo, Ticker};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

// Child module of this test binary (an integration-test root is its own
// crate root, so the path is explicit).
#[path = "shutdown/logic.rs"]
mod logic;
use logic::{Quiet, QuietShard};

const WAIT: Duration = Duration::from_secs(5);
const HZ: f64 = 60.0;
/// The rooms' control capacity: far below the member count, so the
/// disconnect burst overfills it (production: 128 vs. 500 members).
const CAP: usize = 2;
/// Live members at stop time (> CAP by a wide margin).
const MEMBERS: u64 = 12;
const ROOM: RoomId = RoomId(40);

/// A registry with a real ticker, plus the handles `stop()` relies on.
struct Rig {
    tx: Mailbox<RegistryMsg>,
    registry: JoinHandle<()>,
    ticker_task: JoinHandle<()>,
    /// The metrics collector's stand-in: a subscription to the tick
    /// broadcast, which `stop()` effectively waits on to close.
    clock: broadcast::Receiver<TickInfo>,
    results: Inbox<MatchResult>,
    /// Live connections' inboxes (kept so the shutdown notices land).
    _inboxes: Vec<mpsc::Receiver<ConnIn>>,
}

fn start(factory: RoomFactory<(), (), (), ()>) -> Rig {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, ticker_task) = Ticker::spawn(HZ, 64).expect("valid tick rate");
    let clock = ticker.subscribe();
    let (metrics_tx, _) = mpsc::channel::<gsb_core::metrics::MetricsEvent>(1);
    let (result_tx, results) = channel::<MatchResult>(16);
    // The registry gets the ONLY `Ticker` clone besides the ticker task
    // itself — the composition root's exact shape.
    let registry = tokio::spawn(
        Registry::new(
            rx,
            tx.clone(),
            factory,
            ticker,
            metrics_tx,
            None,
            None,
            Some(result_tx),
        )
        .run(),
    );
    Rig {
        tx,
        registry,
        ticker_task,
        clock,
        results,
        _inboxes: Vec::new(),
    }
}

async fn ask<T>(tx: &Mailbox<RegistryMsg>, msg: RegistryMsg, rx: oneshot::Receiver<T>) -> T {
    tx.send(msg).await.expect("registry gone");
    tokio::time::timeout(WAIT, rx)
        .await
        .expect("the registry never answered (blocked in a room send?)")
        .expect("reply dropped")
}

/// Create the room, then join `MEMBERS` connections and wait until the
/// registry's table carries all of them.
async fn populate(rig: &mut Rig) {
    let config = RoomConfig {
        id: ROOM,
        tick_hz: HZ,
        control_capacity: CAP,
        ..Default::default()
    };
    let (reply, rx) = oneshot::channel();
    ask(&rig.tx, RegistryMsg::CreateRoom { config, reply }, rx)
        .await
        .expect("create failed");
    for c in 1..=MEMBERS {
        let conn = ConnectionId(c);
        let (inbox, inbox_rx) = mpsc::channel::<ConnIn>(16);
        rig._inboxes.push(inbox_rx);
        let msg = RegistryMsg::ConnOpened { conn, inbox };
        rig.tx.send(msg).await.expect("registry gone");
        let (out, _out_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply, rx) = oneshot::channel::<Result<(EntityId, Mailbox<Action>), _>>();
        let msg = RegistryMsg::SpawnPlayer {
            conn,
            room: ROOM,
            out,
            identity: String::new(),
            reply,
        };
        ask(&rig.tx, msg, rx).await.expect("join failed");
    }
    let deadline = Instant::now() + WAIT;
    loop {
        let (reply, rx) = oneshot::channel();
        let status = ask(&rig.tx, RegistryMsg::RoomStatus { id: ROOM, reply }, rx).await;
        if status
            == (RoomStatus::Running {
                members: MEMBERS as u32,
            })
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "members never settled: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Stop the clock, then kill every member's transport and let the routed
/// detaches overfill the (now undrained) control channel.
async fn disconnect_all_without_ticks(rig: &mut Rig) {
    rig.ticker_task.abort();
    let _ = (&mut rig.ticker_task).await;
    for c in 1..=MEMBERS {
        let msg = RegistryMsg::ConnClosed {
            conn: ConnectionId(c),
        };
        rig.tx.send(msg).await.expect("registry gone");
    }
    // Every dispatcher runs until it parks on the full channel.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// What `stop()` waits on: the registry ends, the broadcast closes, and
/// each of the `rooms` actors ran its teardown.
async fn expect_stopped(mut rig: Rig, rooms: usize) {
    tokio::time::timeout(WAIT, rig.registry)
        .await
        .expect("the registry hung in its teardown (bounded send into a full control channel)")
        .expect("registry panicked");
    let closed = async {
        loop {
            if let Err(broadcast::error::RecvError::Closed) = rig.clock.recv().await {
                return;
            }
        }
    };
    tokio::time::timeout(WAIT, closed)
        .await
        .expect("the tick broadcast never closed: stop() would hang");
    for _ in 0..rooms {
        tokio::time::timeout(WAIT, rig.results.recv())
            .await
            .expect("a room never ran its teardown")
            .expect("result sink closed early");
    }
}

fn single_room() -> RoomFactory<(), (), (), ()> {
    Arc::new(|_id, _config| BuiltRoom::Single {
        world: (),
        logic: Box::new(Quiet) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    })
}

fn sharded_room() -> RoomFactory<(), (), (), ()> {
    Arc::new(|_id, _config| {
        let shard = |index| {
            let logic = Box::new(QuietShard { index });
            (
                (),
                logic as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
            )
        };
        BuiltRoom::Sharded {
            shards: vec![shard(0), shard(1)],
            home_shard: Arc::new(|conn: ConnectionId, _identity: &str| (conn.0 % 2) as usize),
        }
    })
}

#[tokio::test]
async fn stop_completes_when_a_room_control_channel_is_full() {
    let mut rig = start(single_room());
    populate(&mut rig).await;
    disconnect_all_without_ticks(&mut rig).await;
    rig.tx
        .send(RegistryMsg::Shutdown)
        .await
        .expect("registry gone");
    expect_stopped(rig, 1).await;
}

#[tokio::test]
async fn stop_completes_when_shard_mailboxes_are_full() {
    let mut rig = start(sharded_room());
    populate(&mut rig).await;
    disconnect_all_without_ticks(&mut rig).await;
    rig.tx
        .send(RegistryMsg::Shutdown)
        .await
        .expect("registry gone");
    expect_stopped(rig, 2).await;
}

/// The destroy path carried the same bounded send: a `DestroyRoom` queued
/// ahead of the stop (an ops call racing it) blocked the registry before
/// it even reached `Shutdown`. The registry must answer at once.
#[tokio::test]
async fn destroy_answers_when_the_room_control_channel_is_full() {
    let mut rig = start(single_room());
    populate(&mut rig).await;
    disconnect_all_without_ticks(&mut rig).await;
    let (reply, rx) = oneshot::channel();
    let status = ask(&rig.tx, RegistryMsg::DestroyRoom { id: ROOM, reply }, rx).await;
    assert_eq!(status, RoomStatus::Destroyed);
    rig.tx
        .send(RegistryMsg::Shutdown)
        .await
        .expect("registry gone");
    expect_stopped(rig, 1).await;
}
