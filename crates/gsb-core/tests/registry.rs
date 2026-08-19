//! Registry actor tests: join/leave/rejoin sequencing, non-blocking control
//! plane, room destruction notifications.
//!
//! The room-side logic ([`SeqLogic`]) is a counting stand-in: it tracks
//! `conn → entity` and broadcasts the current player count once per tick,
//! so room-side state changes are observable over the fan-out channel.
//!
//! Rooms are driven by a real [`Ticker`] (60 Hz), and each room runs at
//! 60 Hz (`run_every = 1`), so joins/leaves are processed on real tick
//! boundaries.

use std::collections::HashMap;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::ConnIn;
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::registry::{Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{Action, RoomConfig, RoomLogic, TickCtx};
use gsb_core::ticker::Ticker;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);
const COUNT_OP: u16 = 0x7E00;
/// Global (and room) tick rate: room rate divides global, run_every = 1.
const HZ: f64 = 60.0;

/// Test room logic: one integer "entity" per connection; the single-group
/// snapshot carries the current player count, re-emitted only when the
/// count changes (a membership change is a change, per the room contract).
struct SeqLogic {
    next: u64,
    conn_entity: HashMap<ConnectionId, EntityId>,
    last_count: usize,
}

impl SeqLogic {
    fn new() -> Self {
        Self {
            next: 0,
            conn_entity: HashMap::new(),
            last_count: 0,
        }
    }
}

impl RoomLogic<()> for SeqLogic {
    type GroupKey = ();

    fn snapshot_op(&self) -> u16 {
        COUNT_OP
    }
    fn private_op(&self) -> u16 {
        COUNT_OP + 1
    }

    fn group_of(&self, _w: &(), _conn: ConnectionId) -> Self::GroupKey {
        Default::default()
    }

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &Self::GroupKey,
        out: &mut bytes::BytesMut,
    ) -> bool {
        let count = self.conn_entity.len();
        if count == self.last_count {
            return false; // unchanged (membership is the whole state here)
        }
        self.last_count = count;
        out.extend_from_slice(&(count as u32).to_le_bytes());
        true
    }

    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> EntityId {
        self.next += 1;
        self.conn_entity.insert(conn, self.next);
        self.next
    }

    fn on_leave(&mut self, _w: &mut (), conn: ConnectionId) {
        self.conn_entity.remove(&conn);
    }

    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }

    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

fn factory() -> RoomFactory<(), ()> {
    std::sync::Arc::new(|_id, _config| (
        (),
        Box::new(SeqLogic::new()) as Box<dyn RoomLogic<(), GroupKey = ()>>,
    ))
}

fn start_registry() -> (Mailbox<RegistryMsg>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _ticker_task) = Ticker::spawn(HZ, 64);
    // Metrics path: sender only; the dropped receiver makes the registry's
    // sample sends fail (ignored) — these tests cover control-plane
    // behaviour, the metric path has its own tests.
    let (metrics_tx, _metrics_rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(
        Registry::new(rx, tx.clone(), factory(), ticker, metrics_tx).run(),
    );
    (tx, handle)
}

async fn create_room(tx: &Mailbox<RegistryMsg>, id: RoomId) {
    let (reply_tx, reply_rx) =
        tokio::sync::oneshot::channel::<Result<RoomId, gsb_core::error::CoreError>>();
    tx.send(RegistryMsg::CreateRoom {
        config: RoomConfig {
            id,
            tick_hz: HZ,
            ..Default::default()
        },
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
        .expect("room creation failed");
}

async fn spawn(
    tx: &Mailbox<RegistryMsg>,
    conn: ConnectionId,
    room: RoomId,
    out: mpsc::Sender<FrameBatch>,
) -> EntityId {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<
        Result<(EntityId, Mailbox<Action>), gsb_core::error::CoreError>,
    >();
    tx.send(RegistryMsg::SpawnPlayer {
        conn,
        room,
        out,
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    let (entity, _actions) = tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
        .expect("spawn failed");
    entity
}

/// Wait (at tick rate) until a batch reports the expected player count.
/// Reading "the next batch" would race the room's processing, so we keep
/// draining until the count we expect is actually observed.
async fn wait_count(rx: &mut mpsc::Receiver<FrameBatch>, expected: u32) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let batch = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for count {expected}"))
            .expect("out channel closed");
        let Some(frame) = batch.iter().find(|f| f.op == COUNT_OP) else {
            panic!("batch without count frame");
        };
        let c = u32::from_le_bytes(frame.payload[..4].try_into().expect("4-byte count"));
        if c == expected {
            return;
        }
    }
}

async fn open_conn(tx: &Mailbox<RegistryMsg>, conn: ConnectionId) {
    let (inbox_tx, _inbox_rx) = mpsc::channel::<ConnIn>(16);
    tx.send(RegistryMsg::ConnOpened {
        conn,
        inbox: inbox_tx,
    })
    .await
    .expect("registry gone");
}

#[tokio::test]
async fn join_leave_rejoin_sequence() {
    let (tx, handle) = start_registry();
    create_room(&tx, RoomId(1)).await;

    // An observer connection stays in the room for the whole test; the
    // player count is read from *its* channel (a leaver's fan-out channel
    // is closed by design, so it cannot observe post-leave state).
    let c_obs = ConnectionId(99);
    open_conn(&tx, c_obs).await;
    let (out_tx_obs, mut out_rx_obs) = mpsc::channel::<FrameBatch>(64);
    let e_obs = spawn(&tx, c_obs, RoomId(1), out_tx_obs).await;
    wait_count(&mut out_rx_obs, 1).await;

    let c1 = ConnectionId(100);
    open_conn(&tx, c1).await;

    // Join.
    let (out_tx1, _out_rx1) = mpsc::channel::<FrameBatch>(64);
    let e1 = spawn(&tx, c1, RoomId(1), out_tx1).await;
    assert!(e1 > e_obs);
    wait_count(&mut out_rx_obs, 2).await; // observer + c1

    // Leave (voluntary): the connection stays registered, the count drops.
    tx.send(RegistryMsg::DespawnPlayer { conn: c1 })
        .await
        .unwrap();
    wait_count(&mut out_rx_obs, 1).await; // only the observer remains

    // Rejoin: a fresh entity, and the count must end at exactly two — a
    // stale leave (overlapping the rejoin) would drive it back to one.
    let (out_tx2, _out_rx2) = mpsc::channel::<FrameBatch>(64);
    let e2 = spawn(&tx, c1, RoomId(1), out_tx2).await;
    assert_ne!(e1, e2, "rejoin creates a fresh entity");
    wait_count(&mut out_rx_obs, 2).await; // rejoined player survives

    // The control plane still works after the churn: a second connection
    // joins the same room without any stalling.
    let c2 = ConnectionId(101);
    open_conn(&tx, c2).await;
    let (out_tx3, _out_rx3) = mpsc::channel::<FrameBatch>(64);
    let e3 = spawn(&tx, c2, RoomId(1), out_tx3).await;
    assert!(e3 > e2);
    wait_count(&mut out_rx_obs, 3).await;

    // ConnClosed removes the entries entirely (no notification path remains).
    tx.send(RegistryMsg::ConnClosed { conn: c1 }).await.unwrap();
    tx.send(RegistryMsg::ConnClosed { conn: c2 }).await.unwrap();
    tx.send(RegistryMsg::ConnClosed { conn: c_obs })
        .await
        .unwrap();

    // Shutdown must finish everything: registry handle resolves.
    tx.send(RegistryMsg::Shutdown).await.unwrap();
    drop(tx);
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not shut down")
        .expect("registry task panicked");
}

#[tokio::test]
async fn destroy_room_notifies_players_and_rejects_new_joins() {
    let (tx, handle) = start_registry();
    create_room(&tx, RoomId(1)).await;

    let c1 = ConnectionId(200);
    let (inbox_tx, mut inbox_rx) = mpsc::channel::<ConnIn>(16);
    tx.send(RegistryMsg::ConnOpened {
        conn: c1,
        inbox: inbox_tx,
    })
    .await
    .unwrap();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(64);
    spawn(&tx, c1, RoomId(1), out_tx).await;

    // Destroy the room: the player is notified, and the *inbox must survive*
    // (kept on the entry, not dropped) so later Shutdown still reaches it.
    tx.send(RegistryMsg::DestroyRoom { id: RoomId(1) })
        .await
        .unwrap();
    let msg = tokio::time::timeout(WAIT, inbox_rx.recv())
        .await
        .expect("timed out")
        .expect("inbox closed");
    assert!(
        matches!(msg, ConnIn::RoomGone(r) if r == RoomId(1)),
        "player must be notified of room destruction"
    );

    // Joining a destroyed room fails cleanly.
    let (out_tx2, _out_rx2) = mpsc::channel::<FrameBatch>(64);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<
        Result<(EntityId, Mailbox<Action>), gsb_core::error::CoreError>,
    >();
    tx.send(RegistryMsg::SpawnPlayer {
        conn: c1,
        room: RoomId(1),
        out: out_tx2,
        reply: reply_tx,
    })
    .await
    .unwrap();
    let err = tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
        .expect_err("join of destroyed room must fail");
    assert!(matches!(err, gsb_core::error::CoreError::RoomNotFound(1)));

    tx.send(RegistryMsg::Shutdown).await.unwrap();
    drop(tx);
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not shut down")
        .expect("registry task panicked");
}

#[tokio::test]
async fn create_room_rejects_rate_that_does_not_divide_global() {
    let (tx, handle) = start_registry();
    let (reply_tx, reply_rx) =
        tokio::sync::oneshot::channel::<Result<RoomId, gsb_core::error::CoreError>>();
    tx.send(RegistryMsg::CreateRoom {
        config: RoomConfig {
            id: RoomId(7),
            tick_hz: 22.0, // 22 does not divide 60
            ..Default::default()
        },
        reply: reply_tx,
    })
    .await
    .unwrap();
    let err = tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
        .expect_err("non-dividing room rate must be rejected");
    assert!(matches!(err, gsb_core::error::CoreError::TickRate { .. }));

    // A rate that *does* divide is accepted (60/2 = 30 → run_every 2).
    create_room(&tx, RoomId(8)).await;

    tx.send(RegistryMsg::Shutdown).await.unwrap();
    drop(tx);
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not shut down")
        .expect("registry task panicked");
}

#[tokio::test]
async fn create_room_rejects_keepalive_above_tick_rate() {
    let (tx, handle) = start_registry();

    // keep-alive 120 Hz on a 60 Hz room (HZ): the room cannot keep alive
    // faster than it ticks, and the old behavior (silent clamp to every
    // step, killing the silence gain) must not happen — reject instead.
    let (reply_tx, reply_rx) =
        tokio::sync::oneshot::channel::<Result<RoomId, gsb_core::error::CoreError>>();
    tx.send(RegistryMsg::CreateRoom {
        config: RoomConfig {
            id: RoomId(21),
            tick_hz: HZ,
            keepalive_hz: 120.0,
            ..Default::default()
        },
        reply: reply_tx,
    })
    .await
    .unwrap();
    let err = tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
        .expect_err("keep-alive above the tick rate must be rejected");
    assert!(matches!(
        err,
        gsb_core::error::CoreError::KeepaliveRate {
            keepalive: 120.0,
            tick: 60.0
        }
    ));

    // The boundary and the usual case are accepted: keep-alive == tick is
    // exactly "one keep-alive per step" (what was configured), keep-alive
    // < tick is the default setup, and 0 disables.
    for (id, keepalive) in [(RoomId(22), HZ), (RoomId(23), 1.0), (RoomId(24), 0.0)] {
        let (reply_tx, reply_rx) =
            tokio::sync::oneshot::channel::<Result<RoomId, gsb_core::error::CoreError>>();
        tx.send(RegistryMsg::CreateRoom {
            config: RoomConfig {
                id,
                tick_hz: HZ,
                keepalive_hz: keepalive,
                ..Default::default()
            },
            reply: reply_tx,
        })
        .await
        .unwrap();
        tokio::time::timeout(WAIT, reply_rx)
            .await
            .expect("timed out")
            .expect("reply dropped")
            .expect("keepalive_hz <= tick_hz must be accepted");
    }

    tx.send(RegistryMsg::Shutdown).await.unwrap();
    drop(tx);
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not shut down")
        .expect("registry task panicked");
}
