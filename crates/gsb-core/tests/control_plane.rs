//! The control-plane entry (feature A) invariants:
//!
//! - runtime room lifecycle: create is IDEMPOTENT (the same config twice
//!   yields one room; a different config conflicts), destroy is a no-op
//!   on an absent room, status reports the registry's view, and a
//!   destroyed room's id is reusable;
//! - the match-result exit seam: a room that produces a result reports
//!   it through the registry's result sink on destroy (best effort,
//!   bounded).
//!
//! The registry is driven directly (the same `RegistryMsg` vocabulary
//! the `ServerHandle`'s public API wraps), so these tests lock the
//! control-plane contract the handle is built on.

use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::ConnIn;
use gsb_core::error::CoreError;
use gsb_core::id::{ConnectionId, EntityId, PlayerId, RoomId};
use gsb_core::registry::{
    BuiltRoom, MatchResult, Registry, RegistryMsg, RoomFactory, RoomStatus, Seat,
};
use gsb_core::room::{Action, Admission, GameLogic, RoomConfig, RoomLogic, TickCtx};
use gsb_core::ticker::Ticker;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);
const HZ: f64 = 60.0;

/// A room logic that reports a fixed match result on shutdown (the
/// seam under test).
struct ResultLogic {
    result: Option<Vec<u8>>,
}

impl GameLogic<()> for ResultLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7F00
    }
    fn private_op(&self) -> u16 {
        0x7F01
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
        Admission {
            player: PlayerId(c.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}

    // Faz 3: the result seam lives on the shared `GameLogic` supertrait.
    fn match_result(&mut self, _w: &mut ()) -> Option<bytes::Bytes> {
        self.result.take().map(Into::into)
    }
}

// Faz 3 promotion: `match_result` moved to `GameLogic`; this impl stays
// as the single-room marker.
impl RoomLogic<()> for ResultLogic {}

fn config(id: RoomId) -> RoomConfig {
    RoomConfig {
        id,
        tick_hz: HZ,
        ..Default::default()
    }
}

fn start(
    factory: RoomFactory<(), (), (), ()>,
    result_sink: Option<Mailbox<MatchResult>>,
) -> (Mailbox<RegistryMsg>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _ticker_task) = Ticker::spawn(HZ, 64).expect("valid tick rate");
    let (metrics_tx, _metrics_rx) = mpsc::channel::<gsb_core::metrics::MetricsEvent>(1);
    let handle = tokio::spawn(
        Registry::new(
            rx,
            tx.clone(),
            factory,
            ticker,
            metrics_tx,
            None,
            None,
            result_sink,
        )
        .run(),
    );
    (tx, handle)
}

async fn create(tx: &Mailbox<RegistryMsg>, id: RoomId) -> Result<RoomStatus, CoreError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Result<RoomStatus, CoreError>>();
    tx.send(RegistryMsg::CreateRoom {
        config: config(id),
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
}

async fn create_with(tx: &Mailbox<RegistryMsg>, cfg: RoomConfig) -> Result<RoomStatus, CoreError> {
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

async fn destroy(tx: &Mailbox<RegistryMsg>, id: RoomId) -> RoomStatus {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<RoomStatus>();
    tx.send(RegistryMsg::DestroyRoom {
        id,
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
    tx.send(RegistryMsg::RoomStatus {
        id,
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
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

async fn spawn(
    tx: &Mailbox<RegistryMsg>,
    conn: ConnectionId,
    room: RoomId,
    out: mpsc::Sender<FrameBatch>,
) -> EntityId {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Result<Seat, CoreError>>();
    tx.send(RegistryMsg::SpawnPlayer {
        conn,
        room,
        out,
        // Anonymous: an ordinary fresh join, no ledger lookup.
        // Anonymous: an ordinary fresh join, no ledger lookup.
        identity: String::new(),
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    let Seat { entity, .. } = tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
        .expect("spawn failed");
    entity
}

/// "Aç odayı iki kez" (open the room twice) with the same config: one
/// room, one factory build, both calls Ok — the control plane's retry
/// pattern is structurally safe.
#[tokio::test]
async fn create_idempotent_same_config_builds_once() {
    // The factory counts its own builds (the idempotency oracle): a
    // duplicate create must not invoke it.
    let (build_tx, mut build_rx) = mpsc::unbounded_channel::<()>();
    let factory: RoomFactory<(), (), (), ()> = Arc::new(move |_id, _config| {
        let _ = build_tx.send(());
        BuiltRoom::Single {
            world: (),
            logic: Box::new(ResultLogic { result: None })
                as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
        }
    });
    let (tx, handle) = start(factory, None);
    let id = RoomId(7);

    let s1 = create(&tx, id).await.expect("first create failed");
    assert_eq!(s1, RoomStatus::Running { members: 0 });
    tokio::time::timeout(WAIT, build_rx.recv())
        .await
        .expect("factory was not invoked for the first create");

    let s2 = create(&tx, id).await.expect("duplicate create failed");
    assert_eq!(s2, RoomStatus::Running { members: 0 });
    // The factory must NOT have been invoked a second time.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), build_rx.recv())
            .await
            .is_err(),
        "the factory built the room twice for one idempotent create"
    );
    // Clean registry shutdown (the test holds the only other mailbox
    // clone; without Shutdown the registry would outlive the test).
    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    drop(tx);
    handle.await.unwrap();
}

/// Same id, DIFFERENT config: a conflict (the control plane's typo
/// guard) — the existing room is untouched.
#[tokio::test]
async fn create_conflict_different_config() {
    let factory: RoomFactory<(), (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Single {
        world: (),
        logic: Box::new(ResultLogic { result: None })
            as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    });
    let (tx, handle) = start(factory, None);
    let id = RoomId(8);

    create(&tx, id).await.expect("create failed");

    let different = RoomConfig {
        id,
        tick_hz: HZ,
        max_players: Some(3), // differs from the default
        ..Default::default()
    };
    match create_with(&tx, different).await {
        Err(CoreError::RoomConflict(room)) => assert_eq!(room, id.0),
        other => panic!("expected RoomConflict, got {other:?}"),
    }

    // The original room still runs (the conflict did not destroy it).
    assert_eq!(status(&tx, id).await, RoomStatus::Running { members: 0 });
    // Clean registry shutdown (the test holds the only other mailbox
    // clone; without Shutdown the registry would outlive the test).
    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    drop(tx);
    handle.await.unwrap();
}

/// The full status lifecycle, including the K8s-delete no-op (destroy
/// on an absent room) and id reuse after destruction.
#[tokio::test]
async fn status_lifecycle_absent_running_destroyed_reused() {
    let factory: RoomFactory<(), (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Single {
        world: (),
        logic: Box::new(ResultLogic { result: None })
            as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    });
    let (tx, handle) = start(factory, None);
    let id = RoomId(9);

    // Never created.
    assert_eq!(status(&tx, id).await, RoomStatus::Absent);
    // Destroying an absent room is a no-op (K8s delete semantics).
    assert_eq!(destroy(&tx, id).await, RoomStatus::Absent);

    // Create.
    assert_eq!(
        create(&tx, id).await.expect("create failed"),
        RoomStatus::Running { members: 0 }
    );
    assert_eq!(status(&tx, id).await, RoomStatus::Running { members: 0 });

    // Destroy: the reply says "destroyed" (it existed).
    assert_eq!(destroy(&tx, id).await, RoomStatus::Destroyed);
    // And the status is absent (the room is gone).
    assert_eq!(status(&tx, id).await, RoomStatus::Absent);
    // Destroying it again: a no-op.
    assert_eq!(destroy(&tx, id).await, RoomStatus::Absent);

    // The id is reusable.
    assert_eq!(
        create(&tx, id).await.expect("recreate failed"),
        RoomStatus::Running { members: 0 }
    );
    // Clean registry shutdown (the test holds the only other mailbox
    // clone; without Shutdown the registry would outlive the test).
    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    drop(tx);
    handle.await.unwrap();
}

/// The status's member count is the registry's affiliation view (a
/// spawned player shows up without the registry ever awaiting the
/// room).
#[tokio::test]
async fn status_reports_members() {
    let factory: RoomFactory<(), (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Single {
        world: (),
        logic: Box::new(ResultLogic { result: None })
            as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    });
    let (tx, handle) = start(factory, None);
    let id = RoomId(10);
    create(&tx, id).await.expect("create failed");

    let conn = ConnectionId(1);
    open_conn(&tx, conn).await;
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(64);
    spawn(&tx, conn, id, out_tx).await;

    assert_eq!(status(&tx, id).await, RoomStatus::Running { members: 1 });
    // Clean registry shutdown (the test holds the only other mailbox
    // clone; without Shutdown the registry would outlive the test).
    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    drop(tx);
    handle.await.unwrap();
}

/// The match-result exit seam: a room whose logic produces a result
/// reports it through the registry's result sink on destroy. The sink
/// is a bounded mailbox; the payload is the logic's opaque bytes.
#[tokio::test]
async fn match_result_reports_on_destroy() {
    let payload: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let factory_payload = payload.clone();
    let factory: RoomFactory<(), (), (), ()> = Arc::new(move |_id, _config| BuiltRoom::Single {
        world: (),
        logic: Box::new(ResultLogic {
            result: Some(factory_payload.clone()),
        }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    });
    let (result_tx, mut result_rx) = channel::<MatchResult>(64);
    let (tx, handle) = start(factory, Some(result_tx));
    let id = RoomId(11);

    create(&tx, id).await.expect("create failed");
    assert_eq!(destroy(&tx, id).await, RoomStatus::Destroyed);

    // The result arrives (the room actor sends it on shutdown, which
    // happens on its next tick — bounded by the wait).
    let result = tokio::time::timeout(WAIT, result_rx.recv())
        .await
        .expect("timed out waiting for the match result")
        .expect("result sink closed");
    assert_eq!(result.room, id);
    assert_eq!(result.payload, bytes::Bytes::from(payload));

    // A re-created room reports again on its destroy (the seam is per
    // room lifetime, not per id):
    create(&tx, id).await.expect("recreate failed");
    assert_eq!(destroy(&tx, id).await, RoomStatus::Destroyed);
    let second = tokio::time::timeout(WAIT, result_rx.recv())
        .await
        .expect("timed out waiting for the second match result")
        .expect("result sink closed");
    assert_eq!(second.room, id);
    // An absent-room destroy must not produce a result:
    assert_eq!(destroy(&tx, id).await, RoomStatus::Absent);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), result_rx.recv())
            .await
            .is_err(),
        "an absent-room destroy reported a result"
    );
    // Clean registry shutdown (the test holds the only other mailbox
    // clone; without Shutdown the registry would outlive the test).
    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    drop(tx);
    handle.await.unwrap();
}
