//! Supervision (the death watch) invariants:
//!
//! - a room whose game logic panics inside `update()` is REAPED: the
//!   registry's table drops the entry (status `Absent`), every affiliated
//!   connection is notified exactly like on a destroy
//!   ([`ConnIn::RoomGone`]), and a subsequent join fails with
//!   `RoomNotFound` — no zombie, no dispatches into a dead mailbox;
//! - with [`RoomConfig::restart_on_panic`], the registry rebuilds the room
//!   from the same factory + config; the rebirth comes back EMPTY (the old
//!   member was notified, a fresh join works), and the panic fired only
//!   once (no restart-per-death cycle when the rebuilt logic is healthy);
//! - the death of ANY shard of a sharded room takes down the WHOLE logical
//!   room (cross-shard channels reference the dead shard — no partial
//!   recovery): same reap, same notifications.
//!
//! The registry is driven directly (the same `RegistryMsg` vocabulary the
//! `ServerHandle`'s public API wraps), with a real ticker; determinism
//! comes from the logics under test: they panic only AFTER the events the
//  assertions depend on have happened (a member joined / a deadline passed),
//! never before them.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gsb_core::channel::{channel, FrameBatch, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::error::CoreError;
use gsb_core::id::{ConnectionId, EntityId, PlayerId, RoomId};
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory, RoomStatus};
use gsb_core::room::{Action, Admission, GameLogic, RoomConfig, RoomLogic, TickCtx};
use gsb_core::shard::{BorderRecord, Migrating, ShardLogic};
use gsb_core::ticker::Ticker;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);
const HZ: f64 = 60.0;

/// A single-room logic that detonates on the first update AFTER its first
/// member joined (`on_join` runs in the tick's CONTROL phase, the panic in
/// SYSTEMS of the same step — so the join reply is already out when the
/// task dies, which is exactly the production shape of "game logic panicked
/// mid-tick"). The flag lives in the logic itself: each incarnation owns
/// its state, nothing is shared.
struct PanicAfterJoinLogic {
    joined: bool,
    /// False for a healthy (rebuilt) incarnation.
    armed: bool,
}

impl GameLogic<()> for PanicAfterJoinLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7E00
    }
    fn private_op(&self) -> u16 {
        0x7E01
    }

    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }

    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        self.joined = true;
        Admission {
            player: PlayerId(c.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        if self.joined && self.armed {
            panic!("supervision test: game logic exploded in update()");
        }
    }
}

impl RoomLogic<()> for PanicAfterJoinLogic {}

/// A shard logic that detonates at a wall-clock deadline it OWNS (moved in
/// at construction — no shared state): ticks before the deadline are
/// harmless, so the test can join its neighbor's members deterministically
/// first, then watch the whole logical room die from this one shard.
struct TimeBombShardLogic {
    index: usize,
    detonate_at: Option<Instant>,
}

impl GameLogic<()> for TimeBombShardLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7D00
    }
    fn private_op(&self) -> u16 {
        0x7D01
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _ctx: &TickCtx,
        _g: &(),
        _borrowed: &[BorderRecord<()>],
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(c.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _ctx: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut (), _ctx: &TickCtx) {
        if let Some(at) = self.detonate_at
            && Instant::now() >= at
        {
            panic!("shard {} detonated", self.index);
        }
    }
}

// Faz 1 trait split: the sharding seam stays on `ShardLogic`.
impl ShardLogic<()> for TimeBombShardLogic {
    type State = ();

    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_base(&self) -> u64 {
        self.index as u64 * 1000
    }
    fn serial_range(&self) -> u64 {
        1000
    }
    fn serial_used(&self) -> u64 {
        0
    }
    fn neighbors(&self) -> &[usize] {
        // No cross-shard traffic needed for the death semantics under test:
        // the registry's wiring treats an empty topology as all-dummy
        // senders, which is fine here.
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut (), _neighbor: usize) -> Vec<Migrating<()>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut (), _wire: u64, _state: (), _player: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut (), _wire: u64) {}
    fn collect_border(&self, _w: &()) -> Vec<BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        Vec::new()
    }
}

fn config(id: RoomId, restart_on_panic: bool) -> RoomConfig {
    RoomConfig {
        id,
        tick_hz: HZ,
        restart_on_panic,
        ..Default::default()
    }
}

fn start(
    factory: RoomFactory<(), (), (), ()>,
) -> (Mailbox<RegistryMsg>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _ticker_task) = Ticker::spawn(HZ, 64).expect("valid tick rate");
    let (metrics_tx, _metrics_rx) = mpsc::channel::<gsb_core::metrics::MetricsEvent>(1);
    let handle =
        tokio::spawn(Registry::new(rx, tx.clone(), factory, ticker, metrics_tx, None, None, None).run());
    (tx, handle)
}

async fn create_with(
    tx: &Mailbox<RegistryMsg>,
    cfg: RoomConfig,
) -> Result<RoomStatus, CoreError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Result<RoomStatus, CoreError>>();
    tx.send(RegistryMsg::CreateRoom {
        config: cfg,
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
}

async fn status(tx: &Mailbox<RegistryMsg>, id: RoomId) -> RoomStatus {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<RoomStatus>();
    tx.send(RegistryMsg::RoomStatus { id, reply: reply_tx })
        .await
        .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
}

/// Poll the status until `pred` holds (the death report travels through two
/// spawned tasks before the table changes — bounded by the wait).
async fn status_until(tx: &Mailbox<RegistryMsg>, id: RoomId, pred: impl Fn(&RoomStatus) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        let s = status(tx, id).await;
        if pred(&s) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "room {id} never reached the expected status; last = {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Open a connection and hand the test its inbox receiver (so the test can
/// observe what the registry sends it — the notification path under test).
async fn open_conn(
    tx: &Mailbox<RegistryMsg>,
    conn: ConnectionId,
) -> mpsc::Receiver<ConnIn> {
    let (inbox_tx, inbox_rx) = mpsc::channel::<ConnIn>(16);
    tx.send(RegistryMsg::ConnOpened {
        conn,
        inbox: inbox_tx,
    })
    .await
    .expect("registry gone");
    inbox_rx
}

async fn spawn(
    tx: &Mailbox<RegistryMsg>,
    conn: ConnectionId,
    room: RoomId,
) -> Result<EntityId, CoreError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<
        Result<(EntityId, Mailbox<Action>), CoreError>,
    >();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(64);
    tx.send(RegistryMsg::SpawnPlayer {
        conn,
        room,
        out: out_tx,
        // Anonymous: an ordinary fresh join, no ledger lookup.
        // Anonymous: an ordinary fresh join, no ledger lookup.
        identity: String::new(),
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    match tokio::time::timeout(WAIT, reply_rx).await {
        Ok(Ok(result)) => result.map(|(entity, _actions)| entity),
        other => panic!("spawn round trip failed: {other:?}"),
    }
}

async fn spawn_ok(tx: &Mailbox<RegistryMsg>, conn: ConnectionId, room: RoomId) -> EntityId {
    spawn(tx, conn, room)
        .await
        .expect("join should succeed")
}

async fn expect_room_gone(inbox_rx: &mut mpsc::Receiver<ConnIn>, id: RoomId) {
    match tokio::time::timeout(WAIT, inbox_rx.recv()).await {
        Ok(Some(ConnIn::RoomGone(gone))) => assert_eq!(gone, id),
        other => panic!("expected ConnIn::RoomGone({id}), got {other:?}"),
    }
}

/// Clean shutdown (the test hands over its mailbox clone; without Shutdown
/// the registry would outlive the test — it breaks its loop on the message,
/// so the sender must simply not be resurrected afterwards).
async fn stop(tx: Mailbox<RegistryMsg>, handle: tokio::task::JoinHandle<()>) {
    tx.send(RegistryMsg::Shutdown)
        .await
        .expect("registry gone");
    drop(tx);
    handle.await.unwrap();
}

/// A panicking single room is reaped: status Absent, the member notified
/// like on a destroy, and a subsequent join answered RoomNotFound (never
/// dispatched into the dead control channel).
#[tokio::test]
async fn panicking_room_is_removed_and_members_notified() {
    let factory: RoomFactory<(), (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Single {
        world: (),
        logic: Box::new(PanicAfterJoinLogic {
            joined: false,
            armed: true,
        }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    });
    let (tx, handle) = start(factory);
    let id = RoomId(21);

    create_with(&tx, config(id, false))
        .await
        .expect("create failed");

    // The observer/member joins BEFORE the panic (its join reply precedes
    // the detonation within the same tick — see the logic's contract).
    let mut member_inbox = open_conn(&tx, ConnectionId(1)).await;
    spawn_ok(&tx, ConnectionId(1), id).await;

    // (a) The table drops the room after the death report arrives.
    status_until(&tx, id, |s| matches!(s, RoomStatus::Absent)).await;

    // (b) The member was notified exactly like on a destroy.
    expect_room_gone(&mut member_inbox, id).await;

    // (c) A subsequent join fails with RoomNotFound — synchronously, at
    // dispatch (nothing hangs on a dead control channel).
    open_conn(&tx, ConnectionId(2)).await;
    match spawn(&tx, ConnectionId(2), id).await {
        // RECONNECT §8: the unrebuilt death RETIRES the id — the join
        // answers ERROR 12 ("definitively over"), not the unknown code.
        Err(CoreError::RoomRetired(room)) => assert_eq!(room, id.0),
        other => panic!("expected RoomRetired after the death, got {other:?}"),
    }

    stop(tx, handle).await;
}

/// With `restart_on_panic`, the registry rebuilds the room from the same
/// factory + config: it comes back EMPTY (the old member was notified, a
/// fresh join works), the factory ran exactly twice (initial + one rebirth
/// — no restart-per-death cycle when the rebuilt logic is healthy).
#[tokio::test]
async fn restarted_room_comes_back_when_policy_enabled() {
    // Which incarnation the factory is building (an atomic only because a
    // factory is a plain shared `Fn`; lock-free by construction).
    let build_n = AtomicU64::new(0);
    // Every build observed by the test through a channel (the codebase's
    // observation idiom — no shared counters read across tasks).
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<u64>();
    let factory: RoomFactory<(), (), (), ()> = Arc::new(move |_id, _config| {
        let n = build_n.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = seen_tx.send(n);
        BuiltRoom::Single {
            world: (),
            // Only the FIRST incarnation carries the bomb: the rebirth must
            // survive (the panic fires once, not forever).
            logic: Box::new(PanicAfterJoinLogic {
                joined: false,
                armed: n == 1,
            }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
        }
    });
    let (tx, handle) = start(factory);
    let id = RoomId(22);

    create_with(&tx, config(id, true))
        .await
        .expect("create failed");

    // Incarnation 1 takes a member, then dies on its next tick.
    let mut member_inbox = open_conn(&tx, ConnectionId(1)).await;
    spawn_ok(&tx, ConnectionId(1), id).await;

    // The old member learns the room is gone...
    expect_room_gone(&mut member_inbox, id).await;
    // ...and the SAME id answers Running again (the rebirth is synchronous
    // with the reap, so the table never rests at Absent for this query).
    status_until(&tx, id, |s| matches!(s, RoomStatus::Running { members: 0 }))
        .await;

    // Exactly two builds happened: initial + one rebirth (drain what is
    // there, then require quiet — a third build would mean the rebuilt
    // logic died too, i.e. a crash loop).
    let mut builds = Vec::new();
    while let Ok(n) = seen_rx.try_recv() {
        builds.push(n);
    }
    assert_eq!(builds, vec![1, 2], "factory built the wrong incarnations");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        seen_rx.try_recv().is_err(),
        "the rebuilt room died again: unexpected crash loop"
    );

    // The rebirth is EMPTY but fully alive: a new entity joins it.
    open_conn(&tx, ConnectionId(2)).await;
    spawn_ok(&tx, ConnectionId(2), id).await;
    assert_eq!(
        status(&tx, id).await,
        RoomStatus::Running { members: 1 },
        "the rebuilt room did not accept the new member"
    );

    stop(tx, handle).await;
}

/// The death of ANY shard breaks the WHOLE logical room (its neighbors hold
/// senders into the dead shard's closed mailbox — no partial recovery):
/// the room is reaped like a single-room death and its members notified.
#[tokio::test]
async fn shard_death_takes_down_the_whole_logical_room() {
    // Shard 1 detonates ~300 ms after creation; shard 0 stays healthy. The
    // deadline lives in the shard's own logic (moved in, not shared), and
    // the margin makes the join-before-death order deterministic without
    // gating the panic on membership.
    let factory: RoomFactory<(), (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Sharded {
        shards: vec![
            (
                (),
                Box::new(TimeBombShardLogic {
                    index: 0,
                    detonate_at: None,
                }) as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
            ),
            (
                (),
                Box::new(TimeBombShardLogic {
                    index: 1,
                    detonate_at: Some(Instant::now() + Duration::from_millis(300)),
                }) as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
            ),
        ],
        // Every join homes to shard 0 (pure router; the death comes from
        // the OTHER shard — the interesting direction).
        home_shard: Arc::new(|_conn| 0),
    });
    let (tx, handle) = start(factory);
    let id = RoomId(23);

    create_with(&tx, config(id, false))
        .await
        .expect("create failed");

    let mut member_inbox = open_conn(&tx, ConnectionId(1)).await;
    spawn_ok(&tx, ConnectionId(1), id).await;

    // One dead shard → the whole logical room reports Absent...
    status_until(&tx, id, |s| matches!(s, RoomStatus::Absent)).await;
    // ...its members are notified...
    expect_room_gone(&mut member_inbox, id).await;
    // ...and the room cannot be rejoined.
    match spawn(&tx, ConnectionId(1), id).await {
        // RECONNECT §8: an unrebuilt logical death retires the id (ERROR 12).
        Err(CoreError::RoomRetired(room)) => assert_eq!(room, id.0),
        other => panic!("expected RoomRetired after the shard death, got {other:?}"),
    }

    stop(tx, handle).await;
}
